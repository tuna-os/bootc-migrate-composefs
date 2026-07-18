//! Human-readable preflight reporting and migration gating shared by every
//! binary that drives the core pipeline (bootc-migrate-composefs, bootc-rebase).
//!
//! Split from the migrator binary so output stays identical across drivers:
//! [`print_report`] and [`print_readiness`] emit the exact preflight summary
//! the E2E suite and users have always seen; [`gate`] encodes the go/no-go
//! rules (`--force` / `--skip-preflight` semantics included).

use super::{PendingTransactionStatus, PreflightReport};

/// Print the detailed preflight report ("  - ..." lines).
pub fn print_report(report: &PreflightReport) {
    println!(
        "  - Booted OSTree backend: {}",
        if report.is_bootc_ostree { "Yes" } else { "No" }
    );
    match report.pending_transaction {
        PendingTransactionStatus::Clean => {}
        ref other => println!(
            "  ⚠ Pending OSTree transaction: {} — aborting (run `ostree admin undeploy` or complete the update first)",
            other
        ),
    }
    println!(
        "  - UEFI Boot Mode:        {}",
        if report.is_uefi {
            "Yes"
        } else {
            "No (Legacy BIOS)"
        }
    );
    println!(
        "  - NVRAM writable:        {}",
        if report.nvram_writable { "Yes" } else { "No" }
    );
    println!(
        "  - ESP Mounted Path:      {}",
        report
            .esp_path
            .as_deref()
            .unwrap_or("None — GRUB2-only migration")
    );
    if let Some(ref fs) = report.esp_fs_type {
        println!("  - ESP Filesystem:        {}", fs);
    }
    println!(
        "  - ESP Free Space:        {:.2} MB",
        report.esp_free_space_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "  - Filesystem:            {}",
        report.fs_type.as_deref().unwrap_or("unknown")
    );
    println!(
        "  - Btrfs Filesystem:      {}",
        if report.is_btrfs { "Yes" } else { "No" }
    );
    if report.sysroot_was_ro {
        println!("  - /sysroot was RO:       Yes (remounted rw for reflink test)");
    }
    println!(
        "  - Reflink (CoW) Support: {}",
        if report.supports_reflink { "Yes" } else { "No" }
    );
    println!(
        "  - OSTree repo size:      {:.2} GB",
        report.ostree_repo_size_bytes as f64 / 1e9
    );
    println!(
        "  - ComposeFS free space:  {:.2} GB",
        report.composefs_free_bytes as f64 / 1e9
    );
    println!(
        "  - GRUB tools available:  {}",
        if report.grub_tools_available {
            "Yes"
        } else {
            "No"
        }
    );
    println!(
        "  - ESP ready for sd-boot: {}",
        if report.esp_ready_for_systemd_boot {
            "Yes (>=150 MB)"
        } else {
            "No"
        }
    );
    println!(
        "  - systemd-boot binaries: {}",
        if report.systemd_boot_binaries_present {
            "Yes (/usr/lib/systemd/boot/efi)"
        } else {
            "No (bootctl install would fail)"
        }
    );
    println!();
}

/// Compute the readiness warnings for this system. Empty means all clear.
pub fn readiness_issues(report: &PreflightReport) -> Vec<&'static str> {
    let mut issues: Vec<&'static str> = Vec::new();
    if !report.is_bootc_ostree {
        issues.push("NOT booted in OSTree mode — migration requires an OSTree-booted system.");
    }
    if !report.is_uefi {
        issues.push("Legacy BIOS boot detected — systemd-boot unavailable; will stay on GRUB2.");
    }
    if report.is_uefi && !report.nvram_writable {
        issues
            .push("UEFI NVRAM not writable — efibootmgr may fail; systemd-boot may not register.");
    }
    if !report.esp_detected {
        issues.push("No ESP found — systemd-boot unavailable; will use GRUB2.");
    }
    if report.is_uefi && report.esp_path.is_some() && !report.esp_ready_for_systemd_boot {
        issues.push("ESP too small for systemd-boot — need >=150 MB free; will use GRUB2 instead.");
    }
    if report.is_uefi && !report.systemd_boot_binaries_present {
        issues.push("systemd-boot binaries missing in source OS — migration will extract them from the target image instead.");
    }
    if !report.grub_tools_available {
        issues.push(
            "No GRUB tools (grub2-reboot, grub2-editenv) — one-shot boot selection may fail.",
        );
    }
    if !report.supports_reflink {
        issues.push("No reflink support — object copies will use 1.5× more disk space.");
    }
    let has_free_space =
        report.composefs_free_bytes as f64 > (report.ostree_repo_size_bytes as f64 * 1.5);
    if !has_free_space && report.ostree_repo_size_bytes > 0 {
        issues.push(
            "Insufficient free space for migration — need >=1.5× repo size (without reflink).",
        );
    }
    issues
}

/// Print the readiness summary and the bootloader plan.
pub fn print_readiness(report: &PreflightReport) {
    println!("=== Migration Readiness ===");
    let issues = readiness_issues(report);
    if issues.is_empty() {
        println!("  ✓ All preflight checks passed.");
    } else {
        for issue in &issues {
            println!("  ⚠ {}", issue);
        }
    }

    // We migrate to systemd-boot by lifting the loader binary out of the target image,
    // so the source OS no longer needs to ship systemd-boot. The systemd_boot_binaries_present
    // field is now purely informational (warning if neither side ships it).
    let use_systemd_boot = report.esp_ready_for_systemd_boot && report.nvram_writable;
    if use_systemd_boot {
        println!("\nBootloader: Will migrate to systemd-boot (ESP ready, NVRAM writable).");
    } else if report.esp_path.is_some() {
        println!("\nBootloader: Will stay on GRUB2 (BLS Type 1).");
        if !report.grub_tools_available {
            println!("  WARNING: grub2-reboot not found. Boot selection may not work.");
            println!(
                "  The composefs entry will be written but you may need to select it manually"
            );
            println!("  from the GRUB menu on next boot.");
        }
    } else {
        println!("\nBootloader: Will stay on GRUB2 (BLS Type 1) — no ESP detected.");
    }
}

/// Go/no-go decision for starting the migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationGate {
    /// All gates passed — start the migration.
    Proceed,
    /// No reflink support: the driver should get explicit confirmation (an
    /// interactive prompt or `--force`) before a full-copy migration.
    ConfirmFullCopy,
    /// Hard refusal with the reason to show the user.
    Refuse(String),
}

/// Evaluate the migration gates in order. `force` overrides everything except
/// nothing; `skip_preflight` additionally waives the pending-transaction gate.
pub fn gate(report: &PreflightReport, force: bool, skip_preflight: bool) -> MigrationGate {
    if !report.is_bootc_ostree && !force {
        return MigrationGate::Refuse(
            "System is not booted into an OSTree deployment. Cannot perform migration.".to_string(),
        );
    }
    // Block on pending transactions — they cause incomplete composefs images
    // and switch-root-os-release-errors on next boot.
    if report.pending_transaction != PendingTransactionStatus::Clean && !force && !skip_preflight {
        return MigrationGate::Refuse(format!(
            "Pending OSTree transaction detected: {}.\n\
             The OSTree repo has uncommitted state from a previous update. The migration\n\
             would produce an incomplete composefs image that cannot boot.\n\
             \n\
             To resolve:\n\
               - If you ran `bootc upgrade` or `rpm-ostree upgrade`, complete it first.\n\
               - If the update was interrupted, run `ostree admin undeploy <index>`\n\
                 to remove the pending deployment.\n\
               - Or run `bootc upgrade` to finish/finalize the pending transaction.\n",
            report.pending_transaction
        ));
    }
    if !report.supports_reflink && !force {
        return MigrationGate::ConfirmFullCopy;
    }
    MigrationGate::Proceed
}
