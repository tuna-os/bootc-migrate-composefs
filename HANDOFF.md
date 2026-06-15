# OSTree → ComposeFS Migration Tool — Handoff

**Repository:** `hanthor/ostree-composefs-rebase`  
**Goal:** In-place migration from OSTree-booted Bluefin:stable to ComposeFS-booted Dakota:stable via systemd-boot  
**Agent:** pi (picked up from Claude Code session)  
**Approach:** TDD vertical slices (5 slices)  
**Last updated:** 2026-06-15 (Slice 1-2 complete; Slice 3-4 partially complete — e2e-sshd.socket active but per-connection sshd -i exits 255 on Dakota; debugging with /etc/ssh-debug.log wrapper)

---

## Current State Summary

**Migration succeeds:** Bluefin boots, SSH connects pre-migration, all 5 phases complete, Dakota composefs boots with systemd-boot, dbus/polkit/logind start, e2e-sshd.socket is active on port 22.

**One remaining gap:** Per-connection `sshd -i` (spawned by e2e-sshd.socket) exits 255/EXCEPTION on Dakota. SSH connections reach the server ("Connection closed by 127.0.0.1") but are immediately dropped. `sshd -t` passes in chroot with deploy /etc + bind-mounted /var. SELinux context investigation pending.

**Last diagnostic action:** `ensure_e2e_ssh_socket` now wraps `sshd -i` in a shell that logs exit code to `/etc/ssh-debug.log`. Awaiting E2E run to capture this (commit `e10aa16`).

---

## TDD Slice Plan

| # | Slice | Status |
|---|-------|--------|
| 1 | Unit: origin file schema is bootc-compatible | ✅ 5 tests green (SHA `1008766`) |
| 2 | Integration: `bootc status` works after migration | ⬜ blocked by SSH (can't run bootc status without SSH) |
| 3 | Integration: `e2e-sshd.socket` active post-migration | ✅ socket active on port 22, accepts connections |
| 4 | Integration: per-connection `sshd -i` works post-migration | 🔄 exits 255 — debugging via /etc/ssh-debug.log wrapper |
| 5 | Persistence: `/var`, `/etc`, `/home` assertions pass | ⬜ blocked by SSH |

---

## End Goal

A Bluefin:stable user runs the migration binary once and ends up booted on Dakota:stable via systemd-boot + composefs, with `/home`, `/etc` customizations, `/var` (flatpaks, container storage, logs), and user accounts preserved. "Migration completed" output is not success — composefs must actually boot AND user data must remain intact.

## What Works

- Phase 0 free-space check, Phase 1 OSTree import (skippable), Phase 2 OCI pull, Phase 3 EROFS seal (idempotent), Phase 4 /etc 3-way merge / .origin / .imginfo / /var handling / dangling symlink pruning / identity DB line-merge / e2e-sshd.socket provisioning, Phase 5 bootloader staging.
- `.origin` file uses `tini::Ini`; includes `container-image-reference`, `boot_type=bls`, real `boot_digest` (sha256 of vmlinuz||initrd, patched after extraction), and `manifest_digest`.
- systemd-boot BLS entry shows up as default; recovery via firmware menu or GRUB.
- composefs boots with dbus.socket, polkit, logind, NetworkManager all reaching `Started`.
- e2e-sshd.socket active on TCP 22 post-migration; accepts connections.
- sshd.service disabled in deploy /etc (prevents port conflict with e2e-sshd.socket).

## Previously Solved

| Blocker | Resolution | SHA |
|---------|------------|-----|
| Phase 5 silently writes ESP BLS entries with no systemd-boot binary on ESP → VM falls back to OSTree | Preflight `systemd_boot_binaries_present` field added; Phase 5 originally routed to GRUB2 when source binary absent | a4b231a |
| GRUB2 fallback path set `next_entry` via `grub2-reboot` but bootupd's grub.cfg has no `if [ "${next_entry}" ]` block, so the one-shot was silently ignored | Phase 5 now writes `saved_entry` directly via `grub2-editenv` | e0b543f |
| Required systemd-boot package on source (Bluefin) OS | Phase 5 sources `systemd-bootx64.efi` from the target image; efibootmgr registers `Linux Boot Manager` | e0b543f |
| Raw EROFS mount returned zero-filled content past inline threshold | Tried `bootc internals cfs oci mount` first (commit `7abda35`) — but it fails because the pull flow doesn't populate `streams/oci-config-<verity>` ref; fell back to broken EROFS path silently | 7abda35 |
| EROFS-corrupted vmlinuz+initrd+sd-boot still ending up on ESP/boot | Switched to `podman create` + `podman cp` to extract real bytes from target image (commit `76628a4`) — but podman pull blew the VM's disk (ENOSPC in `/var/lib/containers/storage`) so extraction failed and migration fell back to GRUB2 with corrupt boot artifacts | 76628a4 |
| Extraction fills disk | Phase 5 now streams OCI layers one-at-a-time from registry via skopeo, extracting boot artifacts directly from compressed tarballs. No overlay expansion, ~1-2 GB footprint | 81c7781 |
| /var fstab synthesis fails when /proc/mounts shows subvolid= instead of subvol= | Fall back to subvolid=, default to subvol=/ if neither present; add diagnostic logging of /proc/mounts line | 468c8eb |
| Previously assumed: "raw EROFS kernel mount zero-fills out-of-line data" — WRONG. EROFS being metadata-only is by design; the composefs overlay supplies content. The overlay was working all along | n/a — diagnosis retracted | TBD |
| dbus.service / polkit / logind cascade-fail post-reboot — real root cause: 3-way /etc merge brought forward Bluefin's enablement symlinks; many point to units that don't exist in Dakota (`dbus.service → /usr/lib/systemd/system/dbus-broker.service` — Dakota uses classic dbus). 102 dangling /etc symlinks total, ~30 in /etc/systemd/system | Added `prune_dangling_usr_symlinks` to mergetc.rs; Phase 4 walks merged /etc after merge and drops symlinks whose `/usr/*` target is absent in the target image | TBD |
| /etc/passwd, /etc/shadow, /etc/group, /etc/gshadow, /etc/subuid, /etc/subgid, /etc/machine-id were getting replaced by Dakota's factory copies (~3 lines, missing messagebus/polkitd/systemd-resolve/etc). Because Bluefin's /usr/etc/passwd matches /etc/passwd on a freshly installed system, the standard 3-way rule (`old==cur, take new`) selected Dakota's near-empty file. Result: dbus/polkit/systemd-resolve/sshd all 217/USER at start | Added `is_identity_db` check in mergetc (line-union by first colon), and replaced the EROFS-mount-based `new_default_etc` source with a registry-streamed `/etc` tree (`extract_subtree_via_registry`). Identity DBs now line-merge against Dakota's actual content, not zero-fill. Phase 4 logs `streamed target /etc from registry for merge source` | TBD |
| Cross-image migration silently dropped source-only files (e2e-sshd.socket, flatpak-nuke-fedora.service, etc.) when source factory ≡ live ≡ target=absent. Standard OSTree upgrade rule "if old==cur and new==None, drop" assumes same-image upgrades; for cross-image migration it deletes legitimate state | Changed file merge arm `(Some(_), Some(cur), None) => Some(cur)` — keep cur. Old test renamed and assertion flipped; new test `merge_keeps_source_only_unit_when_target_lacks_it` guards the e2e-sshd.socket case | TBD |
| `bootc status` fails with "No manifest_digest in origin and no legacy .imginfo file" | Switched to `tini::Ini` for byte-compatible .origin formatting; key `container` → `container-image-reference` (matches `ORIGIN_CONTAINER` constant); added `manifest_digest` to `[boot]` section so bootc can fetch OCI manifest from registry; `patch_origin_boot_digest` computes sha256(vmlinuz || initrd) after Phase 5 extraction | `9abeb0b` |
| OSTree fallback BLS entry on ESP broke `bootc status` (bootc parses every non-EFI ESP entry as composefs deployment, bails on missing `composefs=` cmdline) | Removed OSTree fallback from ESP entirely; recovery via firmware menu (`Fedora\shimx64.efi`) or GRUB; `build_ostree_fallback_on_esp` kept as `#[allow(dead_code)]` | `9abeb0b` |
| Origin file schema testable | Extracted `build_origin_content` + `patch_boot_digest_in_content` pure fns; 5 unit tests | `1008766` |
| sshd 255/EXCEPTION root cause #1: `sshd_config.d/40-redhat-crypto-policies.conf` from Bluefin survived merge, referencing `/etc/crypto-policies/` absent in Dakota | Adopted composefs 3-way merge semantic: `(Some(old), Some(cur), None)` with `old==cur` → drop (system file the target removed) | `9027a5f` |
| sshd 255/EXCEPTION root cause #2: `sshd.service` enablement symlink from Bluefin survived merge into Dakota deploy /etc, causing port conflict with e2e-sshd.socket | `ensure_e2e_ssh_socket` removes `multi-user.target.wants/sshd.service` symlink in deploy /etc | `4c703d6` |
| Migration binary not used in E2E (build was from old binary) | E2E uses `cargo build` at start of each run; binary is always fresh | n/a — workflow fix |
| `sshd` binary at `/usr/bin/sshd`, not `/usr/sbin/sshd` in Bluefin/Dakota | Fixed path in e2e-sshd@.service | `7a10476` |
| GitHub issues cleanup | Closed 12 implemented issues; filed #15 for config drift GUI | n/a |
| E2E injection writing to ESP (vfat) instead of btrfs root | Fixed to find btrfs partition via blkid | `fc0c3a5` |
| sshd_config.d/90-e2e.conf not created (missing mkdir -p) | Fixed mkdir -p for sshd_config.d directory | `b7d8cc3` |

## Current Blocker: sshd -i exits 255 on Dakota (per-connection, socket-activated)

**Symptom:** SSH connections reach the VM ("Connection closed by 127.0.0.1"). e2e-sshd.socket is active on TCP 22. Each per-connection `sshd -i` exits 255 immediately.

**What's verified working:**
- `sshd -t` exits 0 with deploy /etc + bind-mounted /var
- `sshd -d -i` accepts connections in chroot (no crash)
- Host keys present with correct 600 perms; sshd_config.d only has `20-systemd-userdb.conf`
- /var/empty exists; sshd.service enablement removed from deploy /etc

**What differs between chroot (works) and real boot (fails):**
- SELinux: chroot has no SELinux; real boot is enforcing
- Environment: systemd socket activation passes fd vs chroot's bash pipe

**Current diagnostic:** `ensure_e2e_ssh_socket` (SHA `e10aa16`) wraps sshd -i in shell logging to `/etc/ssh-debug.log`. Awaiting E2E run.

**Candidate next probes:**
1. Read `/etc/ssh-debug.log` after next E2E
2. `restorecon -Rv /etc/ssh` in Phase 4 to fix SELinux labels
3. `UsePAM no` in sshd_config to rule out PAM
4. `journalctl -u 'e2e-sshd@*'` via serial console

## Pending

- **Slice 2**: `bootc status` verification (blocked by SSH)
- **Slice 4**: Debug per-connection sshd -i 255 → read `/etc/ssh-debug.log`
- **Slice 5**: /var, /etc, /home persistence assertions (blocked by SSH)
- Realistic user setup in E2E
- `--post-hook-dir` flag
- Exercise `commit` subcommand

## Future UX

- **Pre-migration config drift GUI** (GitHub issue #15): interactive TUI showing diff between OSTree factory /etc and live /etc with per-file checkboxes.

## Original Blocker Doc (kept for reference)

The primary migration path now installs systemd-boot from the target image:
- Writes `bootc_*.conf` (composefs default) and `ostree-fallback-0.conf` (Bluefin OSTree) to `<ESP>/loader/entries/`.
- Writes `<ESP>/loader/loader.conf` with `timeout 3` so the user can pick the fallback during evaluation.
- Falls back to the GRUB2 path automatically if the target image doesn't ship systemd-boot.

Need to re-run the E2E and confirm:
1. The VM boots into the composefs entry via systemd-boot.
2. `bootc status` reports the composefs deployment.
3. `bootc-migrate-composefs commit` removes the OSTree fallback from the ESP cleanly.

### Diagnostics to run

- Pre-reboot, on the VM: `ls <ESP>/EFI/systemd/`, `ls <ESP>/EFI/BOOT/`, `cat <ESP>/loader/loader.conf`, `efibootmgr -v | grep -i 'Linux Boot Manager'`, `ls <ESP>/loader/entries/`.
- After reboot: `cat /proc/cmdline` should contain `composefs=<hex>` and the booted loader (visible at `/run/systemd/efi/`) should be systemd-boot.

### Next candidate fixes

1. If `efibootmgr` fails to parse the ESP device path (LVM/dm-crypt), `\EFI\BOOT\BOOTX64.EFI` removable-media path acts as a fallback — confirm firmware picks it up.
2. If target image lacks systemd-boot, the GRUB2 branch should fire automatically; verify the warning message surfaces.
3. The `efibootmgr --create` call inserts at the front of `BootOrder` by default — confirm Fedora\shimx64.efi remains accessible by selecting it from firmware menu if composefs fails.

## E2E Test Infrastructure

### Local Registry (fast pulls)

```bash
# Start registry (one-time)
sudo podman run -d --name e2e-registry --network=host docker.io/library/registry:2

# Cache images (one-time)
sudo podman tag ghcr.io/projectbluefin/bluefin:stable 127.0.0.1:5000/bluefin:stable
sudo podman tag ghcr.io/projectbluefin/dakota:stable 127.0.0.1:5000/dakota:stable
sudo podman push --tls-verify=false 127.0.0.1:5000/bluefin:stable
sudo podman push --tls-verify=false 127.0.0.1:5000/dakota:stable
```

### Run

```bash
cd /var/home/james/dev/ostree-composefs-rebase && \
sudo -E env PATH=$PATH \
  BASE_IMAGE=ghcr.io/projectbluefin/bluefin:stable \
  TARGET_IMAGE=ghcr.io/projectbluefin/dakota:stable \
  ./tests/run-e2e.sh
```

### Optimizations

| Feature | Status | Time Saved |
|---------|--------|------------|
| Podman build cache (base image) | ✅ | ~30s |
| Local registry (target pull) | ✅ | ~20 min → ~30s |
| Disk checkpoint (skip install) | ✅ | ~5 min |
| --skip-import (skip Phase 1) | ✅ | ~10 min |
| Podman system prune (disk cleanup) | Manual | Frees ~100GB |

### Cleanup

```bash
# Kill stale QEMU processes
sudo kill $(pgrep -f 'qemu-system.*disk.raw') 2>/dev/null

# Free disk space
sudo podman system prune -af
rm -f disk.raw disk.raw.pre-migration qemu.log test_key*
```

## Architecture

```
src/
├── main.rs              — CLI: --bootloader, --dry-run, --skip-import, commit subcommand
├── preflight.rs         — System checks: ESP detection via lsblk partition GUID
├── reflink.rs           — FICLONE ioctl
├── ostree.rs            — OSTree repo scanner
├── composefs.rs         — bootc CLI wrappers for OCI operations
├── types.rs             — VerityDigest newtype (bare hex vs sha512: prefix)
├── xattr.rs             — xattr-preserving file/dir copy
├── mergetc.rs           — 3-way /etc merge with symlink support
└── migration/
    ├── mod.rs           — Orchestrator: 5 phases + lock file + mount guard
    ├── kernel_options.rs — composefs= cmdline builder (filters OSTree args)
    ├── os_release.rs    — /etc/os-release reader + BLS filename builder
    └── bootloader/
        ├── mod.rs       — BlsEntry struct
        ├── grub.rs      — GRUB2 operations (stub)
        └── systemd_boot.rs — systemd-boot operations (stub)
```

## Key Design Decisions

1. **VerityDigest newtype** — Prevents sha512: prefix bugs
2. **3-way /etc merge** — Falls back to flat copy on failure
3. **Dual-bootloader setup** — systemd-boot (primary, ESP) + GRUB2 (fallback, /boot)
4. **ESP auto-discovery** — Via lsblk partition type GUID when not auto-mounted
5. **Staged entries** — entries.staged/ → entries/ atomic rename
6. **Lock file** — F_OFD_SETLK at /var/run/bootc-migrate-composefs.lock
7. **MountGuard** — Drop-guard ensures umount on panic
8. **Free-space precheck** — Phase 0 before any mutations
9. **Idempotency** — Phase 3 skips seal if image exists; Phase 4 skips if .origin exists
10. **Local registry** — 10.0.2.2:5000 for fast VM pulls in E2E tests

## Test Suite

55 unit tests, 0 failures. Coverage includes:
- VerityDigest construction/formatting/panics (7)
- xattr-preserving copy + symlinks (5)
- 3-way /etc merge all cases + symlinks (13)
- Kernel option filtering + representative Bluefin cmdline (11)
- os-release parsing + BLS filename construction (8)
- ESP parsing, preflight, BLS rendering, reflink, OSTree scan

## CLI

```
bootc-migrate-compose --target-image <image>
  --force              Skip interactive prompts
  --dry-run            Print actions without executing
  --skip-import        Skip Phase 1 (OSTree object hashing)
  --bootloader <name>  "systemd-boot" (default) or "grub2"
  --skip-preflight     Skip preflight validation

bootc-migrate-composefs commit   # Make composefs permanent after successful boot
```

## Preflight Report Example

```
=== Migration Readiness ===
  ✓ All preflight checks passed.
  - ESP: auto-detected (498 MB free, mounts during migration)
  - ESP ready for sd-boot: Yes (>=150 MB)
  - GRUB tools available: Yes
  - Reflink (CoW) Support: Yes

Bootloader: Will migrate to systemd-boot (ESP ready, NVRAM writable).
```

## Next Steps (ordered by priority)

1. **Re-run E2E with dangling-symlink fix** — confirm SSH-after-reboot, `bootc status` reports composefs, and `cat /proc/cmdline` contains `composefs=<hex>`.
2. **Exercise `commit` subcommand** — After composefs boots stably, run `bootc-migrate-composefs commit` and confirm the OSTree fallback is removed from the ESP cleanly.
3. **Realistic Bluefin user setup in E2E** — Add a primary `bluefin` user via useradd inside the VM pre-migration, drop `gnome-initial-setup-done` markers, populate dconf/.local/share to mirror a real first-boot state.
4. **Post-reboot validation** — Verify /var, /etc, /home persistence after successful composefs boot.
5. **Reconsider prune scope** — current prune only drops symlinks under /usr/* with absent targets. Watch for cases where target is in /opt or /var (rare); broader audit may be needed if other cascades surface.
