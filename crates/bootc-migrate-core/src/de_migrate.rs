//! DE config stash/restore + migration hook contract (issue #68, M3 scenario
//! E: GNOME ↔ KDE user-state handling around a cross-DE re-base).
//!
//! The DE switch itself (which packages/session files land) is owned by the
//! target image; this module owns the user-state mechanics around it: moving
//! the outgoing DE's per-user config out of the way (never deleting it),
//! restoring it on a later re-base back, extracting a small best-effort
//! "portable subset" of preferences (wallpaper, dark-mode, locale, keyboard
//! layout), and running third-party hook scripts around the switch.
//!
//! What this module deliberately does **not** do: detect the target image's
//! default DE (that needs registry streaming + the cross-base hardening work
//! this issue is gated behind — see #68's "Depends on" section) or actually
//! apply the portable subset to the target DE (translation is opinionated
//! and explicitly meant to live in hook scripts, not the engine — this
//! module only computes the best-effort mapping as data for a hook to use).

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A desktop environment this module knows how to stash/restore config for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopEnvironment {
    Gnome,
    Kde,
}

/// Per-user config paths (relative to `$HOME`) considered part of a DE's
/// state, per the issue's scope list. Not exhaustive — a deliberately small,
/// well-known set.
const GNOME_STASH_PATHS: &[&str] = &[
    ".config/dconf/user",
    ".config/gnome-shell",
    ".local/share/gnome-shell",
];

const KDE_STASH_PATHS: &[&str] = &[
    ".config/kdeglobals",
    ".config/kwinrc",
    ".config/plasma-org.kde.plasma.desktop-appletsrc",
    ".config/plasmarc",
    ".local/share/plasma",
];

fn stash_paths(de: DesktopEnvironment) -> &'static [&'static str] {
    match de {
        DesktopEnvironment::Gnome => GNOME_STASH_PATHS,
        DesktopEnvironment::Kde => KDE_STASH_PATHS,
    }
}

/// One entry in a stash/restore plan: a path relative to `$HOME`, and
/// whether the source side of the move currently exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanEntry {
    pub relative_path: String,
    pub source_exists: bool,
}

/// Compute what a [`stash`] call would move, without touching the
/// filesystem beyond checking existence. `home` is the outgoing DE's user
/// home directory.
pub fn compute_stash_plan(de: DesktopEnvironment, home: &Path) -> Vec<PlanEntry> {
    stash_paths(de)
        .iter()
        .map(|p| PlanEntry {
            relative_path: (*p).to_string(),
            source_exists: home.join(p).exists(),
        })
        .collect()
}

/// Compute what a [`restore`] call would move back, given a stash root
/// previously populated by [`stash`] for `de`.
pub fn compute_restore_plan(de: DesktopEnvironment, stash_root: &Path) -> Vec<PlanEntry> {
    let de_root = stash_root.join(de_dir_name(de));
    stash_paths(de)
        .iter()
        .map(|p| PlanEntry {
            relative_path: (*p).to_string(),
            source_exists: de_root.join(p).exists(),
        })
        .collect()
}

fn de_dir_name(de: DesktopEnvironment) -> &'static str {
    match de {
        DesktopEnvironment::Gnome => "gnome",
        DesktopEnvironment::Kde => "kde",
    }
}

/// Move `home`'s config for `de` under `stash_root/<de>/`, per the plan from
/// [`compute_stash_plan`]. Paths that don't exist are skipped. Uses a rename
/// (never a delete) — content is relocated, not destroyed. Returns the
/// relative paths actually moved. No-op (but still returns what *would*
/// move) when `dry_run` is set.
pub fn stash(
    de: DesktopEnvironment,
    home: &Path,
    stash_root: &Path,
    dry_run: bool,
) -> Result<Vec<String>> {
    let de_root = stash_root.join(de_dir_name(de));
    let mut moved = Vec::new();
    for entry in compute_stash_plan(de, home) {
        if !entry.source_exists {
            continue;
        }
        let src = home.join(&entry.relative_path);
        let dst = de_root.join(&entry.relative_path);
        if !dry_run {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating stash parent dir {}", parent.display()))?;
            }
            rename_or_copy(&src, &dst)
                .with_context(|| format!("stashing {} to {}", src.display(), dst.display()))?;
        }
        moved.push(entry.relative_path);
    }
    Ok(moved)
}

/// Inverse of [`stash`]: move `stash_root/<de>/...` back under `home`.
pub fn restore(
    de: DesktopEnvironment,
    home: &Path,
    stash_root: &Path,
    dry_run: bool,
) -> Result<Vec<String>> {
    let de_root = stash_root.join(de_dir_name(de));
    let mut moved = Vec::new();
    for entry in compute_restore_plan(de, stash_root) {
        if !entry.source_exists {
            continue;
        }
        let src = de_root.join(&entry.relative_path);
        let dst = home.join(&entry.relative_path);
        if !dry_run {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating restore parent dir {}", parent.display()))?;
            }
            rename_or_copy(&src, &dst)
                .with_context(|| format!("restoring {} to {}", src.display(), dst.display()))?;
        }
        moved.push(entry.relative_path);
    }
    Ok(moved)
}

/// Rename, falling back to a recursive copy + remove when `src` and `dst`
/// are on different filesystems (`EXDEV`) — stash roots under
/// `~/.local/share` are normally same-fs as `$HOME`, but don't assume it.
fn rename_or_copy(src: &Path, dst: &Path) -> Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc_exdev()) => {
            copy_recursive(src, dst)?;
            if src.is_dir() {
                fs::remove_dir_all(src)?;
            } else {
                fs::remove_file(src)?;
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// `EXDEV` is 18 on Linux; avoid pulling in the `libc` crate for one constant.
fn libc_exdev() -> i32 {
    18
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
        fs::copy(src, dst)?;
    } else {
        fs::copy(src, dst)?;
    }
    Ok(())
}

/// A small, explicitly best-effort subset of DE preferences considered
/// portable across GNOME ↔ KDE: wallpaper, dark-mode/accent, locale,
/// keyboard layout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PortableSubset {
    pub wallpaper_path: Option<String>,
    pub dark_mode: Option<bool>,
    pub accent_color: Option<String>,
    pub locale: Option<String>,
    pub keyboard_layout: Option<String>,
}

/// Extract the portable subset from a `dconf dump /` (or `dconf dump
/// /org/gnome/`) text blob. Parses `key=value` lines under the relevant
/// section headers; unrecognized keys are ignored. Best-effort: a dump
/// missing a key just leaves that field `None`.
pub fn extract_portable_subset_gnome(dconf_dump: &str) -> PortableSubset {
    let mut subset = PortableSubset::default();
    let mut section = String::new();
    for line in dconf_dump.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('\'').to_string();
        match (section.as_str(), key) {
            ("org/gnome/desktop/background", "picture-uri") => {
                subset.wallpaper_path = Some(value);
            }
            ("org/gnome/desktop/interface", "color-scheme") => {
                subset.dark_mode = Some(value.contains("dark"));
            }
            ("org/gnome/desktop/interface", "accent-color") => {
                subset.accent_color = Some(value);
            }
            ("org/gnome/system/locale", "region") | ("org/gnome/desktop/interface", "locale") => {
                subset.locale = Some(value);
            }
            ("org/gnome/desktop/input-sources", "sources") => {
                subset.keyboard_layout = Some(value);
            }
            _ => {}
        }
    }
    subset
}

/// Render the portable subset as `kwriteconfig5` invocations that *would*
/// apply it to KDE — returned as argv vectors (data, not executed). Actually
/// running these is a hook script's job, not this module's: cross-DE
/// preference translation is opinionated and belongs outside the engine per
/// the issue's hook-contract design.
pub fn portable_subset_to_kde_kwriteconfig_args(subset: &PortableSubset) -> Vec<Vec<String>> {
    let mut cmds = Vec::new();
    let arg = |s: &str| s.to_string();
    if let Some(wallpaper) = &subset.wallpaper_path {
        cmds.push(vec![
            arg("kwriteconfig5"),
            arg("--file"),
            arg("plasma-org.kde.plasma.desktop-appletsrc"),
            arg("--group"),
            arg("Wallpaper"),
            arg("--key"),
            arg("Image"),
            wallpaper.clone(),
        ]);
    }
    if let Some(dark) = subset.dark_mode {
        cmds.push(vec![
            arg("kwriteconfig5"),
            arg("--file"),
            arg("kdeglobals"),
            arg("--group"),
            arg("General"),
            arg("--key"),
            arg("ColorScheme"),
            arg(if dark { "BreezeDark" } else { "BreezeLight" }),
        ]);
    }
    if let Some(layout) = &subset.keyboard_layout {
        cmds.push(vec![
            arg("kwriteconfig5"),
            arg("--file"),
            arg("kxkbrc"),
            arg("--group"),
            arg("Layout"),
            arg("--key"),
            arg("LayoutList"),
            layout.clone(),
        ]);
    }
    cmds
}

/// Hook contract dirs, per #68's design.
pub const PRE_SWITCH_HOOK_DIR: &str = "/usr/lib/bootc-rebase/hooks/pre-switch.d";
pub const POST_SWITCH_HOOK_DIR: &str = "/usr/lib/bootc-rebase/hooks/post-switch.d";

/// List executable regular files directly under `dir`, sorted by filename —
/// the run order a hook directory implies. Missing `dir` yields an empty
/// list (no hooks installed is the common case, not an error).
pub fn discover_hooks(dir: &Path) -> Result<Vec<PathBuf>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading hook dir {}", dir.display())),
    };
    let mut hooks = Vec::new();
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::metadata(&path)?;
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            hooks.push(path);
        }
    }
    hooks.sort();
    Ok(hooks)
}

/// Build the env vars a hook script receives, per #68's hook contract.
pub fn build_hook_env(
    from_de: DesktopEnvironment,
    to_de: DesktopEnvironment,
    stash_dir: &Path,
    home: &Path,
) -> Vec<(String, String)> {
    vec![
        (
            "REBASE_FROM_DE".to_string(),
            de_dir_name(from_de).to_string(),
        ),
        ("REBASE_TO_DE".to_string(), de_dir_name(to_de).to_string()),
        (
            "REBASE_STASH_DIR".to_string(),
            stash_dir.display().to_string(),
        ),
        ("REBASE_HOME".to_string(), home.display().to_string()),
    ]
}

/// Outcome of running one hook.
#[derive(Debug, Clone, Serialize)]
pub struct HookResult {
    pub path: String,
    pub success: bool,
    pub exit_code: Option<i32>,
}

/// Run each hook in order with the given env vars set, stopping only for
/// I/O errors spawning a hook — a hook that exits non-zero is recorded in
/// its [`HookResult`] but does not abort the remaining hooks (matches
/// kernel-install's own hook-runner semantics: one broken script shouldn't
/// block every other hook). No-op (returns an empty result per hook,
/// `success: true`) when `dry_run` is set.
pub fn run_hooks(
    hooks: &[PathBuf],
    env: &[(String, String)],
    dry_run: bool,
) -> Result<Vec<HookResult>> {
    let mut results = Vec::new();
    for hook in hooks {
        if dry_run {
            results.push(HookResult {
                path: hook.display().to_string(),
                success: true,
                exit_code: None,
            });
            continue;
        }
        let mut cmd = Command::new(hook);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let status = cmd
            .status()
            .with_context(|| format!("running hook {}", hook.display()))?;
        results.push(HookResult {
            path: hook.display().to_string(),
            success: status.success(),
            exit_code: status.code(),
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn stash_plan_reports_existing_and_missing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write_file(&home.join(".config/dconf/user"), "x");
        let plan = compute_stash_plan(DesktopEnvironment::Gnome, &home);
        let dconf = plan
            .iter()
            .find(|e| e.relative_path == ".config/dconf/user")
            .unwrap();
        assert!(dconf.source_exists);
        let shell = plan
            .iter()
            .find(|e| e.relative_path == ".config/gnome-shell")
            .unwrap();
        assert!(!shell.source_exists);
    }

    #[test]
    fn stash_moves_existing_paths_and_skips_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let stash_root = tmp.path().join("stash");
        write_file(&home.join(".config/dconf/user"), "gnome dconf data");

        let moved = stash(DesktopEnvironment::Gnome, &home, &stash_root, false).unwrap();

        assert_eq!(moved, vec![".config/dconf/user".to_string()]);
        assert!(!home.join(".config/dconf/user").exists());
        let stashed = stash_root.join("gnome/.config/dconf/user");
        assert_eq!(fs::read_to_string(&stashed).unwrap(), "gnome dconf data");
    }

    #[test]
    fn stash_dry_run_touches_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let stash_root = tmp.path().join("stash");
        write_file(&home.join(".config/dconf/user"), "data");

        let moved = stash(DesktopEnvironment::Gnome, &home, &stash_root, true).unwrap();

        assert_eq!(moved, vec![".config/dconf/user".to_string()]);
        assert!(
            home.join(".config/dconf/user").exists(),
            "dry-run must not move"
        );
        assert!(!stash_root.exists(), "dry-run must not create the stash");
    }

    #[test]
    fn restore_moves_stashed_paths_back() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let stash_root = tmp.path().join("stash");
        write_file(&home.join(".config/kdeglobals"), "kde data");

        stash(DesktopEnvironment::Kde, &home, &stash_root, false).unwrap();
        assert!(!home.join(".config/kdeglobals").exists());

        let restored = restore(DesktopEnvironment::Kde, &home, &stash_root, false).unwrap();

        assert_eq!(restored, vec![".config/kdeglobals".to_string()]);
        assert_eq!(
            fs::read_to_string(home.join(".config/kdeglobals")).unwrap(),
            "kde data"
        );
    }

    #[test]
    fn stash_never_deletes_missing_source_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let stash_root = tmp.path().join("stash");
        fs::create_dir_all(&home).unwrap();

        let moved = stash(DesktopEnvironment::Kde, &home, &stash_root, false).unwrap();
        assert!(moved.is_empty());
    }

    #[test]
    fn extract_portable_subset_parses_known_gnome_keys() {
        let dump = r#"
[org/gnome/desktop/background]
picture-uri='file:///usr/share/backgrounds/foo.jpg'

[org/gnome/desktop/interface]
color-scheme='prefer-dark'
accent-color='blue'

[org/gnome/desktop/input-sources]
sources=[('xkb', 'us')]
"#;
        let subset = extract_portable_subset_gnome(dump);
        assert_eq!(
            subset.wallpaper_path.as_deref(),
            Some("file:///usr/share/backgrounds/foo.jpg")
        );
        assert_eq!(subset.dark_mode, Some(true));
        assert_eq!(subset.accent_color.as_deref(), Some("blue"));
        assert_eq!(subset.keyboard_layout.as_deref(), Some("[('xkb', 'us')]"));
    }

    #[test]
    fn extract_portable_subset_missing_keys_are_none() {
        let subset = extract_portable_subset_gnome("[org/gnome/desktop/background]\n");
        assert_eq!(subset.wallpaper_path, None);
        assert_eq!(subset.dark_mode, None);
    }

    #[test]
    fn portable_subset_kde_args_only_emitted_for_present_fields() {
        let subset = PortableSubset {
            wallpaper_path: Some("/tmp/wall.jpg".to_string()),
            ..Default::default()
        };
        let cmds = portable_subset_to_kde_kwriteconfig_args(&subset);
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains(&"plasma-org.kde.plasma.desktop-appletsrc".to_string()));
    }

    #[test]
    fn discover_hooks_lists_only_executable_files_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_dir = tmp.path().join("hooks.d");
        fs::create_dir_all(&hooks_dir).unwrap();

        let exec_hook = hooks_dir.join("20-second.sh");
        fs::write(&exec_hook, "#!/bin/sh\ntrue\n").unwrap();
        let mut perms = fs::metadata(&exec_hook).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&exec_hook, perms).unwrap();

        let exec_hook_first = hooks_dir.join("10-first.sh");
        fs::write(&exec_hook_first, "#!/bin/sh\ntrue\n").unwrap();
        let mut perms = fs::metadata(&exec_hook_first).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&exec_hook_first, perms).unwrap();

        let non_exec = hooks_dir.join("README");
        fs::write(&non_exec, "not a hook").unwrap();

        let hooks = discover_hooks(&hooks_dir).unwrap();
        assert_eq!(hooks, vec![exec_hook_first, exec_hook]);
    }

    #[test]
    fn discover_hooks_missing_dir_is_empty_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks = discover_hooks(&tmp.path().join("does-not-exist")).unwrap();
        assert!(hooks.is_empty());
    }

    #[test]
    fn build_hook_env_contains_contract_vars() {
        let env = build_hook_env(
            DesktopEnvironment::Gnome,
            DesktopEnvironment::Kde,
            Path::new("/stash"),
            Path::new("/home/user"),
        );
        assert!(env.contains(&("REBASE_FROM_DE".to_string(), "gnome".to_string())));
        assert!(env.contains(&("REBASE_TO_DE".to_string(), "kde".to_string())));
        assert!(env.contains(&("REBASE_STASH_DIR".to_string(), "/stash".to_string())));
    }

    #[test]
    fn run_hooks_dry_run_executes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = tmp.path().join("hook.sh");
        // A script that would fail loudly if actually run.
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        let mut perms = fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms).unwrap();

        let results = run_hooks(&[hook], &[], true).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    #[test]
    fn run_hooks_records_failure_but_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let failing = tmp.path().join("10-fail.sh");
        fs::write(&failing, "#!/bin/sh\nexit 3\n").unwrap();
        fs::set_permissions(&failing, fs::Permissions::from_mode(0o755)).unwrap();
        let passing = tmp.path().join("20-pass.sh");
        fs::write(&passing, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&passing, fs::Permissions::from_mode(0o755)).unwrap();

        let results = run_hooks(&[failing, passing], &[], false).unwrap();

        assert_eq!(results.len(), 2);
        assert!(!results[0].success);
        assert_eq!(results[0].exit_code, Some(3));
        assert!(results[1].success);
    }
}
