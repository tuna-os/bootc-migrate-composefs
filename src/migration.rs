use std::fs;
use std::process::Command;
use std::path::Path;
use anyhow::{anyhow, Result, Context};
use crate::preflight::PreflightReport;

pub fn inspect_image(image_id: &str) -> Result<String> {
    let output = Command::new("bootc")
        .args(["internals", "cfs", "--system", "oci", "inspect", image_id])
        .output()
        .context("failed to execute bootc internals cfs oci inspect")?;
        
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("inspect failed: {}", stderr));
    }
    
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.to_string())
}

pub fn mount_image(image_id: &str, mount_path: &Path) -> Result<()> {
    let mount_str = mount_path.to_str().ok_or_else(|| anyhow!("invalid mount path"))?;
    
    // Try direct erofs mount of the backing file first (simpler, no seal required)
    let image_path = Path::new("/sysroot/composefs/images").join(image_id);
    if image_path.exists() {
        let output = Command::new("mount")
            .args(["-t", "erofs", "-o", "ro,loop", 
                   image_path.to_str().unwrap_or(""), mount_str])
            .output()
            .context("failed to mount erofs image")?;
        if output.status.success() {
            return Ok(());
        }
        // Fall through to bootc cfs oci mount
    }
    
    let output = Command::new("bootc")
        .args(["internals", "cfs", "--system", "oci", "mount", image_id, mount_str])
        .output()
        .context("failed to execute bootc internals cfs oci mount")?;
        
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("mount failed: {}", stderr));
    }
    
    Ok(())
}

pub fn run_migration(report: &PreflightReport, target_image: &str) -> Result<()> {
    println!("Remounting /sysroot read-write...");
    let _ = Command::new("mount")
        .args(["-o", "remount,rw", "/sysroot"])
        .status();
    // /boot may also be read-only on OSTree systems
    let _ = Command::new("mount")
        .args(["-o", "remount,rw", "/boot"])
        .status();

    println!("=== Phase 1: Importing OSTree objects ===");
    let ostree_repo = "/sysroot/ostree/repo";
    if Path::new(ostree_repo).exists() {
        let file_objects = crate::ostree::scan_ostree_file_objects(ostree_repo)
            .context("failed to scan ostree repository")?;
        let total_objects = file_objects.len();
        println!("Found {} file objects to import.", total_objects);
        
        let mut count = 0;
        let mut reflink_count = 0;
        
        for obj in file_objects {
            // Compute SHA-512 of the object content
            let sha512 = crate::ostree::compute_sha512(&obj.path)
                .context("failed to compute sha512")?;
            
            // ComposeFS object path: objects/xx/xxxxxxxx...
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
                println!("Imported {}/{} objects...", count, list_count_placeholder(total_objects));
            }
        }
        println!("Successfully imported {} objects ({} reflinked).", count, reflink_count);
    } else {
        println!("No OSTree repository found at {}. Skipping object import.", ostree_repo);
    }

    println!("=== Phase 2: Pulling OCI image ===");
    println!("Pulling target image: {}...", target_image);
    let pull_output = crate::composefs::pull_image(target_image)
        .context("failed to pull OCI image")?;
    
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
    println!("Target image pulled. Manifest digest: {}, Config digest: {}", manifest_digest, config_digest);

    println!("=== Phase 3: Creating ComposeFS EROFS Image ===");
    let sha512_verity = crate::composefs::create_image(&config_digest)
        .context("failed to create composefs image")?;
    // Strip the sha512: prefix for cleaner directory/file names (GRUB doesn't handle colons well)
    let verity_hash = sha512_verity.strip_prefix("sha512:").unwrap_or(&sha512_verity);
    println!("ComposeFS EROFS image created. Verity digest: {}", sha512_verity);

    // Seal the image so it can be mounted
    println!("Sealing composefs image...");
    crate::composefs::seal_image(&config_digest)
        .context("failed to seal composefs image")?;
    println!("Image sealed successfully.");

    println!("=== Phase 4: Staging Deployment State ===");
    let deploy_dir = Path::new("/sysroot/state/deploy").join(verity_hash);
    fs::create_dir_all(&deploy_dir).context("failed to create deployment directory")?;

    // Create etc and var directories
    let etc_dir = deploy_dir.join("etc");
    fs::create_dir_all(&etc_dir).context("failed to create deployment etc directory")?;
    
    // Copy etc from current booted system
    println!("Copying /etc config...");
    copy_dir_all("/etc", &etc_dir).context("failed to copy /etc")?;

    // Create var symlink: var -> ../../os/default/var
    let var_symlink = deploy_dir.join("var");
    if var_symlink.exists() {
        let _ = fs::remove_file(&var_symlink);
    }
    std::os::unix::fs::symlink("../../os/default/var", &var_symlink)
        .context("failed to create /var symlink")?;

    // Write .origin file
    let origin_path = deploy_dir.join(format!("{}.origin", sha512_verity));
    let origin_content = format!(
        "[origin]\ncontainer-image-reference = ostree-unverified-image:docker://{}\n\n[boot]\nboot_type = bls\ndigest = {}\n",
        target_image, sha512_verity
    );
    fs::write(&origin_path, origin_content).context("failed to write .origin file")?;

    // Write .imginfo file
    println!("Writing .imginfo file...");
    if let Ok(config_json) = inspect_image(&config_digest) {
        let imginfo_path = deploy_dir.join(format!("{}.imginfo", sha512_verity));
        let _ = fs::write(&imginfo_path, config_json);
    }

    println!("=== Phase 5: Setting Up Bootloader ===");
    // Determine bootloader
    let use_systemd_boot = report.is_uefi && report.esp_path.is_some() && report.esp_free_space_bytes >= 300 * 1024 * 1024;
    
    // Create temp mount path inside workspace to mount ComposeFS image
    let temp_mount = Path::new("/var/home/james/dev/ostree-composefs-rebase").join("mnt-temp");
    let _ = fs::remove_dir_all(&temp_mount);
    fs::create_dir_all(&temp_mount)?;
    
    // The EROFS image is stored under the verity hash in /sysroot/composefs/images/
    println!("Mounting ComposeFS image to extract boot artifacts...");
    mount_image(verity_hash, &temp_mount).context("failed to mount composefs image")?;
    
    let result = (|| -> Result<()> {
        // Find kernel version from mounted image /usr/lib/modules
        let modules_dir = temp_mount.join("usr/lib/modules");
        let kver = fs::read_dir(&modules_dir)?
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .ok_or_else(|| anyhow!("could not find kernel version in mounted image"))?;
            
        println!("Found kernel version: {}", kver);
        
        let mounted_vmlinuz = modules_dir.join(&kver).join("vmlinuz");
        let mounted_initrd = modules_dir.join(&kver).join("initramfs.img"); // or initrd
        
        let vmlinuz_src = if mounted_vmlinuz.exists() {
            mounted_vmlinuz
        } else {
            temp_mount.join("boot").join(format!("vmlinuz-{}", kver))
        };
        
        let initrd_src = if mounted_initrd.exists() {
            mounted_initrd
        } else {
            temp_mount.join("boot").join(format!("initramfs-{}.img", kver))
        };
        
        let options_str = get_kernel_options(&sha512_verity)?;
        if use_systemd_boot {
            let esp = report.esp_path.as_ref().unwrap();
            println!("Migrating to systemd-boot on ESP: {}...", esp);
            
            // Install systemd-boot
            let status = Command::new("bootctl")
                .args(["--path", esp, "install", "--no-variables"])
                .status()?;
            if !status.success() {
                return Err(anyhow!("bootctl install failed"));
            }
            
            // Create target boot directory on ESP
            let boot_dir_name = format!("bootc_composefs-{}", verity_hash);
            let esp_boot_dir = Path::new(esp).join("EFI/Linux").join(&boot_dir_name);
            fs::create_dir_all(&esp_boot_dir)?;
            
            // Copy boot files
            fs::copy(&vmlinuz_src, esp_boot_dir.join("vmlinuz"))?;
            if initrd_src.exists() {
                fs::copy(&initrd_src, esp_boot_dir.join("initrd"))?;
            }
            
            // Write systemd-boot BLS entry
            let entry_path = Path::new(esp).join("loader/entries").join(format!("bootc_bluefin_dakota-{}.conf", verity_hash));
            fs::create_dir_all(entry_path.parent().unwrap())?;
            
            let entry_content = format!(
                "title Dakota\nversion {}\nlinux /EFI/Linux/{}/vmlinuz\ninitrd /EFI/Linux/{}/initrd\noptions {}\nsort-key bootc-bluefin-dakota-0\n",
                kver, boot_dir_name, boot_dir_name, options_str
            );
            fs::write(entry_path, entry_content)?;
            
            // Update UEFI boot manager
            if let Some((disk, part)) = get_esp_disk_and_part(esp) {
                let _ = Command::new("efibootmgr")
                    .args(["-c", "-d", &disk, "-p", &part, "-L", "Linux Boot Manager (systemd-boot)", "-l", "\\EFI\\systemd\\systemd-bootx64.efi"])
                    .status();
            } else {
                let _ = Command::new("efibootmgr")
                    .args(["-c", "-d", "/dev/vda", "-p", "1", "-L", "Linux Boot Manager (systemd-boot)", "-l", "\\EFI\\systemd\\systemd-bootx64.efi"])
                    .status();
            }
        } else {
            println!("Staying on GRUB2 bootloader (BLS Type 1)...");
            let boot_dir_name = format!("bootc_composefs-{}", verity_hash);
            let grub_boot_dir = Path::new("/boot").join(&boot_dir_name);
            fs::create_dir_all(&grub_boot_dir)?;
            
            fs::copy(&vmlinuz_src, grub_boot_dir.join("vmlinuz"))?;
            if initrd_src.exists() {
                fs::copy(&initrd_src, grub_boot_dir.join("initrd"))?;
            }
            
            let entry_path = Path::new("/boot/loader/entries").join(format!("bootc_bluefin_dakota-{}.conf", verity_hash));
            let entry_content = format!(
                "title Dakota\nversion {}\nlinux /{}/vmlinuz\ninitrd /{}/initrd\noptions {}\nsort-key bootc-bluefin-dakota-0\n",
                kver, boot_dir_name, boot_dir_name, options_str
            );
            fs::write(entry_path, entry_content)?;
            
            // Set the new entry as default via grubenv (skip grub2-mkconfig which
            // fails on composefs= kernel options). grubenv is a 1024-byte block
            // with a specific format — must use grub2-editenv, not raw writes.
            let entry_id = format!("bootc_bluefin_dakota-{}", verity_hash);
            let grubenv = "/boot/grub2/grubenv";
            let ee = Command::new("grub2-editenv")
                .args([grubenv, "set", &format!("saved_entry={}", entry_id)])
                .status();
            if !matches!(ee, Ok(s) if s.success()) {
                let _ = Command::new("grub2-set-default").arg(&entry_id).status();
            }
            // Ensure GRUB_DEFAULT=saved so saved_entry is actually consulted
            if let Ok(cfg) = fs::read_to_string("/etc/default/grub") {
                let mut new_cfg = String::new();
                let mut found = false;
                for line in cfg.lines() {
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
                let _ = fs::write("/etc/default/grub", new_cfg);
            }
        }
        
        Ok(())
    })();
    
    // Unmount composefs image
    let _ = Command::new("umount").arg(&temp_mount).status();
    let _ = fs::remove_dir_all(&temp_mount);
    
    result.context("failed to copy boot files or create boot entries")?;

    // Handle moving /var data to ComposeFS state directory
    println!("=== Migrating /var data to ComposeFS state ===");
    let target_var = Path::new("/sysroot/state/os/default/var");
    
    if report.is_btrfs {
        // Check if /var is a separate mount
        let mounts = fs::read_to_string("/proc/mounts")?;
        let var_is_subvol = mounts.lines().any(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            parts.len() >= 3 && parts[1] == "/var" && parts[2] == "btrfs"
        });
        
        if var_is_subvol {
            println!("Preserving Btrfs 'var' subvolume mount.");
            // Copy mount options to new etc/fstab in the deployment
            if let Ok(fstab_content) = fs::read_to_string("/etc/fstab") {
                let mut new_fstab = String::new();
                for line in fstab_content.lines() {
                    if line.contains("/var") && !line.trim_start().starts_with('#') {
                        new_fstab.push_str(line);
                        new_fstab.push('\n');
                    }
                }
                if !new_fstab.is_empty() {
                    let new_fstab_path = etc_dir.join("fstab");
                    let _ = fs::write(&new_fstab_path, new_fstab);
                }
            }
            // On separate /var subvolume, data stays in place; symlink resolves correctly
            println!("/var is on a separate Btrfs subvolume — data stays in place.");
        }
    }
    
    // /var is not a separate mount — migrate data
    if !target_var.exists() {
        fs::create_dir_all(target_var.parent().unwrap())?;
        
        // Find the actual source of /var data
        let source_var = if Path::new("/sysroot/ostree/deploy/default/var").exists() {
            "/sysroot/ostree/deploy/default/var".to_string()
        } else if Path::new("/sysroot/ostree/deploy/default/var").is_symlink() {
            // Resolve the symlink to find the real var location
            fs::read_link("/sysroot/ostree/deploy/default/var")
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/sysroot/ostree/deploy/default/var".to_string())
        } else {
            "/var".to_string()
        };
        
        println!("Migrating /var data from {} to ComposeFS state...", source_var);
        copy_dir_all(&source_var, &target_var)
            .context("failed to migrate /var data to ComposeFS state")?;
        println!("/var data migrated successfully.");
    }

    println!("\n=== MIGRATION COMPLETED ===");
    println!("Staged ComposeFS deployment: {}", verity_hash);
    if use_systemd_boot {
        println!("Primary bootloader updated to: systemd-boot");
    } else {
        println!("Boot entry created in GRUB2 (BLS Type 1)");
    }
    println!("Please reboot the system to finalize the transition.");
    println!("After reboot, run 'bootc internals cleanup' to remove the old OSTree files.");
    
    Ok(())
}

fn list_count_placeholder(val: usize) -> usize {
    val
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dst.join(file_name);
        let ty = entry.file_type()?;
        
        if ty.is_dir() {
            copy_dir_all(&path, &dest_path)?;
        } else if ty.is_symlink() {
            if dest_path.exists() || dest_path.is_symlink() {
                let _ = fs::remove_file(&dest_path);
            }
            let link_target = fs::read_link(&path)?;
            std::os::unix::fs::symlink(link_target, &dest_path)?;
        } else if ty.is_file() {
            if dest_path.exists() || dest_path.is_symlink() {
                let _ = fs::remove_file(&dest_path);
            }
            fs::copy(&path, &dest_path)?;
        } else {
            // Skip sockets, pipes, devices, etc. to avoid errors
            eprintln!("Warning: skipping special file at {:?}", path);
        }
    }
    Ok(())
}

fn get_kernel_options(sha512_verity: &str) -> Result<String> {
    let cmdline = fs::read_to_string("/proc/cmdline")
        .context("failed to read /proc/cmdline")?;
    let mut options = Vec::new();
    for word in cmdline.split_whitespace() {
        if word.starts_with("ostree=") || word.starts_with("BOOT_IMAGE=") || word.starts_with("initrd=") {
            continue;
        }
        options.push(word.to_string());
    }
    options.push(format!("composefs={}", sha512_verity));
    Ok(options.join(" "))
}

fn get_esp_disk_and_part(esp_path: &str) -> Option<(String, String)> {
    let output = Command::new("findmnt")
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
    
    // Parse disk and partition.
    // e.g. /dev/vda1 -> /dev/vda and 1
    // e.g. /dev/nvme0n1p1 -> /dev/nvme0n1 and 1
    // e.g. /dev/sda1 -> /dev/sda and 1
    if source.contains("nvme") || source.contains("loop") {
        if let Some(pos) = source.rfind('p') {
            let disk = source[..pos].to_string();
            let part = source[pos+1..].to_string();
            return Some((disk, part));
        }
    } else {
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
            return Some((disk, part));
        }
    }
    None
}
