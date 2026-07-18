//! Per-direction preflight validators.
//!
//! Consume a [`SystemInfo`](super::SystemInfo) and produce judgments for a
//! specific migration direction. Currently: OSTree → ComposeFS.

use super::PreflightReport;
use super::system_info::SystemInfo;

/// Validate readiness for an OSTree → ComposeFS migration.
///
/// The only direction-specific judgment today is ESP readiness for
/// systemd-boot (≥150 MB free on a detected ESP); everything else in the
/// report is generic system state passed through from [`SystemInfo`].
pub fn ostree_to_composefs(sys: SystemInfo) -> PreflightReport {
    let esp_ready_for_systemd_boot =
        sys.esp_detected && sys.esp_free_space_bytes >= 150 * 1024 * 1024;
    PreflightReport {
        is_bootc_ostree: sys.is_bootc_ostree,
        pending_transaction: sys.pending_transaction,
        is_uefi: sys.is_uefi,
        nvram_writable: sys.nvram_writable,
        esp_path: sys.esp_path,
        esp_free_space_bytes: sys.esp_free_space_bytes,
        esp_fs_type: sys.esp_fs_type,
        esp_detected: sys.esp_detected,
        supports_reflink: sys.supports_reflink,
        is_btrfs: sys.is_btrfs,
        fs_type: sys.fs_type,
        ostree_repo_size_bytes: sys.ostree_repo_size_bytes,
        composefs_free_bytes: sys.composefs_free_bytes,
        esp_ready_for_systemd_boot,
        systemd_boot_binaries_present: sys.systemd_boot_binaries_present,
        grub_tools_available: sys.grub_tools_available,
        sysroot_was_ro: sys.sysroot_was_ro,
    }
}
