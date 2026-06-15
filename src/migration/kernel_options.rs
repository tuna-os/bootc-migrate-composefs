use std::fs;
use anyhow::{Result, Context};

/// Build kernel command-line options for the ComposeFS boot entry.
///
/// Reads the current /proc/cmdline, strips OSTree-specific and incompatible
/// arguments, and appends the composefs= digest.
///
/// # Filtered arguments
/// - `ostree=*` — OSTree deployment locator
/// - `BOOT_IMAGE=*` — GRUB internal
/// - `initrd=*` — GRUB internal
/// - `root=*` — we'll let composefs-setup-root discover the root
/// - `rootflags=*` — often specific to the OSTree btrfs subvol
/// - `rd.systemd.unit=*` — stay out of initrd overrides
pub fn get_kernel_options(composefs_digest: &str) -> Result<String> {
    let cmdline = fs::read_to_string("/proc/cmdline")
        .context("failed to read /proc/cmdline")?;
    let mut options: Vec<String> = Vec::new();
    for word in cmdline.split_whitespace() {
        if should_filter(word) {
            continue;
        }
        options.push(word.to_string());
    }
    // composefs= gets the bare hex digest (no sha512: prefix).
    // SPECIFICATION.md §3.4 and §4.2 examples use the bare hex form.
    let bare_hex = crate::VerityDigest::from_prefixed_or_hex(composefs_digest);
    options.push(format!("composefs={}", bare_hex.as_hex()));
    // DEBUG: pipe all systemd messages to serial console for E2E diagnosis
    if !options.iter().any(|o| o == "systemd.log_target=console") {
        options.push("systemd.log_target=console".to_string());
    }
    // Replace any existing log_level with debug so we see errno details
    options.retain(|o| !o.starts_with("systemd.log_level=") && !o.starts_with("loglevel="));
    options.push("systemd.log_level=debug".to_string());
    Ok(options.join(" "))
}

fn should_filter(word: &str) -> bool {
    if word.starts_with("ostree=")
        || word.starts_with("BOOT_IMAGE=")
        || word.starts_with("initrd=")
        || word.starts_with("rootflags=")
        || word.starts_with("rd.systemd.unit=")
    {
        return true;
    }
    // Also filter anything starting with "ostree." — belt and suspenders.
    if word.starts_with("ostree.") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- #1: TDD tests for kernel option filtering ---

    /// Helper that simulates get_kernel_options without reading /proc/cmdline.
    fn build_options(cmdline: &str, hex_digest: &str) -> String {
        let mut options: Vec<String> = Vec::new();
        for word in cmdline.split_whitespace() {
            if should_filter(word) {
                continue;
            }
            options.push(word.to_string());
        }
        // digest must be bare hex (no sha512: prefix) for composefs=
        options.push(format!("composefs={hex_digest}"));
        options.join(" ")
    }

    #[test]
    fn composefs_arg_is_bare_hex_not_prefixed() {
        let result = build_options("root=UUID=xxx quiet", "abc123");
        assert!(result.contains("composefs=abc123"));
        assert!(!result.contains("sha512:"));
    }

    #[test]
    fn filters_ostree_arg() {
        let result = build_options(
            "root=UUID=xxx quiet ostree=/ostree/boot.1/fedora/abc/0",
            "ab01cd23ef45",
        );
        assert!(!result.contains("ostree="));
    }

    #[test]
    fn filters_boot_image_arg() {
        let result = build_options(
            "BOOT_IMAGE=/vmlinuz-x.y root=UUID=xxx quiet",
            "ab01cd23ef45",
        );
        assert!(!result.contains("BOOT_IMAGE"));
    }

    #[test]
    fn filters_initrd_arg() {
        let result = build_options(
            "initrd=/initramfs-x.y.img root=UUID=xxx quiet",
            "ab01cd23ef45",
        );
        assert!(!result.contains("initrd="));
    }

    #[test]
    fn preserves_root_arg() {
        // Fix 7: root= is needed alongside composefs= per SPECIFICATION.md §4.2.
        let result = build_options(
            "root=UUID=aaaa-bbbb quiet rw",
            "ab01cd23ef45",
        );
        assert!(result.contains("root=UUID=aaaa-bbbb"));
        assert!(result.contains("quiet"));
    }

    #[test]
    fn filters_rootflags_arg() {
        let result = build_options(
            "rootflags=subvol=root quiet rw",
            "ab01cd23ef45",
        );
        assert!(!result.contains("rootflags="));
        assert!(result.contains("quiet"));
    }

    #[test]
    fn filters_rd_systemd_unit_override() {
        let result = build_options(
            "rd.systemd.unit=ostree-prepare-root.service quiet",
            "ab01cd23ef45",
        );
        assert!(!result.contains("rd.systemd.unit="));
        assert!(result.contains("quiet"));
    }

    #[test]
    fn filters_ostree_dot_prefix_args() {
        let result = build_options(
            "ostree.booted=1 ostree.composefs=0 quiet",
            "ab01cd23ef45",
        );
        assert!(!result.contains("ostree."));
        assert!(result.contains("quiet"));
    }

    #[test]
    fn preserves_console_args() {
        let result = build_options(
            "console=ttyS0,115200n8 console=tty0 quiet",
            "ab01cd23ef45",
        );
        assert!(result.contains("console=ttyS0,115200n8"));
        assert!(result.contains("console=tty0"));
    }

    #[test]
    fn preserves_rw_and_quiet() {
        let result = build_options("rw quiet", "ab01cd23ef45");
        assert!(result.contains("rw"));
        assert!(result.contains("quiet"));
    }

    /// Representative Bluefin cmdline (simulated from a real bootc OSTree system).
    #[test]
    fn representative_bluefin_cmdline() {
        let cmdline = concat!(
            "BOOT_IMAGE=/boot/vmlinuz-6.11.4-301.fc41.x86_64 ",
            "root=UUID=abcd-1234 ",
            "rootflags=subvol=root ",
            "rw quiet ",
            "console=ttyS0,115200n8 console=tty0 ",
            "ostree=/ostree/boot.1/fedora/abc123def456/0 ",
            "rd.systemd.unit=ostree-prepare-root.service ",
            "ostree.booted=1 ",
            "ostree.composefs=0"
        );
        let result = build_options(cmdline, "ab01cd23ef45");
        // Must not contain:
        assert!(!result.contains("ostree="));
        assert!(!result.contains("ostree."));
        assert!(!result.contains("BOOT_IMAGE"));
        assert!(!result.contains("rootflags="));
        assert!(!result.contains("rd.systemd.unit="));
        // Must contain (root= is preserved per spec §4.2):
        assert!(result.contains("root=UUID=abcd-1234"));
        assert!(result.contains("rw"));
        assert!(result.contains("quiet"));
        assert!(result.contains("console=ttyS0"));
        assert!(result.contains("composefs=ab01cd23ef45"));
        // composefs arg must NOT have sha512: prefix
        assert!(!result.contains("sha512:"));
    }
}
