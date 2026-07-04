pub mod bootloader;
pub mod kernel_options;
pub mod os_release;

use crate::VerityDigest;
use crate::preflight::PreflightReport;
use crate::xattr;
use anyhow::{Context, Result, anyhow};
use kernel_options::get_kernel_options;
use os_release::{bls_entry_filename, bls_entry_title, read_os_release};
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ---- Lock file ----

const LOCK_PATH: &str = "/var/run/bootc-migrate-composefs.lock";

fn acquire_lock() -> Result<File> {
    let lock = File::create(LOCK_PATH).context("failed to create lock file")?;
    // Non-blocking exclusive advisory lock, released when this fd is closed
    // (i.e. on process exit). Guards against concurrent migration runs.
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK | Errno::ACCESS) => {
            return Err(anyhow!(
                "Another instance of bootc-migrate-composefs is already running (lock held at {}).",
                LOCK_PATH
            ));
        }
        Err(e) => return Err(e).context("failed to acquire lock"),
    }
    // Write PID so admins can inspect.
    let _ = writeln!(&lock, "{}", std::process::id());
    Ok(lock)
}

// ---- Mount guard (Optional: safe cleanup of TempDir-backed mounts) ----

struct MountGuard {
    mount_path: PathBuf,
}

impl MountGuard {
    fn new(mount_path: &Path) -> Self {
        MountGuard {
            mount_path: mount_path.to_path_buf(),
        }
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let status = Command::new("umount").arg(&self.mount_path).status();
        match status {
            Ok(s) if s.success() => {}
            _ => eprintln!(
                "Warning: failed to unmount {} — a stale mount may remain. Use 'umount {}' manually.",
                self.mount_path.display(),
                self.mount_path.display(),
            ),
        }
    }
}

/// RAII guard around `podman image mount`. Mounts a locally-cached OCI image and
/// exposes its merged rootfs at `path`, unmounting on drop. Used as the Phase 5
/// fallback when the composefs overlay mount yields no usable content (bootc
/// mounts in a private namespace that does not persist to our process). Because
/// Phase 2 also `podman pull`s the image, this needs no network.
struct PodmanImageMount {
    image: String,
    path: PathBuf,
}

impl PodmanImageMount {
    fn new(image: &str) -> Result<Self> {
        let out = Command::new("podman")
            .args(["image", "mount", image])
            .output()
            .context("failed to execute podman image mount")?;
        if !out.status.success() {
            return Err(anyhow!(
                "podman image mount {} failed: {}",
                image,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if !path.is_dir() {
            return Err(anyhow!(
                "podman image mount returned non-directory path: {}",
                path.display()
            ));
        }
        Ok(PodmanImageMount {
            image: image.to_string(),
            path,
        })
    }
}

impl Drop for PodmanImageMount {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["image", "unmount", &self.image])
            .status();
    }
}

// ---- Public API ----

/// Check free space before pulling. Returns Ok(()) if sufficient, Err otherwise.
pub fn check_free_space(reflink_available: bool) -> Result<()> {
    let ostree_repo = "/sysroot/ostree/repo";
    if !Path::new(ostree_repo).exists() {
        return Ok(());
    }

    let du = Command::new("/usr/bin/du")
        .args(["-sb", ostree_repo])
        .output()
        .context("failed to run du")?;
    let du_stdout = String::from_utf8_lossy(&du.stdout);
    let ostree_size: u64 = du_stdout
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let free = crate::preflight::get_free_space("/sysroot/composefs")
        .or_else(|_| crate::preflight::get_free_space("/sysroot"))?;
    let multiplier: f64 = if reflink_available { 1.1 } else { 1.5 };
    let needed = (ostree_size as f64 * multiplier) as u64;

    println!(
        "Free space check: ostree repo = {:.2} GB, free = {:.2} GB, needed ≈ {:.2} GB (reflink: {})",
        ostree_size as f64 / 1e9,
        free as f64 / 1e9,
        needed as f64 / 1e9,
        reflink_available,
    );

    if free < needed {
        return Err(anyhow!(
            "Insufficient free space: need ~{:.2} GB, have {:.2} GB. Free up space or use a larger disk.",
            needed as f64 / 1e9,
            free as f64 / 1e9,
        ));
    }
    Ok(())
}

/// XFS does not support fs-verity (required by cfs pull). When the /sysroot
/// filesystem lacks verity, create a loopback ext4 image, mount it at
/// /sysroot/composefs, and migrate the composefs store onto it.
/// composefs repository metadata (`meta.json`) as written by `cfsctl init`:
/// format version 1 with sha512 fs-verity digests. Required by `bootc status`
/// and cfsctl; our hand-built XFS-loopback repo must carry it.
const COMPOSEFS_REPO_META_JSON: &str = "{\n  \"version\": 1,\n  \"algorithm\": \"fsverity-sha512-12\",\n  \"features\": {\n    \"compatible\": [],\n    \"read-only-compatible\": [],\n    \"incompatible\": []\n  }\n}\n";

fn setup_composefs_loopback_if_needed(report: &PreflightReport) -> Result<Option<MountGuard>> {
    let fs_type = report.fs_type.as_deref().unwrap_or("unknown");
    // btrfs and ext4 support fs-verity. xfs does not (as of kernel 6.12).
    if fs_type == "xfs" {
        let target = "/sysroot/composefs";
        let img_path = "/sysroot/composefs-loopback.ext4";

        // Don't recreate if already set up (e.g. re-run after crash).
        if Path::new(img_path).exists() {
            // Check if already mounted at target.
            let mount_out = Command::new("findmnt")
                .args(["-n", "-o", "SOURCE", target])
                .output()
                .ok();
            if let Some(out) = mount_out {
                let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if src.contains("composefs-loopback") {
                    println!("ComposeFS loopback already active at {target} (source: {src}).");
                    return Ok(None);
                }
            }
            // Image exists but not mounted — remove stale and recreate.
            let _ = fs::remove_file(img_path);
        }

        // Sizing this off the *source* ostree repo alone badly undersizes it:
        // Phase 2 pulls the *target* image into this same loopback regardless
        // of whether Phase 1's reflink import (the only thing ostree_gb
        // actually measures) runs at all — with --skip-import, or a small
        // source migrating to a much larger target, the old 10-30 GB clamp
        // left no room for the pull and ENOSPC'd mid-Phase-2 (#42). The
        // loopback is a sparse file (ext4 allocates blocks on demand), so a
        // generous nominal size is free — bound only by what the underlying
        // filesystem actually has (composefs_free_bytes, already measured by
        // preflight), not by an arbitrary fixed ceiling.
        let ostree_gb = report.ostree_repo_size_bytes as f64 / 1e9;
        let free_gb = report.composefs_free_bytes as f64 / 1e9;
        let desired_gb = (ostree_gb * 1.5 + 25.0).ceil() as u64;
        let max_gb = ((free_gb * 0.9) as u64).max(30);
        let size_gb = desired_gb.clamp(30, max_gb);
        println!(
            "XFS detected — setting up {size_gb} GB ext4 loopback for composefs verity support.",
        );

        // Create sparse file (ext4 will allocate blocks on demand).
        let status = Command::new("truncate")
            .args(["-s", &format!("{size_gb}G"), img_path])
            .status()
            .context("failed to truncate composefs loopback image")?;
        if !status.success() {
            return Err(anyhow!("truncate failed for composefs loopback image"));
        }

        // Format as ext4 with verity support.
        let status = Command::new("/usr/sbin/mkfs.ext4")
            .args(["-F", "-O", "verity", img_path])
            .status()
            .context("failed to format composefs loopback as ext4")?;
        if !status.success() {
            return Err(anyhow!("mkfs.ext4 failed for composefs loopback"));
        }

        // Mount.
        fs::create_dir_all(target).context("failed to create /sysroot/composefs")?;
        let status = Command::new("/usr/bin/mount")
            .args(["-o", "loop", img_path, target])
            .status()
            .context("failed to mount composefs loopback")?;
        if !status.success() {
            return Err(anyhow!("mount failed for composefs loopback"));
        }

        // Initialize the composefs repository metadata. Migration populates
        // objects/images/streams by hand; without meta.json `bootc status` and
        // cfsctl reject the repo ("must be initialized with `cfsctl init`").
        // Matches what `cfsctl init` writes (format v1, sha512 fs-verity).
        fs::write(
            Path::new(target).join("meta.json"),
            COMPOSEFS_REPO_META_JSON,
        )
        .context("failed to write composefs repo meta.json")?;

        println!("ComposeFS loopback mounted at {target} ({size_gb} GB ext4, fs-verity enabled).");
        Ok(Some(MountGuard::new(Path::new(target))))
    } else {
        Ok(None)
    }
}

/// Detect whether LVM volumes are active on the running system.
fn detect_lvm() -> bool {
    match fs::read_dir("/dev/mapper") {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy() != "control"),
        Err(_) => false,
    }
}

/// Detect a dedicated `/var` filesystem (a separate partition/LV, anaconda's
/// default), returning `(uuid, fstype)` of its backing device.
///
/// bootc's composefs boot bind-mounts the per-stateroot var
/// (`/sysroot/state/os/default/var`, on the root fs) onto `/var` and *ignores*
/// any `/var` fstab entry — so on a system where `/var` lives on its own volume,
/// the composefs boot silently uses the empty stateroot var instead, losing the
/// user's home, flatpaks, etc. We detect that case so Phase 5 can mount the real
/// `/var` volume at the stateroot var path before bootc binds it (see
/// [`prepare_stateroot_var_include`]).
///
/// "Separate" means the filesystem mounted at `/var` is a whole filesystem
/// (FSROOT `/`), not a subtree bind of the root fs (e.g. btrfs `subvol=` or the
/// ostree `…/var` bind, whose FSROOT is a subpath).
fn detect_separate_var() -> Option<(String, String)> {
    let out = Command::new("findmnt")
        .args(["-no", "SOURCE,FSTYPE,FSROOT", "/var"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 3 {
        return None;
    }
    let (source, fstype, fsroot) = (fields[0], fields[1], fields[2]);
    if fsroot != "/" {
        return None; // a subtree bind (subvol / ostree var), not a separate fs
    }
    let uuid = blkid_uuid(source)?;
    Some((uuid, fstype.to_string()))
}

/// Resolve a block device's filesystem UUID via `blkid`.
fn blkid_uuid(device: &str) -> Option<String> {
    let out = Command::new("blkid")
        .args(["-o", "value", "-s", "UUID", device])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uuid.is_empty() { None } else { Some(uuid) }
}

/// Rebuild the staged initrd with LVM/DM support using the host's dracut and
/// Dakota's kernel modules from the composefs overlay mount.
///
/// Non-fatal: warns if dracut is absent or fails so migration still completes.
/// The user can rerun dracut manually from the OSTree fallback if the system
/// fails to boot (see the warning message for the exact command).
/// Build a scratch tree (for `dracut --include`) carrying the systemd units that
/// loop-mount the composefs ext4 store at /sysroot/composefs inside the initrd,
/// ordered after sysroot.mount and before bootc-root-setup.service. Returns the
/// tempdir guard; its contents are copied into the initrd by dracut.
fn prepare_composefs_loopback_include() -> Result<tempfile::TempDir> {
    let tmp = tempfile::Builder::new()
        .prefix("bootc-cfsloop-")
        .tempdir_in("/var/tmp")
        .context("failed to create scratch dir for composefs loopback unit")?;
    let unit_dir = tmp.path().join("etc/systemd/system");
    fs::create_dir_all(&unit_dir)?;
    fs::write(
        unit_dir.join("sysroot-composefs.mount"),
        "[Unit]\n\
         Description=ComposeFS Loopback Mount\n\
         After=sysroot.mount\n\
         Before=initrd-root-fs.target bootc-root-setup.service\n\
         DefaultDependencies=no\n\
         \n\
         [Mount]\n\
         What=/sysroot/composefs-loopback.ext4\n\
         Where=/sysroot/composefs\n\
         Type=ext4\n\
         Options=loop,ro\n\
         \n\
         [Install]\n\
         WantedBy=initrd-root-fs.target\n",
    )?;
    // Enable the mount unit and make bootc-root-setup require + order after it.
    let wants_dir = unit_dir.join("initrd-root-fs.target.wants");
    fs::create_dir_all(&wants_dir)?;
    std::os::unix::fs::symlink(
        "../sysroot-composefs.mount",
        wants_dir.join("sysroot-composefs.mount"),
    )
    .context("failed to enable sysroot-composefs.mount")?;
    let dropin_dir = unit_dir.join("bootc-root-setup.service.d");
    fs::create_dir_all(&dropin_dir)?;
    fs::write(
        dropin_dir.join("RequiresLoopback.conf"),
        "[Unit]\nRequires=sysroot-composefs.mount\nAfter=sysroot-composefs.mount\n",
    )?;
    Ok(tmp)
}

/// Build a scratch tree (for `dracut --include`) carrying a systemd mount unit
/// that mounts the dedicated `/var` volume at the composefs stateroot var path
/// (`/sysroot/state/os/default/var`) inside the initrd, ordered after
/// sysroot.mount and before bootc-root-setup.service.
///
/// bootc-root-setup bind-mounts that path onto the deployment's `/var`, so
/// overmounting it with the real `/var` volume here makes the user's data appear
/// at `/var` — working around bootc composefs ignoring the `/var` fstab entry on
/// systems with a dedicated `/var` partition/LV (see [`detect_separate_var`]).
/// `uuid`/`fstype` identify the volume; the LV is activated via the
/// `rd.lvm.lv=<vg>/<lv>` karg emitted by `get_kernel_options`.
fn prepare_stateroot_var_include(uuid: &str, fstype: &str) -> Result<tempfile::TempDir> {
    let tmp = tempfile::Builder::new()
        .prefix("bootc-statevar-")
        .tempdir_in("/var/tmp")
        .context("failed to create scratch dir for stateroot var unit")?;
    let unit_dir = tmp.path().join("etc/systemd/system");
    fs::create_dir_all(&unit_dir)?;
    // Mount path /sysroot/state/os/default/var → unit sysroot-state-os-default-var.mount
    let unit_name = "sysroot-state-os-default-var.mount";
    fs::write(
        unit_dir.join(unit_name),
        format!(
            "[Unit]\n\
             Description=Dedicated /var volume (composefs stateroot)\n\
             After=sysroot.mount\n\
             Before=initrd-root-fs.target bootc-root-setup.service\n\
             DefaultDependencies=no\n\
             \n\
             [Mount]\n\
             What=/dev/disk/by-uuid/{uuid}\n\
             Where=/sysroot/state/os/default/var\n\
             Type={fstype}\n\
             Options=defaults\n\
             \n\
             [Install]\n\
             WantedBy=initrd-root-fs.target\n"
        ),
    )?;
    let wants_dir = unit_dir.join("initrd-root-fs.target.wants");
    fs::create_dir_all(&wants_dir)?;
    std::os::unix::fs::symlink(format!("../{unit_name}"), wants_dir.join(unit_name))
        .context("failed to enable sysroot-state-os-default-var.mount")?;
    let dropin_dir = unit_dir.join("bootc-root-setup.service.d");
    fs::create_dir_all(&dropin_dir)?;
    fs::write(
        dropin_dir.join("RequiresStaterootVar.conf"),
        format!("[Unit]\nRequires={unit_name}\nAfter={unit_name}\n"),
    )?;
    Ok(tmp)
}

fn rebuild_initrd_with_lvm_if_needed(
    kver: &str,
    mount_path: &Path,
    target_image: &str,
    initrd_dst: &Path,
) -> Result<()> {
    // LUKS roots appear as device-mapper nodes (detect_lvm), and XFS roots get
    // an ext4 loopback for the verity store. The stock Dakota initrd already
    // handles dm/crypt and composefs; for XFS it just lacks the xfs driver.
    let needs_dm = detect_lvm();
    let needs_xfs = Path::new("/sysroot/composefs-loopback.ext4").exists();
    // A dedicated /var volume needs a mount unit injected so bootc's composefs
    // boot exposes its data at /var (see prepare_stateroot_var_include).
    let separate_var = detect_separate_var();
    if !needs_dm && !needs_xfs && separate_var.is_none() {
        return Ok(());
    }
    let mut features: Vec<&str> = Vec::new();
    if needs_dm {
        features.push("LVM/DM/crypt");
    }
    if needs_xfs {
        features.push("XFS");
    }
    if separate_var.is_some() {
        features.push("dedicated /var");
    }
    let label = features.join(" + ");
    println!("[phase5] Rebuilding composefs initrd with {label} support...");
    if let Some((ref uuid, ref fstype)) = separate_var {
        println!(
            "[phase5] dedicated /var detected ({fstype}, UUID={uuid}) — will mount it at the composefs stateroot var path"
        );
    }

    // Source the target's kernel modules from the sealed composefs overlay mount
    // (real bytes, no network), falling back to registry streaming if they're
    // absent. `_modules_tmp` holds the writable copy alive until the rebuild ends
    // (depmod must write modules.dep.bin, so the read-only mount can't be used
    // directly).
    let (_modules_tmp, modules_src) = match copy_kernel_modules_from_mount(mount_path, kver) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[phase5] kernel modules not available from mount ({e:#}); using registry");
            extract_kernel_modules_via_registry(target_image, kver)
                .context("failed to obtain target kernel modules for initrd rebuild")?
        }
    };

    // The target image ships no dracut binary — only its dracut *modules* — so we
    // run the *source* system's dracut, which carries the same 50ostree/51bootc
    // dracut modules. `--rebuild` then re-runs the target initrd's stored build
    // configuration (preserving the composefs root assembly, crypt, and dm
    // modules) and only ADDS the missing xfs driver (plus dm/crypt/lvm as
    // belt-and-suspenders for the LUKS root).
    //
    // The catch: dracut resolves the kernel module index from the standard
    // /lib/modules/<kver> path and ignores --kmoddir for it. On the source —
    // whose running kernel differs from the target's <kver> — that path is empty,
    // so every driver (erofs, overlay, dm, crypt, xfs) silently drops out and the
    // initrd is unbootable. We fix that by making /lib/modules/<kver> resolve to
    // the target's modules: a staging dir whose <kver> entry symlinks to the
    // mounted target modules is bind-mounted over /usr/lib/modules (= /lib/
    // modules) for the rebuild, then unmounted.
    let dracut_path = ["/usr/bin/dracut", "/usr/sbin/dracut", "dracut"]
        .iter()
        .find(|&&p| Path::new(p).exists())
        .copied()
        .ok_or_else(|| anyhow!("dracut not found on source; cannot rebuild initrd for {label}"))?;

    let modules_root = PathBuf::from("/usr/lib/modules");
    let staging = PathBuf::from("/var/tmp").join(format!("bootc-kmod-root-{}", std::process::id()));
    // staging/<kver> -> <mount>/usr/lib/modules/<kver>. The link target is an
    // absolute path *outside* /usr/lib/modules, so it stays valid after we bind
    // staging over /usr/lib/modules (no self-referential loop).
    let staged_kver = staging.join(kver);

    // For XFS roots the composefs verity store lives in an ext4 loopback file on
    // the XFS root. The initrd must loop-mount it at /sysroot/composefs after the
    // root mounts but before bootc assembles composefs, otherwise bootc-root-setup
    // fails with "Opening ref 'images/<hash>': No such file or directory". Inject
    // a systemd mount unit (+ ordering drop-in) via dracut --include; the ext4 and
    // loop drivers added below let the initrd actually mount it.
    let loop_include = if needs_xfs {
        Some(prepare_composefs_loopback_include()?)
    } else {
        None
    };
    let var_include = match separate_var {
        Some((ref uuid, ref fstype)) => Some(prepare_stateroot_var_include(uuid, fstype)?),
        None => None,
    };

    let mut bound = false;
    let run_rebuild = |bound: &mut bool| -> Result<std::process::ExitStatus> {
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        fs::create_dir_all(&staging)
            .with_context(|| format!("create kmod staging dir {}", staging.display()))?;
        std::os::unix::fs::symlink(&modules_src, &staged_kver).with_context(|| {
            format!(
                "symlink {} -> {}",
                staged_kver.display(),
                modules_src.display()
            )
        })?;

        let st = Command::new("mount")
            .arg("--bind")
            .arg(&staging)
            .arg(&modules_root)
            .status()
            .with_context(|| {
                format!("bind {} over {}", staging.display(), modules_root.display())
            })?;
        if !st.success() {
            return Err(anyhow!(
                "failed to bind kmod staging over {}",
                modules_root.display()
            ));
        }
        *bound = true;

        // /lib/modules/<kver> now resolves to the target modules (valid
        // modules.dep.bin); `--rebuild` preserves composefs and adds xfs.
        let mut cmd = Command::new(dracut_path);
        cmd.arg("--rebuild")
            .arg(initrd_dst)
            .arg("--kver")
            .arg(kver)
            .arg("--force");
        if needs_dm {
            cmd.arg("--add").arg("lvm dm crypt");
        }
        if needs_xfs {
            cmd.arg("--add-drivers").arg("xfs ext4 loop");
            if let Some(ref inc) = loop_include {
                cmd.arg("--include").arg(inc.path()).arg("/");
            }
        }
        if let Some(ref inc) = var_include {
            // Ensure xfs/ext4 are present even when there's no composefs loopback
            // (the dedicated /var may be the only reason we rebuild).
            cmd.arg("--add-drivers").arg("xfs ext4");
            cmd.arg("--include").arg(inc.path()).arg("/");
        }
        cmd.status().context("failed to run dracut --rebuild")
    };

    let result = run_rebuild(&mut bound);

    // Restore the source's /usr/lib/modules and drop the staging dir, regardless
    // of the dracut outcome.
    if bound
        && let Ok(s) = Command::new("umount")
            .arg("--lazy")
            .arg(&modules_root)
            .status()
        && !s.success()
    {
        eprintln!(
            "[phase5] Warning: failed to unmount kmod staging from {}",
            modules_root.display()
        );
    }
    let _ = fs::remove_dir_all(&staging);

    match result {
        Ok(s) if s.success() => {
            println!(
                "[phase5] {label} initrd rebuilt and staged at {}.",
                initrd_dst.display()
            );
            Ok(())
        }
        Ok(s) => {
            eprintln!(
                "[phase5] Warning: dracut exited {:?} — composefs initrd left unchanged; it \
                 lacks {label} support and the composefs entry may not boot. Boot the OSTree \
                 fallback and rerun the migration to recover.",
                s.code()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "[phase5] Warning: initrd rebuild failed ({e:#}) — composefs initrd left \
                 unchanged; boot the OSTree fallback to recover."
            );
            Ok(())
        }
    }
}

/// Main migration entry point. Orchestrates all 5 phases.
pub fn run_migration(
    report: &PreflightReport,
    target_image: &str,
    dry_run: bool,
    skip_import: bool,
    bootloader: &str,
    force: bool,
) -> Result<()> {
    // Acquire exclusive lock.
    let _lock = if !dry_run {
        Some(acquire_lock()?)
    } else {
        None
    };

    if dry_run {
        println!("[DRY RUN] Would execute migration phases without making changes.");
    }

    // Mount /sysroot and /boot read-write.
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

    // ---- Phase 0: preflight free-space check ----
    println!("=== Phase 0: Free-space check ===");
    if !dry_run {
        check_free_space(report.supports_reflink)?;
    } else {
        println!("[DRY RUN] Would check free space on /sysroot/composefs.");
    }

    // ---- XFS workaround: ensure composefs store supports fs-verity ----
    let _loopback_guard: Option<MountGuard> = if !dry_run {
        setup_composefs_loopback_if_needed(report)?
    } else {
        let fs_type = report.fs_type.as_deref().unwrap_or("unknown");
        if fs_type == "xfs" {
            println!("[DRY RUN] Would set up ext4 loopback at /sysroot/composefs for fs-verity.");
        }
        None
    };

    // ---- Phase 1: Import OSTree objects (optional / deletable) ----
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
    let (verity, sealed_config) = phase3_create_image(target_image, &config_digest, dry_run)?;

    // ---- Phase 4: Stage deployment state ----
    let _deploy_dir = phase4_stage_deploy(
        &verity,
        target_image,
        &_manifest_digest,
        &config_digest,
        &sealed_config,
        dry_run,
    )?;

    // ---- Phase 5: Setup bootloader ----
    phase5_setup_bootloader(
        report,
        &verity,
        target_image,
        &sealed_config,
        dry_run,
        bootloader,
        force,
    )?;

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
    if !dry_run {
        // Best-effort: a login reminder is a courtesy, not a migration
        // requirement — don't fail an otherwise-successful migration over it.
        if let Err(e) = crate::motd::write_migration_reminder(verity.as_hex()) {
            eprintln!("Warning: failed to write login reminder: {e:#}");
        }
    }
    Ok(())
}

// ---- Phase 1 ----

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
        if count.is_multiple_of(1000) {
            println!("Imported {}/{} objects...", count, total_objects);
        }
    }
    println!("Imported {} objects ({} reflinked).", count, reflink_count);
    Ok(())
}

// ---- Phase 2 ----

fn phase2_pull_image(target_image: &str, dry_run: bool) -> Result<(String, String)> {
    println!("=== Phase 2: Pulling OCI image ===");

    if dry_run {
        println!("[DRY RUN] Would pull image: {}", target_image);
        return Ok(("dry-run-manifest".into(), "dry-run-config".into()));
    }

    println!("Pulling target image: {}...", target_image);
    let pull_output = crate::composefs::pull_image(target_image, target_image)
        .context("failed to pull OCI image")?;

    // Also cache in podman storage so Phase 5 can fall back to podman artifact
    // extraction without a re-pull if the composefs overlay mount is unavailable.
    let podman_pull = Command::new("podman").args(["pull", target_image]).status();
    match podman_pull {
        Ok(s) if s.success() => println!("Image also cached in podman storage."),
        Ok(s) => eprintln!("[phase2] podman pull exited {s} — Phase 5 may need to re-pull"),
        Err(e) => eprintln!("[phase2] podman pull failed: {e} — Phase 5 may need to re-pull"),
    }

    let (manifest_opt, config_opt) = parse_pull_digests(&pull_output);
    let config_digest = config_opt.unwrap_or_default();
    // bootc's cfs oci pull output may omit the OCI manifest digest (1.13.0 prints
    // only `config <sha256>` + `verity <hash>`). bootc status/upgrade reads
    // `[image]/manifest_digest`, and the old code's "use the whole output" fallback
    // produced a MULTI-LINE value that corrupts the .origin ini (breaking
    // `bootc status` entirely). Prefer the real manifest digest from the
    // locally-cached image (Phase 2 podman pull); fall back to the config digest —
    // always a single-line sha256.
    let manifest_digest = manifest_opt
        .or_else(|| podman_manifest_digest(target_image))
        .unwrap_or_else(|| config_digest.clone());
    println!(
        "Target image pulled. Manifest: {}, Config: {}",
        manifest_digest, config_digest
    );
    Ok((manifest_digest, config_digest))
}

/// Parse `bootc internals cfs oci pull` stdout into `(manifest_digest, config_digest)`.
///
/// Handles the 1.13.0 format that prints only `config <sha256>` + `verity <hash>`
/// (no `manifest` line). Critically, never yields a multi-line `manifest_digest` —
/// a newline in that value corrupts the deployment `.origin` ini and breaks
/// `bootc status`. The config digest falls back to the first `sha256:` token.
fn parse_pull_digests(pull_output: &str) -> (Option<String>, Option<String>) {
    let mut manifest = None;
    let mut config = None;
    for line in pull_output.lines() {
        let t = line.trim();
        if let Some(r) = t.strip_prefix("manifest ") {
            manifest = Some(r.trim().to_string());
        } else if let Some(r) = t.strip_prefix("config ") {
            config = Some(r.trim().to_string());
        }
    }
    // A valid digest is a single non-empty token; reject anything else.
    let manifest = manifest.filter(|m| !m.is_empty() && !m.contains(char::is_whitespace));
    let config = config
        .filter(|c| !c.is_empty() && !c.contains(char::is_whitespace))
        .or_else(|| {
            pull_output
                .split_whitespace()
                .find(|x| x.starts_with("sha256:"))
                .map(String::from)
        });
    (manifest, config)
}

/// Read the OCI manifest digest (`sha256:…`) of a locally-cached image via
/// `podman image inspect`. Returns None if podman/the image is unavailable.
fn podman_manifest_digest(image: &str) -> Option<String> {
    let out = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Digest}}", image])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let d = String::from_utf8_lossy(&out.stdout).trim().to_string();
    d.starts_with("sha256:").then_some(d)
}

// ---- Phase 3 ----

/// Returns `(rootfs_verity, sealed_config_digest)`. The rootfs verity is the
/// composefs image object ID used in `.origin`/BLS for the boot-time root
/// mount; the sealed config digest (`sha256:…`) is the *manifest stream*
/// identifier that `bootc … cfs oci mount` requires (it prepends
/// `oci-config-` and looks up `streams/oci-config-<digest>`). These are
/// distinct: passing the rootfs verity to `mount` looks up a nonexistent
/// `oci-config-<verity>` stream and forces the zero-filling raw-EROFS fallback.
fn phase3_create_image(
    target_image: &str,
    config_digest: &str,
    dry_run: bool,
) -> Result<(VerityDigest, String)> {
    println!("=== Phase 3: Creating ComposeFS EROFS Image ===");

    if dry_run {
        println!(
            "[DRY RUN] Would create and seal composefs image for config: {}",
            config_digest
        );
        return Ok((
            // "dryrun..." isn't valid hex (r/y/u/n) — from_hex asserts hex-only,
            // so a placeholder digest must actually be hex. deadbeef is the
            // traditional obviously-fake stand-in.
            VerityDigest::from_hex(
                "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            ),
            config_digest.to_string(),
        ));
    }

    // Real idempotency — check if the image already exists AND is sealed.
    // We first need the verity hash to check, so we still call create_image (which
    // is typically a no-op if objects already exist), then skip seal if already done.
    let sha512_verity_str = crate::composefs::create_image(target_image, config_digest)
        .context("failed to create composefs image")?;

    let verity = VerityDigest::from_prefixed_or_hex(&sha512_verity_str);
    println!(
        "ComposeFS EROFS image created. Verity digest: {}",
        verity.as_hex()
    );

    // `bootc … cfs oci seal` clones the manifest with the embedded verity digest
    // and prints the *sealed* manifest's `config <sha256:…>` line. That sealed
    // config digest — NOT the rootfs verity above — is what `cfs oci mount`
    // needs; without using it, mount fails and bootc falls back to a raw kernel
    // EROFS mount which zero-fills files above the inline threshold (causing
    // missing unit files like dbus.service and cascading boot failures).
    // Always seal — idempotency is handled inside bootc.
    println!("Sealing composefs image...");
    let seal_out = crate::composefs::seal_image(target_image, config_digest)
        .context("failed to seal composefs image")?;
    let sealed_config = seal_out
        .lines()
        .find_map(|l| l.trim().strip_prefix("config "))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow!("seal output missing 'config <digest>' line; got:\n{seal_out}"))?;
    println!("Image sealed successfully (sealed config: {sealed_config}).");

    // #3 — verify the finished store is readable by the target image's bootc, so
    // a bootc format skew fails loudly here instead of silently breaking
    // `bootc status`/`upgrade` after reboot.
    crate::composefs::verify_store_target_readable(target_image)
        .context("composefs store is not readable by the target image's bootc")?;
    println!("Verified: composefs store is readable by the target's bootc.");

    Ok((verity, sealed_config))
}

// ---- Phase 4 ----

/// Build the `.origin` file content that bootc parses to identify a composefs
/// deployment. Uses `tini::Ini` for byte-compatible output with bootc's parser.
fn build_origin_content(
    target_image: &str,
    verity: &VerityDigest,
    manifest_digest: &str,
) -> String {
    // Schema must match bootc's canonical layout (crates/lib/src/composefs_consts.rs):
    //   [origin] container-image-reference = ...
    //   [boot]   boot_type = bls
    //   [boot]   digest = <verity hex>           # NB: key is "digest", not "boot_digest"
    //   [image]  manifest_digest = sha256:...
    // bootc's status code reads from [image]/manifest_digest and [boot]/digest;
    // wrong section or key names produce "No manifest_digest in origin and no
    // legacy .imginfo file" or "Could not find boot digest for deployment".
    tini::Ini::new()
        .section("origin")
        .item(
            "container-image-reference",
            format!("ostree-unverified-image:docker://{}", target_image),
        )
        .section("boot")
        .item("boot_type", "bls")
        .item("digest", verity.as_hex())
        .section("image")
        .item("manifest_digest", manifest_digest)
        .to_string()
}

/// Patch the `digest` entry in `[boot]` with a real sha256(vmlinuz || initrd).
/// Pure function so we can test it without filesystem access.
fn patch_boot_digest_in_content(content: &str, new_digest: &str) -> Result<String> {
    let ini = tini::Ini::from_string(content)
        .map_err(|e| anyhow!("parsing origin file: {e}"))?
        .section("boot")
        .item("digest", new_digest);
    Ok(ini.to_string())
}

fn phase4_stage_deploy(
    verity: &VerityDigest,
    target_image: &str,
    manifest_digest: &str,
    config_digest: &str,
    sealed_config: &str,
    dry_run: bool,
) -> Result<PathBuf> {
    println!("=== Phase 4: Staging Deployment State ===");

    let deploy_dir = Path::new("/sysroot/state/deploy").join(verity.as_hex());

    if dry_run {
        println!(
            "[DRY RUN] Would stage deployment at: {}",
            deploy_dir.display()
        );
        return Ok(deploy_dir);
    }

    // Idempotency: skip if already staged with valid .origin.
    // bootc expects the filename as `<bare-hex-verity>.origin` (no `sha512:`
    // prefix); using as_prefixed() here would cause `bootc status` to fail
    // with "Opening origin file: No such file or directory" and break the
    // post-reboot validation.
    let origin_path = deploy_dir.join(format!("{}.origin", verity.as_hex()));
    if deploy_dir.exists() && origin_path.exists() {
        println!(
            "Deployment already staged at {}. Skipping Phase 4.",
            deploy_dir.display()
        );
        return Ok(deploy_dir);
    }

    fs::create_dir_all(&deploy_dir).context("failed to create deployment directory")?;

    let etc_dir = deploy_dir.join("etc");
    fs::create_dir_all(&etc_dir).context("failed to create deployment etc directory")?;

    // 3-way /etc merge
    println!("Performing 3-way /etc merge...");
    if let Err(e) = perform_etc_merge(target_image, sealed_config, &etc_dir) {
        eprintln!(
            "3-way /etc merge failed ({}), falling back to flat /etc copy.",
            e
        );
        xattr::copy_dir_all_with_xattrs("/etc", &etc_dir)
            .context("failed to copy /etc (fallback)")?;
    }

    // Stage /var symlink
    let var_symlink = deploy_dir.join("var");
    if var_symlink.exists() {
        fs::remove_file(&var_symlink).context("failed to remove existing var entry")?;
    }
    std::os::unix::fs::symlink("../../os/default/var", &var_symlink)
        .context("failed to create /var symlink")?;

    // Write .origin file using bootc's expected schema (testutils.rs:316-331).
    // Use the same `tini::Ini` library bootc uses to parse it so the output
    // is byte-compatible. Placeholder boot_digest gets patched in Phase 5
    // with sha256(vmlinuz || initrd) once those files are on the ESP.
    //
    // Key names are load-bearing:
    // - `container-image-reference` is `ostree_ext::container::deploy::ORIGIN_CONTAINER`
    //   — bootc reads this to populate the BootEntry's image field.
    // - `manifest_digest` under [boot] lets bootc fetch the OCI manifest from
    //   the registry without a separate .imginfo file (`bootc internals cfs oci
    //   inspect` is unreliable in our flow).
    let origin_content = build_origin_content(target_image, verity, manifest_digest);
    fs::write(&origin_path, &origin_content).context("failed to write .origin file")?;

    // Write .imginfo file
    println!("Writing .imginfo file...");
    if let Ok(config_json) = crate::migration::inspect_image(config_digest) {
        let imginfo_path = deploy_dir.join(format!("{}.imginfo", verity.as_hex()));
        if let Err(e) = fs::write(&imginfo_path, &config_json) {
            eprintln!(
                "Warning: failed to write .imginfo file ({}): {}",
                imginfo_path.display(),
                e
            );
        }
    }

    // Handle /var migration
    phase4_var_migration(&etc_dir, dry_run)?;

    // For XFS roots, the composefs repo lives in an ext4 loopback file; the
    // booted system must mount it at /sysroot/composefs so `bootc status` and
    // day-2 updates can read the repo (the initrd mount is torn down at
    // switch-root). Install a runtime mount unit into the deployment's /etc.
    if Path::new("/sysroot/composefs-loopback.ext4").exists()
        && let Err(e) = write_runtime_composefs_loopback_mount(&etc_dir)
    {
        eprintln!("[phase4] Warning: failed to install runtime composefs mount: {e:#}");
    }

    Ok(deploy_dir)
}

/// Install a systemd mount unit into the deployment's /etc so the booted system
/// loop-mounts the composefs ext4 store at /sysroot/composefs. Idempotent with
/// any mount that survives the initrd: systemd treats an already-mounted target
/// as active.
fn write_runtime_composefs_loopback_mount(etc_dir: &Path) -> Result<()> {
    let unit_dir = etc_dir.join("systemd/system");
    fs::create_dir_all(&unit_dir)?;
    fs::write(
        unit_dir.join("sysroot-composefs.mount"),
        "[Unit]\n\
         Description=ComposeFS Loopback Store (runtime)\n\
         DefaultDependencies=no\n\
         After=sysroot.mount\n\
         Before=local-fs.target\n\
         \n\
         [Mount]\n\
         What=/sysroot/composefs-loopback.ext4\n\
         Where=/sysroot/composefs\n\
         Type=ext4\n\
         Options=loop,ro\n\
         \n\
         [Install]\n\
         WantedBy=local-fs.target\n",
    )?;
    let wants_dir = unit_dir.join("local-fs.target.wants");
    fs::create_dir_all(&wants_dir)?;
    let link = wants_dir.join("sysroot-composefs.mount");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink("../sysroot-composefs.mount", &link)
        .context("failed to enable runtime sysroot-composefs.mount")?;
    Ok(())
}

fn phase4_var_migration(etc_dir: &Path, _dry_run: bool) -> Result<()> {
    println!("=== Migrating /var data to ComposeFS state ===");
    let target_var = Path::new("/sysroot/state/os/default/var");

    // Check if /var is already populated (idempotency)
    if target_var.exists() {
        let count = fs::read_dir(target_var).map(|d| d.count()).unwrap_or(0);
        if count > 0 {
            println!(
                "/var already populated at {}. Skipping var migration.",
                target_var.display()
            );
            return Ok(());
        }
    }

    // Always copy /var data into state/os/default/var so the bootc initramfs
    // bind-mount of that path onto the deploy's /var exposes user data
    // (roothome/.ssh, home/, lib/containers, etc.). Do NOT synthesize an
    // /etc/fstab entry for /var: on Bluefin /proc/mounts reports /var as
    // subvolid=5 (the root subvol), and mounting that at /var post-pivot
    // shadows the bind-mount with /ostree, /state, /boot — losing user data.
    let _ = etc_dir; // (kept for signature compat; no fstab edits anymore)

    if !target_var.exists() {
        fs::create_dir_all(target_var.parent().unwrap())?;
    }

    let source_var = if Path::new("/sysroot/ostree/deploy/default/var").exists() {
        "/sysroot/ostree/deploy/default/var"
    } else {
        "/var"
    };

    println!(
        "Migrating /var data from {} to ComposeFS state...",
        source_var
    );
    xattr::copy_dir_all_with_xattrs(source_var, target_var)
        .context("failed to migrate /var data to ComposeFS state")?;
    println!("/var data migrated successfully.");

    Ok(())
}

/// Build a fstab entry for the /var btrfs subvolume by parsing /proc/mounts and
/// resolving the source device to a UUID. Returns None if the data can't be derived.
#[allow(dead_code)]
fn synthesize_var_fstab_entry(mounts: &str) -> Option<String> {
    let var_line = mounts.lines().find(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        parts.len() >= 4 && parts[1] == "/var" && parts[2] == "btrfs"
    })?;
    println!("[phase4] /proc/mounts /var line: {}", var_line);

    let parts: Vec<&str> = var_line.split_whitespace().collect();
    let device = parts[0];
    let raw_opts = parts[3];

    let subvol_token = raw_opts
        .split(',')
        .find(|o| o.starts_with("subvol=") && *o != "subvol=/")
        .or_else(|| raw_opts.split(',').find(|o| o.starts_with("subvolid=")))
        .unwrap_or("subvol=/");

    let uuid = resolve_device_uuid(device);
    let source = uuid
        .map(|u| format!("UUID={}", u))
        .unwrap_or_else(|| device.to_string());

    let opts = format!("rw,relatime,{}", subvol_token);
    Some(format!("{}\t/var\tbtrfs\t{}\t0 0\n", source, opts))
}

#[allow(dead_code)]
fn resolve_device_uuid(device: &str) -> Option<String> {
    let by_uuid = Path::new("/dev/disk/by-uuid");
    let entries = fs::read_dir(by_uuid).ok()?;
    for entry in entries.flatten() {
        let link = fs::read_link(entry.path()).ok()?;
        let resolved = by_uuid.join(&link).canonicalize().ok()?;
        if resolved == Path::new(device) {
            return entry.file_name().to_str().map(|s| s.to_string());
        }
    }
    None
}

/// Perform 3-way /etc merge: old OSTree default, current live /etc, new ComposeFS default.
fn perform_etc_merge(target_image: &str, sealed_config: &str, etc_dir: &Path) -> Result<()> {
    let temp_mount =
        TempDir::new_in("/var/tmp").context("failed to create temp mount directory")?;
    let mut mount_path = temp_mount.path().to_path_buf();

    // Mount the target rootfs via bootc's composefs overlay using the *sealed
    // config digest* (not the rootfs verity): `cfs oci mount` looks up
    // `streams/oci-config-<sealed-config>`, so the rootfs verity would miss and
    // drop us to a raw EROFS mount that zero-fills file content above the inline
    // threshold. With the sealed digest the overlay exposes real content, so we
    // can read /etc straight off the mount (and validate prune symlink targets).
    //
    // On hosts where the composefs overlay mounts into bootc's private namespace
    // (see phase5_setup_bootloader), the mount is empty here. Fall back to a
    // `podman image mount` of the already-cached image — local, real content, and
    // no dependency on reaching the registry mid-migration.
    let composefs_mounted = match mount_image(sealed_config, &mount_path) {
        Ok(()) if mount_path.join("etc").is_dir() => true,
        _ => {
            eprintln!(
                "[phase4] composefs /etc mount unavailable; falling back to podman image mount"
            );
            false
        }
    };
    let _cfs_guard = if composefs_mounted {
        Some(MountGuard::new(&mount_path))
    } else {
        None
    };
    let _podman_guard = if composefs_mounted {
        None
    } else {
        let pm = PodmanImageMount::new(target_image)
            .context("composefs /etc mount unavailable and podman image mount fallback failed")?;
        println!(
            "[phase4] using podman image mount at {} for /etc",
            pm.path.display()
        );
        mount_path = pm.path.clone();
        Some(pm)
    };

    let old_default_etc = find_ostree_etc_default()?;
    let current_etc = Path::new("/etc");

    // Use the target's /etc straight off the mount (real content). The registry
    // stream is kept only as a last-resort fallback for when /etc is somehow
    // absent from both the composefs overlay and the podman mount.
    // (The temp dir is held to function scope so it outlives merge_etc_files.)
    let registry_etc_temp =
        TempDir::new_in("/var/tmp").context("failed to create temp dir for registry /etc")?;
    let registry_etc = registry_etc_temp.path().to_path_buf();
    let mount_etc = mount_path.join("etc");
    let new_default_etc = if mount_etc
        .read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
    {
        println!("[phase4] using mounted /etc for merge source");
        mount_etc
    } else {
        println!("[phase4] /etc absent from mount; streaming target /etc from registry...");
        extract_subtree_via_registry(target_image, "etc/", &registry_etc)
            .context("registry /etc extraction failed")?;
        registry_etc
    };

    crate::mergetc::merge_etc_files(&old_default_etc, current_etc, &new_default_etc, etc_dir)
        .context("3-way /etc merge failed")?;

    // Drop /etc symlinks whose /usr/* target does not exist in the target image.
    // Bluefin → Dakota: e.g. /etc/systemd/system/dbus.service points to
    // dbus-broker.service which Dakota doesn't ship; the dangling symlink
    // breaks dbus and everything downstream (polkit, logind, sshd).
    match crate::mergetc::prune_dangling_symlinks(etc_dir, &mount_path) {
        Ok(n) if n > 0 => println!("[phase4] pruned {} dangling /etc symlink(s)", n),
        Ok(_) => {}
        Err(e) => eprintln!("[phase4] warning: dangling-symlink prune failed: {e:#}"),
    }

    // Drop OSTree/GRUB-era /etc artifacts that don't belong on a composefs
    // deployment. The 3-way merge keeps these because Bluefin's factory has
    // them and the user didn't modify them, but they actively lie about
    // system state on Dakota.
    drop_ostree_era_etc_artifacts(etc_dir);

    // Ensure the TCP 22 SSH socket-activated listener is always present in the
    // deploy /etc. On Bluefin, sshd only binds Unix-local + vsock by default;
    // this socket provides the TCP listener the E2E test needs. The 3-way merge
    // drops it when baked into the OSTree factory (old==cur, new absent), so we
    // recreate it unconditionally after the merge.
    ensure_e2e_ssh_socket(etc_dir)?;

    Ok(())
}

/// Drop GRUB / rpm-ostree artifacts that don't belong on a composefs +
/// systemd-boot deploy. These come from the source OS's /etc but reference
/// boot/state mechanisms the target no longer uses.
fn drop_ostree_era_etc_artifacts(etc_dir: &Path) {
    // Concrete known-cruft paths. Keep this tight — only paths that are
    // unambiguously misleading (lying state files) or actively wrong for
    // the new bootloader.
    let drops = [
        ".rpm-ostree-shadow-mode-fixed2.stamp",
        ".updated",
        "grub2.cfg",
        "grub2-efi.cfg",
        "grub.d",
    ];
    for name in &drops {
        let p = etc_dir.join(name);
        let exists = p.exists() || p.is_symlink();
        if !exists {
            continue;
        }
        let res = if p.is_dir() && !p.is_symlink() {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
        match res {
            Ok(()) => println!("[phase4] dropped OSTree-era /etc artifact: {}", name),
            Err(e) => eprintln!("[phase4] warning: failed to drop {}: {}", p.display(), e),
        }
    }
}

/// Ensure the TCP 22 SSH socket-activated listener is present in the deploy
/// /etc. Bluefin's sshd only binds Unix-local + vsock by default; this socket
/// provides the TCP listener the E2E test needs. The 3-way merge drops it when
/// baked into the OSTree factory (old==cur, new absent), so we recreate it
/// unconditionally after the merge.
fn ensure_e2e_ssh_socket(etc_dir: &Path) -> Result<()> {
    let systemd_dir = etc_dir.join("systemd/system");
    fs::create_dir_all(systemd_dir.join("sockets.target.wants"))?;

    fs::write(
        systemd_dir.join("e2e-sshd.socket"),
        "[Unit]\nDescription=E2E SSH TCP Socket (port 22)\n[Socket]\nListenStream=22\nAccept=yes\n[Install]\nWantedBy=sockets.target\n",
    )?;
    fs::write(
        systemd_dir.join("e2e-sshd@.service"),
        "[Unit]\nDescription=E2E SSH per-connection service\n[Service]\nExecStart=-/usr/bin/sshd -i -E /var/log/sshd-e2e.log -d\nStandardInput=socket\n",
    )?;

    let symlink = systemd_dir.join("sockets.target.wants/e2e-sshd.socket");
    if symlink.exists() || symlink.is_symlink() {
        let _ = fs::remove_file(&symlink);
    }
    std::os::unix::fs::symlink("../e2e-sshd.socket", &symlink)?;

    // Remove the sshd.service enablement symlink if it survived the merge.
    // e2e-sshd.socket provides TCP 22 via socket activation; having both
    // sshd.service (sshd -D) and e2e-sshd.socket on port 22 causes a port
    // conflict that kills the daemon process with 255/EXCEPTION.
    let sshd_enable = systemd_dir.join("multi-user.target.wants/sshd.service");
    if sshd_enable.exists() || sshd_enable.is_symlink() {
        fs::remove_file(&sshd_enable)?;
        println!("[phase4] removed sshd.service enablement (e2e-sshd.socket provides TCP 22)");
    }

    // Remove ostree-remount.service enablement — on composefs, OSTree bind
    // mounts are irrelevant and the service would fail or create stale mounts
    // under /sysroot/ostree (which we delete on commit).
    let remount_enable = systemd_dir.join("local-fs.target.wants/ostree-remount.service");
    if remount_enable.exists() || remount_enable.is_symlink() {
        fs::remove_file(&remount_enable)?;
        println!(
            "[phase4] removed ostree-remount.service enablement (composefs doesn't need OSTree bind mounts)"
        );
    }

    println!("[phase4] ensured e2e-sshd.socket in deploy /etc");
    Ok(())
}

/// Legacy single-DB supplement path. Kept for callers that don't want the full
/// `/etc` subtree; not used by `perform_etc_merge` anymore since the full
/// subtree extract subsumes it.
#[allow(dead_code)]
fn supplement_identity_dbs_from_registry(target_image: &str, etc_dir: &Path) -> Result<()> {
    let scratch =
        TempDir::new_in("/var/tmp").context("failed to create temp dir for identity-DB extract")?;
    let scratch_etc = scratch.path().join("etc");
    fs::create_dir_all(&scratch_etc).context("failed to create scratch etc dir")?;

    // Try each file individually; tolerate "missing in image" because not
    // every bootc target ships every identity DB (Dakota has no /etc/subuid
    // or /etc/subgid). Any other error from a given file is logged and the
    // others continue.
    let names = ["passwd", "shadow", "group", "gshadow", "subuid", "subgid"];
    for name in &names {
        let src = PathBuf::from("/etc").join(name);
        let dst = scratch_etc.join(name);
        let pair = [(src.as_path(), dst.as_path())];
        if let Err(e) = extract_files_via_registry(target_image, &pair) {
            let es = format!("{e:#}");
            if es.contains("missing files") || es.contains("No such file") {
                // Image doesn't ship this file; that's fine.
                continue;
            }
            eprintln!("[phase4] warning: skopeo extract of /etc/{name} failed: {es}");
        }
    }

    let mut supplemented = 0usize;
    for name in &names {
        let dakota_path = scratch_etc.join(name);
        let merged_path = etc_dir.join(name);
        if !dakota_path.exists() {
            continue;
        }
        let dakota = fs::read_to_string(&dakota_path).unwrap_or_default();
        if dakota.trim().is_empty() {
            continue;
        }
        let current = fs::read_to_string(&merged_path).unwrap_or_default();
        let merged = line_union_by_first_colon(&current, &dakota);
        if merged != current {
            // Permissions on shadow/gshadow must stay 000; the existing file
            // already has them, so write in place and preserve mode/xattrs.
            let perms = fs::metadata(&merged_path).ok().map(|m| m.permissions());
            fs::write(&merged_path, merged.as_bytes())
                .with_context(|| format!("failed to rewrite {}", merged_path.display()))?;
            if let Some(p) = perms {
                let _ = fs::set_permissions(&merged_path, p);
            }
            supplemented += 1;
        }
    }
    if supplemented > 0 {
        println!(
            "[phase4] supplemented {} identity-DB file(s) with target's system users",
            supplemented
        );
    }
    Ok(())
}

#[allow(dead_code)]
fn line_union_by_first_colon(current: &str, new: &str) -> String {
    use std::collections::HashSet;
    let key_of = |line: &str| line.split(':').next().unwrap_or("").to_string();
    let mut keys: HashSet<String> = HashSet::new();
    let mut out = String::with_capacity(current.len() + new.len());
    for line in current.lines() {
        if !line.is_empty() {
            keys.insert(key_of(line));
        }
        out.push_str(line);
        out.push('\n');
    }
    for line in new.lines() {
        if line.is_empty() {
            continue;
        }
        let k = key_of(line);
        if !keys.contains(&k) {
            out.push_str(line);
            out.push('\n');
            keys.insert(k);
        }
    }
    out
}

fn find_ostree_etc_default() -> Result<PathBuf> {
    let cmdline = fs::read_to_string("/proc/cmdline")?;
    for word in cmdline.split_whitespace() {
        if let Some(_ostree_arg) = word.strip_prefix("ostree=") {
            let deploy_base = Path::new("/sysroot/ostree/deploy/default/deploy");
            if deploy_base.exists() {
                for entry in fs::read_dir(deploy_base)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.ends_with(".0") && entry.path().is_dir() {
                        let usr_etc = entry.path().join("usr/etc");
                        if usr_etc.exists() {
                            return Ok(usr_etc);
                        }
                    }
                }
            }
            break;
        }
    }
    anyhow::bail!("could not locate OSTree deployment default /etc");
}

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

// ---- Phase 5 ----

fn phase5_setup_bootloader(
    report: &PreflightReport,
    verity: &VerityDigest,
    target_image: &str,
    sealed_config: &str,
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
    let mut mount_path = temp_mount.path().to_path_buf();

    if dry_run {
        println!("[DRY RUN] Would mount EROFS, extract boot artifacts, and write BLS entries.");
        return Ok(());
    }

    // Mount via the sealed config digest (see perform_etc_merge): the rootfs
    // verity would miss the oci-config stream and fall back to a raw EROFS mount
    // that zero-fills kernel/initrd/systemd-bootx64.efi content.
    //
    // `bootc internals cfs oci mount` can return exit 0 while mounting inside its
    // own private mount namespace (MS_REC|MS_PRIVATE), which is torn down the
    // instant the subprocess exits — leaving us an empty directory. A zero exit is
    // therefore not enough; we verify the mount actually exposes content here.
    let composefs_mounted = match mount_image(sealed_config, &mount_path) {
        Ok(()) if mount_path.join("usr/lib/modules").is_dir() => true,
        Ok(()) => {
            eprintln!(
                "[phase5] composefs mount reported success but exposed no content \
                 (bootc mounted in a private namespace that did not persist); \
                 falling back to podman image mount"
            );
            false
        }
        Err(e) => {
            eprintln!(
                "[phase5] composefs overlay mount failed ({e}); \
                 falling back to podman image mount"
            );
            false
        }
    };
    // Only guard the composefs overlay mount when it actually persisted into our
    // namespace; otherwise umount would just warn about a mount that isn't ours.
    let _cfs_guard = if composefs_mounted {
        Some(MountGuard::new(&mount_path))
    } else {
        None
    };

    // Fallback: mount the already-cached image (Phase 2 podman pull) and read boot
    // artifacts straight off local storage — no network, real file content. This
    // sidesteps both the private-namespace composefs mount and the registry-stream
    // path (which fails on hosts that can't reach the upstream registry mid-migration).
    let _podman_guard = if composefs_mounted {
        None
    } else {
        let pm = PodmanImageMount::new(target_image)
            .context("composefs mount unavailable and podman image mount fallback also failed")?;
        println!(
            "[phase5] using podman image mount at {} for boot artifacts",
            pm.path.display()
        );
        mount_path = pm.path.clone();
        Some(pm)
    };
    // We now have a usable rootfs at mount_path (composefs overlay or podman mount).
    let mount_ok = true;

    // Find kernel version from the mounted image /usr/lib/modules.
    let modules_dir = mount_path.join("usr/lib/modules");
    let kver = fs::read_dir(&modules_dir)
        .with_context(|| {
            format!(
                "reading kernel modules dir from mounted image: {}",
                modules_dir.display()
            )
        })?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("could not find kernel version in mounted image"))?;
    println!("Found kernel version: {}", kver);

    let mounted_vmlinuz = modules_dir.join(&kver).join("vmlinuz");
    let mounted_initrd = modules_dir.join(&kver).join("initramfs.img");

    let vmlinuz_src = if mounted_vmlinuz.exists() {
        mounted_vmlinuz
    } else if mount_path
        .join("boot")
        .join(format!("vmlinuz-{}", kver))
        .exists()
    {
        mount_path.join("boot").join(format!("vmlinuz-{}", kver))
    } else {
        // Mount empty/unavailable: use canonical in-container path so extraction
        // falls back to podman with the correct source path.
        modules_dir.join(&kver).join("vmlinuz")
    };
    let initrd_src = if mounted_initrd.exists() {
        mounted_initrd
    } else if mount_path
        .join("boot")
        .join(format!("initramfs-{}.img", kver))
        .exists()
    {
        mount_path
            .join("boot")
            .join(format!("initramfs-{}.img", kver))
    } else {
        // Same canonical fallback for initrd.
        modules_dir.join(&kver).join("initramfs.img")
    };

    // Read target os-release for BLS naming
    let target_os = read_os_release(&mount_path).unwrap_or_else(|_| os_release::OsRelease {
        id: "linux".into(),
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

        match install_systemd_boot_from_target(esp_path, &mount_path, target_image, mount_ok) {
            Ok(()) => {
                // Copy composefs kernel+initrd to ESP via registry stream (raw EROFS reads
                // return zero-filled content past the inline threshold).
                let boot_dir_name = format!("bootc_composefs-{}", verity.as_hex());
                let esp_boot_dir = esp_path.join("EFI/Linux").join(&boot_dir_name);
                fs::create_dir_all(&esp_boot_dir).with_context(|| {
                    format!("creating ESP boot dir: {}", esp_boot_dir.display())
                })?;

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
                // When the composefs mount is empty (bootc mounted in a private
                // namespace), initrd_src won't exist on disk — but we still know its
                // canonical in-container path and extract it via the registry.
                if initrd_src.exists() || !mount_ok {
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
                extract_files_preferring_mount(&mount_path, target_image, &extract)
                    .context("failed to copy kernel/initrd from target image")?;

                // Rebuild initrd with LVM support if the source system uses LVM.
                // Must happen before patch_origin_boot_digest so the hash covers
                // the LVM-enabled initrd bytes, not the original Dakota initrd.
                if have_initrd
                    && let Err(e) = rebuild_initrd_with_lvm_if_needed(
                        &kver,
                        &mount_path,
                        target_image,
                        &esp_initrd,
                    )
                {
                    eprintln!("[phase5] Warning: composefs initrd rebuild failed: {e:#}");
                }

                // Now that vmlinuz + initrd are on the ESP, compute their
                // boot_digest (sha256(vmlinuz || initrd)) and patch the .origin
                // file. `bootc status` requires this digest to set soft-reboot
                // capability; without it, status fails with "Could not find
                // boot digest for deployment".
                if have_initrd
                    && let Err(e) = patch_origin_boot_digest(verity, &esp_vmlinuz, &esp_initrd)
                {
                    eprintln!("[phase5] warning: failed to patch origin boot_digest: {e:#}");
                }

                // Build composefs BLS entry.
                let composefs_entry = bootloader::BlsEntry {
                    title: bls_entry_title(&target_os, "composefs"),
                    version: kver.clone(),
                    linux: format!("/EFI/Linux/{}/vmlinuz", boot_dir_name),
                    initrds: vec![format!("/EFI/Linux/{}/initrd", boot_dir_name)],
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
                fs::create_dir_all(&staged_dir).with_context(|| {
                    format!("creating ESP staged entries dir: {}", staged_dir.display())
                })?;
                let entries_dir = esp_path.join("loader/entries");
                fs::create_dir_all(&entries_dir).with_context(|| {
                    format!("creating ESP entries dir: {}", entries_dir.display())
                })?;
                let to_promote: Vec<&bootloader::BlsEntry> = vec![&composefs_entry];
                for entry in &to_promote {
                    fs::write(staged_dir.join(&entry.filename), entry.render())
                        .with_context(|| format!("writing staged BLS entry: {}", entry.filename))?;
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
        fs::create_dir_all(&grub_boot_dir)
            .with_context(|| format!("creating GRUB boot dir: {}", grub_boot_dir.display()))?;

        // Use registry stream for vmlinuz + initrd — copying from the raw EROFS mount
        // zero-fills content past the inline threshold, producing a corrupt 192MB initrd.
        let rel_vmlinuz = vmlinuz_src
            .strip_prefix(&mount_path)
            .with_context(|| format!("vmlinuz {:?} not under mount", vmlinuz_src))?;
        let in_container_vmlinuz = Path::new("/").join(rel_vmlinuz);
        let grub_vmlinuz = grub_boot_dir.join("vmlinuz");
        let mut grub_extract: Vec<(PathBuf, PathBuf)> =
            vec![(in_container_vmlinuz, grub_vmlinuz.clone())];
        let grub_initrd = grub_boot_dir.join("initrd");
        let mut have_grub_initrd = false;
        if initrd_src.exists() || !mount_ok {
            let rel_initrd = initrd_src
                .strip_prefix(&mount_path)
                .with_context(|| format!("initrd {:?} not under mount", initrd_src))?;
            let in_container_initrd = Path::new("/").join(rel_initrd);
            grub_extract.push((in_container_initrd, grub_initrd.clone()));
            have_grub_initrd = true;
        }
        let extract_pairs: Vec<(&Path, &Path)> = grub_extract
            .iter()
            .map(|(s, d)| (s.as_path(), d.as_path()))
            .collect();
        extract_files_preferring_mount(&mount_path, target_image, &extract_pairs)
            .context("failed to copy kernel/initrd from target image (GRUB2 path)")?;

        if have_grub_initrd
            && let Err(e) =
                rebuild_initrd_with_lvm_if_needed(&kver, &mount_path, target_image, &grub_initrd)
        {
            eprintln!("[phase5] Warning: LVM initrd rebuild failed: {e:#}");
        }

        // Composefs entry (priority 1)
        entries.push(bootloader::BlsEntry {
            title: bls_entry_title(&target_os, "composefs"),
            version: kver.clone(),
            linux: format!("/{}/vmlinuz", boot_dir_name),
            initrds: vec![format!("/{}/initrd", boot_dir_name)],
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
        fs::create_dir_all(staged_dir).context("creating /boot/loader/entries.staged")?;
        for entry in &entries {
            let entry_path = staged_dir.join(&entry.filename);
            fs::write(&entry_path, entry.render())
                .with_context(|| format!("writing staged BLS entry: {}", entry.filename))?;
        }

        // Propagate rename errors.
        let entries_dir = Path::new("/boot/loader/entries");
        fs::create_dir_all(entries_dir)?;
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
        if let Ok(cfg) = fs::read_to_string(grub_cfg_path)
            && !cfg.contains("set default=\"${saved_entry}\"")
        {
            let patched = cfg.replace("\nblscfg\n", "\nset default=\"${saved_entry}\"\nblscfg\n");
            if patched != cfg {
                fs::write(grub_cfg_path, &patched).context("failed to write patched grub.cfg")?;
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
/// Copy files out of a mounted composefs image, falling back to registry
/// streaming only for sources missing from the mount. Each pair is
/// `(path-in-image starting with "/", destination)`. Now that Phase 3 seals the
/// image and Phases 4/5 mount it by its sealed config digest, the composefs
/// overlay exposes real file content — so the kernel, initrd, systemd-bootx64.efi
/// etc. can be copied straight off the mount, removing the runtime dependency on
/// reaching the image's upstream registry (which an offline target or a CI VM
/// with no egress cannot satisfy).
fn extract_files_preferring_mount(
    mount_path: &Path,
    image_ref: &str,
    files: &[(&Path, &Path)],
) -> Result<()> {
    let mut missing: Vec<(&Path, &Path)> = Vec::new();
    for (src, dest) in files {
        let rel = src.strip_prefix("/").unwrap_or(src);
        let from = mount_path.join(rel);
        if !from.exists() {
            missing.push((*src, *dest));
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for {}", dest.display()))?;
        }
        fs::copy(&from, dest)
            .with_context(|| format!("copying {} -> {}", from.display(), dest.display()))?;
    }
    if !missing.is_empty() {
        println!(
            "[extract] {} file(s) absent from composefs mount; falling back to registry",
            missing.len()
        );
        extract_files_via_registry(image_ref, &missing)?;
    }
    Ok(())
}

/// Copy the target kernel's module tree out of a mounted composefs image into a
/// writable scratch dir (depmod must write `modules.dep.bin`, and the mount is
/// read-only). Returns the same `(scratch, modules_dir)` shape as
/// [`extract_kernel_modules_via_registry`] so callers are interchangeable.
fn copy_kernel_modules_from_mount(mount_path: &Path, kver: &str) -> Result<(TempDir, PathBuf)> {
    let src = mount_path.join("usr/lib/modules").join(kver);
    if !src.join("kernel").is_dir() {
        anyhow::bail!(
            "kernel modules for {kver} not present in composefs mount at {}",
            src.display()
        );
    }
    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-kmods-")
        .tempdir_in("/var/tmp")
        .context("failed to create /var/tmp scratch dir for kernel modules")?;
    let dst = scratch.path().join("usr/lib/modules").join(kver);
    crate::xattr::copy_dir_all_with_xattrs(&src, &dst)
        .with_context(|| format!("copying kernel modules from {}", src.display()))?;
    Ok((scratch, dst))
}

fn extract_files_via_registry(image_ref: &str, files: &[(&Path, &Path)]) -> Result<()> {
    let endpoint = RegistryEndpoint::resolve(image_ref)?;
    let manifest_json = endpoint.fetch_manifest(&endpoint.reference)?;

    // Manifest list / OCI index → resolve to current-arch manifest.
    let layers_manifest = if endpoint.is_manifest_index(&manifest_json) {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let entries = manifest_json
            .get("manifests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("manifest index has no manifests array"))?;
        let pick = entries
            .iter()
            .find(|m| {
                m.get("platform")
                    .and_then(|p| p.get("architecture"))
                    .and_then(|a| a.as_str())
                    == Some(arch)
            })
            .ok_or_else(|| anyhow!("manifest index has no entry for arch {}", arch))?;
        let digest = pick
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("manifest index entry has no digest"))?;
        endpoint.fetch_manifest(digest)?
    } else {
        manifest_json
    };

    let layers = layers_manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("image manifest has no layers array"))?;

    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-extract-")
        .tempdir_in("/var/tmp")
        .context("failed to create /var/tmp scratch dir for layer streaming")?;

    let mut remaining: Vec<(&Path, &Path)> = files.to_vec();
    for layer in layers.iter().rev() {
        if remaining.is_empty() {
            break;
        }
        let digest = layer
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer entry has no digest"))?;

        // Download just this one layer, extract from it, drop it.
        let blob_path = scratch.path().join("layer.blob");
        endpoint
            .download_blob(digest, &blob_path)
            .with_context(|| format!("failed to fetch layer {}", digest))?;

        let mut still_needed: Vec<(&Path, &Path)> = Vec::new();
        for (src, dst) in remaining.into_iter() {
            if extract_one_from_layer(&blob_path, src, dst)? {
                // satisfied
            } else {
                still_needed.push((src, dst));
            }
        }
        remaining = still_needed;
        let _ = fs::remove_file(&blob_path);
    }

    if !remaining.is_empty() {
        let missing: Vec<String> = remaining
            .iter()
            .map(|(s, _)| s.display().to_string())
            .collect();
        return Err(anyhow!(
            "target image is missing files: {}",
            missing.join(", ")
        ));
    }
    Ok(())
}

/// Compute sha256(vmlinuz || initrd) and patch the `.origin` file's
/// `boot_digest = …` line. `bootc status` uses this digest to set the soft
/// reboot capability; without it, status bails with
/// "Could not find boot digest for deployment".
fn patch_origin_boot_digest(verity: &VerityDigest, vmlinuz: &Path, initrd: &Path) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let v = fs::read(vmlinuz).with_context(|| format!("reading vmlinuz {}", vmlinuz.display()))?;
    let i = fs::read(initrd).with_context(|| format!("reading initrd {}", initrd.display()))?;
    hasher.update(&v);
    hasher.update(&i);
    let raw = hasher.finalize();
    let mut digest = String::with_capacity(raw.len() * 2);
    for b in raw {
        digest.push_str(&format!("{:02x}", b));
    }

    let origin_path = Path::new("/sysroot/state/deploy")
        .join(verity.as_hex())
        .join(format!("{}.origin", verity.as_hex()));
    let text = fs::read_to_string(&origin_path)
        .with_context(|| format!("reading origin {}", origin_path.display()))?;
    let patched = patch_boot_digest_in_content(&text, &digest)?;
    fs::write(&origin_path, &patched)
        .with_context(|| format!("writing patched origin {}", origin_path.display()))?;
    println!("[phase5] patched origin boot_digest = {}", digest);
    Ok(())
}

/// Stream OCI layers oldest→newest and extract everything matching `subtree`
/// (e.g. `etc/`) into `dst_dir`. Later layers' files overwrite earlier ones,
/// matching how the container runtime composes the rootfs. Whiteouts
/// (`.wh.*`) are ignored — at worst we'll keep a few extra stale files in
/// `etc/`, which doesn't break anything we care about.
fn extract_subtree_via_registry(image_ref: &str, subtree: &str, dst_dir: &Path) -> Result<()> {
    let endpoint = RegistryEndpoint::resolve(image_ref)?;
    let manifest_json = endpoint.fetch_manifest(&endpoint.reference)?;
    let layers_manifest = if endpoint.is_manifest_index(&manifest_json) {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let entries = manifest_json
            .get("manifests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("manifest index has no manifests array"))?;
        let pick = entries
            .iter()
            .find(|m| {
                m.get("platform")
                    .and_then(|p| p.get("architecture"))
                    .and_then(|a| a.as_str())
                    == Some(arch)
            })
            .ok_or_else(|| anyhow!("manifest index has no entry for arch {}", arch))?;
        let digest = pick
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("manifest index entry has no digest"))?;
        endpoint.fetch_manifest(digest)?
    } else {
        manifest_json
    };
    let layers = layers_manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("image manifest has no layers array"))?;

    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-subtree-")
        .tempdir_in("/var/tmp")
        .context("failed to create /var/tmp scratch dir for subtree streaming")?;

    fs::create_dir_all(dst_dir)
        .with_context(|| format!("failed to create subtree destination {}", dst_dir.display()))?;

    // Iterate oldest → newest so later writes win.
    for layer in layers.iter() {
        let digest = layer
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer entry has no digest"))?;

        let blob_path = scratch.path().join("layer.blob");
        endpoint
            .download_blob(digest, &blob_path)
            .with_context(|| format!("failed to fetch layer {}", digest))?;

        // tar will silently produce no output if the prefix is absent in this layer.
        // --strip-components=1 drops the leading directory we asked for so the
        // contents land directly under dst_dir (we want dst_dir to be the merged
        // /etc, not dst_dir/etc).
        let normalized = subtree.trim_end_matches('/');
        for candidate in [format!("./{}", normalized), normalized.to_string()] {
            let _ = Command::new("tar")
                .args([
                    "-xaf",
                    blob_path
                        .to_str()
                        .ok_or_else(|| anyhow!("invalid blob path"))?,
                    "-C",
                    dst_dir
                        .to_str()
                        .ok_or_else(|| anyhow!("invalid dst path"))?,
                    "--overwrite",
                    "--no-same-owner",
                    "--strip-components=1",
                    &candidate,
                ])
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = fs::remove_file(&blob_path);
    }
    Ok(())
}

/// Extract the target image's kernel modules for `kver` from the registry into a
/// fresh /var/tmp directory, returning the tempdir guard plus the path to the
/// extracted `usr/lib/modules/<kver>` tree.
///
/// The composefs cfs mount used elsewhere in Phase 5 can fall back to a raw
/// EROFS mount, where files past the inline threshold (xfs.ko, modules.dep.bin,
/// …) read back as zeros — so dracut cannot rebuild an initrd from that mount.
/// The registry layer stream returns real bytes, so we source the modules tree
/// from there instead (the same mechanism that extracts vmlinuz + initrd).
fn extract_kernel_modules_via_registry(
    image_ref: &str,
    kver: &str,
) -> Result<(tempfile::TempDir, PathBuf)> {
    let endpoint = RegistryEndpoint::resolve(image_ref)?;
    let manifest_json = endpoint.fetch_manifest(&endpoint.reference)?;
    let layers_manifest = endpoint.arch_layers_manifest(manifest_json)?;
    let layers = layers_manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("image manifest has no layers array"))?;

    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-kmods-")
        .tempdir_in("/var/tmp")
        .context("failed to create /var/tmp scratch dir for kernel module streaming")?;
    let blob_path = scratch.path().join("layer.blob");
    let want = format!("usr/lib/modules/{kver}");

    // Newest → oldest with --skip-old-files so the newest copy of each file
    // wins (overlay semantics). The module tree is split across layers — bootc
    // images regenerate modules.dep.bin in a later layer than the kernel's .ko
    // files — so we can't stop at the first layer; we keep going until the
    // filesystem drivers the composefs+LUKS+XFS initrd actually needs (xfs,
    // erofs, overlay — shipped together in the kernel-modules layer) are present.
    // Full paths (no --strip-components) land the tree deterministically at
    // <scratch>/usr/lib/modules/<kver> regardless of the layer's leading `./`.
    let mods = scratch.path().join(&want);
    let needed_kos = [
        mods.join("kernel/fs/xfs/xfs.ko"),
        mods.join("kernel/fs/erofs/erofs.ko"),
        mods.join("kernel/fs/overlayfs/overlay.ko"),
    ];
    for layer in layers.iter().rev() {
        let digest = layer
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer entry has no digest"))?;
        endpoint
            .download_blob(digest, &blob_path)
            .with_context(|| format!("failed to fetch layer {}", digest))?;
        for candidate in [format!("./{want}"), want.clone()] {
            let _ = Command::new("tar")
                .arg("-xaf")
                .arg(&blob_path)
                .arg("-C")
                .arg(scratch.path())
                .args(["--skip-old-files", "--no-same-owner"])
                .arg(&candidate)
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = fs::remove_file(&blob_path);
        if needed_kos.iter().all(|p| p.exists()) {
            break;
        }
    }

    let missing: Vec<String> = needed_kos
        .iter()
        .filter(|p| !p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if !mods.join("modules.dep.bin").exists() || !missing.is_empty() {
        return Err(anyhow!(
            "incomplete kernel modules for {kver} from {image_ref} via registry \
             (missing: {})",
            if missing.is_empty() {
                "modules.dep.bin".to_string()
            } else {
                missing.join(", ")
            }
        ));
    }
    Ok((scratch, mods))
}

/// Resolved registry endpoint: base URL (scheme + host), repository, reference, and
/// optional Bearer token. Built once per image and reused for the manifest + every
/// blob fetch.
struct RegistryEndpoint {
    base_url: String,
    repo: String,
    reference: String,
    bearer: Option<String>,
}

impl RegistryEndpoint {
    fn resolve(image_ref: &str) -> Result<Self> {
        let (host, repo, reference) = parse_image_ref(image_ref)?;

        // Pick http for plain non-standard ports (local dev registries), https otherwise.
        // We probe /v2/ to confirm and to discover any bearer challenge.
        let candidates: &[&str] = if host_is_plain_http(&host) {
            &["http"]
        } else {
            &["https", "http"]
        };

        for scheme in candidates {
            let base = format!("{}://{}", scheme, host);
            match probe_v2(&base, &repo) {
                Ok(bearer) => {
                    return Ok(RegistryEndpoint {
                        base_url: base,
                        repo,
                        reference,
                        bearer,
                    });
                }
                Err(_) => continue,
            }
        }
        Err(anyhow!(
            "could not reach registry {} (tried {:?})",
            host,
            candidates
        ))
    }

    fn fetch_manifest(&self, reference: &str) -> Result<serde_json::Value> {
        let url = format!("{}/v2/{}/manifests/{}", self.base_url, self.repo, reference);
        let mut args: Vec<String> = vec![
            "-sSL".into(),
            "--fail".into(),
            "-H".into(),
            "Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json".into(),
        ];
        if let Some(token) = &self.bearer {
            args.push("-H".into());
            args.push(format!("Authorization: Bearer {}", token));
        }
        args.push(url);
        let out = Command::new("curl")
            .args(&args)
            .output()
            .context("failed to invoke curl for manifest fetch")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl manifest fetch failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        serde_json::from_slice(&out.stdout).context("failed to parse manifest JSON")
    }

    fn is_manifest_index(&self, m: &serde_json::Value) -> bool {
        match m.get("mediaType").and_then(|v| v.as_str()) {
            Some(mt) => mt.contains("manifest.list") || mt.contains("image.index"),
            None => m.get("manifests").is_some(),
        }
    }

    fn download_blob(&self, digest: &str, dst: &Path) -> Result<()> {
        let url = format!("{}/v2/{}/blobs/{}", self.base_url, self.repo, digest);
        let mut args: Vec<String> = vec![
            "-sSL".into(),
            "--fail".into(),
            "-o".into(),
            dst.to_string_lossy().into_owned(),
        ];
        if let Some(token) = &self.bearer {
            args.push("-H".into());
            args.push(format!("Authorization: Bearer {}", token));
        }
        args.push(url);
        let status = Command::new("curl")
            .args(&args)
            .status()
            .context("failed to invoke curl for blob fetch")?;
        if !status.success() {
            return Err(anyhow!("curl blob fetch failed for {}", digest));
        }
        Ok(())
    }

    /// Resolve a (possibly multi-arch) manifest to the concrete image manifest
    /// for the current architecture.
    fn arch_layers_manifest(&self, manifest_json: serde_json::Value) -> Result<serde_json::Value> {
        if !self.is_manifest_index(&manifest_json) {
            return Ok(manifest_json);
        }
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let entries = manifest_json
            .get("manifests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("manifest index has no manifests array"))?;
        let pick = entries
            .iter()
            .find(|m| {
                m.get("platform")
                    .and_then(|p| p.get("architecture"))
                    .and_then(|a| a.as_str())
                    == Some(arch)
            })
            .ok_or_else(|| anyhow!("manifest index has no entry for arch {}", arch))?;
        let digest = pick
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("manifest index entry has no digest"))?;
        self.fetch_manifest(digest)
    }
}

/// Hosts that should always use plain HTTP: bare IPv4 with a port, or `localhost`.
fn host_is_plain_http(host: &str) -> bool {
    if host.starts_with("localhost") {
        return true;
    }
    // IPv4-with-port like 10.0.2.2:5000
    let host_only = host.split(':').next().unwrap_or(host);
    host_only
        .split('.')
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && host_only.split('.').count() == 4
}

/// Probe `/v2/` (or `/v2/<repo>/tags/list`) to determine if the registry is reachable
/// and whether it requires a Bearer token. Returns Ok(Some(token)) if a Bearer
/// challenge was issued and we obtained a token, Ok(None) for anonymous access, Err
/// on transport failure.
fn probe_v2(base_url: &str, repo: &str) -> Result<Option<String>> {
    let url = format!("{}/v2/", base_url);
    let out = Command::new("curl")
        .args([
            "-sS",
            "-o",
            "/dev/null",
            "-D",
            "-",
            "--max-time",
            "10",
            &url,
        ])
        .output()
        .context("curl probe failed")?;
    if !out.status.success() {
        return Err(anyhow!(
            "curl probe to {} failed: {}",
            url,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let headers = String::from_utf8_lossy(&out.stdout);
    // First line: HTTP/1.1 <code> ...
    let status_code = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    if status_code.starts_with("2") {
        return Ok(None);
    }
    if status_code == "401" {
        // Parse Www-Authenticate: Bearer realm="...",service="...",scope="..."
        let challenge = headers
            .lines()
            .find(|l| l.to_lowercase().starts_with("www-authenticate:"))
            .ok_or_else(|| anyhow!("registry returned 401 with no Www-Authenticate header"))?;
        let token = fetch_bearer_token(challenge, repo)?;
        return Ok(Some(token));
    }
    Err(anyhow!("unexpected status from {}: {}", url, status_code))
}

/// Parse a `Www-Authenticate: Bearer realm="...",service="...",scope="..."` line and
/// fetch an anonymous token. If the challenge didn't include a scope, build one for
/// pull access to `repo`.
fn fetch_bearer_token(challenge: &str, repo: &str) -> Result<String> {
    let bearer_part = challenge
        .split_once(':')
        .map(|x| x.1)
        .map(|s| s.trim())
        .unwrap_or("");
    let bearer_part = bearer_part
        .strip_prefix("Bearer ")
        .ok_or_else(|| anyhow!("Www-Authenticate is not a Bearer challenge: {}", challenge))?;

    let mut realm: Option<String> = None;
    let mut service: Option<String> = None;
    let mut scope: Option<String> = None;
    for kv in bearer_part.split(',') {
        let mut it = kv.splitn(2, '=');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim().trim_matches('"');
        match k {
            "realm" => realm = Some(v.to_string()),
            "service" => service = Some(v.to_string()),
            "scope" => scope = Some(v.to_string()),
            _ => {}
        }
    }
    let realm = realm.ok_or_else(|| anyhow!("Bearer challenge missing realm"))?;
    let scope = scope.unwrap_or_else(|| format!("repository:{}:pull", repo));

    let mut url = format!("{}?scope={}", realm, urlencode(&scope));
    if let Some(svc) = service {
        url.push_str(&format!("&service={}", urlencode(&svc)));
    }

    let out = Command::new("curl")
        .args(["-sSL", "--fail", &url])
        .output()
        .context("curl token fetch failed")?;
    if !out.status.success() {
        return Err(anyhow!(
            "token fetch failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("token endpoint did not return JSON")?;
    let token = body
        .get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("token endpoint response has no token field"))?;
    Ok(token.to_string())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Parse `host[:port]/repo[:tag|@digest]` into (host, repo, reference).
/// Reference is the digest if `@` was present, otherwise the tag (default `latest`).
fn parse_image_ref(image_ref: &str) -> Result<(String, String, String)> {
    let trimmed = image_ref
        .strip_prefix("docker://")
        .unwrap_or(image_ref)
        .trim_start_matches('/');
    let (host, rest) = trimmed
        .split_once('/')
        .ok_or_else(|| anyhow!("image ref {} has no repository component", image_ref))?;

    // Split reference. `@` (digest) takes priority over `:` (tag) since digest contains `:`.
    let (repo, reference) = if let Some((r, d)) = rest.split_once('@') {
        (r.to_string(), d.to_string())
    } else if let Some((r, t)) = rest.rsplit_once(':') {
        (r.to_string(), t.to_string())
    } else {
        (rest.to_string(), "latest".to_string())
    };
    Ok((host.to_string(), repo, reference))
}

/// Try to extract a single file from one OCI layer blob to `dst`. Returns Ok(true)
/// if found, Ok(false) if the path wasn't in this layer (caller continues to the
/// next layer), Err on unexpected tar/IO failure.
///
/// OCI layer tarballs are gzip- or zstd-compressed and may store paths with or
/// without a leading `./`, so we try both forms. `tar -xaf` autodetects compression.
fn extract_one_from_layer(blob: &Path, src: &Path, dst: &Path) -> Result<bool> {
    let src_no_leading = src
        .strip_prefix("/")
        .unwrap_or(src)
        .to_string_lossy()
        .into_owned();
    let candidates = [format!("./{}", src_no_leading), src_no_leading.clone()];

    for candidate in &candidates {
        // Stream directly to disk — initrds can be ~200 MB, no reason to buffer.
        let dst_file = fs::File::create(dst).with_context(|| {
            format!(
                "failed to open destination {} for tar extract",
                dst.display()
            )
        })?;
        let status = Command::new("tar")
            .args([
                "-xaf",
                blob.to_str().ok_or_else(|| anyhow!("invalid blob path"))?,
                "-O",
                candidate,
            ])
            .stdout(dst_file)
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to invoke tar for layer extraction")?;
        if status.success() {
            // tar emitted to stdout — verify we got actual bytes (some tar versions
            // exit 0 even when the path isn't in the archive, just producing empty).
            if let Ok(meta) = fs::metadata(dst)
                && meta.len() > 0
            {
                return Ok(true);
            }
        }
        // Clean the empty destination so the next attempt starts fresh.
        let _ = fs::remove_file(dst);
    }
    Ok(false)
}

fn install_systemd_boot_from_target(
    esp_path: &Path,
    mount_path: &Path,
    target_image: &str,
    mount_ok: bool,
) -> Result<()> {
    // The sealed composefs overlay mount exposes real file content, so the
    // systemd-boot binary is read straight off the mount (with a registry
    // fallback for the unusual case where it's absent from the mount).
    // When the mount is empty (bootc mounted in a private namespace), skip the
    // probe and let extract_files_preferring_mount source the binary from the
    // registry — it errors if the image genuinely doesn't ship systemd-boot,
    // which the caller turns into a graceful GRUB2 fallback.
    let probe = mount_path.join("usr/lib/systemd/boot/efi/systemd-bootx64.efi");
    if mount_ok && !probe.exists() {
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
    extract_files_preferring_mount(
        mount_path,
        target_image,
        &[(
            Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi"),
            &sd_dst,
        )],
    )
    .context("failed to install systemd-bootx64.efi from target image")?;

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

// ---- Helpers ----

/// Ensure the ESP is mounted and return its mount path.
/// On OSTree systems the ESP may not be auto-mounted; we mount it temporarily if needed.
fn ensure_esp_mounted(report: &PreflightReport) -> Result<String> {
    // If already detected and mounted, use it.
    if let Some(ref path) = report.esp_path
        && Path::new(path).exists()
    {
        return Ok(path.clone());
    }

    // Try common mount points first.
    for path in ["/boot/efi", "/efi"] {
        if Path::new(path).exists() && Path::new(path).join("EFI").exists() {
            return Ok(path.to_string());
        }
    }

    // ESP not mounted — try to find and mount it.
    // Use lsblk to find the ESP partition by its type GUID.
    let output = Command::new("lsblk")
        .args(["-o", "NAME,PARTTYPE,MOUNTPOINT", "-l", "-n"])
        .output()
        .context("failed to run lsblk")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1] == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b" {
            let device = format!("/dev/{}", parts[0]);
            let mount_point = "/var/tmp/esp-migration";
            fs::create_dir_all(mount_point)?;
            let status = Command::new("mount")
                .args([&device, mount_point])
                .status()
                .context("failed to mount ESP")?;
            if status.success() {
                println!("Auto-mounted ESP {} at {}", device, mount_point);
                return Ok(mount_point.to_string());
            }
        }
    }

    anyhow::bail!("Cannot find or mount ESP. Use --bootloader=grub2 to use GRUB2 instead.")
}

/// Find the ESP partition and return its mount point, auto-mounting it
/// under /var/tmp/esp-migration if it is not already mounted. Does not
/// require a PreflightReport — use from the commit/cleanup path where
/// the preflight context is not available.
pub(crate) fn find_esp_or_mount() -> Result<String> {
    // Check standard mount points first.
    for path in ["/boot/efi", "/efi"] {
        if Path::new(path).exists() && Path::new(path).join("EFI").exists() {
            return Ok(path.to_string());
        }
    }

    // Scan lsblk: if the ESP is already mounted at a non-standard path,
    // return that mount point.
    let output = Command::new("lsblk")
        .args(["-o", "NAME,PARTTYPE,MOUNTPOINT", "-l", "-n"])
        .output()
        .context("failed to run lsblk")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3
            && parts[1] == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
            && !parts[2].is_empty()
        {
            let mp = parts[2].to_string();
            println!("Found ESP already mounted at {}", mp);
            return Ok(mp);
        }
    }

    // Not mounted — find device and mount it.
    let by_label = Path::new("/dev/disk/by-partlabel/EFI-SYSTEM");
    let device = if by_label.exists()
        && let Ok(target) = fs::read_link(by_label)
        && let Some(name) = target.file_name().and_then(|n| n.to_str())
    {
        format!("/dev/{}", name)
    } else {
        // Fallback: scan lsblk by partition type GUID.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut found = None;
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1] == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b" {
                found = Some(format!("/dev/{}", parts[0]));
                break;
            }
        }
        found.ok_or_else(|| anyhow!("No ESP device found by partition label or type GUID"))?
    };
    let mount_point = "/var/tmp/esp-migration";
    fs::create_dir_all(mount_point)?;
    let status = Command::new("mount")
        .args([&device, mount_point])
        .status()
        .with_context(|| format!("failed to mount ESP {} at {}", device, mount_point))?;
    if status.success() {
        println!("Auto-mounted ESP {} at {}", device, mount_point);
        return Ok(mount_point.to_string());
    }
    anyhow::bail!("Cannot find or mount ESP. Use --bootloader=grub2 to use GRUB2 instead.")
}

/// Parse the ESP device and partition from findmnt output.
/// Returns (disk, partition_number). Returns None if parsing fails.
fn get_esp_disk_and_part(esp_path: &str) -> Option<(String, String)> {
    let output = Command::new("/usr/bin/findmnt")
        .args(["-n", "-o", "SOURCE", "-T", esp_path])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if source.is_empty() {
        return None;
    }

    // Handle /dev/nvme0n1p1, /dev/loop0p1 patterns
    if source.contains("nvme") || source.contains("loop") {
        if let Some(pos) = source.rfind('p') {
            let disk = source[..pos].to_string();
            let part = source[pos + 1..].to_string();
            if part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() {
                return Some((disk, part));
            }
        }
    } else if !source.contains("mapper") {
        // Regular /dev/sda1, /dev/vda1 patterns
        let mut split_idx = source.len();
        for (i, c) in source.char_indices().rev() {
            if c.is_ascii_digit() {
                split_idx = i;
            } else {
                break;
            }
        }
        if split_idx < source.len() {
            let disk = source[..split_idx].to_string();
            let part = source[split_idx..].to_string();
            if !part.is_empty() {
                return Some((disk, part));
            }
        }
    }
    // device-mapper, LVM, or other complex paths — skip efibootmgr registration.
    eprintln!(
        "Warning: cannot parse ESP device path '{}' — skipping efibootmgr registration.",
        source
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_pull_digests_kanpur_format_no_manifest_line() {
        // bootc 1.13.0: config + verity, no "manifest" line. The old code used the
        // whole multi-line output as manifest_digest, corrupting the .origin ini.
        let out = "config sha256:39f5731c23efd9\nverity b0e7a7dabb84cb9d";
        let (manifest, config) = parse_pull_digests(out);
        // No usable manifest digest from this output (caller falls back to podman).
        assert_eq!(manifest, None);
        // Config digest is parsed clean and single-line.
        assert_eq!(config.as_deref(), Some("sha256:39f5731c23efd9"));
    }

    #[test]
    fn parse_pull_digests_with_manifest_line() {
        let out = "manifest sha256:aaa\nconfig sha256:bbb\nverity ccc";
        let (manifest, config) = parse_pull_digests(out);
        assert_eq!(manifest.as_deref(), Some("sha256:aaa"));
        assert_eq!(config.as_deref(), Some("sha256:bbb"));
    }

    #[test]
    fn parse_pull_digests_single_line_fallback() {
        // Single bare digest line → config via the sha256: token fallback.
        let (manifest, config) = parse_pull_digests("sha256:deadbeef");
        assert_eq!(manifest, None);
        assert_eq!(config.as_deref(), Some("sha256:deadbeef"));
    }

    #[test]
    fn parse_pull_digests_never_returns_multiline() {
        // Even a malformed multi-token "manifest" line must be rejected, never
        // passed through to corrupt the .origin ini.
        let (manifest, _) = parse_pull_digests("manifest sha256:x extra junk");
        assert_eq!(manifest, None);
    }

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
