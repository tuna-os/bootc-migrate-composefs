//! Phase 2: pull the target OCI image into the composefs repository.

use super::*;

// ---- Phase 2 ----

pub fn phase2_pull_image(
    store: &dyn crate::composefs::ComposefsStore,
    target_image: &str,
    dry_run: bool,
) -> Result<(String, String)> {
    println!("=== Phase 2: Pulling OCI image ===");

    if dry_run {
        println!("[DRY RUN] Would pull image: {}", target_image);
        return Ok(("dry-run-manifest".into(), "dry-run-config".into()));
    }

    println!("Pulling target image: {}...", target_image);
    let pull_output = store
        .pull_image(target_image, target_image)
        .context("failed to pull OCI image")?;

    // Also cache in podman storage so Phase 5 can fall back to podman artifact
    // extraction without a re-pull if the composefs overlay mount is unavailable.
    let podman_pull = Command::new("podman").args(["pull", target_image]).status();
    match podman_pull {
        Ok(s) if s.success() => println!("Image also cached in podman storage."),
        Ok(s) => eprintln!("[phase2] podman pull exited {s} — Phase 5 may need to re-pull"),
        Err(e) => eprintln!("[phase2] podman pull failed: {e} — Phase 5 may need to re-pull"),
    }

    let (manifest_opt, config_opt) = parse_pull_digests(&pull_output);
    let config_digest = config_opt.unwrap_or_default();
    // bootc's cfs oci pull output may omit the OCI manifest digest (1.13.0 prints
    // only `config <sha256>` + `verity <hash>`). bootc status/upgrade reads
    // `[image]/manifest_digest`, and the old code's "use the whole output" fallback
    // produced a MULTI-LINE value that corrupts the .origin ini (breaking
    // `bootc status` entirely). Prefer the real manifest digest from the
    // locally-cached image (Phase 2 podman pull); fall back to the config digest —
    // always a single-line sha256.
    let manifest_digest = manifest_opt
        .or_else(|| podman_manifest_digest(target_image))
        .unwrap_or_else(|| config_digest.clone());
    println!(
        "Target image pulled. Manifest: {}, Config: {}",
        manifest_digest, config_digest
    );
    Ok((manifest_digest, config_digest))
}

/// Parse `bootc internals cfs oci pull` stdout into `(manifest_digest, config_digest)`.
///
/// Handles the 1.13.0 format that prints only `config <sha256>` + `verity <hash>`
/// (no `manifest` line). Critically, never yields a multi-line `manifest_digest` —
/// a newline in that value corrupts the deployment `.origin` ini and breaks
/// `bootc status`. The config digest falls back to the first `sha256:` token.
pub(crate) fn parse_pull_digests(pull_output: &str) -> (Option<String>, Option<String>) {
    let mut manifest = None;
    let mut config = None;
    for line in pull_output.lines() {
        let t = line.trim();
        if let Some(r) = t.strip_prefix("manifest ") {
            manifest = Some(r.trim().to_string());
        } else if let Some(r) = t.strip_prefix("config ") {
            config = Some(r.trim().to_string());
        }
    }
    // A valid digest is a single non-empty token; reject anything else.
    let manifest = manifest.filter(|m| !m.is_empty() && !m.contains(char::is_whitespace));
    let config = config
        .filter(|c| !c.is_empty() && !c.contains(char::is_whitespace))
        .or_else(|| {
            pull_output
                .split_whitespace()
                .find(|x| x.starts_with("sha256:"))
                .map(String::from)
        });
    (manifest, config)
}

/// Read the OCI manifest digest (`sha256:…`) of a locally-cached image via
/// `podman image inspect`. Returns None if podman/the image is unavailable.
fn podman_manifest_digest(image: &str) -> Option<String> {
    let out = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Digest}}", image])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let d = String::from_utf8_lossy(&out.stdout).trim().to_string();
    d.starts_with("sha256:").then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pull_digests_kanpur_format_no_manifest_line() {
        // bootc 1.13.0: config + verity, no "manifest" line. The old code used the
        // whole multi-line output as manifest_digest, corrupting the .origin ini.
        let out = "config sha256:39f5731c23efd9\nverity b0e7a7dabb84cb9d";
        let (manifest, config) = parse_pull_digests(out);
        // No usable manifest digest from this output (caller falls back to podman).
        assert_eq!(manifest, None);
        // Config digest is parsed clean and single-line.
        assert_eq!(config.as_deref(), Some("sha256:39f5731c23efd9"));
    }

    #[test]
    fn parse_pull_digests_with_manifest_line() {
        let out = "manifest sha256:aaa\nconfig sha256:bbb\nverity ccc";
        let (manifest, config) = parse_pull_digests(out);
        assert_eq!(manifest.as_deref(), Some("sha256:aaa"));
        assert_eq!(config.as_deref(), Some("sha256:bbb"));
    }

    #[test]
    fn parse_pull_digests_single_line_fallback() {
        // Single bare digest line → config via the sha256: token fallback.
        let (manifest, config) = parse_pull_digests("sha256:deadbeef");
        assert_eq!(manifest, None);
        assert_eq!(config.as_deref(), Some("sha256:deadbeef"));
    }

    #[test]
    fn parse_pull_digests_never_returns_multiline() {
        // Even a malformed multi-token "manifest" line must be rejected, never
        // passed through to corrupt the .origin ini.
        let (manifest, _) = parse_pull_digests("manifest sha256:x extra junk");
        assert_eq!(manifest, None);
    }

    #[test]
    fn phase2_runs_against_mock_store() {
        use crate::composefs::MockComposefsStore;
        let store = MockComposefsStore::default();
        // example.invalid never resolves, so the podman-cache side effect
        // fails fast and the phase proceeds on the mock's pull output alone.
        let (manifest, config) =
            phase2_pull_image(&store, "example.invalid/mock:latest", false).unwrap();
        assert!(manifest.starts_with("sha256:0000"));
        assert!(config.starts_with("sha256:1111"));
        let calls = store.calls.lock().unwrap();
        assert_eq!(calls.as_slice(), ["pull_image example.invalid/mock:latest"]);
        // dry_run never touches the store.
        drop(calls);
        let dry = MockComposefsStore::default();
        let _ = phase2_pull_image(&dry, "example.invalid/mock:latest", true).unwrap();
        assert!(dry.calls.lock().unwrap().is_empty());
    }
}
