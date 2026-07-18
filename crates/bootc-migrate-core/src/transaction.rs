//! Two-phase apply: finalize (`commit`) or roll back (`undo`) a staged
//! migration.
//!
//! The migration pipeline stages a composefs deployment next to the existing
//! OSTree one and leaves both bootable. This module supplies the two terminal
//! operations of that transaction:
//!
//! - [`commit`] — after a successful composefs boot, make it permanent:
//!   remove the OSTree fallback boot entry, delete the OSTree object store
//!   and deploys, and leave the on-disk layout matching a fresh install of
//!   the target image. One-way.
//! - [`undo`] — remove composefs boot artifacts and staged deployments while
//!   preserving the OSTree deployment (and, unless `full`, the composefs
//!   object store, which is expensive to rebuild across retries).
//!
//! Both support `dry_run` previews. Callers are responsible for privilege
//! checks; these functions assume root.

use crate::migration;
use crate::motd;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub fn commit(dry_run: bool) -> Result<()> {
    println!("=== Committing composefs deployment as permanent default ===");
    if dry_run {
        println!("*** DRY RUN — no changes will be made ***");
    }

    // Sanity check: refuse to run if booted via the OSTree side. Committing
    // would delete the rootfs we're currently mounted on top of.
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !cmdline.contains("composefs=") {
        anyhow::bail!(
            "/proc/cmdline does not contain composefs= — current boot looks like OSTree. \
             Reboot into the composefs entry before running commit.\n\
             cmdline: {}",
            cmdline.trim()
        );
    }

    // Detect bootloader — check ESP entries for systemd-boot first.
    let esp_candidates = ["/boot/efi", "/efi"];
    let mut entries_dir = PathBuf::from("/boot/loader/entries");
    let mut is_systemd_boot = false;

    for esp in &esp_candidates {
        let esp_entries = Path::new(esp).join("loader/entries");
        if esp_entries.exists() {
            // Check if there are bootc_ entries on the ESP.
            if let Ok(mut rd) = std::fs::read_dir(&esp_entries)
                && rd.any(|e| {
                    e.map(|en| en.file_name().to_string_lossy().starts_with("bootc_"))
                        .unwrap_or(false)
                })
            {
                entries_dir = esp_entries;
                is_systemd_boot = true;
                break;
            }
        }
    }

    let mut composefs_entries: Vec<_> = Vec::new();
    if entries_dir.exists() {
        for entry in std::fs::read_dir(&entries_dir)? {
            let entry = entry?;
            let name_str = entry.file_name().to_string_lossy().into_owned();
            if name_str.starts_with("bootc_") {
                composefs_entries.push(name_str);
            }
        }
    }
    // If we found bootc_ entries at the default /boot location but not
    // on the ESP, we're still on composefs — just without an auto-mounted
    // ESP at /boot/efi or /efi (common in E2E QEMU runs).
    if !composefs_entries.is_empty() {
        is_systemd_boot = true;
    }

    // Fallback: the ESP may be unmounted or at a non-standard path
    // (e.g. after LUKS migration where Phase 5 auto-mounted it at
    // /var/tmp/esp-migration and the boot-time fstab doesn't know about
    // it). Try auto-mounting the ESP by partition type GUID.
    if composefs_entries.is_empty()
        && let Ok(esp_path) = migration::find_esp_or_mount()
    {
        let esp_entries = Path::new(&esp_path).join("loader/entries");
        if esp_entries.exists() {
            for entry in std::fs::read_dir(&esp_entries)? {
                let entry = entry?;
                let name_str = entry.file_name().to_string_lossy().into_owned();
                if name_str.starts_with("bootc_") {
                    composefs_entries.push(name_str);
                }
            }
            if !composefs_entries.is_empty() {
                entries_dir = esp_entries;
                is_systemd_boot = true;
                println!(
                    "Found composefs BLS entries on auto-mounted ESP at {}",
                    esp_path
                );
            }
        }
    }

    if composefs_entries.is_empty() {
        if is_systemd_boot {
            println!("No composefs BLS entries found on ESP. Nothing to commit.");
            println!(
                "Note: for systemd-boot, the composefs entry should already be the default if it has the lowest sort-key."
            );
        } else {
            println!("No composefs BLS entries found. Nothing to commit.");
        }
        return Ok(());
    }

    // Sort by priority (higher first) and pick the highest
    composefs_entries.sort();
    composefs_entries.reverse();
    let primary = composefs_entries[0].trim_end_matches(".conf");

    if is_systemd_boot {
        // Remove the OSTree fallback entry + its kernel/initrd from the ESP so the next
        // boot menu only shows the composefs entry. The composefs entry remains the
        // loader.conf default; nothing else needs to change.
        let esp_root = entries_dir.parent().and_then(|p| p.parent());
        if let Some(esp_root) = esp_root {
            let fallback_entry = entries_dir.join("ostree-fallback-0.conf");
            if fallback_entry.exists() {
                std::fs::remove_file(&fallback_entry)
                    .with_context(|| format!("failed to remove {}", fallback_entry.display()))?;
                println!("Removed OSTree fallback BLS entry from ESP.");
            }
            let fallback_dir = esp_root.join("EFI/Linux/ostree-fallback");
            if fallback_dir.exists() {
                std::fs::remove_dir_all(&fallback_dir)
                    .with_context(|| format!("failed to remove {}", fallback_dir.display()))?;
                println!("Removed OSTree fallback kernel/initrd from ESP.");
            }
            // Drop the timeout now that composefs is the only entry.
            let loader_conf = esp_root.join("loader/loader.conf");
            if loader_conf.exists() {
                let body = format!("default {}\ntimeout 0\nconsole-mode keep\n", primary);
                std::fs::write(&loader_conf, body)
                    .with_context(|| format!("failed to rewrite {}", loader_conf.display()))?;
            }
        }
        println!(
            "Composefs deployment '{}' committed as the permanent systemd-boot default.",
            primary
        );
    } else {
        if !dry_run {
            let status = std::process::Command::new("grub2-set-default")
                .arg(primary)
                .status();
            if !matches!(status, Ok(s) if s.success()) {
                anyhow::bail!("failed to set GRUB default");
            }
            // Drop GRUB2-side OSTree fallback artifacts too.
            let _ = std::fs::remove_file("/boot/loader/entries/ostree-fallback-0.conf");
            let _ = std::fs::remove_dir_all("/boot/ostree-fallback");
        }
        println!(
            "Composefs deployment '{}' is now the permanent default.",
            primary
        );
    }

    // --- Full OSTree-side cleanup so the on-disk layout matches a fresh
    //     bootc install of the target image. ---
    // /sysroot is typically read-only on a composefs-booted system.
    // Even after remount rw, the composefs EROFS overlay blocks mutation
    // of paths that are pinned by the metadata tree (e.g. /sysroot/ostree).
    // To bypass the overlay, mount the underlying btrfs device at a
    // temporary location — that mount is a plain btrfs, no EROFS.
    let alt_root = Path::new("/var/tmp/commit-cleanup");
    let has_alt_mount = if !dry_run {
        let _ = std::fs::create_dir_all(alt_root);
        mount_sysroot_btrfs_at(alt_root).is_ok()
    } else {
        false
    };
    let mut total_freed: u64 = 0;

    // 1. /sysroot/ostree — the entire OSTree object store + deploys + the
    //    leaked Bluefin /var copy under ostree/deploy/<n>/var.
    //    If we have an alternate mount (bypassing the composefs EROFS overlay),
    //    operate through that; otherwise fall back to the direct path.
    let ostree_label = "OSTree object store + deploys (incl. leaked pre-migration /var)";
    if has_alt_mount {
        total_freed += remove_path_with_size(&alt_root.join("ostree"), ostree_label, dry_run);
        // .bootc-aleph.json also through the alt mount for a clean delete.
        total_freed += remove_path_with_size(
            &alt_root.join(".bootc-aleph.json"),
            "stale Bluefin install-provenance marker",
            dry_run,
        );
    } else {
        total_freed += remove_path_with_size(Path::new("/sysroot/ostree"), ostree_label, dry_run);
    }
    // .bootc-aleph.json — via alt mount if available, otherwise direct.
    if !has_alt_mount {
        total_freed += remove_path_with_size(
            Path::new("/sysroot/.bootc-aleph.json"),
            "stale Bluefin install-provenance marker",
            dry_run,
        );
    }

    // /boot may be read-only under composefs (e.g. separate ext4 /boot partition
    // on LUKS where the initramfs mounts it ro). Remount rw before cleanup.
    if !dry_run {
        let _ = std::process::Command::new("mount")
            .args(["-o", "remount,rw", "/boot"])
            .status();
    }

    // 2. Stale OSTree BLS entries under /boot/loader/entries. The ESP-side
    //    ostree-fallback was removed above; /boot/loader/entries/ostree-*.conf
    //    is the GRUB-side equivalent.
    if let Ok(rd) = std::fs::read_dir("/boot/loader/entries") {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("ostree-") && name.ends_with(".conf") {
                total_freed += remove_path_with_size(
                    &entry.path(),
                    &format!("stale OSTree BLS entry: {}", name),
                    dry_run,
                );
            }
        }
    }

    // 3. When we migrated to systemd-boot, drop the GRUB2 bits the user no
    //    longer needs. Keep them when --bootloader grub2 was used.
    if is_systemd_boot {
        for path in &["/boot/grub2", "/boot/efi/EFI/fedora"] {
            total_freed += remove_path_with_size(
                Path::new(path),
                "GRUB2 boot artifacts (migrated to systemd-boot)",
                dry_run,
            );
        }
    }

    // 4. Drop ostree-remount.service enablement. On a composefs-booted
    //    system OSTree bind mounts are irrelevant; the symlink may be
    //    re-created during boot by the target image's presets even though
    //    Phase 4 removed it from the deploy /etc.
    let remount_link =
        Path::new("/etc/systemd/system/local-fs.target.wants/ostree-remount.service");
    if remount_link.exists() || remount_link.is_symlink() {
        if dry_run {
            println!(
                "[DRY RUN] Would remove ostree-remount.service enablement (composefs doesn't need OSTree bind mounts)."
            );
        } else {
            std::fs::remove_file(remount_link)
                .with_context(|| format!("failed to remove {}", remount_link.display()))?;
            println!(
                "Removed ostree-remount.service enablement (composefs doesn't need OSTree bind mounts)."
            );
        }
    }

    let human = format_bytes(total_freed);
    if dry_run {
        println!("\nWould reclaim: {} ({} bytes)", human, total_freed);
        println!("Re-run without --dry-run to apply.");
    } else {
        println!("\nReclaimed: {} ({} bytes)", human, total_freed);
        println!(
            "On-disk layout is now consistent with a fresh '{}' install.",
            if is_systemd_boot {
                "systemd-boot"
            } else {
                "GRUB2"
            }
        );
    }
    if has_alt_mount {
        let _ = std::process::Command::new("umount").arg(alt_root).status();
        let _ = std::fs::remove_dir(alt_root);
    }
    if !dry_run && let Err(e) = motd::clear_migration_reminder() {
        eprintln!("Warning: failed to clear login reminder: {e:#}");
    }
    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.file_type().is_symlink() || meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    let rd = match std::fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    for entry in rd.flatten() {
        total += dir_size(&entry.path());
    }
    total
}

fn remove_path_with_size(path: &Path, label: &str, dry_run: bool) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let size = dir_size(path);
    let human = format_bytes(size);
    if dry_run {
        println!(
            "[dry-run] would remove {} — {} ({})",
            path.display(),
            label,
            human
        );
        return size;
    }
    let res = if meta.is_dir() && !meta.file_type().is_symlink() {
        // On OSTree/bootc systems, /sysroot/ostree is typically a btrfs
        // subvolume — rm -rf returns EPERM. Try `btrfs subvolume delete`
        // first; if that fails, clear the immutable flag (chattr -i) and
        // fall back to remove_dir_all.
        let btrfs_ok = std::process::Command::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if btrfs_ok {
            Ok(())
        } else {
            // Clear immutable flag — OSTree often sets chattr +i on
            // /sysroot/ostree to prevent accidental deletion. Suppress
            // stderr: chattr on OSTree deploy checkouts (symlink farms)
            // produces thousands of "Operation not supported" lines.
            let _ = std::process::Command::new("chattr")
                .args(["-R", "-i"])
                .arg(path)
                .stderr(std::process::Stdio::null())
                .status();
            std::fs::remove_dir_all(path)
        }
    } else {
        std::fs::remove_file(path)
    };
    match res {
        Ok(()) => {
            println!("Removed {} — {} ({})", path.display(), label, human);
            size
        }
        Err(e) => {
            eprintln!("warning: failed to remove {}: {}", path.display(), e);
            0
        }
    }
}

fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

/// Mount the device backing /sysroot at `target`, bypassing the
/// composefs EROFS overlay so that paths like /sysroot/ostree can be
/// mutated directly on the underlying filesystem. Works on btrfs, xfs,
/// and any other filesystem that can be mounted twice.
fn mount_sysroot_btrfs_at(target: &Path) -> Result<()> {
    let mounts = std::fs::read_to_string("/proc/mounts").context("failed to read /proc/mounts")?;
    let device = mounts
        .lines()
        .find(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            parts.len() >= 3 && parts[1] == "/sysroot"
        })
        .and_then(|line| line.split_whitespace().next())
        .map(|s| s.to_string())
        .context("could not find device for /sysroot in /proc/mounts")?;

    let status = std::process::Command::new("mount")
        .arg(&device)
        .arg(target)
        .status()
        .context("failed to execute mount for alt-root cleanup")?;
    if !status.success() {
        anyhow::bail!("mount {} → {} failed", device, target.display());
    }
    Ok(())
}

/// Undo a partial or failed migration. Removes all composefs artifacts
/// (staged deployments, boot artifacts, BLS entries, loopback images,
/// composefs object store) while leaving the OSTree deployment intact.
pub fn undo(dry_run: bool, full: bool) -> Result<()> {
    // Always release the migration lock so a subsequent run doesn't fail
    // with "already running". The lock guard drops automatically at process
    // exit, but if the previous run crashed mid-phase the lock can linger.
    let lock_path = "/var/run/bootc-migrate-composefs.lock";
    if !dry_run {
        let _ = std::fs::remove_file(lock_path);
    }

    println!("=== Undoing composefs migration ===");
    if dry_run {
        println!("*** DRY RUN — no changes will be made ***");
    }

    // /sysroot is mounted read-only on an OSTree-booted system (composefs or
    // classic). Remount rw so we can delete staged deployments and loopback
    // images that live there. Ignore the error — if it's already rw this is
    // a no-op; if it genuinely can't be made rw the subsequent removes will
    // surface the real error.
    if !dry_run {
        let _ = std::process::Command::new("mount")
            .args(["-o", "remount,rw", "/sysroot"])
            .status();
    }

    let mut removed = 0usize;
    let mut skipped = 0usize;

    // 1. Remove staged composefs deployments.
    let deploy_dir = Path::new("/sysroot/state/deploy");
    if deploy_dir.exists()
        && let Ok(rd) = std::fs::read_dir(deploy_dir)
    {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip the OSTree deploy dir (numeric or short names).
            // Composefs deploy dirs are long hex strings (64+ chars).
            if name.len() < 40 {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                println!("Removing staged deployment: {}", path.display());
                if !dry_run {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
                removed += 1;
            }
        }
    }
    if removed == 0 {
        println!("No composefs deployments found in /sysroot/state/deploy/.");
    }

    // 2. Remove composefs boot artifacts.
    let boot_dir = Path::new("/boot");
    if let Ok(rd) = std::fs::read_dir(boot_dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("bootc_composefs-") {
                let path = entry.path();
                println!("Removing boot artifacts: {}", path.display());
                if !dry_run {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
                removed += 1;
            }
        }
    }

    // 3. Remove composefs BLS entries from /boot/loader/entries.
    let bls_dir = Path::new("/boot/loader/entries");
    if bls_dir.exists()
        && let Ok(rd) = std::fs::read_dir(bls_dir)
    {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("bootc_") || name.starts_with("ostree-fallback-") {
                let path = entry.path();
                println!("Removing BLS entry: {}", path.display());
                if !dry_run {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
                removed += 1;
            }
        }
    }

    // 4. Remove composefs BLS entries from ESP.
    for esp in &["/boot/efi", "/efi"] {
        let esp_entries = Path::new(esp).join("loader/entries");
        if esp_entries.exists() {
            if let Ok(rd) = std::fs::read_dir(&esp_entries) {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("bootc_") || name.starts_with("ostree-fallback-") {
                        let path = entry.path();
                        println!("Removing ESP BLS entry: {}", path.display());
                        if !dry_run {
                            std::fs::remove_file(&path)
                                .with_context(|| format!("failed to remove {}", path.display()))?;
                        }
                        removed += 1;
                    }
                }
            }
            // Remove loader.conf if we wrote one.
            let loader_conf = Path::new(esp).join("loader/loader.conf");
            if loader_conf.exists() {
                println!("Removing ESP loader.conf: {}", loader_conf.display());
                if !dry_run {
                    std::fs::remove_file(&loader_conf)?;
                    removed += 1;
                }
            }
        }
        // Remove systemd-boot from ESP.
        let sd_dir = Path::new(esp).join("EFI/systemd");
        if sd_dir.exists() {
            println!("Removing systemd-boot from ESP: {}", sd_dir.display());
            if !dry_run {
                std::fs::remove_dir_all(&sd_dir)?;
                removed += 1;
            }
        }
        let esp_linux = Path::new(esp).join("EFI/Linux");
        if esp_linux.exists()
            && let Ok(rd) = std::fs::read_dir(&esp_linux)
        {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("bootc_composefs-") {
                    let path = entry.path();
                    println!("Removing ESP EFI/Linux entry: {}", path.display());
                    if !dry_run {
                        std::fs::remove_dir_all(&path)
                            .with_context(|| format!("failed to remove {}", path.display()))?;
                    }
                    removed += 1;
                }
            }
        }
    }

    // 5. Remove composefs loopback image (only with --full).
    if full {
        let loopback = Path::new("/sysroot/composefs-loopback.ext4");
        if loopback.exists() {
            println!("Removing composefs loopback image: {}", loopback.display());
            if !dry_run {
                std::fs::remove_file(loopback)?;
                removed += 1;
            }
        }

        // 6. Remove composefs object store (only with --full).
        let composefs_dir = Path::new("/sysroot/composefs");
        if composefs_dir.exists() {
            let has_objects = composefs_dir.join("objects").exists();
            let has_images = composefs_dir.join("images").exists();
            if has_objects || has_images {
                println!(
                    "Removing composefs object store: {}",
                    composefs_dir.display()
                );
                if !dry_run {
                    for sub in &["objects", "images", "streams", "tmp"] {
                        let p = composefs_dir.join(sub);
                        if p.exists() {
                            std::fs::remove_dir_all(&p)
                                .with_context(|| format!("failed to remove {}", p.display()))?;
                        }
                    }
                    removed += 1;
                }
            } else {
                println!("Composefs directory exists but is empty (no objects/images).");
                skipped += 1;
            }
        }
    } else {
        println!("Composefs object store and loopback preserved (re-run --full to clean).");
    }

    // 7. Optionally warn about NVRAM entries (can't clean those from userspace easily).
    println!();
    if dry_run {
        println!("Would remove {} artifact(s).", removed);
        println!("Re-run without --dry-run to apply.");
    } else {
        println!("Removed {} composefs artifact(s).", removed);
        if skipped > 0 {
            println!("{} path(s) skipped (empty or already clean).", skipped);
        }
        println!("The system is now in its pre-migration OSTree state.");
        println!("Run 'bootc-migrate-composefs --target-image <image>' to try again.");
    }
    if !dry_run && let Err(e) = motd::clear_migration_reminder() {
        eprintln!("Warning: failed to clear login reminder: {e:#}");
    }
    Ok(())
}
