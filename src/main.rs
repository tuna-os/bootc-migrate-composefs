use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process;

mod composefs;
mod mergetc;
mod migration;
mod motd;
mod ostree;
mod preflight;
mod reflink;
mod tui;
mod types;
mod xattr;

pub use types::VerityDigest;

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
    if report.pending_transaction != preflight::PendingTransactionStatus::Clean && !args.force {
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
    println!("=== Committing composefs deployment as permanent default ===");
    if dry_run {
        println!("*** DRY RUN — no changes will be made ***");
    }

    // Sanity check: refuse to run if booted via the OSTree side. Committing
    // would delete the rootfs we're currently mounted on top of.
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !cmdline.contains("composefs=") {
        anyhow::bail!(
            "/proc/cmdline does not contain composefs= — current boot looks like OSTree. \
             Reboot into the composefs entry before running commit.\n\
             cmdline: {}",
            cmdline.trim()
        );
    }

    // Detect bootloader — check ESP entries for systemd-boot first.
    let esp_candidates = ["/boot/efi", "/efi"];
    let mut entries_dir = PathBuf::from("/boot/loader/entries");
    let mut is_systemd_boot = false;

    for esp in &esp_candidates {
        let esp_entries = Path::new(esp).join("loader/entries");
        if esp_entries.exists() {
            // Check if there are bootc_ entries on the ESP.
            if let Ok(mut rd) = std::fs::read_dir(&esp_entries)
                && rd.any(|e| {
                    e.map(|en| en.file_name().to_string_lossy().starts_with("bootc_"))
                        .unwrap_or(false)
                })
            {
                entries_dir = esp_entries;
                is_systemd_boot = true;
                break;
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
    // If we found bootc_ entries at the default /boot location but not
    // on the ESP, we're still on composefs — just without an auto-mounted
    // ESP at /boot/efi or /efi (common in E2E QEMU runs).
    if !composefs_entries.is_empty() {
        is_systemd_boot = true;
    }

    // Fallback: the ESP may be unmounted or at a non-standard path
    // (e.g. after LUKS migration where Phase 5 auto-mounted it at
    // /var/tmp/esp-migration and the boot-time fstab doesn't know about
    // it). Try auto-mounting the ESP by partition type GUID.
    if composefs_entries.is_empty()
        && let Ok(esp_path) = crate::migration::find_esp_or_mount()
    {
        let esp_entries = Path::new(&esp_path).join("loader/entries");
        if esp_entries.exists() {
            for entry in std::fs::read_dir(&esp_entries)? {
                let entry = entry?;
                let name_str = entry.file_name().to_string_lossy().into_owned();
                if name_str.starts_with("bootc_") {
                    composefs_entries.push(name_str);
                }
            }
            if !composefs_entries.is_empty() {
                entries_dir = esp_entries;
                is_systemd_boot = true;
                println!(
                    "Found composefs BLS entries on auto-mounted ESP at {}",
                    esp_path
                );
            }
        }
    }

    if composefs_entries.is_empty() {
        if is_systemd_boot {
            println!("No composefs BLS entries found on ESP. Nothing to commit.");
            println!(
                "Note: for systemd-boot, the composefs entry should already be the default if it has the lowest sort-key."
            );
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
        // Remove the OSTree fallback entry + its kernel/initrd from the ESP so the next
        // boot menu only shows the composefs entry. The composefs entry remains the
        // loader.conf default; nothing else needs to change.
        let esp_root = entries_dir.parent().and_then(|p| p.parent());
        if let Some(esp_root) = esp_root {
            let fallback_entry = entries_dir.join("ostree-fallback-0.conf");
            if fallback_entry.exists() {
                std::fs::remove_file(&fallback_entry)
                    .with_context(|| format!("failed to remove {}", fallback_entry.display()))?;
                println!("Removed OSTree fallback BLS entry from ESP.");
            }
            let fallback_dir = esp_root.join("EFI/Linux/ostree-fallback");
            if fallback_dir.exists() {
                std::fs::remove_dir_all(&fallback_dir)
                    .with_context(|| format!("failed to remove {}", fallback_dir.display()))?;
                println!("Removed OSTree fallback kernel/initrd from ESP.");
            }
            // Drop the timeout now that composefs is the only entry.
            let loader_conf = esp_root.join("loader/loader.conf");
            if loader_conf.exists() {
                let body = format!("default {}\ntimeout 0\nconsole-mode keep\n", primary);
                std::fs::write(&loader_conf, body)
                    .with_context(|| format!("failed to rewrite {}", loader_conf.display()))?;
            }
        }
        println!(
            "Composefs deployment '{}' committed as the permanent systemd-boot default.",
            primary
        );
    } else {
        if !dry_run {
            let status = std::process::Command::new("grub2-set-default")
                .arg(primary)
                .status();
            if !matches!(status, Ok(s) if s.success()) {
                anyhow::bail!("failed to set GRUB default");
            }
            // Drop GRUB2-side OSTree fallback artifacts too.
            let _ = std::fs::remove_file("/boot/loader/entries/ostree-fallback-0.conf");
            let _ = std::fs::remove_dir_all("/boot/ostree-fallback");
        }
        println!(
            "Composefs deployment '{}' is now the permanent default.",
            primary
        );
    }

    // --- Full OSTree-side cleanup so the on-disk layout matches a fresh
    //     bootc install of the target image. ---
    // /sysroot is typically read-only on a composefs-booted system.
    // Even after remount rw, the composefs EROFS overlay blocks mutation
    // of paths that are pinned by the metadata tree (e.g. /sysroot/ostree).
    // To bypass the overlay, mount the underlying btrfs device at a
    // temporary location — that mount is a plain btrfs, no EROFS.
    let alt_root = Path::new("/var/tmp/commit-cleanup");
    let has_alt_mount = if !dry_run {
        let _ = std::fs::create_dir_all(alt_root);
        mount_sysroot_btrfs_at(alt_root).is_ok()
    } else {
        false
    };
    let mut total_freed: u64 = 0;

    // 1. /sysroot/ostree — the entire OSTree object store + deploys + the
    //    leaked Bluefin /var copy under ostree/deploy/<n>/var.
    //    If we have an alternate mount (bypassing the composefs EROFS overlay),
    //    operate through that; otherwise fall back to the direct path.
    let ostree_label = "OSTree object store + deploys (incl. leaked pre-migration /var)";
    if has_alt_mount {
        total_freed += remove_path_with_size(&alt_root.join("ostree"), ostree_label, dry_run);
        // .bootc-aleph.json also through the alt mount for a clean delete.
        total_freed += remove_path_with_size(
            &alt_root.join(".bootc-aleph.json"),
            "stale Bluefin install-provenance marker",
            dry_run,
        );
    } else {
        total_freed += remove_path_with_size(Path::new("/sysroot/ostree"), ostree_label, dry_run);
    }
    // .bootc-aleph.json — via alt mount if available, otherwise direct.
    if !has_alt_mount {
        total_freed += remove_path_with_size(
            Path::new("/sysroot/.bootc-aleph.json"),
            "stale Bluefin install-provenance marker",
            dry_run,
        );
    }

    // /boot may be read-only under composefs (e.g. separate ext4 /boot partition
    // on LUKS where the initramfs mounts it ro). Remount rw before cleanup.
    if !dry_run {
        let _ = std::process::Command::new("mount")
            .args(["-o", "remount,rw", "/boot"])
            .status();
    }

    // 2. Stale OSTree BLS entries under /boot/loader/entries. The ESP-side
    //    ostree-fallback was removed above; /boot/loader/entries/ostree-*.conf
    //    is the GRUB-side equivalent.
    if let Ok(rd) = std::fs::read_dir("/boot/loader/entries") {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("ostree-") && name.ends_with(".conf") {
                total_freed += remove_path_with_size(
                    &entry.path(),
                    &format!("stale OSTree BLS entry: {}", name),
                    dry_run,
                );
            }
        }
    }

    // 3. When we migrated to systemd-boot, drop the GRUB2 bits the user no
    //    longer needs. Keep them when --bootloader grub2 was used.
    if is_systemd_boot {
        for path in &["/boot/grub2", "/boot/efi/EFI/fedora"] {
            total_freed += remove_path_with_size(
                Path::new(path),
                "GRUB2 boot artifacts (migrated to systemd-boot)",
                dry_run,
            );
        }
    }

    // 4. Drop ostree-remount.service enablement. On a composefs-booted
    //    system OSTree bind mounts are irrelevant; the symlink may be
    //    re-created during boot by the target image's presets even though
    //    Phase 4 removed it from the deploy /etc.
    let remount_link =
        Path::new("/etc/systemd/system/local-fs.target.wants/ostree-remount.service");
    if remount_link.exists() || remount_link.is_symlink() {
        if dry_run {
            println!(
                "[DRY RUN] Would remove ostree-remount.service enablement (composefs doesn't need OSTree bind mounts)."
            );
        } else {
            std::fs::remove_file(remount_link)
                .with_context(|| format!("failed to remove {}", remount_link.display()))?;
            println!(
                "Removed ostree-remount.service enablement (composefs doesn't need OSTree bind mounts)."
            );
        }
    }

    let human = format_bytes(total_freed);
    if dry_run {
        println!("\nWould reclaim: {} ({} bytes)", human, total_freed);
        println!("Re-run without --dry-run to apply.");
    } else {
        println!("\nReclaimed: {} ({} bytes)", human, total_freed);
        println!(
            "On-disk layout is now consistent with a fresh '{}' install.",
            if is_systemd_boot {
                "systemd-boot"
            } else {
                "GRUB2"
            }
        );
    }
    if has_alt_mount {
        let _ = std::process::Command::new("umount").arg(alt_root).status();
        let _ = std::fs::remove_dir(alt_root);
    }
    if !dry_run && let Err(e) = motd::clear_migration_reminder() {
        eprintln!("Warning: failed to clear login reminder: {e:#}");
    }
    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.file_type().is_symlink() || meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    let rd = match std::fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    for entry in rd.flatten() {
        total += dir_size(&entry.path());
    }
    total
}

fn remove_path_with_size(path: &Path, label: &str, dry_run: bool) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let size = dir_size(path);
    let human = format_bytes(size);
    if dry_run {
        println!(
            "[dry-run] would remove {} — {} ({})",
            path.display(),
            label,
            human
        );
        return size;
    }
    let res = if meta.is_dir() && !meta.file_type().is_symlink() {
        // On OSTree/bootc systems, /sysroot/ostree is typically a btrfs
        // subvolume — rm -rf returns EPERM. Try `btrfs subvolume delete`
        // first; if that fails, clear the immutable flag (chattr -i) and
        // fall back to remove_dir_all.
        let btrfs_ok = std::process::Command::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if btrfs_ok {
            Ok(())
        } else {
            // Clear immutable flag — OSTree often sets chattr +i on
            // /sysroot/ostree to prevent accidental deletion. Suppress
            // stderr: chattr on OSTree deploy checkouts (symlink farms)
            // produces thousands of "Operation not supported" lines.
            let _ = std::process::Command::new("chattr")
                .args(["-R", "-i"])
                .arg(path)
                .stderr(std::process::Stdio::null())
                .status();
            std::fs::remove_dir_all(path)
        }
    } else {
        std::fs::remove_file(path)
    };
    match res {
        Ok(()) => {
            println!("Removed {} — {} ({})", path.display(), label, human);
            size
        }
        Err(e) => {
            eprintln!("warning: failed to remove {}: {}", path.display(), e);
            0
        }
    }
}

fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

/// Mount the device backing /sysroot at `target`, bypassing the
/// composefs EROFS overlay so that paths like /sysroot/ostree can be
/// mutated directly on the underlying filesystem. Works on btrfs, xfs,
/// and any other filesystem that can be mounted twice.
fn mount_sysroot_btrfs_at(target: &Path) -> Result<()> {
    let mounts = std::fs::read_to_string("/proc/mounts").context("failed to read /proc/mounts")?;
    let device = mounts
        .lines()
        .find(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            parts.len() >= 3 && parts[1] == "/sysroot"
        })
        .and_then(|line| line.split_whitespace().next())
        .map(|s| s.to_string())
        .context("could not find device for /sysroot in /proc/mounts")?;

    let status = std::process::Command::new("mount")
        .arg(&device)
        .arg(target)
        .status()
        .context("failed to execute mount for alt-root cleanup")?;
    if !status.success() {
        anyhow::bail!("mount {} → {} failed", device, target.display());
    }
    Ok(())
}

/// Undo a partial or failed migration. Removes all composefs artifacts
/// (staged deployments, boot artifacts, BLS entries, loopback images,
/// composefs object store) while leaving the OSTree deployment intact.
fn run_undo(dry_run: bool, full: bool) -> Result<()> {
    check_root_privilege()?;

    // Always release the migration lock so a subsequent run doesn't fail
    // with "already running". The lock guard drops automatically at process
    // exit, but if the previous run crashed mid-phase the lock can linger.
    let lock_path = "/var/run/bootc-migrate-composefs.lock";
    if !dry_run {
        let _ = std::fs::remove_file(lock_path);
    }

    println!("=== Undoing composefs migration ===");
    if dry_run {
        println!("*** DRY RUN — no changes will be made ***");
    }

    // /sysroot is mounted read-only on an OSTree-booted system (composefs or
    // classic). Remount rw so we can delete staged deployments and loopback
    // images that live there. Ignore the error — if it's already rw this is
    // a no-op; if it genuinely can't be made rw the subsequent removes will
    // surface the real error.
    if !dry_run {
        let _ = std::process::Command::new("mount")
            .args(["-o", "remount,rw", "/sysroot"])
            .status();
    }

    let mut removed = 0usize;
    let mut skipped = 0usize;

    // 1. Remove staged composefs deployments.
    let deploy_dir = Path::new("/sysroot/state/deploy");
    if deploy_dir.exists()
        && let Ok(rd) = std::fs::read_dir(deploy_dir)
    {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip the OSTree deploy dir (numeric or short names).
            // Composefs deploy dirs are long hex strings (64+ chars).
            if name.len() < 40 {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                println!("Removing staged deployment: {}", path.display());
                if !dry_run {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
                removed += 1;
            }
        }
    }
    if removed == 0 {
        println!("No composefs deployments found in /sysroot/state/deploy/.");
    }

    // 2. Remove composefs boot artifacts.
    let boot_dir = Path::new("/boot");
    if let Ok(rd) = std::fs::read_dir(boot_dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("bootc_composefs-") {
                let path = entry.path();
                println!("Removing boot artifacts: {}", path.display());
                if !dry_run {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
                removed += 1;
            }
        }
    }

    // 3. Remove composefs BLS entries from /boot/loader/entries.
    let bls_dir = Path::new("/boot/loader/entries");
    if bls_dir.exists()
        && let Ok(rd) = std::fs::read_dir(bls_dir)
    {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("bootc_") || name.starts_with("ostree-fallback-") {
                let path = entry.path();
                println!("Removing BLS entry: {}", path.display());
                if !dry_run {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
                removed += 1;
            }
        }
    }

    // 4. Remove composefs BLS entries from ESP.
    for esp in &["/boot/efi", "/efi"] {
        let esp_entries = Path::new(esp).join("loader/entries");
        if esp_entries.exists() {
            if let Ok(rd) = std::fs::read_dir(&esp_entries) {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("bootc_") || name.starts_with("ostree-fallback-") {
                        let path = entry.path();
                        println!("Removing ESP BLS entry: {}", path.display());
                        if !dry_run {
                            std::fs::remove_file(&path)
                                .with_context(|| format!("failed to remove {}", path.display()))?;
                        }
                        removed += 1;
                    }
                }
            }
            // Remove loader.conf if we wrote one.
            let loader_conf = Path::new(esp).join("loader/loader.conf");
            if loader_conf.exists() {
                println!("Removing ESP loader.conf: {}", loader_conf.display());
                if !dry_run {
                    std::fs::remove_file(&loader_conf)?;
                    removed += 1;
                }
            }
        }
        // Remove systemd-boot from ESP.
        let sd_dir = Path::new(esp).join("EFI/systemd");
        if sd_dir.exists() {
            println!("Removing systemd-boot from ESP: {}", sd_dir.display());
            if !dry_run {
                std::fs::remove_dir_all(&sd_dir)?;
                removed += 1;
            }
        }
        let esp_linux = Path::new(esp).join("EFI/Linux");
        if esp_linux.exists()
            && let Ok(rd) = std::fs::read_dir(&esp_linux)
        {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("bootc_composefs-") {
                    let path = entry.path();
                    println!("Removing ESP EFI/Linux entry: {}", path.display());
                    if !dry_run {
                        std::fs::remove_dir_all(&path)
                            .with_context(|| format!("failed to remove {}", path.display()))?;
                    }
                    removed += 1;
                }
            }
        }
    }

    // 5. Remove composefs loopback image (only with --full).
    if full {
        let loopback = Path::new("/sysroot/composefs-loopback.ext4");
        if loopback.exists() {
            println!("Removing composefs loopback image: {}", loopback.display());
            if !dry_run {
                std::fs::remove_file(loopback)?;
                removed += 1;
            }
        }

        // 6. Remove composefs object store (only with --full).
        let composefs_dir = Path::new("/sysroot/composefs");
        if composefs_dir.exists() {
            let has_objects = composefs_dir.join("objects").exists();
            let has_images = composefs_dir.join("images").exists();
            if has_objects || has_images {
                println!(
                    "Removing composefs object store: {}",
                    composefs_dir.display()
                );
                if !dry_run {
                    for sub in &["objects", "images", "streams", "tmp"] {
                        let p = composefs_dir.join(sub);
                        if p.exists() {
                            std::fs::remove_dir_all(&p)
                                .with_context(|| format!("failed to remove {}", p.display()))?;
                        }
                    }
                    removed += 1;
                }
            } else {
                println!("Composefs directory exists but is empty (no objects/images).");
                skipped += 1;
            }
        }
    } else {
        println!("Composefs object store and loopback preserved (re-run --full to clean).");
    }

    // 7. Optionally warn about NVRAM entries (can't clean those from userspace easily).
    println!();
    if dry_run {
        println!("Would remove {} artifact(s).", removed);
        println!("Re-run without --dry-run to apply.");
    } else {
        println!("Removed {} composefs artifact(s).", removed);
        if skipped > 0 {
            println!("{} path(s) skipped (empty or already clean).", skipped);
        }
        println!("The system is now in its pre-migration OSTree state.");
        println!("Run 'bootc-migrate-composefs --target-image <image>' to try again.");
    }
    if !dry_run && let Err(e) = motd::clear_migration_reminder() {
        eprintln!("Warning: failed to clear login reminder: {e:#}");
    }
    Ok(())
}
