use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::OnceLock;

/// The system composefs store. During migration this is writable: on btrfs it is
/// a plain directory on `/sysroot`; on XFS it is the ext4-verity loopback mounted
/// here by `setup_composefs_loopback_if_needed`.
const STORE: &str = "/sysroot/composefs";

/// Remembers whether the store was (re)built with the TARGET image's bootc, so
/// `create_image`/`seal` use the same bootc that wrote the store. Set by
/// `pull_image`.
static USE_TARGET_BOOTC: OnceLock<bool> = OnceLock::new();

/// Run `bootc internals cfs --system oci <args>` with the host (source) bootc.
/// This is the fast path: it runs natively, so on btrfs it reflinks the image's
/// blobs into the store (near-zero extra disk) with no container overhead.
fn host_cfs_oci(args: &[&str]) -> Result<Output> {
    Command::new("bootc")
        .args(["internals", "cfs", "--system", "oci"])
        .args(args)
        .output()
        .context("failed to execute host bootc internals cfs oci")
}

/// Run `bootc internals cfs --repo <store> <args>` with the **target image's own
/// bootc** (via podman), so the store is written in the format the migrated
/// system's bootc reads at runtime. Used only when the host bootc is too old to
/// produce a target-readable store (see `pull_image`). The host container storage
/// is bind-mounted so the image is read from the local cache (Phase 2 `podman
/// pull`) rather than re-downloaded.
fn target_cfs(target_image: &str, args: &[&str]) -> Result<Output> {
    Command::new("podman")
        .args([
            "run",
            "--rm",
            "--privileged",
            "--net=host",
            "--security-opt",
            "label=disable",
            "-v",
        ])
        .arg(format!("{STORE}:{STORE}"))
        .arg("-v")
        .arg("/var/lib/containers/storage:/var/lib/containers/storage")
        .arg(target_image)
        .args(["bootc", "internals", "cfs", "--repo", STORE])
        .args(args)
        .output()
        .context("failed to execute target-image bootc via podman")
}

/// True if the store has at least one `oci-manifest-*` stream — the marker that a
/// pull produced a deployment the target bootc can read (`bootc status`/`upgrade`
/// open `streams/oci-manifest-<digest>`). Older bootc (≤1.13) writes config+layer
/// streams but no manifest stream.
fn manifest_stream_present() -> bool {
    std::fs::read_dir(format!("{STORE}/streams"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("oci-manifest-"))
        })
        .unwrap_or(false)
}

/// Remove the store's content so it can be rebuilt cleanly with the target bootc.
/// (The target bootc can't reconcile a store an older bootc already wrote —
/// "Expected exactly 1 external object in splitstream" — so a partial reuse fails.)
fn wipe_store() -> Result<()> {
    for sub in ["objects", "streams", "images"] {
        let p = format!("{STORE}/{sub}");
        if Path::new(&p).exists() {
            std::fs::remove_dir_all(&p).with_context(|| format!("wiping {p}"))?;
        }
    }
    // Drop the (host-written) meta.json so the target bootc init writes its own.
    let _ = std::fs::remove_file(format!("{STORE}/meta.json"));
    Ok(())
}

/// Normalize an image reference to a `docker://` (or already-prefixed) transport.
fn with_transport(image_ref: &str) -> String {
    if image_ref.contains("://") {
        image_ref.to_string()
    } else {
        format!("docker://{}", image_ref)
    }
}

pub fn pull_image(target_image: &str, image_ref: &str) -> Result<String> {
    // Fast path: pull with the host (source) bootc. On a current source this
    // reflinks blobs into the store and produces a target-readable deployment.
    let docker_ref = with_transport(image_ref);
    let host_out = host_cfs_oci(&["pull", &docker_ref])?;
    if host_out.status.success() && manifest_stream_present() {
        let _ = USE_TARGET_BOOTC.set(false);
        return Ok(String::from_utf8_lossy(&host_out.stdout).to_string());
    }

    // The host bootc is too old: its pull either failed or produced no
    // oci-manifest stream, which leaves `bootc status`/`upgrade` broken after an
    // otherwise-successful migration. Rebuild the store with the target image's
    // own bootc so it's written in the format the migrated system expects.
    if host_out.status.success() {
        eprintln!(
            "[cfs] host bootc produced no oci-manifest stream (too old for the \
             target); rebuilding the store with the target image's bootc"
        );
    } else {
        eprintln!(
            "[cfs] host bootc pull failed ({}); rebuilding the store with the \
             target image's bootc",
            String::from_utf8_lossy(&host_out.stderr).trim()
        );
    }

    wipe_store()?;
    let init = target_cfs(target_image, &["init"])?;
    if !init.status.success() {
        return Err(anyhow!(
            "target bootc repo init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        ));
    }

    // Prefer the locally-cached image (Phase 2 `podman pull`) so we don't make a
    // second on-disk copy; fall back to the registry transport.
    let cs_ref = format!("containers-storage:{}", image_ref);
    let mut out = target_cfs(target_image, &["oci", "pull", &cs_ref])?;
    if !out.status.success() {
        eprintln!(
            "[cfs] containers-storage pull failed ({}); retrying via registry",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        out = target_cfs(target_image, &["oci", "pull", &docker_ref])?;
    }
    if !out.status.success() {
        return Err(anyhow!(
            "pull failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let _ = USE_TARGET_BOOTC.set(true);
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Whether `pull_image` rebuilt the store with the target bootc. `create_image`
/// and `seal` must use the same bootc that wrote the store.
fn use_target() -> bool {
    *USE_TARGET_BOOTC.get().unwrap_or(&false)
}

pub fn create_image(target_image: &str, image_id: &str) -> Result<String> {
    let output = if use_target() {
        target_cfs(target_image, &["oci", "create-image", image_id])?
    } else {
        host_cfs_oci(&["create-image", image_id])?
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("create-image failed: {}", stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Post-build verification (#3): confirm the finished store is readable by the
/// TARGET image's bootc — the binary that reads it at runtime — so a bootc
/// format skew can never silently ship a migration that boots but breaks
/// `bootc status`/`bootc upgrade`.
///
/// How we know it's readable:
/// - In the rebuild path (`use_target()`), the **target bootc itself** wrote the
///   store (its pull/create/seal succeeded), so it is target-readable by
///   construction.
/// - In the host path, we assert the `oci-manifest-*` stream is present — the
///   exact artifact the target's status/upgrade open
///   (`streams/oci-manifest-<digest>`). Its absence is the known symptom of an
///   older source bootc and *guarantees* breakage.
///
/// Either way, a missing manifest stream here is a hard failure with a clear
/// message — we refuse to declare success on a store the target can't read.
/// This also future-proofs us: if a later bootc changes the format such that
/// neither path produces the stream, the migration fails loudly instead of
/// silently regressing updates.
pub fn verify_store_target_readable(target_image: &str) -> Result<()> {
    if manifest_stream_present() {
        return Ok(());
    }
    Err(anyhow!(
        "post-build verification failed: the composefs store has no oci-manifest \
         stream, so the target image's bootc ({target_image}) cannot read this \
         deployment — `bootc status`/`bootc upgrade` would break after reboot. \
         This is a bootc format incompatibility between the source bootc and the \
         target that was not resolved by rebuilding the store. Refusing to ship a \
         silently-broken migration; update the source system's bootc and retry."
    ))
}

pub fn seal_image(target_image: &str, image_id: &str) -> Result<String> {
    let output = if use_target() {
        target_cfs(target_image, &["oci", "seal", image_id])?
    } else {
        host_cfs_oci(&["seal", image_id])?
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("seal failed: {}", stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
