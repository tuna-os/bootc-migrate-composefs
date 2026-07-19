//! Native composefs-rs store backend (issue #13), behind the
//! `composefs-native` feature.
//!
//! [`NativeStore`] implements [`ComposefsStore`] with the `composefs` /
//! `composefs-oci` crates directly instead of shelling out to `bootc
//! internals cfs`. This is the permanent answer to the upstream CLI drift in
//! issue #72 (the `oci create-image` / `oci seal` subcommands were removed
//! from new-generation bootc): no CLI to drift against, and digests come back
//! as typed values instead of scraped stdout.
//!
//! ## Generation matrix
//!
//! The store format is defined by the bootc that *reads* it at boot — the
//! target image's. This backend writes the new-generation (composefs-rs
//! ≥ 0.7) store model, so it is the right writer when the target ships a
//! new-generation bootc. Legacy-generation targets keep using
//! [`BootcCliStore`](crate::composefs::BootcCliStore) delegation. Selection
//! is by the target-generation probe introduced for issue #72.
//!
//! ## String contracts
//!
//! The trait predates typed digests, so methods return the same line formats
//! the CLI printed and the phase code parses:
//! - `pull_image` → `manifest <sha256:…>\nconfig <sha256:…>`
//! - `create_image` → the EROFS image's fs-verity digest (bare hex)
//! - `seal_image` → `config <sha256:…>` (the sealed config digest)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use composefs::fsverity::{FsVerityHashValue, Sha512HashValue};
use composefs::repository::{Repository, RepositoryConfig};
use composefs::tree::FileSystem;
use composefs_oci::{PullOptions, image::create_filesystem, open_config, pull};

use crate::composefs::ComposefsStore;

/// The fs-verity flavour of the store: **sha512**, matching what bootc's cfs
/// CLI writes — `VerityDigest` carries a `sha512:` prefix in `.origin` /
/// `.imginfo`, and `composefs=` takes the bare sha512 hex. A sha256 repo here
/// would produce object IDs the rest of the pipeline (and the target's bootc
/// at boot) cannot resolve.
type ObjectId = Sha512HashValue;

/// Config label that marks an OCI image as sealed, holding the EROFS image's
/// fs-verity object ID. This is what new-generation `oci fsck` checks (the
/// composefs-oci sealing model — there is no discrete `seal` subcommand).
const SEAL_LABEL: &str = "containers.composefs.fsverity";

/// A [`ComposefsStore`] that writes the store with composefs-rs natively.
#[derive(Debug)]
pub struct NativeStore {
    /// Filesystem path of the composefs repository (`/sysroot/composefs` on a
    /// real migration).
    repo_path: PathBuf,
}

impl NativeStore {
    /// A store at an explicit repository path (tests, staging roots).
    pub fn new(repo_path: impl Into<PathBuf>) -> Self {
        Self {
            repo_path: repo_path.into(),
        }
    }

    /// The store at the system location the migration pipeline uses.
    pub fn system() -> Self {
        Self::new("/sysroot/composefs")
    }

    /// Open the repository, creating it if absent and upgrading
    /// pre-`meta.json` repositories in place (the non-destructive path for
    /// stores written by a legacy bootc CLI).
    ///
    /// Discriminates on [`repo_exists`], not bare path existence: on a real
    /// migration `/sysroot/composefs` is typically a pre-created (or
    /// loopback-mounted, on XFS) **empty** directory before store init, and
    /// an empty dir must take the init path, not the upgrade path.
    fn repo(&self) -> Result<Arc<Repository<ObjectId>>> {
        let cwd = rustix::fs::CWD;
        let repo = if repo_exists(&self.repo_path) {
            let (repo, _upgraded) = Repository::open_upgrade(cwd, &self.repo_path)
                .with_context(|| format!("opening composefs repo at {:?}", self.repo_path))?;
            repo
        } else {
            let algorithm = composefs::fsverity::Algorithm::for_hash::<ObjectId>();
            match Repository::init_path(cwd, &self.repo_path, RepositoryConfig::new(algorithm)) {
                Ok((repo, _created)) => repo,
                // The filesystem may not support fs-verity (test tmpdirs, or
                // hosts without the feature). The repo still works in insecure
                // mode — objects are unverified at read time, matching what a
                // non-verity legacy store gave us. Boot-time sealing strength
                // is a target-image concern, not a store-write concern.
                Err(verity_err) => {
                    let config = RepositoryConfig::new(algorithm).set_insecure();
                    let (repo, _created) = Repository::init_path(cwd, &self.repo_path, config)
                        .with_context(|| {
                            format!(
                                "initializing composefs repo at {:?} \
                                 (fs-verity init also failed: {verity_err:#})",
                                self.repo_path
                            )
                        })?;
                    repo
                }
            }
        };
        Ok(Arc::new(repo))
    }

    /// Build the EROFS filesystem for a pulled config and commit it to the
    /// repo, returning its fs-verity object ID. Shared by `create_image` and
    /// the build-if-missing path in `seal_image`.
    fn commit_erofs(
        &self,
        repo: &Arc<Repository<ObjectId>>,
        config_digest: &composefs_oci::OciDigest,
    ) -> Result<ObjectId> {
        let fs: FileSystem<ObjectId> = create_filesystem(repo, config_digest, None)
            .context("building composefs filesystem from OCI layers")?;
        fs.commit_image(repo, None)
            .context("committing EROFS image to the repo")
    }
}

/// Normalize an image reference to a skopeo transport (`docker://` unless a
/// transport is already present). `containers-storage:` is the one common
/// transport spelled without `//` — it triggers the crate's native zero-copy
/// local import path, so it must pass through untouched.
fn with_transport(image_ref: &str) -> String {
    if image_ref.contains("://") || image_ref.starts_with("containers-storage:") {
        image_ref.to_string()
    } else {
        format!("docker://{image_ref}")
    }
}

/// Parse a `sha256:…` string into a typed OCI digest.
fn oci_digest(s: &str) -> Result<composefs_oci::OciDigest> {
    s.parse()
        .map_err(|e| anyhow::anyhow!("invalid OCI digest {s:?}: {e}"))
}

/// A small single-threaded runtime for the async composefs-oci entry points
/// (`pull`, fsck). The pipeline itself is synchronous.
fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime for composefs-oci")
}

impl ComposefsStore for NativeStore {
    fn pull_image(&self, _target_image: &str, image_ref: &str) -> Result<String> {
        let repo = self.repo()?;
        let transport_ref = with_transport(image_ref);
        let result = runtime()?
            .block_on(pull(
                &repo,
                &transport_ref,
                Some(image_ref),
                PullOptions::default(),
            ))
            .with_context(|| format!("pulling {transport_ref} into the composefs repo"))?;
        // Same line format the legacy CLI printed; parse_pull_digests reads it.
        Ok(format!(
            "manifest {}\nconfig {}",
            result.manifest_digest, result.config_digest
        ))
    }

    fn create_image(&self, _target_image: &str, image_id: &str) -> Result<String> {
        let repo = self.repo()?;
        let config_digest = oci_digest(image_id)?;
        let erofs = self.commit_erofs(&repo, &config_digest)?;
        Ok(erofs.to_hex())
    }

    fn seal_image(&self, _target_image: &str, image_id: &str) -> Result<String> {
        let repo = self.repo()?;
        let config_digest = oci_digest(image_id)?;
        let oc = open_config(&repo, &config_digest, None)
            .with_context(|| format!("opening pulled config {image_id}"))?;

        // The EROFS image to embed: reuse the one linked to the config if
        // create_image already ran, else build it now.
        let erofs = match oc.image_ref.clone().or_else(|| oc.image_ref_v1.clone()) {
            Some(id) => id,
            None => self.commit_erofs(&repo, &config_digest)?,
        };

        // Seal = clone the config with the fs-verity label and write it back
        // as a new config splitstream carrying the EROFS named ref. The
        // returned digest is the *sealed* config identifier the pipeline
        // records for boot-time lookup — distinct from the pulled config.
        let mut sealed = oc.config;
        let mut inner = sealed.config().clone().unwrap_or_default();
        let mut labels = inner.labels().clone().unwrap_or_default();
        labels.insert(SEAL_LABEL.to_string(), erofs.to_id());
        inner.set_labels(Some(labels));
        sealed.set_config(Some(inner));

        let (sealed_digest, _sealed_verity) = composefs_oci::write_config(
            &repo,
            &sealed,
            oc.layer_refs,
            Some(&erofs),
            None,
            None,
            None,
        )
        .context("writing sealed config splitstream")?;

        Ok(format!("config {sealed_digest}"))
    }

    fn verify_store_target_readable(&self, _target_image: &str) -> Result<()> {
        // Native analog of the CLI store's check: the repository must reopen
        // cleanly (meta.json parses, format is compatible) and its OCI refs
        // must enumerate. A full byte-level fsck would re-hash every object of
        // a multi-GB store — far too slow for a migration phase. Cross-checking
        // against the *target's* bootc binary stays meaningful only for the
        // CLI store; for a new-generation target this store IS the target's
        // native format (see the generation matrix in the module docs).
        let repo = self.repo()?;
        composefs_oci::oci_image::list_refs(&repo).context("enumerating OCI refs in the store")?;
        Ok(())
    }
}

/// True if `repo_path` looks like an initialized composefs repository. Used
/// by store selection to distinguish "fresh migration" from "resume".
pub fn repo_exists(repo_path: &Path) -> bool {
    repo_path.join("objects").is_dir() || repo_path.join("meta.json").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_transport_adds_docker_prefix() {
        assert_eq!(
            with_transport("ghcr.io/projectbluefin/dakota:stable"),
            "docker://ghcr.io/projectbluefin/dakota:stable"
        );
        assert_eq!(
            with_transport("containers-storage:localhost/x"),
            "containers-storage:localhost/x"
        );
    }

    #[test]
    fn oci_digest_rejects_garbage() {
        assert!(oci_digest("not-a-digest").is_err());
        assert!(
            oci_digest("sha256:0000000000000000000000000000000000000000000000000000000000000000")
                .is_ok()
        );
    }

    #[test]
    fn store_initializes_a_fresh_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        let store = NativeStore::new(&path);
        assert!(!repo_exists(&path));
        // repo() creates the repository directory + meta.json.
        store.repo().unwrap();
        assert!(repo_exists(&path));
        // And a second open takes the open_upgrade path cleanly.
        store.repo().unwrap();
    }

    #[test]
    fn store_initializes_when_dir_exists_but_is_empty() {
        // /sysroot/composefs is typically a pre-created (or loopback-mounted)
        // empty directory before store init — this must init, not try to
        // "upgrade" a repo that was never there.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        std::fs::create_dir(&path).unwrap();
        let store = NativeStore::new(&path);
        store.repo().unwrap();
        assert!(repo_exists(&path));
    }

    #[test]
    fn verify_on_empty_store_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let store = NativeStore::new(dir.path().join("repo"));
        store.verify_store_target_readable("unused").unwrap();
    }

    #[test]
    fn create_image_on_unknown_config_errors_cleanly() {
        // Phase 3 hands us a config digest from Phase 2; if the store lost it
        // (wiped repo, wrong path) this must be a contextual error, not a
        // panic or a bogus digest.
        let dir = tempfile::tempdir().unwrap();
        let store = NativeStore::new(dir.path().join("repo"));
        let err = store
            .create_image(
                "unused",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("composefs"),
            "error should carry repo context: {err:#}"
        );
    }

    #[test]
    fn seal_image_on_unknown_config_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let store = NativeStore::new(dir.path().join("repo"));
        let err = store
            .seal_image(
                "unused",
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("sha256:2222"),
            "error should name the missing config: {err:#}"
        );
    }

    #[test]
    fn create_image_rejects_a_bare_verity_hex() {
        // Guard against the verity-vs-config-digest confusion the glossary
        // warns about: a bare 64-hex verity digest is not an OCI config
        // digest and must be rejected at the boundary, silently looking up
        // nothing.
        let dir = tempfile::tempdir().unwrap();
        let store = NativeStore::new(dir.path().join("repo"));
        assert!(
            store
                .create_image(
                    "unused",
                    "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                )
                .is_err()
        );
    }
}
