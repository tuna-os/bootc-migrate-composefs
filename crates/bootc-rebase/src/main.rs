//! `bootc-rebase` — universal bootc re-base engine.
//!
//! Consumes `bootc-migrate-core` to re-base a bootc system between backends,
//! bootloaders, and images. Today the OSTree → ComposeFS route drives the
//! core pipeline directly; the routing table in [`routing`] tracks what else
//! is planned. See issues #30 and #45 in tuna-os/bootc-migrate-composefs for
//! the roadmap.

use anyhow::{Result, bail};
use bootc_migrate_core::migration;
use bootc_migrate_core::preflight::{self, readiness};
use clap::Parser;

mod routing;

use routing::{Backend, Strategy, route};

#[derive(Parser, Debug)]
#[command(name = "bootc-rebase")]
#[command(about = "Re-base a bootc system between backends, bootloaders, and images", long_about = None)]
struct Args {
    /// Target bootable container image (e.g. ghcr.io/projectbluefin/dakota:stable)
    #[arg(short, long)]
    target_image: String,

    /// Source backend: "auto" (detect), "ostree", or "composefs"
    #[arg(long, default_value = "auto")]
    source_backend: String,

    /// Target backend: "ostree" or "composefs"
    #[arg(long, default_value = "composefs")]
    target_backend: String,

    /// Bootloader to use: "systemd-boot" (default, when UEFI), "grub2", or "auto"
    #[arg(long, default_value = "systemd-boot")]
    bootloader: String,

    /// Force the re-base even if readiness warnings are encountered
    #[arg(short, long)]
    force: bool,

    /// Skip preflight validation checks (unrecommended, use with caution)
    #[arg(long)]
    skip_preflight: bool,

    /// Skip OSTree object import (phase 1)
    #[arg(long)]
    skip_import: bool,

    /// Dry-run: print every action without executing
    #[arg(long)]
    dry_run: bool,

    /// Print the planned route and exit without touching the system
    #[arg(long)]
    plan: bool,
}

fn parse_backend(s: &str) -> Result<Backend> {
    match s {
        "ostree" => Ok(Backend::Ostree),
        "composefs" => Ok(Backend::Composefs),
        other => bail!("unknown backend '{other}' (expected 'ostree' or 'composefs')"),
    }
}

fn detect_source_backend() -> Result<Backend> {
    let sys = preflight::SystemInfo::gather()?;
    if sys.is_bootc_ostree {
        Ok(Backend::Ostree)
    } else {
        // Not OSTree-booted; assume composefs (a later capability scan will
        // verify this properly — see issue #24).
        Ok(Backend::Composefs)
    }
}

fn check_root_privilege() -> Result<()> {
    if !rustix::process::getuid().is_root() {
        bail!("This command must be run as root (e.g., using sudo).");
    }
    Ok(())
}

/// Drive the proven OSTree → ComposeFS pipeline from bootc-migrate-core:
/// preflight, readiness report, gating, then the phase 0–5 migration.
fn run_core_migration(args: &Args) -> Result<()> {
    check_root_privilege()?;

    // Validate target_image to prevent INI injection in the .origin file.
    if args.target_image.contains('\n')
        || args.target_image.contains('\r')
        || args.target_image.contains('\0')
    {
        bail!("--target-image contains invalid characters (newlines, nulls).");
    }

    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }
    println!("Checking system state...");

    let report = preflight::run_preflight_checks()?;
    readiness::print_report(&report);
    readiness::print_readiness(&report);

    match readiness::gate(&report, args.force, args.skip_preflight) {
        readiness::MigrationGate::Proceed => {}
        readiness::MigrationGate::Refuse(reason) => bail!("{reason}"),
        readiness::MigrationGate::ConfirmFullCopy => {
            // bootc-rebase is non-interactive by design: no prompt, just a
            // clear instruction (the migrator binary offers the y/N prompt).
            bail!(
                "Reflink support not detected on /sysroot — the migration would perform a \
                 full copy of repository objects. Re-run with --force to accept the extra \
                 disk usage."
            );
        }
    }

    println!("Starting migration to OCI image: {}...", args.target_image);
    migration::run_migration(
        &report,
        &args.target_image,
        args.dry_run,
        args.skip_import,
        &args.bootloader,
        args.force,
    )
}

fn main() -> Result<()> {
    let args = Args::parse();

    let from = if args.source_backend == "auto" {
        detect_source_backend()?
    } else {
        parse_backend(&args.source_backend)?
    };
    let to = parse_backend(&args.target_backend)?;

    let Some(r) = route(from, to) else {
        bail!("no route from {from} to {to}");
    };

    println!(
        "Route: {from} -> {to} via {:?} ({})",
        r.strategy,
        if r.implemented {
            "implemented"
        } else {
            "planned, not yet implemented"
        }
    );

    if args.plan {
        return Ok(());
    }

    if !r.implemented {
        bail!(
            "the {from} -> {to} route is not implemented yet; \
             see https://github.com/tuna-os/bootc-migrate-composefs/issues/30"
        );
    }

    match r.strategy {
        Strategy::CoreMigration => run_core_migration(&args),
        Strategy::OstreeDeploy => run_ostree_deploy(&args),
        Strategy::ImageSwap => unreachable!("gated by implemented above"),
    }
}

/// Scenario A (issue #30): re-base to another image as a plain OSTree
/// deployment. `bootc switch` already does the heavy lifting on an
/// OSTree-backed system — staging the target with OSTree's native 3-way /etc
/// merge and shared /var — so this route is preflight + gating + `bootc
/// switch` + verification. The previous deployment stays as the rollback
/// entry, matching the engine's two-phase contract.
///
/// Bootloader: per the decision on issue #64, this route will migrate to
/// systemd-boot when the system is ready — wired in once #65's audited
/// bootloader entry point lands. Until then the current bootloader is kept.
fn run_ostree_deploy(args: &Args) -> Result<()> {
    check_root_privilege()?;

    if args.target_image.contains('\n')
        || args.target_image.contains('\r')
        || args.target_image.contains('\0')
    {
        bail!("--target-image contains invalid characters (newlines, nulls).");
    }

    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }
    println!("Checking system state...");
    let report = preflight::run_preflight_checks()?;

    if !report.is_bootc_ostree && !args.force {
        bail!("System is not booted into an OSTree deployment. Cannot perform an ostree re-base.");
    }
    if report.pending_transaction != preflight::PendingTransactionStatus::Clean
        && !args.force
        && !args.skip_preflight
    {
        bail!(
            "Pending OSTree transaction detected: {}. Complete or undeploy it first \
             (see `ostree admin status`).",
            report.pending_transaction
        );
    }
    if report.esp_ready_for_systemd_boot && report.nvram_writable {
        println!(
            "Note: system is ready for systemd-boot; bootloader migration will be \
             integrated into this route via the migrate-bootloader work (#65). \
             Keeping the current bootloader for this re-base."
        );
    }

    if args.dry_run {
        println!("[DRY RUN] Would run: bootc switch {}", args.target_image);
        return Ok(());
    }

    println!(
        "Staging OSTree deployment of {} via `bootc switch`...",
        args.target_image
    );
    let status = std::process::Command::new("bootc")
        .args(["switch", &args.target_image])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to execute bootc switch: {e}"))?;
    if !status.success() {
        bail!("bootc switch {} failed (exit {status})", args.target_image);
    }

    // Verify the switch actually staged a deployment for the target image.
    let out = std::process::Command::new("bootc")
        .args(["status", "--json"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to execute bootc status: {e}"))?;
    if !out.status.success() {
        bail!(
            "bootc status failed after switch: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow::anyhow!("parsing bootc status json: {e}"))?;
    let staged_image = json
        .pointer("/status/staged/image/image/image")
        .and_then(|v| v.as_str());
    match staged_image {
        Some(img)
            if args.target_image.contains(img)
                || img.contains(args.target_image.trim_start_matches("docker://")) =>
        {
            println!("Staged deployment verified: {img}");
        }
        Some(img) => bail!(
            "bootc switch staged '{img}' but the requested target was '{}'",
            args.target_image
        ),
        None => bail!("no staged deployment found after bootc switch"),
    }

    println!(
        "Re-base staged. Reboot to enter the new deployment; the previous \
         deployment remains in the boot menu as rollback."
    );
    Ok(())
}
