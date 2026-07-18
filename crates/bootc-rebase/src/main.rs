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
        Strategy::ImageSwap | Strategy::OstreeDeploy => unreachable!("gated by implemented above"),
    }
}
