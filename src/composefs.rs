use anyhow::{Context, Result, anyhow};
use std::process::{Command, Output};

/// The system composefs store. During migration this is writable: on btrfs it is
/// a plain directory on `/sysroot`; on XFS it is the ext4-verity loopback mounted
/// here by `setup_composefs_loopback_if_needed`.
const STORE: &str = "/sysroot/composefs";

/// Run `bootc internals cfs --repo <store> oci <op_args>` using the **target
/// image's own bootc** (via podman), so the composefs store is written in the
/// exact on-disk format the migrated system's bootc will read at runtime.
///
/// Why not the host's bootc: an older source bootc can write a store that the
/// (newer) target bootc cannot fully read — e.g. it omits the `oci-manifest`
/// streams the target expects — which silently breaks `bootc status` / `bootc
/// upgrade` *after* a successful migration (the system still boots). Running the
/// target's bootc against the bind-mounted store removes that version skew.
///
/// Falls back to the host bootc only if podman itself can't be executed (better a
/// possibly-skewed store than a failed migration); a warning is printed so the
/// skew is visible. A non-zero exit from the containerized bootc is NOT a
/// fallback trigger — it's surfaced as a real error.
fn run_cfs(target_image: &str, cfs_args: &[&str]) -> Result<Output> {
    let mut podman = Command::new("podman");
    podman
        .args([
            "run",
            "--rm",
            "--privileged",
            "--net=host",
            // The store may carry SELinux labels the container can't write
            // through; disabling label confinement lets bootc write objects.
            "--security-opt",
            "label=disable",
            "-v",
        ])
        // Bind the store at the same path inside so --repo matches host paths.
        .arg(format!("{STORE}:{STORE}"))
        // Bind the host's container image storage so `pull` can read the image
        // straight from the local cache (Phase 2 `podman pull`) via the
        // `containers-storage:` transport, instead of re-downloading and
        // re-unpacking it into the container's own storage (which triples the
        // image's disk footprint and ENOSPCs on tight disks).
        .arg("-v")
        .arg("/var/lib/containers/storage:/var/lib/containers/storage")
        .arg(target_image)
        .args(["bootc", "internals", "cfs", "--repo", STORE])
        .args(cfs_args);

    match podman.output() {
        Ok(out) => Ok(out),
        Err(e) => {
            eprintln!(
                "[cfs] could not run the target image's bootc via podman ({e}); \
                 falling back to the host bootc. If the host bootc is older than \
                 the target's, `bootc status`/`upgrade` may not work post-migration."
            );
            Command::new("bootc")
                .args(["internals", "cfs", "--system"])
                .args(cfs_args)
                .output()
                .context("failed to execute host bootc internals cfs")
        }
    }
}

/// `bootc internals cfs … oci <op_args>` via the target image's bootc.
fn run_cfs_oci(target_image: &str, op_args: &[&str]) -> Result<Output> {
    let mut args = vec!["oci"];
    args.extend_from_slice(op_args);
    run_cfs(target_image, &args)
}

/// Initialize the composefs store with the target image's bootc if it isn't
/// already (XFS gets a `meta.json` from the loopback setup; btrfs does not).
/// `bootc … cfs … oci pull` requires an initialized repo.
fn ensure_store_initialized(target_image: &str) -> Result<()> {
    if std::path::Path::new(STORE).join("meta.json").exists() {
        return Ok(());
    }
    let out = run_cfs(target_image, &["init"])?;
    if !out.status.success() {
        return Err(anyhow!(
            "cfs repo init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// Normalize an image reference to a `docker://` (or already-prefixed) transport.
fn with_transport(image_ref: &str) -> String {
    // Only add docker:// when there's no explicit transport (e.g. docker://,
    // containers-storage:, oci-archive:). A lone colon (a registry port) is not
    // a transport prefix.
    if image_ref.contains("://") {
        image_ref.to_string()
    } else {
        format!("docker://{}", image_ref)
    }
}

pub fn pull_image(target_image: &str, image_ref: &str) -> Result<String> {
    ensure_store_initialized(target_image)?;
    // Prefer the locally-cached image (Phase 2 `podman pull`) via the
    // containers-storage transport — bootc imports its layers straight into the
    // cfs store with no second download/unpack. Only if that's unavailable do we
    // fall back to the registry (correct, but needs disk for an extra copy).
    let cs_ref = format!("containers-storage:{}", image_ref);
    let output = run_cfs_oci(target_image, &["pull", &cs_ref])?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let cs_err = String::from_utf8_lossy(&output.stderr).into_owned();
    eprintln!(
        "[cfs] containers-storage pull failed ({}); retrying via registry",
        cs_err.trim()
    );
    let final_ref = with_transport(image_ref);
    let output = run_cfs_oci(target_image, &["pull", &final_ref])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("pull failed: {}", stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn create_image(target_image: &str, image_id: &str) -> Result<String> {
    let output = run_cfs_oci(target_image, &["create-image", image_id])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("create-image failed: {}", stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn seal_image(target_image: &str, image_id: &str) -> Result<String> {
    let output = run_cfs_oci(target_image, &["seal", image_id])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("seal failed: {}", stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
