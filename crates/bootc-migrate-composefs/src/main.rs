use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use std::process;

use bootc_migrate_core::{migration, preflight, transaction};

mod tui;

#[derive(Parser, Debug)]
#[command(name = "bootc-migrate-composefs")]
#[command(about = "In-place migration utility from OSTree backend to ComposeFS backend", long_about = None)]
#[command(version = env!("BUILD_GIT_HASH"))]
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
    ///
    /// Permanently deletes the OSTree-Bluefin deployment from disk: removes
    /// /sysroot/ostree (object store + deploys + leaked /var copy), drops
    /// stale /boot/loader/entries/ostree-*.conf, removes GRUB2 bits when
    /// migrated to systemd-boot, refreshes /sysroot/.bootc-aleph.json.
    /// The composefs system becomes byte-shape identical to a fresh
    /// `bootc install` of the target image.
    #[command(name = "commit")]
    Commit {
        /// Preview deletions and reclaimed bytes; touch nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Undo a partial or failed migration — remove composefs boot artifacts
    /// and staged deployments while preserving the composefs object store.
    ///
    /// Removes staged deployments, boot artifacts, BLS entries from ESP.
    /// Does NOT touch the composefs object store or loopback image — those
    /// are expensive to rebuild and survive across retries. Use --full for
    /// complete cleanup including the object store.
    #[command(name = "undo")]
    Undo {
        /// Preview what would be removed; touch nothing.
        #[arg(long)]
        dry_run: bool,
        /// Full cleanup: also remove composefs object store and loopback image.
        #[arg(long)]
        full: bool,
    },
    /// Launch the interactive TUI wizard.
    #[command(name = "tui")]
    Tui,
}

fn check_root_privilege() -> Result<()> {
    if !rustix::process::getuid().is_root() {
        return Err(anyhow!(
            "This command must be run as root (e.g., using sudo)."
        ));
    }
    Ok(())
}

/// Redirect this process's stdout/stderr through a pipe to a background thread
/// that fans every chunk out to both the real terminal and `log_file`.
///
/// Best-effort: returns an error if the pipe/dup setup fails, in which case the
/// caller proceeds without persistent logging.
/// Holds the tee thread + a copy of the real stdout. Call [`TeeGuard::finish`]
/// before the process exits so short-lived commands (`commit --dry-run`) don't
/// lose their stdout: the thread only sees EOF once every writer of the pipe is
/// closed, which on a fast exit races process teardown.
#[derive(Debug)]
struct TeeGuard {
    handle: std::thread::JoinHandle<()>,
    real_stdout: rustix::fd::OwnedFd,
}

impl TeeGuard {
    /// Flush, restore the real stdout/stderr (closing the pipe so the tee thread
    /// sees EOF), and wait for the thread to drain everything to stdout + log.
    fn finish(self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        let _ = rustix::stdio::dup2_stdout(&self.real_stdout);
        let _ = rustix::stdio::dup2_stderr(&self.real_stdout);
        let _ = self.handle.join();
    }
}

fn tee_stdio_to_log(log_file: std::fs::File) -> rustix::io::Result<TeeGuard> {
    use std::io::{Read, Write};

    let (pipe_read, pipe_write) = rustix::pipe::pipe()?;
    // One dup for the tee thread to reach the terminal, one kept by the guard to
    // restore fd 1/2 on shutdown (which closes the pipe and unblocks the thread).
    let thread_stdout = rustix::io::dup(rustix::stdio::stdout())?;
    let real_stdout = rustix::io::dup(rustix::stdio::stdout())?;

    let handle = std::thread::spawn(move || {
        let mut reader = std::fs::File::from(pipe_read);
        let mut stdout = std::fs::File::from(thread_stdout);
        let mut log = log_file;
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let _ = log.write_all(&buf[..n]);
            let _ = stdout.write_all(&buf[..n]);
        }
        let _ = log.flush();
        let _ = stdout.flush();
    });

    rustix::stdio::dup2_stdout(&pipe_write)?;
    rustix::stdio::dup2_stderr(&pipe_write)?;
    // Dropping our copy of the write end leaves only the redirected stdout/stderr
    // referencing it, so the tee thread sees EOF once those close (process exit
    // or TeeGuard::finish).
    drop(pipe_write);
    Ok(TeeGuard {
        handle,
        real_stdout,
    })
}

fn main() {
    let args = Args::parse();

    // Open persistent log file — all migration output is tee'd here so the
    // user can inspect results even if the terminal session is lost.
    let log_path = "/var/log/bootc-migrate-composefs.log";
    let log_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(f) => {
            eprintln!("Logging migration output to {}", log_path);
            Some(f)
        }
        Err(e) => {
            eprintln!("Warning: could not open log file {}: {}", log_path, e);
            None
        }
    };

    // Tee stdout+stderr to the log file via a pipe so output is visible both
    // on the terminal (over SSH for E2E) and in the persistent log.
    let mut tee_guard = log_file.and_then(|f| tee_stdio_to_log(f).ok());

    // Drain the tee thread (flushing all buffered output to terminal + log)
    // then exit. process::exit() skips Rust destructors, so without this
    // the last few lines of output (including the error message) are lost.
    macro_rules! exit_flushed {
        ($code:expr) => {{
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            if let Some(g) = tee_guard.take() {
                g.finish();
            }
            process::exit($code);
        }};
    }

    // Handle --commit subcommand
    if let Some(Command::Commit { dry_run }) = args.command {
        let result = run_commit(dry_run);
        if let Err(e) = result {
            eprintln!("Error: {}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    // Handle --undo subcommand
    if let Some(Command::Undo { dry_run, full }) = args.command {
        let result = run_undo(dry_run, full);
        if let Err(e) = result {
            eprintln!("Error: {}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    // Handle explicit `tui` subcommand, or fall into the wizard automatically
    // when no target image was given on the command line. Root isn't required
    // just to browse the wizard — the migration subprocess it spawns on Run
    // enforces that itself.
    if matches!(args.command, Some(Command::Tui)) || args.target_image.is_none() {
        let result = tui::run_tui();
        if let Err(e) = result {
            eprintln!("Error: {}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    if let Err(e) = check_root_privilege() {
        eprintln!("Error: {}", e);
        exit_flushed!(1);
    }

    let target_image = match args.target_image {
        Some(t) => t,
        None => {
            eprintln!("Error: --target-image is required for migration");
            exit_flushed!(1);
        }
    };

    // Validate target_image to prevent INI injection in the .origin file.
    if target_image.contains('\n') || target_image.contains('\r') || target_image.contains('\0') {
        eprintln!("Error: --target-image contains invalid characters (newlines, nulls).");
        exit_flushed!(1);
    }

    let version = env!("BUILD_GIT_HASH");
    println!("=== OSTree to ComposeFS Migration Utility v{} ===", version);
    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }
    println!("Checking system state...");

    let report = match preflight::run_preflight_checks() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Preflight failure: {}", e);
            if !args.skip_preflight {
                exit_flushed!(1);
            }
            preflight::PreflightReport {
                is_bootc_ostree: true,
                pending_transaction: preflight::PendingTransactionStatus::Clean,
                is_uefi: true,
                nvram_writable: true,
                esp_path: Some("/boot/efi".to_string()),
                esp_free_space_bytes: 500 * 1024 * 1024,
                esp_fs_type: Some("vfat".to_string()),
                supports_reflink: true,
                is_btrfs: true,
                fs_type: Some("btrfs".to_string()),
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
    println!(
        "  - Booted OSTree backend: {}",
        if report.is_bootc_ostree { "Yes" } else { "No" }
    );
    match report.pending_transaction {
        preflight::PendingTransactionStatus::Clean => {}
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

    // Validate requirements
    if !report.is_bootc_ostree && !args.force {
        eprintln!(
            "Error: System is not booted into an OSTree deployment. Cannot perform migration."
        );
        exit_flushed!(1);
    }

    // Block on pending transactions — they cause incomplete composefs images
    // and switch-root-os-release-errors on next boot.
    if report.pending_transaction != preflight::PendingTransactionStatus::Clean
        && !args.force
        && !args.skip_preflight
    {
        eprintln!(
            "Error: Pending OSTree transaction detected: {}.\n\
             The OSTree repo has uncommitted state from a previous update. The migration\n\
             would produce an incomplete composefs image that cannot boot.\n\
             \n\
             To resolve:\n\
               - If you ran `bootc upgrade` or `rpm-ostree upgrade`, complete it first.\n\
               - If the update was interrupted, run `ostree admin undeploy <index>`\n\
                 to remove the pending deployment.\n\
               - Or run `bootc upgrade` to finish/finalize the pending transaction.\n",
            report.pending_transaction
        );
        exit_flushed!(1);
    }

    if !report.supports_reflink && !args.force {
        println!(
            "Warning: Reflink support not detected on /sysroot. Migration will perform a full copy of repository objects, which will require significant disk space."
        );
        print!("Do you want to proceed anyway? (y/N): ");
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            let input = input.trim().to_lowercase();
            if input != "y" && input != "yes" {
                println!("Migration aborted.");
                exit_flushed!(0);
            }
        } else {
            println!("Migration aborted.");
            exit_flushed!(0);
        }
    }

    println!("Starting migration to OCI image: {}...", target_image);
    if let Err(e) = migration::run_migration(
        &report,
        &target_image,
        args.dry_run,
        args.skip_import,
        &args.bootloader,
        args.force,
    ) {
        eprintln!("\nMigration Failed: {:#}", e);
        exit_flushed!(1);
    }
}

/// Commit the composefs deployment as the permanent default.
fn run_commit(dry_run: bool) -> Result<()> {
    check_root_privilege()?;
    transaction::commit(dry_run)
}

fn run_undo(dry_run: bool, full: bool) -> Result<()> {
    check_root_privilege()?;
    transaction::undo(dry_run, full)
}
