use crate::VerityDigest;
use crate::migration::phase4::patch_boot_digest_in_content;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Check if a directory has any files (non-recursive). Used by subtree extraction
/// to verify that tar actually extracted content.
fn has_files(dir: &Path) -> bool {
    if let Ok(mut rd) = fs::read_dir(dir) {
        rd.any(|e| e.is_ok())
    } else {
        false
    }
}

fn try_extract_from_podman(image_ref: &str, files: &[(&Path, &Path)]) -> bool {
    // Check if podman has the image.
    let has = Command::new("podman")
        .args(["image", "exists", image_ref])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !has {
        return false;
    }

    // Create a container from the image (don't run it).
    let create = Command::new("podman")
        .args(["create", "--name", "migrate-extract", image_ref])
        .output();
    let container_id = match create {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return false,
    };
    if container_id.is_empty() {
        return false;
    }

    // Copy each requested file out of the container.
    // podman cp fails on vfat (ESP) because it tries to set xattrs.
    // Extract to a temp dir first, then plain-cp to the final destination.
    let tmp_dir = match TempDir::new_in("/var/tmp") {
        Ok(t) => t,
        Err(_) => {
            let _ = Command::new("podman")
                .args(["rm", "-f", "migrate-extract"])
                .status();
            return false;
        }
    };
    let mut all_ok = true;
    for (src, dst) in files {
        let src_str = src.to_string_lossy();
        let basename = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "extracted".into());
        let tmp_dst = tmp_dir.path().join(&basename);

        println!("[extract] podman cp {} -> /var/tmp/...", src_str);
        let cp = Command::new("podman")
            .args([
                "cp",
                &format!("{}:{}", container_id, src_str),
                tmp_dst.to_str().unwrap_or(""),
            ])
            .status();
        match cp {
            Ok(s) if s.success() => {
                if let Ok(meta) = fs::metadata(&tmp_dst) {
                    if meta.len() > 0 {
                        // Plain copy to final destination (tolerates vfat).
                        if fs::copy(&tmp_dst, dst).is_ok() {
                            continue;
                        }
                    }
                }
            }
            _ => {}
        }
        all_ok = false;
        break;
    }

    // Clean up the container.
    let _ = Command::new("podman")
        .args(["rm", "-f", "migrate-extract"])
        .status();

    if all_ok {
        println!("[extract] all files extracted from local podman cache");
    }
    all_ok
}

pub(crate) fn extract_files_via_registry(image_ref: &str, files: &[(&Path, &Path)]) -> Result<()> {
    let file_list: Vec<String> = files.iter().map(|(s, _)| s.display().to_string()).collect();
    println!(
        "[extract] Extracting {} file(s): {}",
        files.len(),
        file_list.join(", ")
    );

    // Try podman cache first — extracts from local storage in seconds vs
    // downloading 120 layers from ghcr.io.
    if try_extract_from_podman(image_ref, files) {
        return Ok(());
    }

    println!("[extract] podman cache miss — falling back to registry download");
    let endpoint = RegistryEndpoint::resolve(image_ref)?;
    let manifest_json = endpoint.fetch_manifest(&endpoint.reference)?;

    // Manifest list / OCI index → resolve to current-arch manifest.
    let layers_manifest = if endpoint.is_manifest_index(&manifest_json) {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let entries = manifest_json
            .get("manifests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("manifest index has no manifests array"))?;
        let pick = entries
            .iter()
            .find(|m| {
                m.get("platform")
                    .and_then(|p| p.get("architecture"))
                    .and_then(|a| a.as_str())
                    == Some(arch)
            })
            .ok_or_else(|| anyhow!("manifest index has no entry for arch {}", arch))?;
        let digest = pick
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("manifest index entry has no digest"))?;
        endpoint.fetch_manifest(digest)?
    } else {
        manifest_json
    };

    let layers = layers_manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("image manifest has no layers array"))?;

    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-extract-")
        .tempdir_in("/var/tmp")
        .context("failed to create /var/tmp scratch dir for layer streaming")?;

    let total_layers = layers.len();
    let mut layer_idx = 0usize;
    let mut remaining: Vec<(&Path, &Path)> = files.iter().copied().collect();
    for layer in layers.iter().rev() {
        layer_idx += 1;
        if remaining.is_empty() {
            break;
        }
        let digest = layer
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer entry has no digest"))?;
        let short_digest = &digest[..digest.len().min(12)];

        println!(
            "[registry] Downloading layer {}/{} ({})...",
            layer_idx, total_layers, short_digest
        );

        // Download just this one layer, extract from it, drop it.
        let blob_path = scratch.path().join("layer.blob");
        endpoint
            .download_blob(digest, &blob_path)
            .with_context(|| format!("failed to fetch layer {}", digest))?;

        let blob_size = fs::metadata(&blob_path).map(|m| m.len()).unwrap_or(0);
        println!(
            "[registry] Layer {}/{} downloaded ({} MB), extracting...",
            layer_idx,
            total_layers,
            blob_size / 1_048_576
        );

        let mut still_needed: Vec<(&Path, &Path)> = Vec::new();
        for (src, dst) in remaining.into_iter() {
            if extract_one_from_layer(&blob_path, src, dst)? {
                println!(
                    "[registry]   ✓ found {} in layer {}",
                    src.display(),
                    short_digest
                );
            } else {
                still_needed.push((src, dst));
            }
        }
        remaining = still_needed;
        let _ = fs::remove_file(&blob_path);
    }

    if !remaining.is_empty() {
        let missing: Vec<String> = remaining
            .iter()
            .map(|(s, _)| s.display().to_string())
            .collect();
        return Err(anyhow!(
            "target image is missing files: {}",
            missing.join(", ")
        ));
    }
    Ok(())
}

/// Compute sha256(vmlinuz || initrd) and patch the `.origin` file's
/// `boot_digest = …` line. `bootc status` uses this digest to set the soft
/// reboot capability; without it, status bails with
/// "Could not find boot digest for deployment".
pub(crate) fn patch_origin_boot_digest(
    verity: &VerityDigest,
    vmlinuz: &Path,
    initrd: &Path,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let v = fs::read(vmlinuz).with_context(|| format!("reading vmlinuz {}", vmlinuz.display()))?;
    let i = fs::read(initrd).with_context(|| format!("reading initrd {}", initrd.display()))?;
    hasher.update(&v);
    hasher.update(&i);
    let raw = hasher.finalize();
    let mut digest = String::with_capacity(raw.len() * 2);
    for b in raw {
        digest.push_str(&format!("{:02x}", b));
    }

    let origin_path = Path::new("/sysroot/state/deploy")
        .join(verity.as_hex())
        .join(format!("{}.origin", verity.as_hex()));
    let text = fs::read_to_string(&origin_path)
        .with_context(|| format!("reading origin {}", origin_path.display()))?;
    let patched = patch_boot_digest_in_content(&text, &digest)?;
    fs::write(&origin_path, &patched)
        .with_context(|| format!("writing patched origin {}", origin_path.display()))?;
    println!("[phase5] patched origin boot_digest = {}", digest);
    Ok(())
}

/// Stream OCI layers oldest→newest and extract everything matching `subtree`
/// (e.g. `etc/`) into `dst_dir`. Later layers' files overwrite earlier ones,
/// matching how the container runtime composes the rootfs. Whiteouts
/// (`.wh.*`) are ignored — at worst we'll keep a few extra stale files in
/// `etc/`, which doesn't break anything we care about.
pub(crate) fn extract_subtree_via_registry(
    image_ref: &str,
    subtree: &str,
    dst_dir: &Path,
) -> Result<()> {
    let endpoint = RegistryEndpoint::resolve(image_ref)?;
    let manifest_json = endpoint.fetch_manifest(&endpoint.reference)?;
    let layers_manifest = if endpoint.is_manifest_index(&manifest_json) {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let entries = manifest_json
            .get("manifests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("manifest index has no manifests array"))?;
        let pick = entries
            .iter()
            .find(|m| {
                m.get("platform")
                    .and_then(|p| p.get("architecture"))
                    .and_then(|a| a.as_str())
                    == Some(arch)
            })
            .ok_or_else(|| anyhow!("manifest index has no entry for arch {}", arch))?;
        let digest = pick
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("manifest index entry has no digest"))?;
        endpoint.fetch_manifest(digest)?
    } else {
        manifest_json
    };
    let layers = layers_manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("image manifest has no layers array"))?;

    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-subtree-")
        .tempdir_in("/var/tmp")
        .context("failed to create /var/tmp scratch dir for subtree streaming")?;

    fs::create_dir_all(dst_dir)
        .with_context(|| format!("failed to create subtree destination {}", dst_dir.display()))?;

    let total_layers = layers.len();
    println!(
        "[registry] Extracting subtree {} from {} ({} layer(s))...",
        subtree, image_ref, total_layers
    );

    // Iterate oldest → newest so later writes win.
    let mut layer_idx = 0usize;
    for layer in layers.iter() {
        layer_idx += 1;
        let digest = layer
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer entry has no digest"))?;
        let short_digest = &digest[..digest.len().min(12)];

        println!(
            "[registry] Downloading subtree layer {}/{} ({})...",
            layer_idx, total_layers, short_digest
        );

        let blob_path = scratch.path().join("layer.blob");
        endpoint
            .download_blob(digest, &blob_path)
            .with_context(|| format!("failed to fetch layer {}", digest))?;

        let blob_size = fs::metadata(&blob_path).map(|m| m.len()).unwrap_or(0);
        println!(
            "[registry] Subtree layer {}/{} downloaded ({} MB), extracting...",
            layer_idx,
            total_layers,
            blob_size / 1_048_576
        );

        // tar will silently produce no output if the prefix is absent in this layer.
        // --strip-components=1 drops the leading directory we asked for so the
        // contents land directly under dst_dir (we want dst_dir to be the merged
        // /etc, not dst_dir/etc). OCI layer tars use either `./etc/...` or `etc/...`
        // path formats; we try both candidates.
        let normalized = subtree.trim_end_matches('/').trim_start_matches('/');
        for candidate in [format!("./{}", normalized), normalized.to_string()] {
            let output = Command::new("tar")
                .args([
                    "-xaf",
                    blob_path
                        .to_str()
                        .ok_or_else(|| anyhow!("invalid blob path"))?,
                    "-C",
                    dst_dir
                        .to_str()
                        .ok_or_else(|| anyhow!("invalid dst path"))?,
                    "--overwrite",
                    "--no-same-owner",
                    "--strip-components=1",
                    &candidate,
                ])
                .output()
                .context("failed to execute tar for subtree extraction")?;
            if output.status.success() && has_files(dst_dir) {
                break;
            }
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!(
                    "[registry] tar extract of '{}' from layer {} failed: {}",
                    candidate,
                    short_digest,
                    stderr.trim()
                );
            }
        }
        let _ = fs::remove_file(&blob_path);
    }
    Ok(())
}

/// Resolved registry endpoint: base URL (scheme + host), repository, reference, and
/// optional Bearer token. Built once per image and reused for the manifest + every
/// blob fetch.
struct RegistryEndpoint {
    base_url: String,
    repo: String,
    reference: String,
    bearer: Option<String>,
}

impl RegistryEndpoint {
    fn resolve(image_ref: &str) -> Result<Self> {
        let (host, repo, reference) = parse_image_ref(image_ref)?;

        // Pick http for plain non-standard ports (local dev registries), https otherwise.
        // We probe /v2/ to confirm and to discover any bearer challenge.
        let candidates: &[&str] = if host_is_plain_http(&host) {
            &["http"]
        } else {
            &["https", "http"]
        };

        for scheme in candidates {
            let base = format!("{}://{}", scheme, host);
            match probe_v2(&base, &repo) {
                Ok(bearer) => {
                    return Ok(RegistryEndpoint {
                        base_url: base,
                        repo,
                        reference,
                        bearer,
                    });
                }
                Err(_) => continue,
            }
        }
        Err(anyhow!(
            "could not reach registry {} (tried {:?})",
            host,
            candidates
        ))
    }

    fn fetch_manifest(&self, reference: &str) -> Result<serde_json::Value> {
        let url = format!("{}/v2/{}/manifests/{}", self.base_url, self.repo, reference);
        let mut args: Vec<String> = vec![
            "-sSL".into(),
            "--fail".into(),
            "-H".into(),
            "Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json".into(),
        ];
        if let Some(token) = &self.bearer {
            args.push("-H".into());
            args.push(format!("Authorization: Bearer {}", token));
        }
        args.push(url);
        let out = Command::new("curl")
            .args(&args)
            .output()
            .context("failed to invoke curl for manifest fetch")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl manifest fetch failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        serde_json::from_slice(&out.stdout).context("failed to parse manifest JSON")
    }

    fn is_manifest_index(&self, m: &serde_json::Value) -> bool {
        match m.get("mediaType").and_then(|v| v.as_str()) {
            Some(mt) => mt.contains("manifest.list") || mt.contains("image.index"),
            None => m.get("manifests").is_some(),
        }
    }

    fn download_blob(&self, digest: &str, dst: &Path) -> Result<()> {
        let url = format!("{}/v2/{}/blobs/{}", self.base_url, self.repo, digest);
        let mut args: Vec<String> = vec![
            "-sSL".into(),
            "--fail".into(),
            "-o".into(),
            dst.to_string_lossy().into_owned(),
        ];
        if let Some(token) = &self.bearer {
            args.push("-H".into());
            args.push(format!("Authorization: Bearer {}", token));
        }
        args.push(url);
        let status = Command::new("curl")
            .args(&args)
            .status()
            .context("failed to invoke curl for blob fetch")?;
        if !status.success() {
            return Err(anyhow!("curl blob fetch failed for {}", digest));
        }
        Ok(())
    }
}

/// Hosts that should always use plain HTTP: bare IPv4 with a port, or `localhost`.
fn host_is_plain_http(host: &str) -> bool {
    if host.starts_with("localhost") {
        return true;
    }
    // IPv4-with-port like 10.0.2.2:5000
    let host_only = host.split(':').next().unwrap_or(host);
    host_only
        .split('.')
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && host_only.split('.').count() == 4
}

/// Probe `/v2/` (or `/v2/<repo>/tags/list`) to determine if the registry is reachable
/// and whether it requires a Bearer token. Returns Ok(Some(token)) if a Bearer
/// challenge was issued and we obtained a token, Ok(None) for anonymous access, Err
/// on transport failure.
fn probe_v2(base_url: &str, repo: &str) -> Result<Option<String>> {
    let url = format!("{}/v2/", base_url);
    let out = Command::new("curl")
        .args([
            "-sS",
            "-o",
            "/dev/null",
            "-D",
            "-",
            "--max-time",
            "10",
            &url,
        ])
        .output()
        .context("curl probe failed")?;
    if !out.status.success() {
        return Err(anyhow!(
            "curl probe to {} failed: {}",
            url,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let headers = String::from_utf8_lossy(&out.stdout);
    // First line: HTTP/1.1 <code> ...
    let status_code = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    if status_code.starts_with("2") {
        return Ok(None);
    }
    if status_code == "401" {
        // Parse Www-Authenticate: Bearer realm="...",service="...",scope="..."
        let challenge = headers
            .lines()
            .find(|l| l.to_lowercase().starts_with("www-authenticate:"))
            .ok_or_else(|| anyhow!("registry returned 401 with no Www-Authenticate header"))?;
        let token = fetch_bearer_token(challenge, repo)?;
        return Ok(Some(token));
    }
    Err(anyhow!("unexpected status from {}: {}", url, status_code))
}

/// Parse a `Www-Authenticate: Bearer realm="...",service="...",scope="..."` line and
/// fetch an anonymous token. If the challenge didn't include a scope, build one for
/// pull access to `repo`.
fn fetch_bearer_token(challenge: &str, repo: &str) -> Result<String> {
    let bearer_part = challenge
        .splitn(2, ':')
        .nth(1)
        .map(|s| s.trim())
        .unwrap_or("");
    let bearer_part = bearer_part
        .strip_prefix("Bearer ")
        .ok_or_else(|| anyhow!("Www-Authenticate is not a Bearer challenge: {}", challenge))?;

    let mut realm: Option<String> = None;
    let mut service: Option<String> = None;
    for kv in bearer_part.split(',') {
        let mut it = kv.splitn(2, '=');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim().trim_matches('"');
        match k {
            "realm" => realm = Some(v.to_string()),
            "service" => service = Some(v.to_string()),
            _ => {}
        }
    }
    let realm = realm.ok_or_else(|| anyhow!("Bearer challenge missing realm"))?;
    // Always use the correct repo scope — the challenge's scope (if present)
    // is a placeholder like "repository:user/image:pull", not our actual repo.
    let scope = format!("repository:{}:pull", repo);

    let mut url = format!("{}?scope={}", realm, urlencode(&scope));
    if let Some(svc) = service {
        url.push_str(&format!("&service={}", urlencode(&svc)));
    }

    let out = Command::new("curl")
        .args(["-sSL", "--fail", &url])
        .output()
        .context("curl token fetch failed")?;
    if !out.status.success() {
        return Err(anyhow!(
            "token fetch failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("token endpoint did not return JSON")?;
    let token = body
        .get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("token endpoint response has no token field"))?;
    Ok(token.to_string())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Parse `host[:port]/repo[:tag|@digest]` into (host, repo, reference).
/// Reference is the digest if `@` was present, otherwise the tag (default `latest`).
fn parse_image_ref(image_ref: &str) -> Result<(String, String, String)> {
    let trimmed = image_ref
        .strip_prefix("docker://")
        .unwrap_or(image_ref)
        .trim_start_matches('/');
    let (host, rest) = trimmed
        .split_once('/')
        .ok_or_else(|| anyhow!("image ref {} has no repository component", image_ref))?;

    // Split reference. `@` (digest) takes priority over `:` (tag) since digest contains `:`.
    let (repo, reference) = if let Some((r, d)) = rest.split_once('@') {
        (r.to_string(), d.to_string())
    } else if let Some((r, t)) = rest.rsplit_once(':') {
        (r.to_string(), t.to_string())
    } else {
        (rest.to_string(), "latest".to_string())
    };
    Ok((host.to_string(), repo, reference))
}

/// Try to extract a single file from one OCI layer blob to `dst`. Returns Ok(true)
/// if found, Ok(false) if the path wasn't in this layer (caller continues to the
/// next layer), Err on unexpected tar/IO failure.
///
/// OCI layer tarballs are gzip- or zstd-compressed and may store paths with or
/// without a leading `./`, so we try both forms. `tar -xaf` autodetects compression.
fn extract_one_from_layer(blob: &Path, src: &Path, dst: &Path) -> Result<bool> {
    let src_no_leading = src
        .strip_prefix("/")
        .unwrap_or(src)
        .to_string_lossy()
        .into_owned();
    let candidates = [format!("./{}", src_no_leading), src_no_leading.clone()];

    for candidate in &candidates {
        // Stream directly to disk — initrds can be ~200 MB, no reason to buffer.
        let dst_file = fs::File::create(dst).with_context(|| {
            format!(
                "failed to open destination {} for tar extract",
                dst.display()
            )
        })?;
        let status = Command::new("tar")
            .args([
                "-xaf",
                blob.to_str().ok_or_else(|| anyhow!("invalid blob path"))?,
                "-O",
                candidate,
            ])
            .stdout(dst_file)
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to invoke tar for layer extraction")?;
        if status.success() {
            // tar emitted to stdout — verify we got actual bytes (some tar versions
            // exit 0 even when the path isn't in the archive, just producing empty).
            if let Ok(meta) = fs::metadata(dst) {
                if meta.len() > 0 {
                    return Ok(true);
                }
            }
        }
        // Clean the empty destination so the next attempt starts fresh.
        let _ = fs::remove_file(dst);
    }
    Ok(false)
}
