//! Human-readable preflight reporting and migration gating shared by every
//! binary that drives the core pipeline (bootc-migrate, bootc-rebase).
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A report with every gate green — the baseline each test perturbs.
    fn healthy() -> PreflightReport {
        PreflightReport {
            is_bootc_ostree: true,
            pending_transaction: PendingTransactionStatus::Clean,
            is_uefi: true,
            nvram_writable: true,
            esp_path: Some("/boot/efi".into()),
            esp_free_space_bytes: 500 * 1024 * 1024,
            esp_fs_type: Some("vfat".into()),
            esp_detected: true,
            supports_reflink: true,
            is_btrfs: true,
            fs_type: Some("btrfs".into()),
            ostree_repo_size_bytes: 8_000_000_000,
            composefs_free_bytes: 33_000_000_000,
            esp_ready_for_systemd_boot: true,
            systemd_boot_binaries_present: true,
            grub_tools_available: true,
            sysroot_was_ro: false,
        }
    }

    #[test]
    fn healthy_report_proceeds_with_no_issues() {
        let r = healthy();
        assert!(readiness_issues(&r).is_empty());
        assert_eq!(gate(&r, false, false), MigrationGate::Proceed);
    }

    #[test]
    fn non_ostree_boot_is_refused_unless_forced() {
        let mut r = healthy();
        r.is_bootc_ostree = false;
        assert!(matches!(gate(&r, false, false), MigrationGate::Refuse(_)));
        // force overrides — the operator has taken responsibility.
        assert_eq!(gate(&r, true, false), MigrationGate::Proceed);
    }

    #[test]
    fn pending_transaction_is_refused_unless_waived() {
        for pending in [
            PendingTransactionStatus::StagedDeployment,
            PendingTransactionStatus::PendingDeployment,
            PendingTransactionStatus::StaleTransactionFiles,
        ] {
            let mut r = healthy();
            r.pending_transaction = pending.clone();
            assert!(
                matches!(gate(&r, false, false), MigrationGate::Refuse(_)),
                "{pending:?} must refuse"
            );
            // Either waiver flag lets it pass.
            assert_eq!(gate(&r, true, false), MigrationGate::Proceed);
            assert_eq!(gate(&r, false, true), MigrationGate::Proceed);
        }
    }

    #[test]
    fn refusal_message_names_the_pending_state_and_a_fix() {
        let mut r = healthy();
        r.pending_transaction = PendingTransactionStatus::StagedDeployment;
        let MigrationGate::Refuse(msg) = gate(&r, false, false) else {
            panic!("expected refusal");
        };
        assert!(msg.contains("Pending OSTree transaction"));
        assert!(msg.contains("undeploy"), "must tell the user how to fix it");
    }

    #[test]
    fn no_reflink_asks_for_confirmation_not_refusal() {
        let mut r = healthy();
        r.supports_reflink = false;
        assert_eq!(gate(&r, false, false), MigrationGate::ConfirmFullCopy);
        assert_eq!(gate(&r, true, false), MigrationGate::Proceed);
        // skip_preflight is NOT a full-copy consent — still asks.
        assert_eq!(gate(&r, false, true), MigrationGate::ConfirmFullCopy);
    }

    #[test]
    fn gate_order_refusal_beats_full_copy_confirmation() {
        // A non-ostree system without reflink must refuse, not ask about disk.
        let mut r = healthy();
        r.is_bootc_ostree = false;
        r.supports_reflink = false;
        assert!(matches!(gate(&r, false, false), MigrationGate::Refuse(_)));
    }

    #[test]
    fn issues_fire_per_condition() {
        let mut r = healthy();
        r.nvram_writable = false;
        r.supports_reflink = false;
        r.systemd_boot_binaries_present = false;
        let issues = readiness_issues(&r);
        assert!(issues.iter().any(|i| i.contains("NVRAM")));
        assert!(issues.iter().any(|i| i.contains("reflink")));
        assert!(issues.iter().any(|i| i.contains("systemd-boot binaries")));
        assert_eq!(issues.len(), 3, "no unexpected extra issues: {issues:?}");
    }

    #[test]
    fn tight_disk_without_reflink_warns_about_space() {
        let mut r = healthy();
        r.supports_reflink = false;
        r.composefs_free_bytes = r.ostree_repo_size_bytes; // < 1.5×
        assert!(
            readiness_issues(&r)
                .iter()
                .any(|i| i.contains("Insufficient free space"))
        );
    }
}
