use clap::Parser;
use anyhow::{Result, anyhow};
use std::process;

mod reflink;
mod preflight;
mod ostree;
mod composefs;
mod migration;

#[derive(Parser, Debug)]
#[command(name = "bootc-migrate-composefs")]
#[command(about = "In-place migration utility from OSTree backend to ComposeFS backend", long_about = None)]
struct Args {
    /// Target bootable container image to migrate to (e.g., ghcr.io/projectbluefin/dakota:stable)
    #[arg(short, long)]
    target_image: String,

    /// Skip preflight validation checks (unrecommended, use with caution)
    #[arg(long)]
    skip_preflight: bool,

    /// Force migration even if warnings are encountered
    #[arg(short, long)]
    force: bool,
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

    if let Err(e) = check_root_privilege() {
        eprintln!("Error: {}", e);
        process::exit(1);
    }

    println!("=== OSTree to ComposeFS Migration Utility ===");
    println!("Checking system state...");

    let report = match preflight::run_preflight_checks() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Preflight failure: {}", e);
            if !args.skip_preflight {
                process::exit(1);
            }
            // Create a default report if preflight skipped
            preflight::PreflightReport {
                is_bootc_ostree: true,
                is_uefi: true,
                esp_path: Some("/boot/efi".to_string()),
                esp_free_space_bytes: 500 * 1024 * 1024,
                supports_reflink: true,
                is_btrfs: true,
            }
        }
    };

    // Output preflight details
    println!("  - Booted OSTree backend: {}", if report.is_bootc_ostree { "Yes" } else { "No" });
    println!("  - UEFI Boot Mode:        {}", if report.is_uefi { "Yes" } else { "No" });
    println!("  - ESP Mounted Path:      {}", report.esp_path.as_deref().unwrap_or("None"));
    println!("  - ESP Free Space:        {:.2} MB", report.esp_free_space_bytes as f64 / (1024.0 * 1024.0));
    println!("  - Btrfs Filesystem:      {}", if report.is_btrfs { "Yes" } else { "No" });
    println!("  - Reflink (CoW) Support: {}", if report.supports_reflink { "Yes" } else { "No" });

    // Validate requirements
    if !report.is_bootc_ostree && !args.force {
        eprintln!("Error: System is not booted into an OSTree deployment. Cannot perform migration.");
        process::exit(1);
    }

    let use_systemd_boot = report.is_uefi && report.esp_path.is_some() && report.esp_free_space_bytes >= 300 * 1024 * 1024;
    if use_systemd_boot {
        println!("System matches systemd-boot criteria. Will migrate bootloader to systemd-boot.");
    } else {
        println!("Warning: System does not match systemd-boot criteria (Legacy boot, small/missing ESP).");
        println!("Staying on GRUB2 bootloader (BLS Type 1).");
        if !args.force && report.is_uefi && report.esp_path.is_some() {
            println!("Proceeding using GRUB2 fallback.");
        }
    }

    if !report.supports_reflink {
        println!("Warning: Reflink support not detected on /sysroot. Migration will perform a full copy of repository objects, which will require significant disk space.");
        if !args.force {
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
    }

    println!("Starting migration to OCI image: {}...", args.target_image);
    if let Err(e) = migration::run_migration(&report, &args.target_image) {
        eprintln!("\nMigration Failed: {:#}", e);
        process::exit(1);
    }
}
