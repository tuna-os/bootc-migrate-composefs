//! Phase 1: import OSTree file objects into the composefs object store.

use super::*;

// ---- Phase 1 ----

pub fn phase1_import_objects(report: &PreflightReport, dry_run: bool) -> Result<()> {
    println!("=== Phase 1: Importing OSTree objects ===");
    let ostree_repo = "/sysroot/ostree/repo";
    if !Path::new(ostree_repo).exists() {
        println!("No OSTree repository found. Skipping.");
        return Ok(());
    }

    let file_objects = crate::ostree::scan_ostree_file_objects(ostree_repo)
        .context("failed to scan ostree repository")?;
    let total_objects = file_objects.len();
    println!("Found {} file objects to import.", total_objects);

    if dry_run {
        println!(
            "[DRY RUN] Would import {} objects into composefs store.",
            total_objects
        );
        return Ok(());
    }

    let mut count = 0usize;
    let mut reflink_count = 0usize;
    for obj in file_objects {
        let sha512 = crate::ostree::compute_sha512(&obj.path)?;
        let prefix = &sha512[..2];
        let rest = &sha512[2..];
        let target_dir = Path::new("/sysroot/composefs/objects").join(prefix);
        let target_path = target_dir.join(rest);

        if !target_path.exists() {
            fs::create_dir_all(&target_dir)?;
            if report.supports_reflink {
                if crate::reflink::reflink(&obj.path, &target_path).is_ok() {
                    reflink_count += 1;
                } else {
                    fs::copy(&obj.path, &target_path)?;
                }
            } else {
                fs::copy(&obj.path, &target_path)?;
            }
        }
        count += 1;
        if count.is_multiple_of(1000) {
            println!("Imported {}/{} objects...", count, total_objects);
        }
    }
    println!("Imported {} objects ({} reflinked).", count, reflink_count);

    // Post-Phase-1 completeness verification: count objects in the composefs
    // store and compare with the expected total. A significant shortfall means
    // the source ostree repo had incomplete state (e.g. a pending transaction).
    let composefs_objects_dir = Path::new("/sysroot/composefs/objects");
    if composefs_objects_dir.exists() {
        let stored = crate::preflight::count_composefs_files(composefs_objects_dir);
        if stored < total_objects.saturating_sub(100) {
            // More than 100 objects short — likely a pending-transaction issue.
            eprintln!(
                "[phase1] WARNING: composefs object store has {} objects but {} were expected.\
                 The source ostree repo may have had incomplete state (e.g. an interrupted update).\
                 The resulting composefs image may not boot correctly.",
                stored, total_objects
            );
        } else {
            println!(
                "[phase1] composefs object completeness OK: {} stored, {} expected.",
                stored, total_objects
            );
        }
    }
    Ok(())
}
