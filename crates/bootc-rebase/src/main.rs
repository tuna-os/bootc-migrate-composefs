//! `bootc-rebase` — universal bootc re-base engine (scaffold).
//!
//! Consumes `bootc-migrate-core` to re-base a bootc system between backends,
//! bootloaders, and images. Today only the proven OSTree → ComposeFS route is
//! implemented (delegated to the core pipeline); the routing table in
//! [`routing`] tracks what's planned. See issues #30 and #45 in
//! tuna-os/bootc-migrate-composefs for the roadmap.

use anyhow::{Result, bail};
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
    let sys = bootc_migrate_core::preflight::SystemInfo::gather()?;
    if sys.is_bootc_ostree {
        Ok(Backend::Ostree)
    } else {
        // Not OSTree-booted; assume composefs (a later capability scan will
        // verify this properly — see issue #24).
        Ok(Backend::Composefs)
    }
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
        Strategy::CoreMigration => {
            // The OSTree → ComposeFS path is the bootc-migrate-composefs
            // binary's proven pipeline. Until the full flag surface
            // (bootloader choice, force, skip-import) is plumbed through
            // here, direct users to it rather than run a subset silently.
            bail!(
                "use the bootc-migrate-composefs binary for the {from} -> {to} migration \
                 (target image: {}); bootc-rebase will drive this route directly in a \
                 future release",
                args.target_image
            );
        }
        Strategy::ImageSwap | Strategy::OstreeDeploy => unreachable!("gated by implemented above"),
    }
}
