//! Target-image capability scan (issue #24).
//!
//! Answers "what can this image be re-based to, on this machine" **before
//! anything is staged**, from a handful of probe files that registry
//! streaming (`crate::registry`) can pull without a full image pull (peak
//! disk ≈ one layer).
//!
//! This module is the pure half: parsers for each probe file and the
//! assembly into [`Capabilities`]. Fetching is a seam — callers hand in
//! [`ProbeFiles`] however they obtained them (registry streaming, a mounted
//! image, fixtures in tests). Routing consumes the result to auto-fill
//! `--target-backend` and refuse impossible routes with evidence instead of
//! mid-phase failures.

use serde::Serialize;

/// Identity of a base image, from `/usr/lib/os-release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BaseInfo {
    pub id: String,
    pub id_like: Option<String>,
    pub version_id: Option<String>,
}

/// One `/usr/lib/sysusers.d/*.conf` allocation. Only `u`/`g` lines with an
/// explicit numeric id matter for cross-base planning (issue #67); dynamic
/// (`-`) allocations are recorded without an id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SysusersEntry {
    /// `'u'` (user, possibly with paired group) or `'g'` (group).
    pub kind: char,
    pub name: String,
    pub id: Option<u32>,
}

/// The probe files the scan reads. Every field is optional — a missing file
/// is a signal (e.g. no prepare-root.conf ⇒ not an ostree/bootc image), not
/// an error.
#[derive(Debug, Clone, Default)]
pub struct ProbeFiles {
    /// `/usr/lib/os-release` content.
    pub os_release: Option<String>,
    /// `/usr/lib/ostree/prepare-root.conf` content.
    pub prepare_root: Option<String>,
    /// Contents of each `/usr/lib/sysusers.d/*.conf`.
    pub sysusers: Vec<String>,
    /// File names under `/usr/share/xsessions` + `/usr/share/wayland-sessions`.
    pub session_files: Vec<String>,
    /// `/usr/lib/systemd/boot/efi/systemd-bootx64.efi` present.
    pub has_systemd_boot_payload: bool,
    /// `/usr/bin/bootc` (or `/usr/lib/bootc/`) present.
    pub has_bootc: bool,
}

/// What the target image supports, assembled from [`ProbeFiles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// prepare-root.conf enables the composefs root (`enabled = true|yes|maybe`).
    pub composefs_capable: bool,
    /// The image is ostree-based at all (prepare-root.conf present).
    pub ostree_capable: bool,
    /// Ships the systemd-boot EFI payload (bootloader migration possible
    /// even when the *source* OS lacks the binaries — the phase-5 property).
    pub systemd_boot_payload: bool,
    /// Ships bootc (required for switch-based strategies).
    pub bootc_present: bool,
    /// Desktop environments inventoried from session files (`gnome`, `kde`, …).
    pub desktops: Vec<String>,
    /// Base identity for cross-base gating (#67).
    pub base: Option<BaseInfo>,
    /// Statically-allocated sysusers ids, input to the remap planner (#67).
    pub sysusers: Vec<SysusersEntry>,
}

impl Capabilities {
    /// The machine-readable form (`bootc-rebase scan --json`).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("Capabilities serialization cannot fail")
    }
}

/// Strip surrounding single or double quotes, os-release style.
fn unquote(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v)
}

/// Parse the identity fields out of os-release content.
pub fn parse_base_info(content: &str) -> Option<BaseInfo> {
    let mut id = None;
    let mut id_like = None;
    let mut version_id = None;
    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("ID=") {
            id = Some(unquote(v).to_string());
        } else if let Some(v) = line.strip_prefix("ID_LIKE=") {
            id_like = Some(unquote(v).to_string());
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            version_id = Some(unquote(v).to_string());
        }
    }
    Some(BaseInfo {
        id: id?,
        id_like,
        version_id,
    })
}

/// Whether prepare-root.conf enables the composefs root. ostree accepts
/// `true`/`yes`/`1` and the signed-only mode `maybe` — all of them mean the
/// initrd can set up a composefs root.
pub fn prepare_root_enables_composefs(content: &str) -> bool {
    let mut in_composefs = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_composefs = line.eq_ignore_ascii_case("[composefs]");
            continue;
        }
        if in_composefs
            && let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case("enabled")
        {
            let v = unquote(v).to_ascii_lowercase();
            return matches!(v.as_str(), "true" | "yes" | "1" | "maybe");
        }
    }
    false
}

/// Parse sysusers.d content: `u name id …` / `g name id …`. Ranges, `-`,
/// and `uid:gid` forms keep only the leading uid; malformed lines are
/// skipped (the scan reports, it must not fail).
pub fn parse_sysusers(content: &str) -> Vec<SysusersEntry> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut f = line.split_whitespace();
            let kind = match f.next()? {
                "u" | "u!" => 'u',
                "g" => 'g',
                _ => return None, // m/r lines carry no allocation
            };
            let name = f.next()?.to_string();
            let id = f.next().and_then(|tok| {
                let lead = tok.split(&[':', '-'][..]).next().unwrap_or("");
                lead.parse().ok()
            });
            Some(SysusersEntry { kind, name, id })
        })
        .collect()
}

/// Map session-file names to desktop identifiers. Unknown names are kept
/// verbatim (minus extension) so nothing is silently dropped.
pub fn desktops_from_sessions(session_files: &[String]) -> Vec<String> {
    let mut out: Vec<String> = session_files
        .iter()
        .filter_map(|f| {
            let stem = f
                .trim_end_matches(".desktop")
                .to_ascii_lowercase()
                .to_string();
            if stem.is_empty() {
                return None;
            }
            Some(if stem.contains("gnome") {
                "gnome".to_string()
            } else if stem.contains("plasma") {
                "kde".to_string()
            } else if stem.contains("xfce") {
                "xfce".to_string()
            } else if stem.contains("cinnamon") {
                "cinnamon".to_string()
            } else if stem.contains("cosmic") {
                "cosmic".to_string()
            } else if stem.contains("sway") {
                "sway".to_string()
            } else {
                stem
            })
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Assemble the capability report from probe files.
pub fn assemble(probe: &ProbeFiles) -> Capabilities {
    Capabilities {
        composefs_capable: probe
            .prepare_root
            .as_deref()
            .map(prepare_root_enables_composefs)
            .unwrap_or(false),
        ostree_capable: probe.prepare_root.is_some(),
        systemd_boot_payload: probe.has_systemd_boot_payload,
        bootc_present: probe.has_bootc,
        desktops: desktops_from_sessions(&probe.session_files),
        base: probe.os_release.as_deref().and_then(parse_base_info),
        sysusers: probe
            .sysusers
            .iter()
            .flat_map(|c| parse_sysusers(c))
            .collect(),
    }
}

/// Cross-base gate (#67): true when the target's base family differs from
/// the host's. Same `ID` is same-base; otherwise membership of either side's
/// `ID_LIKE` chain still counts as same family (fedora ↔ "ID_LIKE=fedora").
pub fn is_cross_base(host: &BaseInfo, target: &BaseInfo) -> bool {
    if host.id == target.id {
        return false;
    }
    let like_contains = |like: &Option<String>, id: &str| {
        like.as_deref()
            .map(|l| l.split_whitespace().any(|w| w == id))
            .unwrap_or(false)
    };
    !(like_contains(&host.id_like, &target.id) || like_contains(&target.id_like, &host.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_info_parses_quoted_and_unquoted() {
        let b = parse_base_info("NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=44\n").unwrap();
        assert_eq!(b.id, "fedora");
        assert_eq!(b.version_id.as_deref(), Some("44"));
        assert_eq!(b.id_like, None);

        let b = parse_base_info("ID='centos'\nID_LIKE=\"rhel fedora\"\n").unwrap();
        assert_eq!(b.id, "centos");
        assert_eq!(b.id_like.as_deref(), Some("rhel fedora"));
    }

    #[test]
    fn prepare_root_variants() {
        assert!(prepare_root_enables_composefs(
            "[composefs]\nenabled = true\n"
        ));
        assert!(prepare_root_enables_composefs(
            "[etc]\ntransient = false\n[composefs]\nenabled = maybe\n"
        ));
        assert!(!prepare_root_enables_composefs(
            "[composefs]\nenabled = false\n"
        ));
        // `enabled` under a different section must not count.
        assert!(!prepare_root_enables_composefs(
            "[root]\nenabled = true\n[composefs]\n"
        ));
        assert!(!prepare_root_enables_composefs(""));
    }

    #[test]
    fn sysusers_lines_parse_with_id_forms() {
        let entries = parse_sysusers(
            "# comment\n\
             u dnsmasq 983 \"Dnsmasq DHCP\" /var/lib/dnsmasq -\n\
             u! locked 984 - -\n\
             g render 105 - -\n\
             u dynamic - \"no static id\"\n\
             u paired 60:61 - -\n\
             m dnsmasq render\n",
        );
        assert_eq!(entries.len(), 5);
        assert_eq!(
            (entries[0].kind, entries[0].name.as_str(), entries[0].id),
            ('u', "dnsmasq", Some(983))
        );
        assert_eq!(entries[1].id, Some(984));
        assert_eq!((entries[2].kind, entries[2].id), ('g', Some(105)));
        assert_eq!(entries[3].id, None);
        assert_eq!(entries[4].id, Some(60));
    }

    #[test]
    fn desktops_map_and_dedupe() {
        let d = desktops_from_sessions(&[
            "gnome.desktop".into(),
            "gnome-xorg.desktop".into(),
            "plasma.desktop".into(),
            "plasmawayland.desktop".into(),
            "weirdwm.desktop".into(),
        ]);
        assert_eq!(d, vec!["gnome", "kde", "weirdwm"]);
    }

    #[test]
    fn assemble_full_probe() {
        let probe = ProbeFiles {
            os_release: Some("ID=fedora\nVERSION_ID=44\n".into()),
            prepare_root: Some("[composefs]\nenabled = true\n".into()),
            sysusers: vec!["u dnsmasq 983 - -\n".into()],
            session_files: vec!["gnome.desktop".into()],
            has_systemd_boot_payload: true,
            has_bootc: true,
        };
        let caps = assemble(&probe);
        assert!(caps.composefs_capable);
        assert!(caps.ostree_capable);
        assert!(caps.systemd_boot_payload);
        assert!(caps.bootc_present);
        assert_eq!(caps.desktops, vec!["gnome"]);
        assert_eq!(caps.base.as_ref().unwrap().id, "fedora");
        assert_eq!(caps.sysusers.len(), 1);
        let json = caps.to_json();
        assert!(json.contains("\"composefs_capable\": true"));
    }

    #[test]
    fn assemble_non_ostree_image() {
        let caps = assemble(&ProbeFiles::default());
        assert!(!caps.composefs_capable);
        assert!(!caps.ostree_capable);
        assert!(caps.base.is_none());
    }

    #[test]
    fn cross_base_respects_id_like() {
        let fedora = BaseInfo {
            id: "fedora".into(),
            id_like: None,
            version_id: None,
        };
        let dakota = BaseInfo {
            id: "dakota".into(),
            id_like: Some("fedora".into()),
            version_id: None,
        };
        let centos = BaseInfo {
            id: "centos".into(),
            id_like: Some("rhel".into()),
            version_id: None,
        };
        assert!(!is_cross_base(&fedora, &fedora));
        assert!(!is_cross_base(&fedora, &dakota));
        assert!(!is_cross_base(&dakota, &fedora));
        assert!(is_cross_base(&fedora, &centos));
        assert!(is_cross_base(&dakota, &centos));
    }
}
