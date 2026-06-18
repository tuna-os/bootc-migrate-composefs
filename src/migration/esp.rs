use crate::preflight::PreflightReport;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn find_esp_device() -> Option<String> {
    // Try the by-partlabel symlink (works inside VMs without lsblk sudo).
    let by_label = Path::new("/dev/disk/by-partlabel/EFI-SYSTEM");
    if by_label.exists() {
        if let Ok(target) = fs::read_link(by_label) {
            if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
                return Some(format!("/dev/{}", name));
            }
        }
    }
    // Fallback: scan lsblk by partition type GUID
    // (C12A7328-F81F-11D2-BA4B-00A0C93EC93B).
    if let Ok(output) = Command::new("lsblk")
        .args(["-ndo", "NAME,PARTTYPE"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1].to_lowercase() == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
            {
                return Some(format!("/dev/{}", parts[0]));
            }
        }
    }
    None
}

/// Main migration entry point. Orchestrates all 5 phases.
pub(crate) fn ensure_esp_mounted(report: &PreflightReport) -> Result<String> {
    // If already detected and mounted, use it.
    if let Some(ref path) = report.esp_path {
        if Path::new(path).exists() {
            return Ok(path.clone());
        }
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

/// Parse the ESP device and partition from findmnt output.
/// Returns (disk, partition_number). Returns None if parsing fails.
pub(crate) fn get_esp_disk_and_part(esp_path: &str) -> Option<(String, String)> {
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
