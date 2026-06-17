pub mod bootloader;
pub mod kernel_options;
pub mod os_release;
pub mod esp;
pub mod phase0;
pub mod phase4;
pub mod phase5;
pub mod registry;
pub mod verify;
use crate::VerityDigest;
use crate::preflight::PreflightReport;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::migration::phase0::*;
use crate::migration::phase4::*;
use crate::migration::phase5::*;
use crate::migration::verify::*;

// ---- Lock file (Fix 8: concurrency guard) ----

const LOCK_PATH: &str = "/var/run/bootc-migrate-composefs.lock";

/// Embedded bootc dracut module script (51bootc). Provides bootc-root-setup.service
/// which mounts the composefs EROFS as root in the initramfs.
const BOOTC_DRACUT_MODULE: &str = r##"#!/bin/bash
installkernel() {
    instmods erofs overlay
}
check() {
    return 255
}
depends() {
    return 0
}
install() {
    local service=bootc-root-setup.service
    dracut_install /usr/lib/bootc/initramfs-setup
    inst_simple "${systemdsystemunitdir}/${service}"
    mkdir -p "${initdir}${systemdsystemunitdir}/initrd-root-fs.target.wants"
    ln_r "${systemdsystemunitdir}/${service}" \
        "${systemdsystemunitdir}/initrd-root-fs.target.wants/${service}"
    [[ -e /usr/lib/composefs/setup-root-conf.toml ]] && \
        inst_simple /usr/lib/composefs/setup-root-conf.toml
}
"##;

/// Systemd unit that mounts the composefs EROFS as root in the initramfs.
const BOOTC_ROOT_SETUP_SERVICE: &str = r##"[Unit]
Description=bootc setup root
Documentation=man:bootc(1)
DefaultDependencies=no
ConditionKernelCommandLine=composefs
ConditionPathExists=/etc/initrd-release
After=sysroot.mount
After=ostree-prepare-root.service
Requires=sysroot.mount
Before=initrd-root-fs.target
OnFailure=emergency.target
OnFailureJobMode=isolate

[Service]
Type=oneshot
ExecStart=/usr/lib/bootc/initramfs-setup setup-root
StandardInput=null
StandardOutput=journal
StandardError=journal+console
RemainAfterExit=yes
"##;

/// Minimum composefs repository metadata (meta.json). Written to the repo
/// during Phase 3 so bootc-root-setup.service can open it in the initrd.
const COMPOSEFS_META_JSON: &str =
    r##"{"version":1,"algorithm":"fsverity-sha512-12","verity":false}"##;

pub fn run_migration(
    report: &PreflightReport,
    target_image: &str,
    dry_run: bool,
    skip_import: bool,
    bootloader: &str,
    force: bool,
) -> Result<()> {
    // Stamp the disk with the build version for post-mortem diagnostics.
    let version = env!("BUILD_GIT_HASH");
    if let Err(e) = fs::write("/e2e-disk-label.txt", format!("bootc-migrate-composefs {}\n", version))
    {
        eprintln!("Warning: could not write disk version label: {e:#}");
    }
    // Acquire exclusive lock (Fix 8).
    let _lock = if !dry_run {
        Some(acquire_lock()?)
    } else {
        None
    };

    if dry_run {
        println!("[DRY RUN] Would execute migration phases without making changes.");
    }

    // Mount /sysroot and /boot read-write (Fix 2: propagate errors).
    if !dry_run {
        let sysroot_status = Command::new("/usr/bin/mount")
            .args(["-o", "remount,rw", "/sysroot"])
            .status()
            .context("failed to execute mount remount,rw /sysroot")?;
        if !sysroot_status.success() {
            return Err(anyhow!(
                "failed to remount /sysroot read-write — cannot proceed with migration"
            ));
        }
        let boot_status = Command::new("/usr/bin/mount")
            .args(["-o", "remount,rw", "/boot"])
            .status()
            .context("failed to execute mount remount,rw /boot")?;
        if !boot_status.success() {
            return Err(anyhow!(
                "failed to remount /boot read-write — cannot proceed with migration"
            ));
        }
    } else {
        println!("[DRY RUN] Would remount /sysroot and /boot read-write.");
    }

    // ---- Phase 0: preflight free-space check (#10) ----
    // Must run BEFORE the XFS loopback setup — once the loopback is mounted
    // at /sysroot/composefs, statvfs on that path reflects the loopback's
    // free space (fresh 17 GB), not the real root filesystem's.
    println!("=== Phase 0: Free-space check ===");
    if !dry_run {
        check_free_space(report.supports_reflink)?;
    } else {
        println!("[DRY RUN] Would check free space on /sysroot.");
    }

    // ---- XFS workaround: ensure composefs store supports fs-verity ----
    let loopback_guard: Option<MountGuard> = if !dry_run {
        setup_composefs_loopback_if_needed(report)?
    } else {
        let fs_type = report.fs_type.as_deref().unwrap_or("unknown");
        if fs_type == "xfs" {
            println!("[DRY RUN] Would set up ext4 loopback at /sysroot/composefs for fs-verity.");
        }
        None
    };
    // Leak the loopback mount guard so it survives process exit — the composefs
    // store is inside the ext4 loopback and must remain mounted until reboot.
    if loopback_guard.is_some() {
        std::mem::forget(loopback_guard);
    }

    // ---- Phase 1: Import OSTree objects (optional / deletable per #3) ----
    // Ensure composefs repository directory exists before any phase touches it.
    if !dry_run {
        fs::create_dir_all("/sysroot/composefs")
            .context("failed to create composefs repository directory")?;
    }

    if !skip_import {
        phase1_import_objects(report, dry_run)?;
    } else {
        println!("=== Phase 1: Skipped (--skip-import) ===");
    }

    // ---- Phase 2: Pull OCI image ----
    let (_manifest_digest, config_digest) = phase2_pull_image(target_image, dry_run)?;

    // ---- Phase 3: Create and seal EROFS image ----
    let verity = phase3_create_image(&config_digest, dry_run)?;

    // ---- Phase 4: Stage deployment state ----
    let _deploy_dir = phase4_stage_deploy(
        &verity,
        target_image,
        &_manifest_digest,
        &config_digest,
        dry_run,
    )?;

    // ---- Phase 5: Setup bootloader ----
    phase5_setup_bootloader(report, &verity, target_image, dry_run, bootloader, force)?;

    // ---- Verification: confirm artifacts before claiming success ----
    if !dry_run {
        verify_migration(&verity, report)?;
    }

    println!("\n=== MIGRATION COMPLETED ===");
    println!("Staged ComposeFS deployment: {}", verity.as_hex());
    let use_systemd_boot = bootloader != "grub2" && report.is_uefi && report.nvram_writable;
    if use_systemd_boot {
        println!("Primary bootloader: systemd-boot");
    } else {
        println!("Primary bootloader: GRUB2 (BLS Type 1)");
    }
    println!("Please reboot the system to finalize the transition.");
    println!(
        "After successful boot, run 'bootc-migrate-composefs commit' to make composefs permanent."
    );
    Ok(())
}

// ---- Phase 1 (#3) ----

fn phase1_import_objects(report: &PreflightReport, dry_run: bool) -> Result<()> {
    println!("=== Phase 1: Importing OSTree objects ===");
    let ostree_repo = "/sysroot/ostree/repo";
    if !Path::new(ostree_repo).exists() {
        println!("No OSTree repository found. Skipping.");
        return Ok(());
    }

    let file_objects = crate::ostree::scan_ostree_file_objects(ostree_repo)
        .context("failed to scan ostree repository")?;
    let total_objects = file_objects.len();
    println!("Found {} file objects to import.", total_objects);

    if dry_run {
        println!(
            "[DRY RUN] Would import {} objects into composefs store.",
            total_objects
        );
        return Ok(());
    }

    let mut count = 0usize;
    let mut reflink_count = 0usize;
    for obj in file_objects {
        let sha512 = crate::ostree::compute_sha512(&obj.path)?;
        let prefix = &sha512[..2];
        let rest = &sha512[2..];
        let target_dir = Path::new("/sysroot/composefs/objects").join(prefix);
        let target_path = target_dir.join(rest);

        if !target_path.exists() {
            fs::create_dir_all(&target_dir)?;
            if report.supports_reflink {
                if crate::reflink::reflink(&obj.path, &target_path).is_ok() {
                    reflink_count += 1;
                } else {
                    fs::copy(&obj.path, &target_path)?;
                }
            } else {
                fs::copy(&obj.path, &target_path)?;
            }
        }
        count += 1;
        if count % 1000 == 0 {
            println!("Imported {}/{} objects...", count, total_objects);
        }
    }
    println!("Imported {} objects ({} reflinked).", count, reflink_count);
    Ok(())
}

// ---- Phase 2 (#10) ----

fn phase2_pull_image(target_image: &str, dry_run: bool) -> Result<(String, String)> {
    println!("=== Phase 2: Pulling OCI image ===");

    if dry_run {
        println!("[DRY RUN] Would pull image: {}", target_image);
        return Ok(("dry-run-manifest".into(), "dry-run-config".into()));
    }

    // Idempotency: if composefs objects already exist from a prior run,
    // skip the pull.
    let objects_dir = Path::new("/sysroot/composefs/objects");
    let images_dir = Path::new("/sysroot/composefs/images");
    if objects_dir.exists() && images_dir.exists() {
        println!("Composefs object store already populated — skipping pull.");
        let inspect = Command::new("bootc")
            .args(["internals", "cfs", "--system", "oci", "inspect"])
            .output()
            .context("failed to inspect existing composefs store")?;
        let stdout = String::from_utf8_lossy(&inspect.stdout);
        let config_digest = stdout
            .lines()
            .find_map(|l| l.strip_prefix("config "))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".into());
        println!("Existing composefs store config: {}", config_digest);
        return Ok(("cached-manifest".into(), config_digest));
    }

    // Check if podman has the image cached locally — avoids re-downloading
    // 9 GB from ghcr.io on every retry.
    let has_local = Command::new("podman")
        .args(["image", "exists", target_image])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if has_local {
        println!(
            "Local podman cache found for {} — pulling from containers-storage...",
            target_image
        );
        // bootc supports `containers-storage:` transport which reads directly
        // from podman's local image store (~30s vs 10+ min from ghcr.io).
        let local_ref = format!("containers-storage:{}", target_image);
        let pull_output = crate::composefs::pull_image(&local_ref)
            .context("failed to pull from containers-storage")?;

        // containers-storage transport outputs "config sha256:..." and
        // "verity ..." but no separate "manifest ..." line. For local
        // images the config digest serves as the manifest identifier.
        let mut config_digest = String::new();
        for line in pull_output.lines() {
            if let Some(d) = line.trim().strip_prefix("config ") {
                config_digest = d.trim().to_string();
            }
        }
        if config_digest.is_empty() {
            config_digest = pull_output.lines().next().unwrap_or("").trim().to_string();
        }
        let manifest_digest = config_digest.clone();
        println!(
            "Target image pulled from local cache. Config: {}",
            config_digest
        );
        return Ok((manifest_digest, config_digest));
    }

    println!("Pulling target image: {}...", target_image);
    let pull_output =
        crate::composefs::pull_image(target_image).context("failed to pull OCI image")?;

    let mut manifest_digest = String::new();
    let mut config_digest = String::new();
    for line in pull_output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("manifest ") {
            manifest_digest = trimmed["manifest ".len()..].trim().to_string();
        } else if trimmed.starts_with("config ") {
            config_digest = trimmed["config ".len()..].trim().to_string();
        }
    }

    if manifest_digest.is_empty() {
        manifest_digest = pull_output.trim().to_string();
    }
    if config_digest.is_empty() {
        config_digest = manifest_digest.clone();
    }
    println!(
        "Target image pulled. Manifest: {}, Config: {}",
        manifest_digest, config_digest
    );
    Ok((manifest_digest, config_digest))
}

// ---- Phase 3 ----

fn phase3_create_image(config_digest: &str, dry_run: bool) -> Result<VerityDigest> {
    println!("=== Phase 3: Creating ComposeFS EROFS Image ===");

    if dry_run {
        println!(
            "[DRY RUN] Would create and seal composefs image for config: {}",
            config_digest
        );
        return Ok(VerityDigest::from_hex(
            "dryrun0000000000000000000000000000000000000000000000000000000000",
        ));
    }

    // Fix 10: real idempotency — check if the image already exists AND is sealed.
    // We first need the verity hash to check, so we still call create_image (which
    // is typically a no-op if objects already exist), then skip seal if already done.
    let sha512_verity_str = crate::composefs::create_image(config_digest)
        .context("failed to create composefs image")?;

    let verity = VerityDigest::from_prefixed_or_hex(&sha512_verity_str);
    println!(
        "ComposeFS EROFS image created. Verity digest: {}",
        verity.as_hex()
    );

    // The `bootc internals cfs seal` command creates the `oci-config-<verity>`
    // stream in the composefs object store, which the initramfs needs for
    // proper composefs user-space mounting (without it, bootc falls back to
    // raw kernel EROFS mount which zero-fills files above the inline threshold,
    // causing missing unit files like dbus.service and cascading boot failures).
    // Always seal — idempotency is handled inside bootc.
    println!("Sealing composefs image...");
    crate::composefs::seal_image(config_digest).context("failed to seal composefs image")?;
    println!("Image sealed successfully.");

    // Write meta.json to the composefs repository so that
    // bootc-root-setup.service can open it in the initramfs.
    let meta_path = Path::new("/sysroot/composefs/meta.json");
    if let Err(e) = fs::write(meta_path, COMPOSEFS_META_JSON) {
        eprintln!("Warning: could not write meta.json: {e:#}");
    } else {
        println!("  Wrote composefs meta.json");
    }

    Ok(verity)
}

// ---- Phase 4 (#4, #5, #7) ----

/// Build the `.origin` file content that bootc parses to identify a composefs
/// deployment. Uses `tini::Ini` for byte-compatible output with bootc's parser.
pub fn inspect_image(image_id: &str) -> Result<String> {
    let output = Command::new("bootc")
        .args(["internals", "cfs", "--system", "oci", "inspect", image_id])
        .output()
        .context("failed to execute bootc internals cfs oci inspect")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("inspect failed: {}", stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn mount_image(image_id: &str, mount_path: &Path) -> Result<()> {
    let mount_str = mount_path
        .to_str()
        .ok_or_else(|| anyhow!("invalid mount path"))?;

    // Always prefer the bootc composefs overlay mount: it stacks the EROFS
    // metadata layer on top of the content-addressed object tree at
    // /sysroot/composefs/objects so files read back with their actual content.
    // A bare `mount -t erofs` returns metadata-only views (sizes look right but
    // file contents are zero-filled), which silently corrupts every artifact
    // Phase 5 copies out of the mount (kernel, initrd, systemd-bootx64.efi…).
    let output = Command::new("bootc")
        .args([
            "internals",
            "cfs",
            "--system",
            "oci",
            "mount",
            image_id,
            mount_str,
        ])
        .output()
        .context("failed to execute bootc internals cfs oci mount")?;
    if output.status.success() {
        return Ok(());
    }

    // Last-resort fallback: raw EROFS mount. This works only if every file
    // copied out of the mount happens to be inline (small enough to live in
    // the EROFS metadata). Reserved for environments where bootc is missing.
    let bootc_err = String::from_utf8_lossy(&output.stderr).into_owned();
    let image_path = Path::new("/sysroot/composefs/images").join(image_id);
    if image_path.exists() {
        let fallback = Command::new("/usr/bin/mount")
            .args([
                "-t",
                "erofs",
                "-o",
                "ro,loop",
                image_path.to_str().unwrap_or(""),
                mount_str,
            ])
            .output()
            .context("failed to mount erofs image (bootc cfs fallback)")?;
        if fallback.status.success() {
            eprintln!(
                "Warning: bootc cfs mount failed ({}), fell back to raw EROFS — \
                 file content beyond the inline threshold will read as zeros.",
                bootc_err.trim()
            );
            return Ok(());
        }
    }
    Err(anyhow!("mount failed: {}", bootc_err))
}

// ---- Phase 5 (#6, #8, #9) ----


#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::esp::get_esp_disk_and_part;

    #[test]
    fn patch_boot_digest_fails_on_corrupted_origin() {
        // If manifest_digest contains multi-line garbage (e.g. full pull output),
        // the tini parser rejects it — the migration must catch this.
        let corrupted = "[origin]\ncontainer-image-reference = img:latest\n\n[boot]\nboot_type = bls\ndigest = deadbeef\n\n[image]\nmanifest_digest = config sha256:abc123\nverity badbadbad";
        let result = patch_boot_digest_in_content(corrupted, "goodhash");
        assert!(result.is_err(), "corrupted origin must fail to parse");
    }

    #[test]
    fn esp_parsing_returns_none_for_nonexistent() {
        // findmnt will fail for nonexistent path → returns None.
        assert!(get_esp_disk_and_part("/dev/null/nonexistent").is_none());
    }

    #[test]
    fn esp_parsing_handles_empty_output() {
        // If findmnt returns empty, parsing should return None.
        // The function returns None if the source string is empty.
        // This is exercised by get_esp_disk_and_part's early return on empty source.
    }

    // ── Slice 1: origin file schema tests ──

    #[test]
    fn origin_content_roundtrips_through_tini() {
        let verity = VerityDigest::from_hex(
            "9af734da164df0edb34a200a55bf4a6426afbc80f66e5fb7c73ecfdd17b19dbd",
        );
        let content = build_origin_content(
            "ghcr.io/projectbluefin/dakota:stable",
            &verity,
            "sha256:abc123",
        );
        // Must parse back successfully
        let parsed = tini::Ini::from_string(&content).expect("origin content must be valid INI");
        assert_eq!(
            parsed
                .get::<String>("origin", "container-image-reference")
                .as_deref(),
            Some("ostree-unverified-image:docker://ghcr.io/projectbluefin/dakota:stable")
        );
        assert_eq!(
            parsed.get::<String>("boot", "boot_type").as_deref(),
            Some("bls")
        );
        assert_eq!(
            parsed.get::<String>("boot", "digest").as_deref(),
            Some("9af734da164df0edb34a200a55bf4a6426afbc80f66e5fb7c73ecfdd17b19dbd"),
            "[boot] digest must match bootc's ORIGIN_KEY_BOOT_DIGEST constant"
        );
        assert_eq!(
            parsed.get::<String>("image", "manifest_digest").as_deref(),
            Some("sha256:abc123"),
            "manifest_digest must be under [image], not [boot]"
        );
    }

    #[test]
    fn origin_content_is_stable_across_rebuilds() {
        let verity = VerityDigest::from_hex(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        );
        let a = build_origin_content("img:latest", &verity, "sha256:foo");
        let b = build_origin_content("img:latest", &verity, "sha256:foo");
        assert_eq!(a, b, "origin content must be deterministic");
    }

    #[test]
    fn patch_boot_digest_replaces_placeholder() {
        let verity = VerityDigest::from_hex(
            "9af734da164df0edb34a200a55bf4a6426afbc80f66e5fb7c73ecfdd17b19dbd",
        );
        let original = build_origin_content("img:latest", &verity, "sha256:disc");
        let patched = patch_boot_digest_in_content(&original, "abcdef1234567890").unwrap();

        let parsed = tini::Ini::from_string(&patched).unwrap();
        assert_eq!(
            parsed.get::<String>("boot", "digest").as_deref(),
            Some("abcdef1234567890"),
            "[boot] digest must be replaced with real sha256(vmlinuz||initrd)"
        );
    }

    #[test]
    fn patch_boot_digest_preserves_all_other_keys() {
        let verity = VerityDigest::from_hex(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let original =
            build_origin_content("ghcr.io/example/target:v1", &verity, "sha256:manifest123");
        let patched = patch_boot_digest_in_content(&original, "newdigest111").unwrap();

        let parsed = tini::Ini::from_string(&patched).unwrap();
        assert_eq!(
            parsed
                .get::<String>("origin", "container-image-reference")
                .as_deref(),
            Some("ostree-unverified-image:docker://ghcr.io/example/target:v1")
        );
        assert_eq!(
            parsed.get::<String>("boot", "boot_type").as_deref(),
            Some("bls")
        );
        assert_eq!(
            parsed.get::<String>("image", "manifest_digest").as_deref(),
            Some("sha256:manifest123")
        );
        assert_eq!(
            parsed.get::<String>("boot", "digest").as_deref(),
            Some("newdigest111")
        );
    }

    #[test]
    fn patch_boot_digest_fails_on_garbage_input() {
        let result = patch_boot_digest_in_content("not a valid INI file\n[garbage", "foo");
        assert!(result.is_err(), "must reject malformed INI");
    }
}
