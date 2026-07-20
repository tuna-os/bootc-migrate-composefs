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

#[cfg(test)]
mod tests {
    use super::super::system_info::PendingTransactionStatus;
    use super::*;

    fn sys_with_esp(detected: bool, free: u64) -> SystemInfo {
        SystemInfo {
            is_bootc_ostree: true,
            pending_transaction: PendingTransactionStatus::Clean,
            is_uefi: true,
            nvram_writable: true,
            esp_path: detected.then(|| "/boot/efi".to_string()),
            esp_free_space_bytes: free,
            esp_fs_type: Some("vfat".into()),
            esp_detected: detected,
            supports_reflink: true,
            is_btrfs: true,
            fs_type: Some("btrfs".into()),
            ostree_repo_size_bytes: 0,
            composefs_free_bytes: 0,
            systemd_boot_binaries_present: true,
            grub_tools_available: true,
            sysroot_was_ro: false,
        }
    }

    #[test]
    fn esp_readiness_threshold_is_150_mb() {
        let at = 150 * 1024 * 1024;
        assert!(ostree_to_composefs(sys_with_esp(true, at)).esp_ready_for_systemd_boot);
        assert!(!ostree_to_composefs(sys_with_esp(true, at - 1)).esp_ready_for_systemd_boot);
    }

    #[test]
    fn undetected_esp_is_never_ready() {
        let r = ostree_to_composefs(sys_with_esp(false, u64::MAX));
        assert!(!r.esp_ready_for_systemd_boot);
    }
}
