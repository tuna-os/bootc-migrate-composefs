use crate::preflight::PreflightReport;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---- Mount guard (Optional: safe cleanup of TempDir-backed mounts) ----

pub(crate) struct MountGuard {
    mount_path: PathBuf,
}

impl MountGuard {
    pub(crate) fn new(mount_path: &Path) -> Self {
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

    let free = crate::preflight::get_free_space("/sysroot")?;
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
pub(crate) fn setup_composefs_loopback_if_needed(
    report: &PreflightReport,
) -> Result<Option<MountGuard>> {
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

        // Calculate size: 1.5× ostree repo + 5 GB buffer, min 10 GB, max 30 GB.
        let ostree_gb = report.ostree_repo_size_bytes as f64 / 1e9;
        let size_gb = ((ostree_gb * 1.5 + 5.0).ceil() as u64).clamp(10, 30);
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

        println!("ComposeFS loopback mounted at {target} ({size_gb} GB ext4, fs-verity enabled).");
        Ok(Some(MountGuard::new(Path::new(target))))
    } else {
        Ok(None)
    }
}

/// Detect whether LVM volumes are active on the running system.
pub(crate) fn detect_lvm() -> bool {
    match fs::read_dir("/dev/mapper") {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy() != "control"),
        Err(_) => false,
    }
}
