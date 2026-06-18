use crate::VerityDigest;
use crate::migration::esp::find_esp_device;
use crate::migration::phase0::detect_lvm;
use crate::preflight::PreflightReport;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

pub(crate) fn verify_migration(verity: &VerityDigest, _report: &PreflightReport) -> Result<()> {
    println!("=== Verifying migration artifacts ===");

    let deploy_dir = Path::new("/sysroot/state/deploy").join(verity.as_hex());

    // 1. Verify .origin file exists and is valid INI.
    let origin_path = deploy_dir.join(format!("{}.origin", verity.as_hex()));
    if !origin_path.exists() {
        anyhow::bail!("Missing .origin file at {}", origin_path.display());
    }
    let origin_text = fs::read_to_string(&origin_path).context("failed to read .origin file")?;
    let _parsed = tini::Ini::from_string(&origin_text)
        .map_err(|e| anyhow!(".origin file is not valid INI: {e}"))?;
    println!("  ✓ .origin file is valid INI");

    // 2. Verify kernel (vmlinuz) is a valid bzImage, not zeros.
    // The ESP may not be mounted at /boot/efi after Phase 5 — search
    // common mount points and try to locate + mount the ESP partition.
    let boot_name = format!("bootc_composefs-{}", verity.as_hex());
    let grub_vmlinuz = Path::new("/boot").join(&boot_name).join("vmlinuz");
    let mut vmlinuz_candidate = if grub_vmlinuz.exists() {
        Some(grub_vmlinuz)
    } else {
        None
    };
    // Check known ESP mount points.
    for esp_mp in &["/boot/efi", "/efi"] {
        if vmlinuz_candidate.is_some() {
            break;
        }
        let p = Path::new(esp_mp)
            .join("EFI/Linux")
            .join(&boot_name)
            .join("vmlinuz");
        if p.exists() {
            vmlinuz_candidate = Some(p);
        }
    }
    // If still not found, look up the ESP device and find where it's
    // already mounted (Phase 5 mounts it at a temp path). Prefer the
    // existing mount over creating a new one to avoid "already mounted"
    // errors.
    let _esp_temp_mount: Option<TempDir> = if vmlinuz_candidate.is_none() {
        if let Some(esp_dev) = find_esp_device() {
            // Check if the ESP is already mounted somewhere.
            let existing_mp = if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
                mounts
                    .lines()
                    .find(|l| l.starts_with(&esp_dev))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .map(|s| s.to_string())
            } else {
                None
            };
            if let Some(ref mp) = existing_mp {
                let p = Path::new(mp)
                    .join("EFI/Linux")
                    .join(&boot_name)
                    .join("vmlinuz");
                if p.exists() {
                    vmlinuz_candidate = Some(p);
                }
                None // don't create a temp mount, use existing
            } else {
                // Not mounted — mount it temporarily.
                let tmp = TempDir::new_in("/tmp").ok();
                if let Some(ref t) = tmp {
                    if Command::new("mount")
                        .args(["-o", "ro", &esp_dev, t.path().to_str().unwrap_or("")])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                    {
                        let p = t.path().join("EFI/Linux").join(&boot_name).join("vmlinuz");
                        if p.exists() {
                            vmlinuz_candidate = Some(p);
                        }
                    }
                }
                tmp
            }
        } else {
            None
        }
    } else {
        None
    };
    let vmlinuz = vmlinuz_candidate.ok_or_else(|| anyhow!("vmlinuz not found in ESP or /boot"))?;
    let magic =
        fs::read(&vmlinuz).with_context(|| format!("failed to read {}", vmlinuz.display()))?;
    if magic.len() < 4 || &magic[..2] != b"MZ" {
        anyhow::bail!(
            "vmlinuz at {} is not a valid kernel (no MZ magic: {:02x?})",
            vmlinuz.display(),
            &magic[..4.min(magic.len())]
        );
    }
    // Also check it's not all zeros (size > 0 and non-zero content).
    if magic.iter().take(1024).all(|&b| b == 0) {
        anyhow::bail!("vmlinuz at {} is all zeros (corrupted)", vmlinuz.display());
    }
    println!("  ✓ vmlinuz is valid kernel ({} bytes)", magic.len());

    // 3. Verify initrd exists and is non-zero (same boot dir as vmlinuz).
    let initrd = vmlinuz.parent().unwrap_or(Path::new("/")).join("initrd");
    if !initrd.exists() {
        // Try the GRUB2 fallback path in /boot.
        let fallback = Path::new("/boot").join(&boot_name).join("initrd");
        if fallback.exists() {
            // initrd is at the GRUB2 path.
            drop(fallback);
        } else {
            anyhow::bail!("initrd not found (checked ESP and /boot)");
        }
    }
    let initrd = if vmlinuz
        .parent()
        .unwrap_or(Path::new("/"))
        .join("initrd")
        .exists()
    {
        vmlinuz.parent().unwrap_or(Path::new("/")).join("initrd")
    } else {
        Path::new("/boot").join(&boot_name).join("initrd")
    };
    let initrd_size = fs::metadata(&initrd).map(|m| m.len()).unwrap_or(0);
    if initrd_size == 0 {
        anyhow::bail!(
            "initrd at {} is 0 bytes — registry extraction may have failed",
            initrd.display()
        );
    }
    println!("  ✓ initrd is valid ({} bytes)", initrd_size);
    if detect_lvm() {
        // Quick check: the initrd is a raw cpio archive — look for dm-mod.ko.
        let listing = Command::new("cpio")
            .args(["-t"])
            .stdin(fs::File::open(&initrd)?)
            .output()
            .context("failed to list initrd contents")?;
        let stdout = String::from_utf8_lossy(&listing.stdout);
        if !stdout.contains("dm-mod") && !stdout.contains("dm_mod") {
            eprintln!(
                "  ⚠ LVM root detected but initrd may lack device-mapper modules — \
                 system may fail to find root device."
            );
        } else {
            println!("  ✓ initrd contains device-mapper modules");
        }
    }

    // 4. Verify BLS entry exists and references composefs.
    // BLS entries may be on the ESP (systemd-boot) or /boot (GRUB2).
    // Use /proc/mounts to find the ESP mount point instead of guessing.
    let mut entries_dirs: Vec<PathBuf> = vec!["/boot/loader/entries".into()];
    if let Some(esp_dev) = find_esp_device() {
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                if line.starts_with(&esp_dev) {
                    if let Some(mp) = line.split_whitespace().nth(1) {
                        entries_dirs.push(Path::new(mp).join("loader/entries"));
                    }
                    break;
                }
            }
        }
        // Also check common static mount points.
        for mp in &["/boot/efi", "/efi"] {
            entries_dirs.push(Path::new(mp).join("loader/entries"));
        }
    }
    let mut found_bls = false;
    for entries_dir in &entries_dirs {
        if let Ok(rd) = fs::read_dir(entries_dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("bootc_") {
                    let content = fs::read_to_string(entry.path())?;
                    if content.contains("linux ") && content.contains("composefs=") {
                        println!("  ✓ BLS entry {} references composefs deployment", name);
                        found_bls = true;
                        break;
                    }
                }
            }
        }
        if found_bls {
            break;
        }
    }
    if !found_bls {
        anyhow::bail!("no composefs BLS entry found in ESP or /boot");
    }

    println!("  ✓ All verification checks passed");
    Ok(())
}
