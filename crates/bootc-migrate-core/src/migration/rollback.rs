//! Subcommand & library helper to automate return to the original OSTree deployment (issue #26).
//!
//! Re-orders UEFI `BootOrder` so the OSTree shim/GRUB boot entry takes priority over systemd-boot.
//! Does not delete the composefs deployment — just changes the default boot entry.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// Check prerequisites for rolling back to OSTree.
pub fn verify_rollback_prerequisites() -> Result<()> {
    // 1. UEFI firmware check
    if !Path::new("/sys/firmware/efi").exists() {
        bail!("rollback requires UEFI firmware.");
    }

    // 2. Commit check: /sysroot/ostree (or /ostree) must exist
    if !Path::new("/sysroot/ostree").exists() && !Path::new("/ostree").exists() {
        bail!(
            "/sysroot/ostree not found — commit has already removed the OSTree deployment. \
             Rollback is not possible after commit."
        );
    }

    // 3. Booted state check: must currently be booted into composefs deployment
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !cmdline.contains("composefs=") {
        bail!("already booted on OSTree — nothing to rollback.");
    }

    // 4. OSTree BLS entry check: /boot/loader/entries/ostree-*.conf must exist
    let bls_dir = Path::new("/boot/loader/entries");
    let has_ostree_bls = if bls_dir.is_dir() {
        std::fs::read_dir(bls_dir)
            .ok()
            .map(|entries| {
                entries.flatten().any(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.starts_with("ostree-") && name.ends_with(".conf")
                })
            })
            .unwrap_or(false)
    } else {
        false
    };

    if !has_ostree_bls {
        bail!("No OSTree BLS entry found under /boot/loader/entries/ostree-*.conf.");
    }

    Ok(())
}

/// Parse output of `efibootmgr` or `efibootmgr -v` to locate the OSTree/Fedora boot entry ID.
/// Searches for lines starting with `BootXXXX` matching "Fedora", "shim", "EFI\fedora", or "GRUB".
pub fn parse_ostree_boot_entry_id(efibootmgr_output: &str) -> Option<String> {
    for line in efibootmgr_output.lines() {
        let line_trim = line.trim();
        if let Some(rest) = line_trim.strip_prefix("Boot")
            && rest.len() >= 4
            && rest[..4].chars().all(|c| c.is_ascii_hexdigit())
        {
            let id = &rest[..4];
            let rest_line = &line_trim[8..];
            let line_lower = rest_line.to_ascii_lowercase();
            if line_lower.contains("fedora")
                || line_lower.contains("shim")
                || line_lower.contains("efi\\fedora")
                || line_lower.contains("grub")
            {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Parse current `BootOrder: XXXX,YYYY,...` line from `efibootmgr` output.
pub fn parse_boot_order(efibootmgr_output: &str) -> Option<String> {
    for line in efibootmgr_output.lines() {
        let line_trim = line.trim();
        if let Some(order) = line_trim.strip_prefix("BootOrder:") {
            return Some(order.trim().to_string());
        }
    }
    None
}

/// Build new `BootOrder` putting `target_id` first.
pub fn build_new_boot_order(current_order: &str, target_id: &str) -> String {
    let mut ids: Vec<String> = vec![target_id.to_string()];
    for id in current_order.split(',') {
        let id_trim = id.trim();
        if !id_trim.is_empty() && !id_trim.eq_ignore_ascii_case(target_id) {
            ids.push(id_trim.to_string());
        }
    }
    ids.join(",")
}

/// Execute rollback: re-orders UEFI BootOrder to prioritize OSTree deployment.
pub fn run_rollback(reboot: bool, dry_run: bool) -> Result<()> {
    if !dry_run {
        verify_rollback_prerequisites()?;
    }

    if dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }

    println!("Checking UEFI NVRAM boot entries...");

    let output = Command::new("efibootmgr")
        .arg("-v")
        .output()
        .context("failed to invoke efibootmgr")?;

    if !output.status.success() {
        bail!(
            "efibootmgr failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let txt = String::from_utf8_lossy(&output.stdout);
    let target_id = parse_ostree_boot_entry_id(&txt);

    let target_id = match target_id {
        Some(id) => id,
        None => {
            println!("No existing Fedora entry found in NVRAM; attempting to re-register...");
            if !dry_run {
                let status = Command::new("efibootmgr")
                    .args([
                        "--create",
                        "--label",
                        "Fedora",
                        "--loader",
                        "\\EFI\\fedora\\shimx64.efi",
                    ])
                    .status()
                    .context("failed to execute efibootmgr --create")?;
                if !status.success() {
                    bail!("Failed to re-register Fedora boot entry in UEFI NVRAM via efibootmgr.");
                }
            } else {
                println!(
                    "[DRY RUN] Would execute: efibootmgr --create --label Fedora --loader \\EFI\\fedora\\shimx64.efi"
                );
            }
            "0000".to_string()
        }
    };

    let current_order = parse_boot_order(&txt).unwrap_or_default();
    let new_order = build_new_boot_order(&current_order, &target_id);

    println!("Reordering UEFI BootOrder to prioritize entry {target_id} (Fedora/OSTree)...");
    if dry_run {
        println!("[DRY RUN] Would execute: efibootmgr --bootorder {new_order}");
    } else {
        let status = Command::new("efibootmgr")
            .args(["--bootorder", &new_order])
            .status()
            .context("failed to execute efibootmgr --bootorder")?;
        if !status.success() {
            bail!("Failed to set UEFI BootOrder via efibootmgr.");
        }
        println!("Successfully set UEFI BootOrder to: {new_order}");
    }

    if reboot {
        println!("Triggering reboot into OSTree deployment...");
        if dry_run {
            println!("[DRY RUN] Would execute: systemctl reboot");
        } else {
            let status = Command::new("systemctl")
                .arg("reboot")
                .status()
                .context("failed to execute systemctl reboot")?;
            if !status.success() {
                bail!("Failed to trigger systemctl reboot.");
            }
        }
    } else {
        println!(
            "Reboot now to return to Bluefin OSTree. \
             Run bootc-migrate commit when ready to finalize."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ostree_boot_entry_id() {
        let sample = "\
BootCurrent: 0001\n\
Timeout: 1 seconds\n\
BootOrder: 0001,0000,0002\n\
Boot0000* Fedora\tHD(1,GPT,123)/File(\\EFI\\fedora\\shimx64.efi)\n\
Boot0001* Linux Boot Manager\tHD(1,GPT,123)/File(\\EFI\\systemd\\systemd-bootx64.efi)\n\
";
        assert_eq!(parse_ostree_boot_entry_id(sample), Some("0000".to_string()));
    }

    #[test]
    fn test_parse_boot_order() {
        let sample = "\
BootCurrent: 0001\n\
Timeout: 1 seconds\n\
BootOrder: 0001,0000,0002\n\
";
        assert_eq!(parse_boot_order(sample), Some("0001,0000,0002".to_string()));
    }

    #[test]
    fn test_build_new_boot_order() {
        assert_eq!(
            build_new_boot_order("0001,0000,0002", "0000"),
            "0000,0001,0002"
        );
        assert_eq!(build_new_boot_order("0000,0001", "0000"), "0000,0001");
        assert_eq!(build_new_boot_order("", "0000"), "0000");
    }
}
