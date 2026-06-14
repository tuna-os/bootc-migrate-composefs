use clap::{Parser, Subcommand};
use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};
use std::process;

mod reflink;
mod preflight;
mod ostree;
mod composefs;
mod migration;
mod types;
mod xattr;
mod mergetc;

pub use types::VerityDigest;

#[derive(Parser, Debug)]
#[command(name = "bootc-migrate-composefs")]
#[command(about = "In-place migration utility from OSTree backend to ComposeFS backend", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Target bootable container image to migrate to (e.g., ghcr.io/projectbluefin/dakota:stable)
    #[arg(short, long)]
    target_image: Option<String>,

    /// Skip preflight validation checks (unrecommended, use with caution)
    #[arg(long)]
    skip_preflight: bool,

    /// Force migration even if warnings are encountered
    #[arg(short, long)]
    force: bool,

    /// Bootloader to use: "systemd-boot" (default, when UEFI), "grub2", or "auto"
    #[arg(long, default_value = "systemd-boot")]
    bootloader: String,

    /// Dry-run: print every action without executing
    #[arg(long)]
    dry_run: bool,

    /// Skip Phase 1 (OSTree object import)
    #[arg(long)]
    skip_import: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Commit the composefs deployment as the permanent default (after successful boot).
    #[command(name = "commit")]
    Commit,
}

fn check_root_privilege() -> Result<()> {
    let uid = unsafe { libc::getuid() };
    if uid != 0 {
        return Err(anyhow!("This command must be run as root (e.g., using sudo)."));
    }
    Ok(())
}

fn main() {
    let args = Args::parse();

    // Handle --commit subcommand (#8)
    if let Some(Command::Commit) = args.command {
        if let Err(e) = run_commit() {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
        return;
    }

    if let Err(e) = check_root_privilege() {
        eprintln!("Error: {}", e);
        process::exit(1);
    }

    let target_image = args.target_image.unwrap_or_else(|| {
        eprintln!("Error: --target-image is required for migration");
        process::exit(1);
    });

    // Fix 1: validate target_image to prevent INI injection in .origin file.
    if target_image.contains('\n') || target_image.contains('\r') || target_image.contains('\0') {
        eprintln!("Error: --target-image contains invalid characters (newlines, nulls).");
        process::exit(1);
    }

    println!("=== OSTree to ComposeFS Migration Utility ===");
    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }
    println!("Checking system state...");

    let report = match preflight::run_preflight_checks() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Preflight failure: {}", e);
            if !args.skip_preflight {
                process::exit(1);
            }
            preflight::PreflightReport {
                is_bootc_ostree: true,
                is_uefi: true,
                nvram_writable: true,
                esp_path: Some("/boot/efi".to_string()),
                esp_free_space_bytes: 500 * 1024 * 1024,
                esp_fs_type: Some("vfat".to_string()),
                supports_reflink: true,
                is_btrfs: true,
                ostree_repo_size_bytes: 0,
                composefs_free_bytes: 0,
                esp_ready_for_systemd_boot: true,
                systemd_boot_binaries_present: false,
                grub_tools_available: true,
                esp_detected: false,
                sysroot_was_ro: false,
            }
        }
    };

    // Output preflight details
    println!("  - Booted OSTree backend: {}", if report.is_bootc_ostree { "Yes" } else { "No" });
    println!("  - UEFI Boot Mode:        {}", if report.is_uefi { "Yes" } else { "No (Legacy BIOS)" });
    println!("  - NVRAM writable:        {}", if report.nvram_writable { "Yes" } else { "No" });
    println!("  - ESP Mounted Path:      {}", report.esp_path.as_deref().unwrap_or("None — GRUB2-only migration"));
    if let Some(ref fs) = report.esp_fs_type {
        println!("  - ESP Filesystem:        {}", fs);
    }
    println!("  - ESP Free Space:        {:.2} MB", report.esp_free_space_bytes as f64 / (1024.0 * 1024.0));
    println!("  - Btrfs Filesystem:      {}", if report.is_btrfs { "Yes" } else { "No" });
    if report.sysroot_was_ro {
        println!("  - /sysroot was RO:       Yes (remounted rw for reflink test)");
    }
    println!("  - Reflink (CoW) Support: {}", if report.supports_reflink { "Yes" } else { "No" });
    println!("  - OSTree repo size:      {:.2} GB", report.ostree_repo_size_bytes as f64 / 1e9);
    println!("  - ComposeFS free space:  {:.2} GB", report.composefs_free_bytes as f64 / 1e9);
    println!("  - GRUB tools available:  {}", if report.grub_tools_available { "Yes" } else { "No" });
    println!("  - ESP ready for sd-boot: {}", if report.esp_ready_for_systemd_boot { "Yes (>=150 MB)" } else { "No" });
    println!("  - systemd-boot binaries: {}", if report.systemd_boot_binaries_present { "Yes (/usr/lib/systemd/boot/efi)" } else { "No (bootctl install would fail)" });
    println!();

    // Migration readiness summary
    println!("=== Migration Readiness ===");
    let mut issues: Vec<&str> = Vec::new();
    if !report.is_bootc_ostree {
        issues.push("NOT booted in OSTree mode — migration requires an OSTree-booted system.");
    }
    if !report.is_uefi {
        issues.push("Legacy BIOS boot detected — systemd-boot unavailable; will stay on GRUB2.");
    }
    if report.is_uefi && !report.nvram_writable {
        issues.push("UEFI NVRAM not writable — efibootmgr may fail; systemd-boot may not register.");
    }
    if !report.esp_detected {
        issues.push("No ESP found — systemd-boot unavailable; will use GRUB2.");
    }
    if report.is_uefi && report.esp_path.is_some() && !report.esp_ready_for_systemd_boot {
        issues.push("ESP too small for systemd-boot — need >=150 MB free; will use GRUB2 instead.");
    }
    if report.is_uefi && !report.systemd_boot_binaries_present {
        issues.push("systemd-boot binaries missing in deployment — bootctl install would fail; will use GRUB2 instead.");
    }
    if !report.grub_tools_available {
        issues.push("No GRUB tools (grub2-reboot, grub2-editenv) — one-shot boot selection may fail.");
    }
    if !report.supports_reflink {
        issues.push("No reflink support — object copies will use 1.5× more disk space.");
    }
    let has_free_space = report.composefs_free_bytes as f64 > (report.ostree_repo_size_bytes as f64 * 1.5);
    if !has_free_space && report.ostree_repo_size_bytes > 0 {
        issues.push("Insufficient free space for migration — need >=1.5× repo size (without reflink).");
    }

    if issues.is_empty() {
        println!("  ✓ All preflight checks passed.");
    } else {
        for issue in &issues {
            println!("  ⚠ {}", issue);
        }
    }

    let use_systemd_boot = report.esp_ready_for_systemd_boot
        && report.nvram_writable
        && report.systemd_boot_binaries_present;
    if use_systemd_boot {
        println!("\nBootloader: Will migrate to systemd-boot (ESP ready, NVRAM writable).");
    } else if report.esp_path.is_some() {
        println!("\nBootloader: Will stay on GRUB2 (BLS Type 1).");
        if !report.grub_tools_available {
            println!("  WARNING: grub2-reboot not found. Boot selection may not work.");
            println!("  The composefs entry will be written but you may need to select it manually");
            println!("  from the GRUB menu on next boot.");
        }
    } else {
        println!("\nBootloader: Will stay on GRUB2 (BLS Type 1) — no ESP detected.");
    }

    // Validate requirements
    if !report.is_bootc_ostree && !args.force {
        eprintln!("Error: System is not booted into an OSTree deployment. Cannot perform migration.");
        process::exit(1);
    }

    if !report.supports_reflink && !args.force {
        println!("Warning: Reflink support not detected on /sysroot. Migration will perform a full copy of repository objects, which will require significant disk space.");
        print!("Do you want to proceed anyway? (y/N): ");
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            let input = input.trim().to_lowercase();
            if input != "y" && input != "yes" {
                println!("Migration aborted.");
                process::exit(0);
            }
        } else {
            println!("Migration aborted.");
            process::exit(0);
        }
    }

    println!("Starting migration to OCI image: {}...", target_image);
    if let Err(e) = migration::run_migration(&report, &target_image, args.dry_run, args.skip_import, &args.bootloader) {
        eprintln!("\nMigration Failed: {:#}", e);
        process::exit(1);
    }
}

/// Commit the composefs deployment as the permanent default (#8).
fn run_commit() -> Result<()> {
    check_root_privilege()?;
    println!("=== Committing composefs deployment as permanent default ===");

    // Fix 6: detect bootloader — check ESP entries for systemd-boot first.
    let esp_candidates = ["/boot/efi", "/efi"];
    let mut entries_dir = PathBuf::from("/boot/loader/entries");
    let mut is_systemd_boot = false;

    for esp in &esp_candidates {
        let esp_entries = Path::new(esp).join("loader/entries");
        if esp_entries.exists() {
            // Check if there are bootc_ entries on the ESP.
            if let Ok(mut rd) = std::fs::read_dir(&esp_entries) {
                if rd.any(|e| {
                    e.map(|en| en.file_name().to_string_lossy().starts_with("bootc_")).unwrap_or(false)
                }) {
                    entries_dir = esp_entries;
                    is_systemd_boot = true;
                    break;
                }
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

    if composefs_entries.is_empty() {
        if is_systemd_boot {
            println!("No composefs BLS entries found on ESP. Nothing to commit.");
            println!("Note: for systemd-boot, the composefs entry should already be the default if it has the lowest sort-key.");
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
        println!("Systemd-boot detected. The composefs entry '{}' should be the default via sort-key.", primary);
        println!("To make it permanent, ensure its sort-key is the lowest value in loader/entries/.");
        println!("Commit complete (no grub2-set-default needed for systemd-boot).");
    } else {
        let status = std::process::Command::new("grub2-set-default")
            .arg(primary)
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            anyhow::bail!("failed to set GRUB default");
        }
        println!("Composefs deployment '{}' is now the permanent default.", primary);
    }

    println!("You may now run 'bootc internals cleanup' to remove old OSTree deployments.");
    Ok(())
}
