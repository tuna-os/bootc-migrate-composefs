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
//! - [`transaction`] — two-phase apply: `commit` / `undo` of a staged migration
//! - [`types`] — shared types such as [`VerityDigest`]

pub mod composefs;
#[cfg(feature = "composefs-native")]
pub mod composefs_native;
pub mod mergetc;
pub mod migration;
pub mod motd;
pub mod ostree;
pub mod preflight;
pub mod reflink;
pub mod registry;
pub mod remap;
pub mod transaction;
pub mod types;
pub mod xattr;

pub use types::VerityDigest;
