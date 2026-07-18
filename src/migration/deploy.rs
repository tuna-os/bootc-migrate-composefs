//! Phase 4: stage the deployment — /etc merge, /var migration, state root.

use super::*;

pub fn phase4_stage_deploy(
    verity: &VerityDigest,
    target_image: &str,
    manifest_digest: &str,
    config_digest: &str,
    sealed_config: &str,
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

    // Idempotency: skip if already staged with valid .origin.
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

    // 3-way /etc merge
    println!("Performing 3-way /etc merge...");
    if let Err(e) = perform_etc_merge(target_image, sealed_config, &etc_dir) {
        eprintln!(
            "3-way /etc merge failed ({}), falling back to flat /etc copy.",
            e
        );
        xattr::copy_dir_all_with_xattrs("/etc", &etc_dir)
            .context("failed to copy /etc (fallback)")?;
    }

    // Stage /var symlink
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
    //   inspect` is unreliable in our flow).
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

    // Handle /var migration
    phase4_var_migration(&etc_dir, dry_run)?;

    // For XFS roots, the composefs repo lives in an ext4 loopback file; the
    // booted system must mount it at /sysroot/composefs so `bootc status` and
    // day-2 updates can read the repo (the initrd mount is torn down at
    // switch-root). Install a runtime mount unit into the deployment's /etc.
    if Path::new("/sysroot/composefs-loopback.ext4").exists()
        && let Err(e) = write_runtime_composefs_loopback_mount(&etc_dir)
    {
        eprintln!("[phase4] Warning: failed to install runtime composefs mount: {e:#}");
    }

    Ok(deploy_dir)
}

/// Install a systemd mount unit into the deployment's /etc so the booted system
/// loop-mounts the composefs ext4 store at /sysroot/composefs. Idempotent with
/// any mount that survives the initrd: systemd treats an already-mounted target
/// as active.
fn write_runtime_composefs_loopback_mount(etc_dir: &Path) -> Result<()> {
    let unit_dir = etc_dir.join("systemd/system");
    fs::create_dir_all(&unit_dir)?;
    fs::write(
        unit_dir.join("sysroot-composefs.mount"),
        "[Unit]\n\
         Description=ComposeFS Loopback Store (runtime)\n\
         DefaultDependencies=no\n\
         After=sysroot.mount\n\
         Before=local-fs.target\n\
         \n\
         [Mount]\n\
         What=/sysroot/composefs-loopback.ext4\n\
         Where=/sysroot/composefs\n\
         Type=ext4\n\
         Options=loop,ro\n\
         \n\
         [Install]\n\
         WantedBy=local-fs.target\n",
    )?;
    let wants_dir = unit_dir.join("local-fs.target.wants");
    fs::create_dir_all(&wants_dir)?;
    let link = wants_dir.join("sysroot-composefs.mount");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink("../sysroot-composefs.mount", &link)
        .context("failed to enable runtime sysroot-composefs.mount")?;
    Ok(())
}

fn phase4_var_migration(etc_dir: &Path, _dry_run: bool) -> Result<()> {
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
    xattr::copy_dir_all_with_xattrs(source_var, target_var)
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
fn perform_etc_merge(target_image: &str, sealed_config: &str, etc_dir: &Path) -> Result<()> {
    let temp_mount =
        TempDir::new_in("/var/tmp").context("failed to create temp mount directory")?;
    let mut mount_path = temp_mount.path().to_path_buf();

    // Mount the target rootfs via bootc's composefs overlay using the *sealed
    // config digest* (not the rootfs verity): `cfs oci mount` looks up
    // `streams/oci-config-<sealed-config>`, so the rootfs verity would miss and
    // drop us to a raw EROFS mount that zero-fills file content above the inline
    // threshold. With the sealed digest the overlay exposes real content, so we
    // can read /etc straight off the mount (and validate prune symlink targets).
    //
    // On hosts where the composefs overlay mounts into bootc's private namespace
    // (see phase5_setup_bootloader), the mount is empty here. Fall back to a
    // `podman image mount` of the already-cached image — local, real content, and
    // no dependency on reaching the registry mid-migration.
    let composefs_mounted = match mount_image(sealed_config, &mount_path) {
        Ok(()) if mount_path.join("etc").is_dir() => true,
        _ => {
            eprintln!(
                "[phase4] composefs /etc mount unavailable; falling back to podman image mount"
            );
            false
        }
    };
    let _cfs_guard = if composefs_mounted {
        Some(MountGuard::new(&mount_path))
    } else {
        None
    };
    let _podman_guard = if composefs_mounted {
        None
    } else {
        let pm = PodmanImageMount::new(target_image)
            .context("composefs /etc mount unavailable and podman image mount fallback failed")?;
        println!(
            "[phase4] using podman image mount at {} for /etc",
            pm.path.display()
        );
        mount_path = pm.path.clone();
        Some(pm)
    };

    let old_default_etc = find_ostree_etc_default()?;
    let current_etc = Path::new("/etc");

    // Use the target's /etc straight off the mount (real content). The registry
    // stream is kept only as a last-resort fallback for when /etc is somehow
    // absent from both the composefs overlay and the podman mount.
    // (The temp dir is held to function scope so it outlives merge_etc_files.)
    let registry_etc_temp =
        TempDir::new_in("/var/tmp").context("failed to create temp dir for registry /etc")?;
    let registry_etc = registry_etc_temp.path().to_path_buf();
    let mount_etc = mount_path.join("etc");
    let new_default_etc = if mount_etc
        .read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
    {
        println!("[phase4] using mounted /etc for merge source");
        mount_etc
    } else {
        println!("[phase4] /etc absent from mount; streaming target /etc from registry...");
        extract_subtree_via_registry(target_image, "etc/", &registry_etc)
            .context("registry /etc extraction failed")?;
        registry_etc
    };

    crate::mergetc::merge_etc_files(&old_default_etc, current_etc, &new_default_etc, etc_dir)
        .context("3-way /etc merge failed")?;

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

/// Legacy single-DB supplement path. Kept for callers that don't want the full
/// `/etc` subtree; not used by `perform_etc_merge` anymore since the full
/// subtree extract subsumes it.
#[allow(dead_code)]
fn supplement_identity_dbs_from_registry(target_image: &str, etc_dir: &Path) -> Result<()> {
    let scratch =
        TempDir::new_in("/var/tmp").context("failed to create temp dir for identity-DB extract")?;
    let scratch_etc = scratch.path().join("etc");
    fs::create_dir_all(&scratch_etc).context("failed to create scratch etc dir")?;

    // Try each file individually; tolerate "missing in image" because not
    // every bootc target ships every identity DB (Dakota has no /etc/subuid
    // or /etc/subgid). Any other error from a given file is logged and the
    // others continue.
    let names = ["passwd", "shadow", "group", "gshadow", "subuid", "subgid"];
    for name in &names {
        let src = PathBuf::from("/etc").join(name);
        let dst = scratch_etc.join(name);
        let pair = [(src.as_path(), dst.as_path())];
        if let Err(e) = extract_files_via_registry(target_image, &pair) {
            let es = format!("{e:#}");
            if es.contains("missing files") || es.contains("No such file") {
                // Image doesn't ship this file; that's fine.
                continue;
            }
            eprintln!("[phase4] warning: skopeo extract of /etc/{name} failed: {es}");
        }
    }

    let mut supplemented = 0usize;
    for name in &names {
        let dakota_path = scratch_etc.join(name);
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
            // Permissions on shadow/gshadow must stay 000; the existing file
            // already has them, so write in place and preserve mode/xattrs.
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
    Ok(())
}

#[allow(dead_code)]
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
