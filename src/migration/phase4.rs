use crate::VerityDigest;
use crate::migration::mount_image;
use crate::migration::phase0::MountGuard;
use crate::migration::registry::{extract_files_via_registry, extract_subtree_via_registry};
use crate::xattr;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub(crate) fn build_origin_content(
    target_image: &str,
    verity: &VerityDigest,
    manifest_digest: &str,
) -> String {
    // Schema must match bootc's canonical layout (crates/lib/src/composefs_consts.rs):
    //   [origin] container-image-reference = ...
    //   [boot]   boot_type = bls
    //   [boot]   digest    = <verity hex>           # NB: key is "digest", not "boot_digest"
    //   [image]  manifest_digest = sha256:...
    // bootc's status code reads from [image]/manifest_digest and [boot]/digest;
    // wrong section or key names produce "No manifest_digest in origin and no
    // legacy .imginfo file" or "Could not find boot digest for deployment".
    tini::Ini::new()
        .section("origin")
        .item(
            "container-image-reference",
            format!("ostree-unverified-image:docker://{}", target_image),
        )
        .section("boot")
        .item("boot_type", "bls")
        .item("digest", verity.as_hex())
        .section("image")
        .item("manifest_digest", manifest_digest)
        .to_string()
}

/// Patch the `digest` entry in `[boot]` with a real sha256(vmlinuz || initrd).
/// Pure function so we can test it without filesystem access.
pub(crate) fn patch_boot_digest_in_content(content: &str, new_digest: &str) -> Result<String> {
    let ini = tini::Ini::from_string(content)
        .map_err(|e| anyhow!("parsing origin file: {e}"))?
        .section("boot")
        .item("digest", new_digest);
    Ok(ini.to_string())
}

pub(crate) fn phase4_stage_deploy(
    verity: &VerityDigest,
    target_image: &str,
    manifest_digest: &str,
    config_digest: &str,
    dry_run: bool,
) -> Result<PathBuf> {
    println!("=== Phase 4: Staging Deployment State ===");

    let deploy_dir = Path::new("/sysroot/state/deploy").join(verity.as_hex());

    if dry_run {
        println!(
            "[DRY RUN] Would stage deployment at: {}",
            deploy_dir.display()
        );
        return Ok(deploy_dir);
    }

    // Idempotency (#11): skip if already staged with valid .origin.
    // bootc expects the filename as `<bare-hex-verity>.origin` (no `sha512:`
    // prefix); using as_prefixed() here would cause `bootc status` to fail
    // with "Opening origin file: No such file or directory" and break the
    // post-reboot validation.
    let origin_path = deploy_dir.join(format!("{}.origin", verity.as_hex()));
    if deploy_dir.exists() && origin_path.exists() {
        println!(
            "Deployment already staged at {}. Skipping Phase 4.",
            deploy_dir.display()
        );
        return Ok(deploy_dir);
    }

    fs::create_dir_all(&deploy_dir).context("failed to create deployment directory")?;

    let etc_dir = deploy_dir.join("etc");
    fs::create_dir_all(&etc_dir).context("failed to create deployment etc directory")?;

    // 3-way /etc merge (#4)
    println!("Performing 3-way /etc merge...");
    if let Err(e) = perform_etc_merge(verity, target_image, &etc_dir) {
        eprintln!(
            "3-way /etc merge failed ({}), falling back to flat /etc copy.",
            e
        );
        xattr::copy_dir_all_with_xattrs("/etc", &etc_dir)
            .context("failed to copy /etc (fallback)")?;
    }

    // Stage /var symlink (#7)
    let var_symlink = deploy_dir.join("var");
    if var_symlink.exists() {
        fs::remove_file(&var_symlink).context("failed to remove existing var entry")?;
    }
    std::os::unix::fs::symlink("../../os/default/var", &var_symlink)
        .context("failed to create /var symlink")?;

    // Write .origin file using bootc's expected schema (testutils.rs:316-331).
    // Use the same `tini::Ini` library bootc uses to parse it so the output
    // is byte-compatible. Placeholder boot_digest gets patched in Phase 5
    // with sha256(vmlinuz || initrd) once those files are on the ESP.
    //
    // Key names are load-bearing:
    // - `container-image-reference` is `ostree_ext::container::deploy::ORIGIN_CONTAINER`
    //   — bootc reads this to populate the BootEntry's image field.
    // - `manifest_digest` under [boot] lets bootc fetch the OCI manifest from
    //   the registry without a separate .imginfo file (`bootc internals cfs oci
    //   inspect` is unreliable in our flow, see [HANDOFF.md]).
    let origin_content = build_origin_content(target_image, verity, manifest_digest);
    fs::write(&origin_path, &origin_content).context("failed to write .origin file")?;

    // Write .imginfo file
    println!("Writing .imginfo file...");
    if let Ok(config_json) = crate::migration::inspect_image(config_digest) {
        let imginfo_path = deploy_dir.join(format!("{}.imginfo", verity.as_hex()));
        if let Err(e) = fs::write(&imginfo_path, &config_json) {
            eprintln!(
                "Warning: failed to write .imginfo file ({}): {}",
                imginfo_path.display(),
                e
            );
        }
    }

    // Handle /var migration (#7)
    phase4_var_migration(&etc_dir, dry_run)?;

    Ok(deploy_dir)
}

pub(crate) fn phase4_var_migration(etc_dir: &Path, _dry_run: bool) -> Result<()> {
    println!("=== Migrating /var data to ComposeFS state ===");
    let target_var = Path::new("/sysroot/state/os/default/var");

    // Check if /var is already populated (idempotency)
    if target_var.exists() {
        let count = fs::read_dir(target_var).map(|d| d.count()).unwrap_or(0);
        if count > 0 {
            println!(
                "/var already populated at {}. Skipping var migration.",
                target_var.display()
            );
            return Ok(());
        }
    }

    // Always copy /var data into state/os/default/var so the bootc initramfs
    // bind-mount of that path onto the deploy's /var exposes user data
    // (roothome/.ssh, home/, lib/containers, etc.). Do NOT synthesize an
    // /etc/fstab entry for /var: on Bluefin /proc/mounts reports /var as
    // subvolid=5 (the root subvol), and mounting that at /var post-pivot
    // shadows the bind-mount with /ostree, /state, /boot — losing user data.
    let _ = etc_dir; // (kept for signature compat; no fstab edits anymore)

    if !target_var.exists() {
        fs::create_dir_all(target_var.parent().unwrap())?;
    }

    let source_var = if Path::new("/sysroot/ostree/deploy/default/var").exists() {
        "/sysroot/ostree/deploy/default/var"
    } else {
        "/var"
    };

    println!(
        "Migrating /var data from {} to ComposeFS state...",
        source_var
    );
    xattr::copy_dir_all_with_xattrs(source_var, &target_var)
        .context("failed to migrate /var data to ComposeFS state")?;
    println!("/var data migrated successfully.");

    Ok(())
}

/// Build a fstab entry for the /var btrfs subvolume by parsing /proc/mounts and
/// resolving the source device to a UUID. Returns None if the data can't be derived.
#[allow(dead_code)]
fn synthesize_var_fstab_entry(mounts: &str) -> Option<String> {
    let var_line = mounts.lines().find(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        parts.len() >= 4 && parts[1] == "/var" && parts[2] == "btrfs"
    })?;
    println!("[phase4] /proc/mounts /var line: {}", var_line);

    let parts: Vec<&str> = var_line.split_whitespace().collect();
    let device = parts[0];
    let raw_opts = parts[3];

    let subvol_token = raw_opts
        .split(',')
        .find(|o| o.starts_with("subvol=") && *o != "subvol=/")
        .or_else(|| raw_opts.split(',').find(|o| o.starts_with("subvolid=")))
        .unwrap_or("subvol=/");

    let uuid = resolve_device_uuid(device);
    let source = uuid
        .map(|u| format!("UUID={}", u))
        .unwrap_or_else(|| device.to_string());

    let opts = format!("rw,relatime,{}", subvol_token);
    Some(format!("{}\t/var\tbtrfs\t{}\t0 0\n", source, opts))
}

#[allow(dead_code)]
fn resolve_device_uuid(device: &str) -> Option<String> {
    let by_uuid = Path::new("/dev/disk/by-uuid");
    let entries = fs::read_dir(by_uuid).ok()?;
    for entry in entries.flatten() {
        let link = fs::read_link(entry.path()).ok()?;
        let resolved = by_uuid.join(&link).canonicalize().ok()?;
        if resolved == Path::new(device) {
            return entry.file_name().to_str().map(|s| s.to_string());
        }
    }
    None
}

/// Perform 3-way /etc merge: old OSTree default, current live /etc, new ComposeFS default.
pub(crate) fn perform_etc_merge(verity: &VerityDigest, target_image: &str, etc_dir: &Path) -> Result<()> {
    // Mount the EROFS image for directory listing / symlink-target validation.
    // The EROFS mount correctly exposes file NAMES and METADATA (stat, readdir,
    // readlink) but ALL file CONTENT reads as zeros through the bare EROFS mount.
    // The bootc overlay mount (bootc cfs mount) would fix this but fails because
    // the oci-config-* stream is missing from the composefs repo when the image
    // was sealed by our migration rather than by bootc's own pipeline.
    let temp_mount =
        TempDir::new_in("/var/tmp").context("failed to create temp mount directory")?;
    let mount_path = temp_mount.path().to_path_buf();
    mount_image(verity.as_hex(), &mount_path).context("failed to mount EROFS for etc merge")?;
    let _guard = MountGuard::new(&mount_path);

    let old_default_etc = find_ostree_etc_default()?;
    let current_etc = Path::new("/etc");

    // Extract the target image's /etc tree from the OCI registry. The EROFS mount
    // gives zero-filled file content for everything, so we must use registry streaming
    // for real file content. This downloads ~120 layers but is the only reliable path.
    let registry_etc =
        TempDir::new_in("/var/tmp").context("failed to create temp dir for registry etc")?;
    let registry_etc_path = registry_etc.path().join("etc");
    println!("[phase4] extracting target /etc from registry...");
    if let Err(e) = extract_subtree_via_registry(target_image, "/etc", &registry_etc_path) {
        eprintln!(
            "[phase4] warning: registry /etc extraction failed: {e:#} — falling back to EROFS"
        );
        // Last-resort fallback: EROFS mount for merge source.
        let new_default_etc = mount_path.join("etc");
        if !new_default_etc.exists() {
            anyhow::bail!("no /etc in new composefs image");
        }
        crate::mergetc::merge_etc_files(&old_default_etc, current_etc, &new_default_etc, etc_dir)
            .context("3-way /etc merge failed")?;
    } else {
        let entry_count = fs::read_dir(&registry_etc_path)
            .map(|d| d.count())
            .unwrap_or(0);
        println!(
            "[phase4] using registry-extracted /etc for merge source ({} entries)",
            entry_count
        );
        crate::mergetc::merge_etc_files(&old_default_etc, current_etc, &registry_etc_path, etc_dir)
            .context("3-way /etc merge failed")?;
    }

    // Drop /etc symlinks whose /usr/* target does not exist in the target image.
    // Bluefin → Dakota: e.g. /etc/systemd/system/dbus.service points to
    // dbus-broker.service which Dakota doesn't ship; the dangling symlink
    // breaks dbus and everything downstream (polkit, logind, sshd).
    match crate::mergetc::prune_dangling_symlinks(etc_dir, &mount_path) {
        Ok(n) if n > 0 => println!("[phase4] pruned {} dangling /etc symlink(s)", n),
        Ok(_) => {}
        Err(e) => eprintln!("[phase4] warning: dangling-symlink prune failed: {e:#}"),
    }

    // Drop OSTree/GRUB-era /etc artifacts that don't belong on a composefs
    // deployment. The 3-way merge keeps these because Bluefin's factory has
    // them and the user didn't modify them, but they actively lie about
    // system state on Dakota.
    drop_ostree_era_etc_artifacts(etc_dir);

    // Ensure the TCP 22 SSH socket-activated listener is always present in the
    // deploy /etc. On Bluefin, sshd only binds Unix-local + vsock by default;
    // this socket provides the TCP listener the E2E test needs. The 3-way merge
    // drops it when baked into the OSTree factory (old==cur, new absent), so we
    // recreate it unconditionally after the merge.
    ensure_e2e_ssh_socket(etc_dir)?;

    // Supplement identity DBs (passwd/shadow/group) with target-image system
    // users. The 3-way merge uses the EROFS mount's /etc as its "new" source,
    // but file content for anything past the ~4KB inline threshold reads as
    // zeros on a bare EROFS mount. Identity DBs are small enough to be inline
    // in theory, but the EROFS overlay mount from bootc cfs might not populate
    // them correctly either. Registry streaming is the authoritative path.
    // Key failure mode this catches: Dakota ships dbus.service with
    // `User=dbus`, but Bluefin (which uses dbus-broker) has no `dbus` user.
    // Without this supplement, dbus-daemon fails with status 217/EXIT_USER
    // and the entire bus/polkit/logind/sshd stack cascade-fails.
    if let Err(e) = supplement_identity_dbs_from_registry(target_image, etc_dir) {
        eprintln!("[phase4] warning: identity-DB supplement failed: {e:#}");
    }

    // Belt-and-suspenders: explicitly drop any dangling dbus-broker symlink
    // that survived the prune. Fedora 42 ships dbus-broker.service under
    // /usr/lib/systemd/system/, so the prune's stat check finds it and keeps
    // the symlink. But bootc-root-setup's composefs overlay might not expose
    // the file for systemd to load at runtime, causing "Failed to load
    // configuration: No such file or directory" on dbus-broker.service.
    let dbus_svc_link = etc_dir.join("systemd/system/dbus.service");
    if dbus_svc_link.is_symlink() || dbus_svc_link.exists() {
        if let Ok(target) = fs::read_link(&dbus_svc_link) {
            let t = target.to_string_lossy();
            if t.contains("dbus-broker") {
                match fs::remove_file(&dbus_svc_link) {
                    Ok(()) => println!("[phase4] dropped dangling dbus.service -> dbus-broker symlink"),
                    Err(e) => eprintln!("[phase4] warning: failed to remove dbus.service symlink: {e}"),
                }
            }
        }
    }

    // Also drop any leftover dbus-broker artifacts in /etc/systemd/system.
    // The target ships its own dbus.service at /usr/lib/systemd/system/dbus.service,
    // which systemd finds automatically once the dangling override is gone.
    for name in &["dbus-broker.service", "dbus.service"] {
        let p = etc_dir.join("systemd/system").join(name);
        if p.exists() || p.is_symlink() {
            let _ = fs::remove_file(&p);
        }
    }

    // Write sysroot-composefs.mount to the deploy /etc so it survives
    // switch-root from initrd to real root. Without this, the loopback
    // device is detected by udev but never mounted — /sysroot/composefs
    // remains an empty mount point and bootc can't find meta.json.
    let loopback_path = Path::new("/sysroot/composefs-loopback.ext4");
    if loopback_path.exists() {
        // --- sysroot-composefs.mount: loopback mount unit ---
        // This mount unit ensures the ext4 loopback (composefs object store)
        // is mounted at /sysroot/composefs. During initrd, xfs-mount.cpio
        // handles this. The deploy /etc copy ensures it persists after
        // switch-root. However, the composefs EROFS overlay HAS a
        // /sysroot/composefs/ directory (empty) which SHADOWS the loopback
        // mount after switch-root. To work around this, we create a bind-mount
        // service that runs after switch-root and makes the loopback visible
        // through the overlay.
        let unit_dir = etc_dir.join("systemd/system");
        fs::create_dir_all(&unit_dir)?;
        let mount_unit = format!(
            r#"[Unit]
Description=ComposeFS Loopback Mount
After=sysroot.mount
Before=initrd-root-fs.target bootc-root-setup.service
DefaultDependencies=no

[Mount]
What=/sysroot/composefs-loopback.ext4
Where=/sysroot/composefs
Type=ext4
Options=loop,ro

[Install]
WantedBy=initrd-root-fs.target
"#
        );
        let unit_path = unit_dir.join("sysroot-composefs.mount");
        if !unit_path.exists() {
            fs::write(&unit_path, mount_unit.as_bytes())
                .context("failed to write sysroot-composefs.mount")?;
            let irf_wants = etc_dir.join("systemd/system/initrd-root-fs.target.wants");
            fs::create_dir_all(&irf_wants)?;
            let link = irf_wants.join("sysroot-composefs.mount");
            if !link.exists() {
                std::os::unix::fs::symlink("../sysroot-composefs.mount", &link)?;
            }
            println!("[phase4] wrote sysroot-composefs.mount to deploy /etc");
        }

        // --- bootc-composefs-rebind.service: re-bind loopback after composefs switch-root ---
        // After switch-root to the composefs EROFS, the /sysroot/composefs
        // directory in the EROFS image shadows the actual loopback mount.
        // This oneshot service bind-mounts the ext4 loopback ON TOP of the
        // EROFS directory so /sysroot/composefs/meta.json is accessible.
        // Runs after sysroot.mount and before services that need the repo.
        let rebind_unit = format!(
            r#"[Unit]
Description=Rebind composefs loopback through EROFS overlay
DefaultDependencies=no
After=sysroot.mount
Before=local-fs.target
Requires=sysroot.mount

[Service]
Type=oneshot
ExecStart=/bin/mount /sysroot/composefs-loopback.ext4 /sysroot/composefs -t ext4 -o loop,ro
RemainAfterExit=yes

[Install]
WantedBy=local-fs.target
"#
        );
        let rebind_path = unit_dir.join("bootc-composefs-rebind.service");
        if !rebind_path.exists() {
            fs::write(&rebind_path, rebind_unit.as_bytes())
                .context("failed to write bootc-composefs-rebind.service")?;
            let lf_wants = etc_dir.join("systemd/system/local-fs.target.wants");
            fs::create_dir_all(&lf_wants)?;
            let rebind_link = lf_wants.join("bootc-composefs-rebind.service");
            if !rebind_link.exists() {
                std::os::unix::fs::symlink("../bootc-composefs-rebind.service", &rebind_link)?;
            }
            println!("[phase4] wrote bootc-composefs-rebind.service to deploy /etc");
        }
    }

    Ok(())
}

/// Drop GRUB / rpm-ostree artifacts that don't belong on a composefs +
/// systemd-boot deploy. These come from the source OS's /etc but reference
/// boot/state mechanisms the target no longer uses.
fn drop_ostree_era_etc_artifacts(etc_dir: &Path) {
    // Concrete known-cruft paths. Keep this tight — only paths that are
    // unambiguously misleading (lying state files) or actively wrong for
    // the new bootloader.
    let drops = [
        ".rpm-ostree-shadow-mode-fixed2.stamp",
        ".updated",
        "grub2.cfg",
        "grub2-efi.cfg",
        "grub.d",
    ];
    for name in &drops {
        let p = etc_dir.join(name);
        let exists = p.exists() || p.is_symlink();
        if !exists {
            continue;
        }
        let res = if p.is_dir() && !p.is_symlink() {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
        match res {
            Ok(()) => println!("[phase4] dropped OSTree-era /etc artifact: {}", name),
            Err(e) => eprintln!("[phase4] warning: failed to drop {}: {}", p.display(), e),
        }
    }
}

/// Ensure the TCP 22 SSH socket-activated listener is present in the deploy
/// /etc. Bluefin's sshd only binds Unix-local + vsock by default; this socket
/// provides the TCP listener the E2E test needs. The 3-way merge drops it when
/// baked into the OSTree factory (old==cur, new absent), so we recreate it
/// unconditionally after the merge.
fn ensure_e2e_ssh_socket(etc_dir: &Path) -> Result<()> {
    let systemd_dir = etc_dir.join("systemd/system");
    fs::create_dir_all(systemd_dir.join("sockets.target.wants"))?;

    fs::write(
        systemd_dir.join("e2e-sshd.socket"),
        "[Unit]\nDescription=E2E SSH TCP Socket (port 22)\n[Socket]\nListenStream=22\nAccept=yes\n[Install]\nWantedBy=sockets.target\n",
    )?;
    fs::write(
        systemd_dir.join("e2e-sshd@.service"),
        "[Unit]\nDescription=E2E SSH per-connection service\n[Service]\nExecStart=-/usr/bin/sshd -i -E /var/log/sshd-e2e.log -d\nStandardInput=socket\n",
    )?;

    let symlink = systemd_dir.join("sockets.target.wants/e2e-sshd.socket");
    if symlink.exists() || symlink.is_symlink() {
        let _ = fs::remove_file(&symlink);
    }
    std::os::unix::fs::symlink("../e2e-sshd.socket", &symlink)?;

    // Remove the sshd.service enablement symlink if it survived the merge.
    // e2e-sshd.socket provides TCP 22 via socket activation; having both
    // sshd.service (sshd -D) and e2e-sshd.socket on port 22 causes a port
    // conflict that kills the daemon process with 255/EXCEPTION.
    let sshd_enable = systemd_dir.join("multi-user.target.wants/sshd.service");
    if sshd_enable.exists() || sshd_enable.is_symlink() {
        fs::remove_file(&sshd_enable)?;
        println!("[phase4] removed sshd.service enablement (e2e-sshd.socket provides TCP 22)");
    }

    // Remove ostree-remount.service enablement — on composefs, OSTree bind
    // mounts are irrelevant and the service would fail or create stale mounts
    // under /sysroot/ostree (which we delete on commit).
    let remount_enable = systemd_dir.join("local-fs.target.wants/ostree-remount.service");
    if remount_enable.exists() || remount_enable.is_symlink() {
        fs::remove_file(&remount_enable)?;
        println!(
            "[phase4] removed ostree-remount.service enablement (composefs doesn't need OSTree bind mounts)"
        );
    }

    println!("[phase4] ensured e2e-sshd.socket in deploy /etc");
    Ok(())
}

/// Supplement identity DBs and critical config files in the merged deploy
/// /etc with target-image versions. Uses registry streaming (reliable) rather
/// than the EROFS mount, which zero-fills content past ~4KB threshold
/// (corrupting e.g. /etc/dbus-1/system.conf -> dbus fails to start, cascading
/// to polkit/logind/sshd).
///
/// Batch-extracts all needed files in a single pass through layers (fast:
/// ~8 min vs ~45 min for individual calls).
fn supplement_identity_dbs_from_registry(target_image: &str, etc_dir: &Path) -> Result<()> {
    let scratch =
        TempDir::new_in("/var/tmp").context("failed to create temp dir for identity-DB extract")?;
    // Batch all files into one single-pass extraction to avoid re-scanning
    // all 120 layers for each file.
    let mut id_bufs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut cfg_bufs: Vec<(PathBuf, PathBuf)> = Vec::new();

    // Identity DBs
    let id_names = ["passwd", "shadow", "group", "gshadow", "subuid", "subgid"];
    for name in &id_names {
        let src = PathBuf::from("/etc").join(name);
        let dst = scratch.path().join(name);
        id_bufs.push((src, dst));
    }

    // Critical EROFS-threshold config files (>~4KB, zero-filled by bare mount)
    let cfg_files = [
        ("/etc/dbus-1/system.conf", "dbus-1/system.conf"),
        ("/etc/dbus-1/session.conf", "dbus-1/session.conf"),
    ];
    for (src_path, rel_dst) in &cfg_files {
        let src = PathBuf::from(src_path);
        let dst = scratch.path().join(rel_dst);
        cfg_bufs.push((src, dst));
    }

    // Build the pairs slice from references to the owned PathBufs
    let mut pairs: Vec<(&Path, &Path)> = Vec::with_capacity(id_bufs.len() + cfg_bufs.len());
    for (src, dst) in &id_bufs {
        pairs.push((src.as_path(), dst.as_path()));
    }
    for (src, dst) in &cfg_bufs {
        pairs.push((src.as_path(), dst.as_path()));
    }

    // Single batch extract — one pass through all layers
    if let Err(e) = extract_files_via_registry(target_image, &pairs) {
        // Log but don't fail — the merge may have produced valid content for some files.
        eprintln!("[phase4] warning: batch registry extract failed: {e:#}");
    }

    // --- Supplement identity DBs (union-merge by first colon field) ---
    let mut supplemented = 0usize;
    for name in &id_names {
        let dakota_path = scratch.path().join(name);
        let merged_path = etc_dir.join(name);
        if !dakota_path.exists() {
            continue;
        }
        let dakota = fs::read_to_string(&dakota_path).unwrap_or_default();
        if dakota.trim().is_empty() {
            continue;
        }
        let current = fs::read_to_string(&merged_path).unwrap_or_default();
        let merged = line_union_by_first_colon(&current, &dakota);
        if merged != current {
            let perms = fs::metadata(&merged_path).ok().map(|m| m.permissions());
            fs::write(&merged_path, merged.as_bytes())
                .with_context(|| format!("failed to rewrite {}", merged_path.display()))?;
            if let Some(p) = perms {
                let _ = fs::set_permissions(&merged_path, p);
            }
            supplemented += 1;
        }
    }
    if supplemented > 0 {
        println!(
            "[phase4] supplemented {} identity-DB file(s) with target's system users",
            supplemented
        );
    }

    // --- Overwrite critical config files that EROFS zero-filled ---
    let cfg_names = [
        "/etc/dbus-1/system.conf",
        "/etc/dbus-1/session.conf",
    ];
    for src_path in &cfg_names {
        let rel_dst = src_path.strip_prefix("/etc/").unwrap_or(src_path);
        let scratch_path = scratch.path().join(rel_dst);
        let merged_path = etc_dir.join(rel_dst);
        if !scratch_path.exists() {
            continue;
        }
        let content = fs::read(&scratch_path).unwrap_or_default();
        if content.is_empty() || content.iter().all(|&b| b == 0) {
            continue;  // Registry also returned zeros — file truly absent
        }
        if let Some(parent) = merged_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&merged_path, &content)
            .with_context(|| format!("failed to write {}", merged_path.display()))?;
        println!("[phase4] overwrote {} with registry-extracted target version", rel_dst);
    }

    Ok(())
}



fn line_union_by_first_colon(current: &str, new: &str) -> String {
    use std::collections::HashSet;
    let key_of = |line: &str| line.split(':').next().unwrap_or("").to_string();
    let mut keys: HashSet<String> = HashSet::new();
    let mut out = String::with_capacity(current.len() + new.len());
    for line in current.lines() {
        if !line.is_empty() {
            keys.insert(key_of(line));
        }
        out.push_str(line);
        out.push('\n');
    }
    for line in new.lines() {
        if line.is_empty() {
            continue;
        }
        let k = key_of(line);
        if !keys.contains(&k) {
            out.push_str(line);
            out.push('\n');
            keys.insert(k);
        }
    }
    out
}

fn find_ostree_etc_default() -> Result<PathBuf> {
    let cmdline = fs::read_to_string("/proc/cmdline")?;
    for word in cmdline.split_whitespace() {
        if let Some(_ostree_arg) = word.strip_prefix("ostree=") {
            let deploy_base = Path::new("/sysroot/ostree/deploy/default/deploy");
            if deploy_base.exists() {
                for entry in fs::read_dir(deploy_base)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.ends_with(".0") && entry.path().is_dir() {
                        let usr_etc = entry.path().join("usr/etc");
                        if usr_etc.exists() {
                            return Ok(usr_etc);
                        }
                    }
                }
            }
            break;
        }
    }
    anyhow::bail!("could not locate OSTree deployment default /etc");
}

