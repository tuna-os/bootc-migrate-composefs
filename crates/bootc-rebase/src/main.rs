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
use clap::{Parser, Subcommand};

mod routing;

use routing::{Backend, Strategy, route};

#[derive(Parser, Debug)]
#[command(name = "bootc-rebase")]
#[command(about = "Re-base a bootc system between backends, bootloaders, and images", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[command(flatten)]
    rebase_args: Args,
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// Inspect target container image capabilities via registry streaming
    Scan(ScanArgs),
    /// Re-base system
    Rebase(Args),
}

#[derive(clap::Args, Debug, Clone)]
struct ScanArgs {
    /// Target container image to scan (e.g. ghcr.io/projectbluefin/dakota:stable)
    image: String,

    /// Output capabilities as machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct Args {
    /// Target bootable container image (e.g. ghcr.io/projectbluefin/dakota:stable)
    #[arg(short, long, default_value = "")]
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

fn print_capabilities_table(image: &str, caps: &bootc_migrate_core::scan::Capabilities) {
    println!("=== Target image capabilities ===");
    println!("Image:                 {image}");
    println!(
        "Composefs:             {}",
        if caps.composefs_capable {
            "capable"
        } else {
            "not enabled in prepare-root.conf"
        }
    );
    println!(
        "OSTree capable:        {}",
        if caps.ostree_capable { "yes" } else { "no" }
    );
    println!(
        "Bootloader payload:    {}",
        if caps.systemd_boot_payload {
            "systemd-boot ✓"
        } else {
            "none"
        }
    );
    println!(
        "bootc present:         {}",
        if caps.bootc_present { "yes" } else { "no" }
    );
    println!(
        "Desktops:              {}",
        if caps.desktops.is_empty() {
            "none".to_string()
        } else {
            caps.desktops.join(", ")
        }
    );
    if let Some(base) = &caps.base {
        println!(
            "Base OS:               {} {}",
            base.id,
            base.version_id.as_deref().unwrap_or("")
        );
    } else {
        println!("Base OS:               unknown");
    }
    println!(
        "Sysusers:              {} static allocation(s)",
        caps.sysusers.len()
    );
    println!(
        "Compatible:            {}",
        if caps.ostree_capable || caps.composefs_capable {
            "YES"
        } else {
            "NO"
        }
    );
}

fn run_scan(args: &ScanArgs) -> Result<()> {
    println!("Scanning target image {}...", args.image);
    let caps = bootc_migrate_core::scan::scan_target_image(&args.image)?;
    if args.json {
        println!("{}", caps.to_json());
    } else {
        print_capabilities_table(&args.image, &caps);
    }
    Ok(())
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

fn execute_rebase(args: &Args) -> Result<()> {
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

    if to == Backend::Composefs
        && !args.skip_preflight
        && let Ok(caps) = bootc_migrate_core::scan::scan_target_image(&args.target_image)
        && !caps.composefs_capable
        && !args.force
    {
        bail!(
            "Target image {} is not composefs-capable (prepare-root.conf lacks composefs enabled). \
             Use --force to override.",
            args.target_image
        );
    }

    match r.strategy {
        Strategy::CoreMigration => run_core_migration(args),
        Strategy::OstreeDeploy => run_ostree_deploy(args),
        Strategy::ImageSwap => run_image_swap(args),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Scan(ref scan_args)) => run_scan(scan_args),
        Some(Commands::Rebase(ref rebase_args)) => {
            if rebase_args.target_image.is_empty() {
                bail!("--target-image (-t) is required for re-base.");
            }
            execute_rebase(rebase_args)
        }
        None => {
            if cli.rebase_args.target_image.is_empty() {
                bail!(
                    "--target-image (-t) is required for re-base. Run `bootc-rebase --help` or `bootc-rebase scan <image>`."
                );
            }
            execute_rebase(&cli.rebase_args)
        }
    }
}

/// Reject target images whose characters would corrupt the `.origin` ini.
fn validate_target_image(target_image: &str) -> Result<()> {
    if target_image.contains('\n') || target_image.contains('\r') || target_image.contains('\0') {
        bail!("--target-image contains invalid characters (newlines, nulls).");
    }
    Ok(())
}

/// Stage `target_image` with `bootc switch` and verify via `bootc status
/// --json` that the staged deployment is exactly the requested image. Shared
/// by the OstreeDeploy and ImageSwap strategies — on both backends, `bootc
/// switch` performs the native staging (3-way /etc merge, shared /var) and
/// leaves the previous deployment as the rollback entry.
fn stage_via_bootc_switch(target_image: &str) -> Result<()> {
    println!("Staging deployment of {target_image} via `bootc switch`...");
    let status = std::process::Command::new("bootc")
        .args(["switch", target_image])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to execute bootc switch: {e}"))?;
    if !status.success() {
        bail!("bootc switch {target_image} failed (exit {status})");
    }

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
    match staged_image_from_status(&json) {
        Some(img) if staged_image_matches(target_image, img) => {
            println!("Staged deployment verified: {img}");
            Ok(())
        }
        Some(img) => {
            bail!("bootc switch staged '{img}' but the requested target was '{target_image}'")
        }
        None => bail!("no staged deployment found after bootc switch"),
    }
}

/// Scenario A' (issue #66): swap the image on a composefs-backed system —
/// no backend conversion. `bootc switch` stages the target natively; this
/// route is gating + switch + verification. The degenerate direct-store path
/// (for targets whose bootc cannot switch) is out of scope until the #13
/// store-selection work lands.
fn run_image_swap(args: &Args) -> Result<()> {
    check_root_privilege()?;
    validate_target_image(&args.target_image)?;

    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }
    println!("Checking system state...");

    // The booted deployment must actually be composefs-backed: the router
    // may have been told --source-backend composefs explicitly, but staging
    // relies on the running bootc's composefs support.
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !cmdline.contains("composefs=") && !args.force {
        bail!(
            "System is not booted from a composefs deployment (/proc/cmdline has no \
             composefs= parameter). Use --force to override, or re-run with \
             --source-backend auto."
        );
    }

    if args.dry_run {
        println!("[DRY RUN] Would run: bootc switch {}", args.target_image);
        return Ok(());
    }

    stage_via_bootc_switch(&args.target_image)?;

    println!(
        "Image swap staged. Reboot to enter the new deployment; the previous \
         deployment remains in the boot menu as rollback."
    );
    Ok(())
}

/// The staged deployment's image spec from `bootc status --json`, if any.
/// (Schema: `.status.staged.image.image.image` — ImageStatus → ImageReference
/// → image spec string; stable across bootc 1.x.)
fn staged_image_from_status(status: &serde_json::Value) -> Option<&str> {
    status
        .pointer("/status/staged/image/image/image")
        .and_then(|v| v.as_str())
}

/// Whether the image bootc reports as staged is the one the user asked for.
///
/// Compares by equality after stripping the transport prefix from both sides
/// (`docker://`, `ostree-unverified-registry:`, …) — bootc's status output
/// omits the transport the user may have typed. Deliberately NOT a substring
/// match: `bluefin:gts-testing` must not "verify" a request for
/// `bluefin:gts`.
fn staged_image_matches(requested: &str, staged: &str) -> bool {
    fn strip_transport(image: &str) -> &str {
        // `scheme://rest` transports first, then the `prefix:name` transports
        // whose remainder still contains a registry path (so a plain
        // `registry/image:tag` — whose only ':' precedes the tag — survives).
        if let Some((_, rest)) = image.split_once("://") {
            return rest;
        }
        for prefix in [
            "ostree-unverified-registry:",
            "ostree-image-signed:",
            "ostree-remote-registry:",
            "containers-storage:",
            "registry:",
        ] {
            if let Some(rest) = image.strip_prefix(prefix) {
                return rest;
            }
        }
        image
    }
    strip_transport(requested) == strip_transport(staged)
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
    validate_target_image(&args.target_image)?;

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

    stage_via_bootc_switch(&args.target_image)?;

    println!(
        "Re-base staged. Reboot to enter the new deployment; the previous \
         deployment remains in the boot menu as rollback."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_match_exact() {
        assert!(staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
    }

    #[test]
    fn staged_match_strips_requested_transport() {
        assert!(staged_image_matches(
            "docker://ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
        assert!(staged_image_matches(
            "ostree-unverified-registry:ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
    }

    #[test]
    fn staged_match_rejects_tag_extension() {
        // The old substring check accepted this: gts-testing contains gts.
        assert!(!staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts-testing"
        ));
        assert!(!staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts-testing",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
    }

    #[test]
    fn staged_match_rejects_different_image() {
        assert!(!staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/dakota:stable"
        ));
    }

    #[test]
    fn staged_match_plain_tag_colon_survives_transport_strip() {
        // A bare registry/image:tag has a ':' but no transport — it must not
        // get mangled by the prefix stripping.
        assert!(staged_image_matches(
            "quay.io/fedora/fedora-bootc:42",
            "quay.io/fedora/fedora-bootc:42"
        ));
    }

    #[test]
    fn staged_image_extracted_from_status_json() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"status":{"staged":{"image":{"image":{"image":"ghcr.io/projectbluefin/bluefin:gts","transport":"registry"}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            staged_image_from_status(&json),
            Some("ghcr.io/projectbluefin/bluefin:gts")
        );
    }

    #[test]
    fn staged_image_absent_when_nothing_staged() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"status":{"staged":null,"booted":{}}}"#).unwrap();
        assert_eq!(staged_image_from_status(&json), None);
    }

    #[test]
    fn parse_backend_accepts_known_and_rejects_unknown() {
        assert!(matches!(parse_backend("ostree"), Ok(Backend::Ostree)));
        assert!(matches!(parse_backend("composefs"), Ok(Backend::Composefs)));
        assert!(parse_backend("btrfs").is_err());
        assert!(parse_backend("").is_err());
    }

    #[test]
    fn test_scan_subcommand_parsing() {
        let cli = Cli::parse_from([
            "bootc-rebase",
            "scan",
            "ghcr.io/projectbluefin/dakota:stable",
            "--json",
        ]);
        match cli.command {
            Some(Commands::Scan(args)) => {
                assert_eq!(args.image, "ghcr.io/projectbluefin/dakota:stable");
                assert!(args.json);
            }
            _ => panic!("expected Commands::Scan"),
        }
    }

    #[test]
    fn test_rebase_subcommand_parsing() {
        let cli = Cli::parse_from([
            "bootc-rebase",
            "-t",
            "ghcr.io/projectbluefin/dakota:stable",
            "--plan",
        ]);
        assert!(cli.command.is_none());
        assert_eq!(
            cli.rebase_args.target_image,
            "ghcr.io/projectbluefin/dakota:stable"
        );
        assert!(cli.rebase_args.plan);

        let cli = Cli::parse_from([
            "bootc-rebase",
            "rebase",
            "-t",
            "ghcr.io/projectbluefin/dakota:stable",
        ]);
        match cli.command {
            Some(Commands::Rebase(args)) => {
                assert_eq!(args.target_image, "ghcr.io/projectbluefin/dakota:stable");
            }
            _ => panic!("expected Commands::Rebase"),
        }
    }
}
