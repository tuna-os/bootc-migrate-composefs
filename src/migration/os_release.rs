use std::fs;
use std::path::Path;
use anyhow::{Result, anyhow, Context};

/// Represents key fields from /etc/os-release used to construct BLS entry names.
#[derive(Debug, Clone)]
pub struct OsRelease {
    pub id: String,
    pub version_id: String,
}

/// Read /etc/os-release from a given root filesystem path (e.g., a mounted EROFS image).
pub fn read_os_release(root: &Path) -> Result<OsRelease> {
    let path = root.join("etc/os-release");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read os-release from {}", path.display()))?;

    let mut id = String::new();
    let mut version_id = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(val) = parse_os_release_value(line, "ID=") {
            if id.is_empty() {
                id = val.to_string();
            }
        }
        if let Some(val) = parse_os_release_value(line, "VERSION_ID=") {
            if version_id.is_empty() {
                version_id = val.to_string();
            }
        }
    }

    if id.is_empty() {
        return Err(anyhow!("ID not found in os-release"));
    }

    Ok(OsRelease { id, version_id })
}

fn parse_os_release_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    if !line.starts_with(key) {
        return None;
    }
    let val = &line[key.len()..];
    // Strip surrounding quotes
    let val = val.strip_prefix('"').unwrap_or(val);
    let val = val.strip_suffix('"').unwrap_or(val);
    let val = val.strip_prefix('\'').unwrap_or(val);
    let val = val.strip_suffix('\'').unwrap_or(val);
    Some(val)
}

/// Build a BLS entry filename following the convention in SPECIFICATION.md §4.2:
///
/// Format: `bootc_{os_id}-{version}-{priority}.conf`
///
/// Where:
/// - `os_id` comes from /etc/os-release's ID (hyphens → underscores)
/// - `version` is the target image version (from os-release VERSION_ID)
/// - `priority` is an integer: higher = preferred by GRUB (DESCENDING sort)
pub fn bls_entry_filename(os: &OsRelease, verity_hash: &str, priority: u32) -> String {
    let id = os.id.replace('-', "_");
    let ver = if os.version_id.is_empty() {
        verity_hash[..12].to_string()
    } else {
        os.version_id.clone()
    };
    format!("bootc_{id}-{ver}-{priority}.conf")
}

/// The title displayed in the GRUB menu for this entry.
pub fn bls_entry_title(os: &OsRelease, kind: &str) -> String {
    // Capitalize first letter of ID for a nice title
    let name = if let Some(first) = os.id.chars().next() {
        let rest = &os.id[first.len_utf8()..];
        format!("{}{}", first.to_uppercase(), rest)
    } else {
        os.id.clone()
    };
    format!("{name} ({kind})")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- #6: TDD tests for BLS entry naming ---

    #[test]
    fn parses_os_release_basic() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(
            etc.join("os-release"),
            "ID=fedora\nVERSION_ID=41\n",
        )
        .unwrap();

        let os = read_os_release(dir.path()).unwrap();
        assert_eq!(os.id, "fedora");
        assert_eq!(os.version_id, "41");
    }

    #[test]
    fn parses_os_release_with_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(
            etc.join("os-release"),
            "ID=\"fedora\"\nVERSION_ID=\"41.20251125.0\"\n",
        )
        .unwrap();

        let os = read_os_release(dir.path()).unwrap();
        assert_eq!(os.id, "fedora");
        assert_eq!(os.version_id, "41.20251125.0");
    }

    #[test]
    fn os_release_missing_id_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("os-release"), "NAME=Fedora\n").unwrap();

        let result = read_os_release(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn bls_entry_filename_format() {
        let os = OsRelease {
            id: "fedora".into(),
            version_id: "41.20251125.0".into(),
        };
        let name = bls_entry_filename(&os, "abc123def456", 1);
        assert_eq!(name, "bootc_fedora-41.20251125.0-1.conf");
    }

    #[test]
    fn bls_entry_filename_hyphens_become_underscores() {
        let os = OsRelease {
            id: "bluefin-dakota".into(),
            version_id: "1.0".into(),
        };
        let name = bls_entry_filename(&os, "hash123", 1);
        assert_eq!(name, "bootc_bluefin_dakota-1.0-1.conf");
    }

    #[test]
    fn bls_entry_filename_priority_zero() {
        let os = OsRelease {
            id: "fedora".into(),
            version_id: "41".into(),
        };
        let name = bls_entry_filename(&os, "hash", 0);
        assert_eq!(name, "bootc_fedora-41-0.conf");
    }

    #[test]
    fn bls_entry_filename_fallback_when_no_version() {
        let os = OsRelease {
            id: "dakota".into(),
            version_id: String::new(),
        };
        // When no version, use first 12 chars of hash
        let name = bls_entry_filename(&os, "abc123def4567890abcdef", 1);
        assert_eq!(name, "bootc_dakota-abc123def456-1.conf");
    }

    #[test]
    fn bls_entry_name_does_not_contain_literal_bluefin_dakota() {
        // Regression test: we must not hardcode "bluefin_dakota"
        let os = OsRelease {
            id: "dakota".into(),
            version_id: "42".into(),
        };
        let name = bls_entry_filename(&os, "abc123", 1);
        // The old code used "bootc_bluefin_dakota-{hash}.conf" — make sure the
        // string "bluefin_dakota" does not appear.
        assert!(!name.contains("bluefin_dakota"));
        assert!(name.contains("dakota"));
    }

    #[test]
    fn bls_entry_title_is_readable() {
        let os = OsRelease {
            id: "fedora".into(),
            version_id: "41".into(),
        };
        let title = bls_entry_title(&os, "composefs");
        assert_eq!(title, "Fedora (composefs)");
    }
}
