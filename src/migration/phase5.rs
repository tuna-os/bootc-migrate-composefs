use crate::VerityDigest;
use crate::migration::bootloader;
use crate::migration::esp::*;
use crate::migration::kernel_options::get_kernel_options;
use crate::migration::mount_image;
use crate::migration::os_release;
use crate::migration::os_release::{bls_entry_filename, bls_entry_title, read_os_release};
use crate::migration::phase0::*;
use crate::migration::registry::patch_origin_boot_digest;
use crate::migration::registry::*;
use crate::migration::{BOOTC_DRACUT_MODULE, BOOTC_ROOT_SETUP_SERVICE};
use crate::preflight::PreflightReport;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

pub(crate) fn phase5_setup_bootloader(
    report: &PreflightReport,
    verity: &VerityDigest,
    target_image: &str,
    dry_run: bool,
    bootloader: &str,
    force: bool,
) -> Result<()> {
    println!("=== Phase 5: Setting Up Bootloader ===");

    // systemd-boot is default when UEFI + NVRAM writable + ESP ready, unless user forces grub2.
    // We no longer require systemd-boot binaries in the *source* OS: Phase 5 extracts the
    // binary from the mounted *target* composefs image and installs it directly. If the
    // target also doesn't ship systemd-boot, Phase 5 falls back to GRUB2 automatically.
    let use_systemd_boot = bootloader != "grub2"
        && report.is_uefi
        && report.nvram_writable
        && report.esp_ready_for_systemd_boot;

    // Optional: Phase 5 idempotency — check if composefs entry already exists.
    let esp = if use_systemd_boot {
        Some(ensure_esp_mounted(report)?)
    } else {
        None
    };
    let entries_check = if let Some(ref esp) = esp {
        Path::new(esp).join("loader/entries")
    } else {
        Path::new("/boot/loader/entries").to_path_buf()
    };
    if entries_check.exists() {
        let has_existing = fs::read_dir(&entries_check)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().starts_with("bootc_"))
            })
            .unwrap_or(false);
        if has_existing && !force {
            println!(
                "BLS entries already present in {}. Skipping Phase 5.",
                entries_check.display()
            );
            return Ok(());
        }
        if has_existing && force {
            println!("[phase5] --force: re-running Phase 5 over existing BLS entries.");
            // Remove existing bootc_ entries so they get cleanly rewritten.
            if let Ok(rd) = fs::read_dir(&entries_check) {
                for entry in rd.flatten() {
                    if entry.file_name().to_string_lossy().starts_with("bootc_") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    let temp_mount =
        TempDir::new_in("/var/tmp").context("failed to create temp mount directory")?;
    let mount_path = temp_mount.path().to_path_buf();

    if dry_run {
        println!("[DRY RUN] Would mount EROFS, extract boot artifacts, and write BLS entries.");
        return Ok(());
    }

    mount_image(verity.as_hex(), &mount_path)
        .context("failed to mount composefs image for boot artifacts")?;
    let _guard = MountGuard::new(&mount_path);

    // Find kernel version from mounted image /usr/lib/modules
    let modules_dir = mount_path.join("usr/lib/modules");
    let kver = fs::read_dir(&modules_dir)?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("could not find kernel version in mounted image"))?;
    println!("Found kernel version: {}", kver);

    let mounted_vmlinuz = modules_dir.join(&kver).join("vmlinuz");
    let mounted_initrd = modules_dir.join(&kver).join("initramfs.img");

    let vmlinuz_src = if mounted_vmlinuz.exists() {
        mounted_vmlinuz
    } else {
        mount_path.join("boot").join(format!("vmlinuz-{}", kver))
    };
    let initrd_src = if mounted_initrd.exists() {
        mounted_initrd
    } else {
        mount_path
            .join("boot")
            .join(format!("initramfs-{}.img", kver))
    };

    // Read target os-release for BLS naming
    let target_os = read_os_release(&mount_path).unwrap_or_else(|_| os_release::OsRelease {
        id: "dakota".into(),
        version_id: String::new(),
        name: String::new(),
        pretty_name: String::new(),
    });

    let options_str = get_kernel_options(verity.as_hex())?;

    // Write to staged entries first, then atomically rename.
    let mut entries: Vec<bootloader::BlsEntry> = Vec::new();

    // Track whether we actually completed the systemd-boot install. If extraction from the
    // target image fails, we fall through to the GRUB2 branch instead of erroring out so the
    // user always ends up with a bootable system.
    let mut sd_boot_installed = false;
    if use_systemd_boot {
        let esp = esp.as_ref().unwrap();
        let esp_path = Path::new(esp);

        match install_systemd_boot_from_target(esp_path, &mount_path, target_image) {
            Ok(()) => {
                // Copy composefs kernel+initrd to ESP via registry stream (raw EROFS reads
                // return zero-filled content past the inline threshold).
                let boot_dir_name = format!("bootc_composefs-{}", verity.as_hex());
                let esp_boot_dir = esp_path.join("EFI/Linux").join(&boot_dir_name);
                fs::create_dir_all(&esp_boot_dir)?;

                // Translate the discovered host-mount paths back to in-container paths.
                let rel_vmlinuz = vmlinuz_src
                    .strip_prefix(&mount_path)
                    .with_context(|| format!("vmlinuz {:?} not under mount", vmlinuz_src))?;
                let in_container_vmlinuz = Path::new("/").join(rel_vmlinuz);
                let esp_vmlinuz = esp_boot_dir.join("vmlinuz");

                let mut extract = vec![(in_container_vmlinuz.as_path(), esp_vmlinuz.as_path())];
                let in_container_initrd;
                let esp_initrd;
                let mut have_initrd = false;
                if initrd_src.exists() {
                    let rel_initrd = initrd_src
                        .strip_prefix(&mount_path)
                        .with_context(|| format!("initrd {:?} not under mount", initrd_src))?;
                    in_container_initrd = Path::new("/").join(rel_initrd);
                    esp_initrd = esp_boot_dir.join("initrd");
                    extract.push((in_container_initrd.as_path(), esp_initrd.as_path()));
                    have_initrd = true;
                } else {
                    esp_initrd = PathBuf::new();
                }
                extract_files_via_registry(target_image, &extract).context(
                    "failed to extract kernel/initrd from target image via registry stream",
                )?;

                // Rebuild initrd with LVM support if the source system uses LVM.
                // Must happen before patch_origin_boot_digest so the hash covers
                // the LVM-enabled initrd bytes, not the original Dakota initrd.
                let mut extra_initrd_name: Option<String> = None;
                if have_initrd {
                    match rebuild_initrd_with_lvm_if_needed(
                        &kver,
                        &mount_path,
                        &esp_initrd,
                        target_image,
                    ) {
                        Ok(extra) => {
                            extra_initrd_name = extra;
                        }
                        Err(e) => {
                            eprintln!("[phase5] Warning: initrd rebuild failed: {e:#}");
                        }
                    }
                }

                // Flush ESP writes to disk. VFAT doesn't sync automatically
                // without unmount — and the ESP stays mounted at
                // /var/tmp/esp-migration for the rest of Phase 5.
                // Without this, in-VM reads see cached data but the raw disk
                // (host-side .raw scan) shows zeros for large files like initrd.
                rustix::fs::sync();

                // Now that vmlinuz + initrd are on the ESP, compute their
                // boot_digest (sha256(vmlinuz || initrd)) and patch the .origin
                // file. `bootc status` requires this digest to set soft-reboot
                // capability; without it, status fails with "Could not find
                // boot digest for deployment".
                if have_initrd {
                    if let Err(e) = patch_origin_boot_digest(verity, &esp_vmlinuz, &esp_initrd) {
                        eprintln!("[phase5] warning: failed to patch origin boot_digest: {e:#}");
                    }
                }

                // Build composefs BLS entry with optional second initrd.
                let mut initrd_paths = vec![format!("/EFI/Linux/{}/initrd", boot_dir_name)];
                if let Some(ref extra) = extra_initrd_name {
                    initrd_paths.push(format!("/EFI/Linux/{}/{}", boot_dir_name, extra));
                }
                let composefs_entry = bootloader::BlsEntry {
                    title: bls_entry_title(&target_os, "composefs"),
                    version: kver.clone(),
                    linux: format!("/EFI/Linux/{}/vmlinuz", boot_dir_name),
                    initrds: initrd_paths,
                    options: options_str.clone(),
                    filename: bls_entry_filename(&target_os, verity.as_hex(), 1),
                    sort_key: format!("bootc-{}-0", target_os.id),
                };

                // Stage + atomic-rename. Only the composefs entry goes on the ESP.
                //
                // We intentionally do NOT write an OSTree fallback BLS entry here:
                // `bootc status` (`Parsers/bls_config.rs::boot_artifact_info`) treats
                // every non-EFI BLS entry on the ESP as a composefs deployment and
                // bails with "No composefs= param" if it finds one without a
                // composefs= cmdline. Adding such an entry breaks `bootc status` and
                // every downstream that depends on it. Recovery is still possible
                // via firmware menu (`Fedora\shimx64.efi` remains in NVRAM BootOrder)
                // or by selecting the OSTree GRUB entry from /boot/loader/entries.
                let staged_dir = esp_path.join("loader/entries.staged");
                fs::create_dir_all(&staged_dir)?;
                let entries_dir = esp_path.join("loader/entries");
                fs::create_dir_all(&entries_dir)?;
                let to_promote: Vec<&bootloader::BlsEntry> = vec![&composefs_entry];
                for entry in &to_promote {
                    fs::write(staged_dir.join(&entry.filename), entry.render())?;
                    fs::rename(
                        staged_dir.join(&entry.filename),
                        entries_dir.join(&entry.filename),
                    )
                    .with_context(|| {
                        format!("failed to promote ESP BLS entry: {}", entry.filename)
                    })?;
                }

                // loader.conf: composefs is the default, 3s timeout so the user can pick the
                // OSTree fallback during the evaluation window.
                let default_id = composefs_entry.filename.trim_end_matches(".conf");
                let loader_conf = format!("default {}\ntimeout 3\nconsole-mode keep\n", default_id);
                fs::write(esp_path.join("loader/loader.conf"), loader_conf)
                    .context("failed to write loader.conf")?;

                // Register Linux Boot Manager in NVRAM (best-effort).
                register_systemd_boot_nvram(esp);

                sd_boot_installed = true;
                println!(
                    "systemd-boot installed from target image. Composefs is the default; \
                     OSTree fallback available in the loader menu (3s timeout)."
                );

                // Also set GRUB's saved_entry so that if OVMF falls back
                // to shim → GRUB (e.g. after NVRAM reset), it still boots
                // composefs rather than Bluefin.
                let entry_id = composefs_entry.filename.trim_end_matches(".conf");
                let grubenv = "/boot/grub2/grubenv";
                if Path::new(grubenv).exists() || Path::new("/boot/grub2/grub.cfg").exists() {
                    if Command::new("grub2-editenv")
                        .args([grubenv, "set", &format!("saved_entry={}", entry_id)])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                    {
                        println!("  Set GRUB saved_entry={} (fallback boot path).", entry_id);
                    } else if Command::new("grub2-set-default")
                        .args([&entry_id])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                    {
                        println!("  Set GRUB default to {} (fallback path).", entry_id);
                    }
                }

                // Also write a GRUB-compatible composefs BLS entry to /boot/
                // so GRUB can boot composefs (even when systemd-boot is used).
                let boot_composefs_dir = Path::new("/boot").join(&boot_dir_name);
                if !boot_composefs_dir.join("vmlinuz").exists() {
                    fs::create_dir_all(&boot_composefs_dir).ok();
                    // Copy files from ESP to /boot/ for GRUB access.
                    let esp_src = esp_path.join("EFI/Linux").join(&boot_dir_name);
                    for f in &["vmlinuz", "initrd", "xfs-mount.cpio"] {
                        let src = esp_src.join(f);
                        if src.exists() {
                            let _ = fs::copy(&src, boot_composefs_dir.join(f));
                        }
                    }
                    // Write a BLS entry with /boot-relative paths (GRUB needs these).
                    let grub_bls = format!(
                        "title Linux (composefs)\nversion 7.0.7\nlinux /boot/{}/vmlinuz\ninitrd /boot/{}/initrd\ninitrd /boot/{}/xfs-mount.cpio\noptions {}\nsort-key bootc-linux-0\n",
                        boot_dir_name, boot_dir_name, boot_dir_name, options_str
                    );
                    let grub_entries = Path::new("/boot/loader/entries");
                    fs::create_dir_all(grub_entries).ok();
                    let _ = fs::write(grub_entries.join(&composefs_entry.filename), &grub_bls);
                    println!(
                        "  Wrote GRUB-compatible composefs BLS entry to /boot/loader/entries/"
                    );

                    // Inject set default="${saved_entry}" into grub.cfg so GRUB
                    // boots composefs as the default.
                    let grub_cfg = "/boot/grub2/grub.cfg";
                    if let Ok(cfg) = fs::read_to_string(grub_cfg) {
                        let default_kwd = "set default=\"${saved_entry}\"";
                        if !cfg.contains(default_kwd) {
                            let patched =
                                cfg.replace("\nblscfg\n", &format!("\n{}\nblscfg\n", default_kwd));
                            if patched != cfg {
                                let _ = fs::write(grub_cfg, &patched);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "Warning: could not install systemd-boot from target image ({}). \
                     Falling back to GRUB2 path.",
                    e
                );
            }
        }
    }

    if !sd_boot_installed {
        // GRUB2 path
        println!("Staying on GRUB2 bootloader (BLS Type 1)...");
        let boot_dir_name = format!("bootc_composefs-{}", verity.as_hex());
        let grub_boot_dir = Path::new("/boot").join(&boot_dir_name);
        fs::create_dir_all(&grub_boot_dir)?;

        // Extract kernel + initrd via registry stream rather than copying
        // from the EROFS mount (which returns zero-filled content for files
        // larger than the inline data threshold — e.g. vmlinuz at 17+ MB).
        let rel_vmlinuz = vmlinuz_src
            .strip_prefix(&mount_path)
            .with_context(|| format!("vmlinuz {:?} not under mount", vmlinuz_src))?;
        let in_container_vmlinuz = Path::new("/").join(rel_vmlinuz);
        let grub_vmlinuz = grub_boot_dir.join("vmlinuz");
        let mut grub_extract: Vec<(&Path, &Path)> =
            vec![(in_container_vmlinuz.as_path(), grub_vmlinuz.as_path())];

        let have_grub_initrd = initrd_src.exists();
        let in_container_initrd;
        let grub_initrd_path;
        if have_grub_initrd {
            let rel_initrd = initrd_src
                .strip_prefix(&mount_path)
                .with_context(|| format!("initrd {:?} not under mount", initrd_src))?;
            in_container_initrd = Path::new("/").join(rel_initrd);
            grub_initrd_path = grub_boot_dir.join("initrd");
        } else {
            in_container_initrd = PathBuf::new();
            grub_initrd_path = PathBuf::new();
        }
        if have_grub_initrd {
            grub_extract.push((in_container_initrd.as_path(), grub_initrd_path.as_path()));
        }
        extract_files_via_registry(target_image, &grub_extract).context(
            "failed to extract kernel/initrd from target image via registry stream (grub2 path)",
        )?;

        let mut extra_initrd_grub: Option<String> = None;
        if have_grub_initrd {
            let grub_initrd = grub_boot_dir.join("initrd");
            match rebuild_initrd_with_lvm_if_needed(&kver, &mount_path, &grub_initrd, target_image)
            {
                Ok(extra) => {
                    extra_initrd_grub = extra;
                }
                Err(e) => {
                    eprintln!("[phase5] Warning: initrd rebuild failed: {e:#}");
                }
            }
        }

        // Composefs entry (priority 1)
        let mut grub_initrd_paths = vec![format!("/{}/initrd", boot_dir_name)];
        if let Some(ref extra) = extra_initrd_grub {
            grub_initrd_paths.push(format!("/{}/{}", boot_dir_name, extra));
        }
        entries.push(bootloader::BlsEntry {
            title: bls_entry_title(&target_os, "composefs"),
            version: kver.clone(),
            linux: format!("/{}/vmlinuz", boot_dir_name),
            initrds: grub_initrd_paths,
            options: options_str.clone(),
            filename: bls_entry_filename(&target_os, verity.as_hex(), 1),
            sort_key: format!("bootc-{}-0", target_os.id),
        });

        // OSTree fallback entry (priority 0)
        if let Ok(ostree_entry) = build_ostree_fallback_entry() {
            entries.push(ostree_entry);
        }

        // Write to entries.staged/ first
        let staged_dir = Path::new("/boot/loader/entries.staged");
        fs::create_dir_all(&staged_dir)?;
        for entry in &entries {
            let entry_path = staged_dir.join(&entry.filename);
            fs::write(&entry_path, entry.render())?;
        }

        // Propagate rename errors.
        let entries_dir = Path::new("/boot/loader/entries");
        fs::create_dir_all(&entries_dir)?;
        for entry in &entries {
            let src = staged_dir.join(&entry.filename);
            let dst = entries_dir.join(&entry.filename);
            fs::rename(&src, &dst)
                .with_context(|| format!("failed to promote staged entry: {}", entry.filename))?;
        }

        // Set the composefs entry as the persistent default via saved_entry.
        // bootupd-shipped grub.cfg only has `set default="${saved_entry}"` — it does NOT
        // include the `if [ "${next_entry}" ]` one-shot block, so grub2-reboot's
        // next_entry is silently ignored. Set saved_entry directly. We also still call
        // grub2-reboot so distros that DO honor next_entry get the one-shot semantics
        // (revert on failed boot), but we don't rely on it.
        let composefs_entry_id = bls_entry_filename(&target_os, verity.as_hex(), 1);
        let entry_id = composefs_entry_id.trim_end_matches(".conf");
        let grubenv = "/boot/grub2/grubenv";

        let saved_ok = Command::new("grub2-editenv")
            .args([grubenv, "set", &format!("saved_entry={}", entry_id)])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !saved_ok {
            // Fall back to grub2-set-default which writes through to grubenv.
            let sd_ok = Command::new("grub2-set-default")
                .arg(entry_id)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !sd_ok {
                eprintln!(
                    "Warning: failed to set grub saved_entry={}. Composefs may not be the default boot target.",
                    entry_id
                );
            }
        }

        // Best-effort one-shot for distros with the next_entry block.
        let _ = Command::new("grub2-reboot").arg(entry_id).status();

        // Ensure GRUB_DEFAULT=saved in /etc/default/grub
        let grub_defaults_path = "/etc/default/grub";
        let existing = fs::read_to_string(grub_defaults_path).unwrap_or_default();
        let mut new_cfg = String::new();
        let mut found = false;
        for line in existing.lines() {
            if line.starts_with("GRUB_DEFAULT=") {
                new_cfg.push_str("GRUB_DEFAULT=saved\n");
                found = true;
            } else {
                new_cfg.push_str(line);
                new_cfg.push('\n');
            }
        }
        if !found {
            new_cfg.push_str("GRUB_DEFAULT=saved\n");
        }
        fs::write(grub_defaults_path, &new_cfg).context("failed to write /etc/default/grub")?;

        // Inject set default="${saved_entry}" into grub.cfg
        let grub_cfg_path = "/boot/grub2/grub.cfg";
        if let Ok(cfg) = fs::read_to_string(grub_cfg_path) {
            if !cfg.contains("set default=\"${saved_entry}\"") {
                let patched =
                    cfg.replace("\nblscfg\n", "\nset default=\"${saved_entry}\"\nblscfg\n");
                if patched != cfg {
                    fs::write(grub_cfg_path, &patched)
                        .context("failed to write patched grub.cfg")?;
                }
            }
        }
    }

    Ok(())
}

/// Copy systemd-boot binaries from the mounted target image to the ESP.
/// This avoids needing systemd-boot installed in the *source* OS (Bluefin) — we lift
/// it straight out of the Dakota image that's already mounted for kernel extraction.
/// Extract a list of files from the target OCI image using `skopeo` + `tar -O`.
///
/// We can't read these files from the local EROFS mount (zero-fills past the inline
/// threshold) and we can't use `podman cp` either: it would have to unpack the whole
/// image into the local overlay store just so it can extract three files, which
/// reliably ENOSPCs on tight bootc systems. Skopeo's `dir:` format keeps the raw
/// compressed layer blobs on disk without expanding them, so the footprint is roughly
/// "compressed image size" instead of "compressed + expanded".
///
/// `files` is a list of (in-container source path, on-host destination path) pairs.
/// Destination parent directories must already exist. The OCI layers are scanned
/// newest-first; the first hit wins (matches how the OCI image overlay would resolve).
///
/// We can't use `skopeo copy ... dir:` either: it downloads every compressed layer
/// to disk before we can touch any of them, which ENOSPCs on the freshly-migrated
/// btrfs (the EROFS image and composefs object store have already eaten most of /var).
/// Instead we hit the registry HTTP API directly and stream one layer at a time,
/// deleting it before moving on so peak disk use is bounded by the largest layer.
/// Extract files from a locally cached podman image using `podman create` +
/// `podman cp`. Returns true if all files were extracted successfully.
fn install_systemd_boot_from_target(
    esp_path: &Path,
    mount_path: &Path,
    target_image: &str,
) -> Result<()> {
    // Probe via the EROFS mount only to confirm the file exists in the target image
    // (file listing works fine on raw EROFS; it's the content reads that are corrupt).
    let probe = mount_path.join("usr/lib/systemd/boot/efi/systemd-bootx64.efi");
    if !probe.exists() {
        return Err(anyhow!(
            "target image does not ship systemd-boot at /usr/lib/systemd/boot/efi/systemd-bootx64.efi"
        ));
    }

    let sd_dir = esp_path.join("EFI/systemd");
    fs::create_dir_all(&sd_dir)?;
    // Removable-media fallback path so the firmware will boot it even if NVRAM is wiped.
    let removable_dir = esp_path.join("EFI/BOOT");
    fs::create_dir_all(&removable_dir)?;

    let sd_dst = sd_dir.join("systemd-bootx64.efi");
    extract_files_via_registry(
        target_image,
        &[(
            Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi"),
            &sd_dst,
        )],
    )
    .context("failed to extract systemd-bootx64.efi from target image via registry stream")?;

    // Mirror to removable-media path. Local copy of the freshly-extracted (real) bytes
    // is safe — no EROFS in the read path.
    fs::copy(&sd_dst, removable_dir.join("BOOTX64.EFI"))
        .context("failed to install BOOTX64.EFI removable-media loader")?;

    Ok(())
}

/// Register `Linux Boot Manager` in UEFI NVRAM pointing at the systemd-boot loader.
/// Idempotent — skips if an entry by that label already exists. Best-effort: warns
/// on failure instead of erroring, since the removable-media loader at \EFI\BOOT\BOOTX64.EFI
/// keeps the system bootable as a last resort.
fn register_systemd_boot_nvram(esp_path: &str) {
    if let Ok(out) = Command::new("efibootmgr").arg("-v").output() {
        let txt = String::from_utf8_lossy(&out.stdout);
        if txt.lines().any(|l| l.contains("Linux Boot Manager")) {
            println!("Linux Boot Manager already registered in UEFI NVRAM.");
            return;
        }
    }

    let (disk, part) = match get_esp_disk_and_part(esp_path) {
        Some(dp) => dp,
        None => {
            eprintln!(
                "Warning: could not parse ESP device for efibootmgr. \
                 systemd-boot binary is on the ESP at \\EFI\\BOOT\\BOOTX64.EFI \
                 (removable-media path) but no NVRAM entry was created."
            );
            return;
        }
    };

    let status = Command::new("efibootmgr")
        .args([
            "--create",
            "--disk",
            &disk,
            "--part",
            &part,
            "--loader",
            "\\EFI\\systemd\\systemd-bootx64.efi",
            "--label",
            "Linux Boot Manager",
        ])
        .status();
    match status {
        Ok(s) if s.success() => println!("Registered 'Linux Boot Manager' in UEFI NVRAM."),
        Ok(s) => eprintln!(
            "Warning: efibootmgr --create failed (exit {:?}). \
             Removable-media loader at \\EFI\\BOOT\\BOOTX64.EFI remains as fallback.",
            s.code()
        ),
        Err(e) => eprintln!("Warning: failed to invoke efibootmgr ({}).", e),
    }
}

/// Build an OSTree fallback BLS entry placed on the ESP (systemd-boot path).
/// Copies the running OSTree deployment's kernel/initrd to <esp>/EFI/Linux/ostree-fallback/.
#[allow(dead_code)]
fn build_ostree_fallback_on_esp(esp_path: &Path) -> Result<bootloader::BlsEntry> {
    let (deploy_root, _checksum) = find_ostree_deployment()?;

    let modules_dir = deploy_root.join("usr/lib/modules");
    let kver = fs::read_dir(&modules_dir)?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("no kernel version in OSTree deployment"))?;

    let vmlinuz_path = modules_dir.join(&kver).join("vmlinuz");
    let initrd_path = modules_dir.join(&kver).join("initramfs.img");

    let fallback_dir = esp_path.join("EFI/Linux/ostree-fallback");
    fs::create_dir_all(&fallback_dir)?;
    if vmlinuz_path.exists() {
        fs::copy(&vmlinuz_path, fallback_dir.join("vmlinuz"))?;
    }
    if initrd_path.exists() {
        fs::copy(&initrd_path, fallback_dir.join("initrd"))?;
    }

    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let options: Vec<&str> = cmdline
        .split_whitespace()
        .filter(|w| !w.starts_with("composefs="))
        .collect();

    Ok(bootloader::BlsEntry {
        title: "Bluefin (OSTree fallback)".into(),
        version: kver,
        linux: "/EFI/Linux/ostree-fallback/vmlinuz".into(),
        initrds: vec!["/EFI/Linux/ostree-fallback/initrd".into()],
        options: options.join(" "),
        filename: "ostree-fallback-0.conf".into(),
        sort_key: "ostree-fallback-99".into(),
    })
}

/// Build a fallback BLS entry for the OSTree deployment — GRUB2 path (copies to /boot).
fn build_ostree_fallback_entry() -> Result<bootloader::BlsEntry> {
    let (deploy_root, _checksum) = find_ostree_deployment()?;

    let modules_dir = deploy_root.join("usr/lib/modules");
    let kver = fs::read_dir(&modules_dir)?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("no kernel version in OSTree deployment"))?;

    let vmlinuz_path = modules_dir.join(&kver).join("vmlinuz");
    let initrd_path = modules_dir.join(&kver).join("initramfs.img");

    let fallback_dir = Path::new("/boot/ostree-fallback");
    fs::create_dir_all(fallback_dir)?;
    if vmlinuz_path.exists() {
        fs::copy(&vmlinuz_path, fallback_dir.join("vmlinuz"))?;
    }
    if initrd_path.exists() {
        fs::copy(&initrd_path, fallback_dir.join("initrd"))?;
    }

    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let options: Vec<&str> = cmdline
        .split_whitespace()
        .filter(|w| !w.starts_with("composefs="))
        .collect();

    Ok(bootloader::BlsEntry {
        title: "OSTree (fallback)".into(),
        version: kver,
        linux: "/ostree-fallback/vmlinuz".into(),
        initrds: vec!["/ostree-fallback/initrd".into()],
        options: options.join(" "),
        filename: "ostree-fallback-0.conf".into(),
        sort_key: "ostree-fallback-99".into(),
    })
}

fn find_ostree_deployment() -> Result<(PathBuf, String)> {
    let deploy_base = Path::new("/sysroot/ostree/deploy/default/deploy");
    if deploy_base.exists() {
        for entry in fs::read_dir(deploy_base)? {
            let entry = entry?;
            let name_str = entry.file_name().to_string_lossy().into_owned();
            if name_str.ends_with(".0") && entry.path().is_dir() {
                let checksum = name_str.trim_end_matches(".0").to_string();
                return Ok((entry.path(), checksum));
            }
        }
    }
    Err(anyhow!("no OSTree deployment found for fallback entry"))
}

fn rebuild_initrd_with_lvm_if_needed(
    kver: &str,
    _mount_path: &Path,
    initrd_dst: &Path,
    target_image: &str,
) -> Result<Option<String>> {
    let needs_lvm = detect_lvm();
    let needs_xfs = Path::new("/sysroot/composefs-loopback.ext4").exists();
    if !needs_lvm && !needs_xfs {
        return Ok(None);
    }

    let mods: Vec<&str> = if needs_lvm { vec!["lvm", "dm"] } else { vec![] };
    let label = if needs_lvm && needs_xfs {
        "LVM+XFS"
    } else if needs_lvm {
        "LVM"
    } else {
        "XFS loopback"
    };
    println!(
        "[phase5] Rebuilding initrd with {} support via dracut...",
        label
    );

    let dracut_path = ["/usr/bin/dracut", "/usr/sbin/dracut", "dracut"]
        .iter()
        .find(|&&p| Path::new(p).exists())
        .copied()
        .ok_or_else(|| {
            anyhow!(
                "dracut not found; cannot rebuild initrd for {}.\n\
             Manual fix after booting OSTree fallback:\n\
             dracut --kver {} --add '{}' --force {}",
                label,
                kver,
                mods.join(" "),
                initrd_dst.display()
            )
        })?;

    // Extract Dakota kernel modules via registry streaming (layer-by-layer).
    // The EROFS mount is in bare-EROFS mode so large files read as zeros.
    // podman cp pulls the full image (~5 GB) into podman storage — ENOSPC.
    // Registry streaming downloads one layer at a time, extracts the needed
    // subtree, and drops the blob before the next. Peak disk: ~500 MB.
    let free = crate::preflight::get_free_space("/var/tmp").unwrap_or(0);
    if free < 1_500_000_000 {
        eprintln!(
            "[phase5] Skipping initrd rebuild: only {} MB free on /var/tmp (need ~1.5 GB).",
            free / 1_048_576
        );
        return Ok(None);
    }
    let kmod_dir = TempDir::new_in("/var/tmp").context("failed to create kmod extract dir")?;
    let subtree = format!("usr/lib/modules/{}", kver);
    println!(
        "[phase5] extracting kernel modules via registry stream (subtree: {})...",
        subtree
    );
    // Extract subtree to kmod_dir root. The subtree is
    // "usr/lib/modules/<kver>" and tar strips the leading component
    // (usr/), so files land at kmod_dir/lib/modules/<kver>/kernel/...
    let kmod_dest = kmod_dir.path().to_path_buf();
    fs::create_dir_all(&kmod_dest)?;
    extract_subtree_via_registry(target_image, &subtree, &kmod_dest)
        .context("failed to extract kernel modules via registry")?;
    let kmoddir_arg = kmod_dest.join("lib/modules").join(kver);
    if !kmoddir_arg.join("kernel").exists() {
        anyhow::bail!(
            "registry stream did not produce kernel modules at {}",
            kmoddir_arg.display()
        );
    }
    println!(
        "[phase5] kernel modules available at {}",
        kmoddir_arg.display()
    );

    // Run depmod so dracut can find all module dependencies.
    if Command::new("depmod")
        .args(["-b", kmod_dir.path().to_str().unwrap_or(""), kver])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        println!("[phase5] ran depmod on extracted kernel modules");
    } else {
        eprintln!("[phase5] Warning: depmod failed — dracut may not find all modules");
    }

    // Prepare bootc dracut module files. Bluefin LTS doesn't have the
    // bootc package, so /usr/lib/dracut/modules.d/51bootc/ doesn't exist.
    // We need to inject the files ourselves.
    let bootc_dir = TempDir::new_in("/var/tmp").ok();
    if let Some(ref bd) = bootc_dir {
        // Write the dracut module script (embedded in binary)
        let mod_dir = bd.path().join("usr/lib/dracut/modules.d/51bootc");
        fs::create_dir_all(&mod_dir).ok();
        fs::write(mod_dir.join("module-setup.sh"), BOOTC_DRACUT_MODULE).ok();

        // Write bootc-root-setup.service
        let svc_dir = bd.path().join("usr/lib/systemd/system");
        fs::create_dir_all(&svc_dir).ok();
        fs::write(
            svc_dir.join("bootc-root-setup.service"),
            BOOTC_ROOT_SETUP_SERVICE,
        )
        .ok();

        // Enable it in initrd-root-fs.target.wants
        let wants_dir = bd
            .path()
            .join("usr/lib/systemd/system/initrd-root-fs.target.wants");
        fs::create_dir_all(&wants_dir).ok();
        let _ = std::os::unix::fs::symlink(
            "../bootc-root-setup.service",
            wants_dir.join("bootc-root-setup.service"),
        );

        // Extract initramfs-setup binary from target image via registry
        println!("[phase5] extracting initramfs-setup from target image...");
        let setup_dst = bd.path().join("usr/lib/bootc/initramfs-setup");
        fs::create_dir_all(setup_dst.parent().unwrap_or(Path::new("/"))).ok();
        let extract_files = vec![(
            Path::new("/usr/lib/bootc/initramfs-setup"),
            setup_dst.as_path(),
        )];
        let setup_ok = match extract_files_via_registry(target_image, &extract_files) {
            Ok(()) => {
                // Make the binary executable — it must be run as /usr/lib/bootc/initramfs-setup
                // in the initramfs, and the kernel's execve requires +x.
                let _ = fs::set_permissions(&setup_dst, std::fs::Permissions::from_mode(0o755));
                true
            }
            Err(e) => {
                eprintln!("[phase5] Warning: could not extract initramfs-setup: {e}");
                false
            }
        };
        if setup_ok {
            println!("[phase5] bootc dracut module files prepared");
        }
    }

    // Rebuild with dracut.
    let mods_str = mods.join(" ");
    let dracut_add = if mods.is_empty() {
        "bootc".to_string()
    } else {
        format!("{} bootc", mods_str)
    };
    let mut cmd = Command::new(dracut_path);
    cmd.arg("--kver")
        .arg(kver)
        .arg("--force")
        .arg("--kmoddir")
        .arg(kmoddir_arg.to_str().unwrap_or(""));
    cmd.env("DRACUT_KMODDIR_OVERRIDE", "1");
    cmd.arg("--add").arg(&dracut_add);
    if let Some(ref bd) = bootc_dir {
        cmd.arg("--include")
            .arg(bd.path().join("usr").to_str().unwrap_or(""))
            .arg("/usr");
    }
    cmd.arg(initrd_dst.to_str().unwrap_or("/dev/null"));
    let dracut_status = cmd.status();

    // xfs.ko + loopback mount unit — write to separate cpio file alongside
    // the initrd. The kernel concatenates them in memory (bootc/sd-boot
    // support multiple initrd= lines). No risky cpio append needed.
    let mut extra_initrd: Option<String> = None;
    if needs_xfs {
        let xfs_cpio_dst = initrd_dst.with_file_name("xfs-mount.cpio");
        let xfs_src = kmoddir_arg.join("kernel/fs/xfs/xfs.ko");
        if xfs_src.exists() {
            let tmp = TempDir::new_in("/var/tmp")?;
            let mod_dir = tmp
                .path()
                .join("usr/lib/modules")
                .join(kver)
                .join("kernel/fs/xfs");
            fs::create_dir_all(&mod_dir)?;
            fs::copy(&xfs_src, mod_dir.join("xfs.ko"))?;
            let unit_dir = tmp.path().join("etc/systemd/system");
            fs::create_dir_all(&unit_dir)?;
            fs::write(
                unit_dir.join("sysroot-composefs.mount"),
                "[Unit]\nDescription=ComposeFS Loopback Mount\nAfter=sysroot.mount\nBefore=initrd-root-fs.target bootc-root-setup.service\nDefaultDependencies=no\n\n[Mount]\nWhat=/sysroot/composefs-loopback.ext4\nWhere=/sysroot/composefs\nType=ext4\nOptions=loop,ro\n\n[Install]\nWantedBy=initrd-root-fs.target\n",
            )?;
            // Enable the mount unit in initrd-root-fs.target.wants and add
            // Requires dependency from bootc-root-setup.service so the
            // loopback is mounted BEFORE composefs is mounted as root.
            let wants_dir = unit_dir.join("initrd-root-fs.target.wants");
            fs::create_dir_all(&wants_dir)?;
            let _ = std::os::unix::fs::symlink(
                "../sysroot-composefs.mount",
                wants_dir.join("sysroot-composefs.mount"),
            );
            let dropin_dir = unit_dir.join("bootc-root-setup.service.d");
            fs::create_dir_all(&dropin_dir)?;
            fs::write(
                dropin_dir.join("RequiresLoopback.conf"),
                "[Unit]\nRequires=sysroot-composefs.mount\nAfter=sysroot-composefs.mount\n",
            )?;
            if !Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "cd {} && find . -mindepth 1 | cpio -o -H newc -R 0:0 > {}",
                    tmp.path().display(),
                    xfs_cpio_dst.display()
                ))
                .status()?
                .success()
            {
                return Err(anyhow!("cpio for xfs+mount"));
            }
            println!(
                "[phase5] wrote xfs.ko + mount unit to separate cpio: {}",
                xfs_cpio_dst.display()
            );
            extra_initrd = Some(
                xfs_cpio_dst
                    .file_name()
                    .unwrap_or(std::ffi::OsStr::new("xfs-mount.cpio"))
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }

    match dracut_status {
        Ok(s) if s.success() => {
            println!(
                "[phase5] {}-enabled initrd staged at {}.",
                label,
                initrd_dst.display()
            );
            Ok(extra_initrd)
        }
        Ok(s) => {
            eprintln!(
                "[phase5] Warning: dracut exited {:?} — initrd may lack {} support.\n\
                 If the system fails to boot, select the OSTree fallback entry and run:\n \
                 dracut --kver {} --add '{}' --force {}",
                s.code(),
                label,
                kver,
                mods_str,
                initrd_dst.display()
            );
            Ok(extra_initrd)
        }
        Err(e) => {
            eprintln!(
                "[phase5] Warning: dracut failed to run ({}) — initrd may lack {} support.\n\
                 If the system fails to boot, select the OSTree fallback entry and run:\n \
                 dracut --kver {} --add '{}' --force {}",
                e,
                label,
                kver,
                mods_str,
                initrd_dst.display()
            );
            Ok(extra_initrd)
        }
    }
}
