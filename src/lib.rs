//! In-place migration from OSTree-backed to composefs-backed bootc systems.
//!
//! This library exposes the building blocks used by the
//! `bootc-migrate-composefs` binary so other tools (e.g. a universal
//! bootc re-base engine) can compose their own migration pipelines:
//!
//! - [`mergetc`] — 3-way /etc merge, identity DB union, dangling-symlink pruning
//! - [`reflink`] — CoW-aware file copy (FICLONE with fallback)
//! - [`xattr`] — xattr-preserving copy helpers
//! - [`ostree`] — OSTree object scanning and hashing
//! - [`composefs`] — composefs image operations (via `bootc internals cfs`)
//! - [`registry`] — disk-bounded, layer-at-a-time file extraction from OCI images
//! - [`preflight`] — system introspection and migration readiness checks
//! - [`migration`] — the phase 0–5 migration pipeline, bootloader/BLS handling,
//!   kernel command-line construction, and os-release parsing
//! - [`types`] — shared types such as [`VerityDigest`]

pub mod composefs;
pub mod mergetc;
pub mod migration;
pub mod motd;
pub mod ostree;
pub mod preflight;
pub mod reflink;
pub mod registry;
pub mod types;
pub mod xattr;

pub use types::VerityDigest;
