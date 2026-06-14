# OSTree → ComposeFS Migration Tool — Handoff

**Repository:** `hanthor/ostree-composefs-rebase`  
**Goal:** In-place migration from OSTree-booted Bluefin:stable to ComposeFS-booted Dakota:stable via systemd-boot  
**Last updated:** 2026-06-14 (EROFS+podman content corruption still open)  

---

## End Goal

A Bluefin:stable user runs the migration binary once and ends up booted on Dakota:stable via systemd-boot + composefs, with `/home`, `/etc` customizations, `/var` (flatpaks, container storage, logs), and user accounts preserved. "Migration completed" output is not success — composefs must actually boot AND user data must remain intact.

## What Works

- Phase 0 free-space check, Phase 1 OSTree import (skippable), Phase 2 OCI pull, Phase 3 EROFS seal (idempotent), Phase 4 /etc 3-way merge / .origin / .imginfo / /var handling, Phase 5 bootloader staging (BLS entries + loader.conf + efibootmgr NVRAM registration).
- systemd-boot BLS entry shows up in the loader menu and is selected as default; OSTree fallback is also presented.

## Previously Solved

| Blocker | Resolution | SHA |
|---------|------------|-----|
| Phase 5 silently writes ESP BLS entries with no systemd-boot binary on ESP → VM falls back to OSTree | Preflight `systemd_boot_binaries_present` field added; Phase 5 originally routed to GRUB2 when source binary absent | a4b231a |
| GRUB2 fallback path set `next_entry` via `grub2-reboot` but bootupd's grub.cfg has no `if [ "${next_entry}" ]` block, so the one-shot was silently ignored | Phase 5 now writes `saved_entry` directly via `grub2-editenv` | e0b543f |
| Required systemd-boot package on source (Bluefin) OS | Phase 5 sources `systemd-bootx64.efi` from the target image; efibootmgr registers `Linux Boot Manager` | e0b543f |
| Raw EROFS mount returned zero-filled content past inline threshold | Tried `bootc internals cfs oci mount` first (commit `7abda35`) — but it fails because the pull flow doesn't populate `streams/oci-config-<verity>` ref; fell back to broken EROFS path silently | 7abda35 |
| EROFS-corrupted vmlinuz+initrd+sd-boot still ending up on ESP/boot | Switched to `podman create` + `podman cp` to extract real bytes from target image (commit `76628a4`) — but podman pull blew the VM's disk (ENOSPC in `/var/lib/containers/storage`) so extraction failed and migration fell back to GRUB2 with corrupt boot artifacts | 76628a4 |

## Current Blocker: extract boot binaries from target image without filling the disk

The target image is already on disk twice (sealed EROFS at `/sysroot/composefs/images/<verity>` and pull artifacts at `/sysroot/composefs/streams/…`), but no straightforward tool reads it back:
- Raw `mount -t erofs -o ro,loop` returns metadata-only views (zero-filled file content past inline threshold).
- `bootc internals cfs --system oci mount <verity>` fails: `Opening ref 'streams/oci-config-<verity>': No such file or directory` — bootc looks for an OCI-config stream keyed by the EROFS verity, which our pull doesn't create.
- `podman pull <target>` works but needs ~5 GB of overlay storage just to extract three files; busted the 11.5 GB free space in the VM during the last run.
- `mount.composefs` / `composefs-info` are not installed on Bluefin.

### Next candidate fixes (ranked)

1. **Skopeo to `dir:` then stream-extract specific files from layer tarballs.** `skopeo copy --src-tls-verify=false docker://… dir:/tmp/oci` writes raw layer tarballs (compressed) without overlay-storage expansion. Walk layers newest-first; for each, `tar -xzf - <path>` looking for `usr/lib/systemd/boot/efi/systemd-bootx64.efi`, `usr/lib/modules/<kver>/vmlinuz`, `usr/lib/modules/<kver>/initramfs.img`. Stop at first hit per file. Compressed layer footprint is ~1–2 GB total, no overlay unpack.
2. **`bootc image copy-to-storage` equivalent.** Investigate whether bootc has a CLI for "give me a file out of an already-pulled OCI image" — would avoid the second download entirely.
3. **In-tree EROFS parser.** Open `/sysroot/composefs/images/<verity>`, walk inodes, resolve content sha256, read from `/sysroot/composefs/objects/<sha[:2]>/<sha[2:]>`. Adds a real dependency (`erofs` crate or hand-rolled parser).

### Diagnostics to run on next E2E

- Inside VM pre-extraction: `ls /sysroot/composefs/`, `ls /sysroot/composefs/streams/ 2>/dev/null`, `df -h /var/lib/containers /sysroot`. Confirms what bootc actually persisted vs what podman/skopeo would need to re-download.
- After extraction: `md5sum <esp>/EFI/systemd/systemd-bootx64.efi <esp>/EFI/Linux/bootc_composefs-*/vmlinuz` and compare to the same files extracted by `podman cp` directly on the host — sanity-check we got real content.

## Recently Tried (and why it failed)

- `bootc internals cfs --system oci mount` (7abda35) — preferred over raw EROFS, but errors out with missing `streams/oci-config-<verity>` ref.
- `podman create` + `podman cp` (76628a4) — pulls correct bytes but ENOSPC when the second image copy doesn't fit on the VM's overlay store. Will work on a bigger disk; not viable for tight VMs.

## Pending (after extraction is solved)

- Realistic user setup in E2E (primary user via useradd, gnome-initial-setup-done, dconf, ~/.config) seeded pre-migration and asserted post-reboot.
- `--post-hook-dir` flag (default `/etc/bootc-migrate-composefs/post-migrate.d`) for migration-specific cleanup like ublue-motd. Hooks get env: `COMPOSEFS_VERITY`, `TARGET_IMAGE`, `ESP_PATH`, `BOOTLOADER`.
- Exercise the `commit` subcommand end-to-end.

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

## Next Steps

1. **Re-run E2E with target-sourced systemd-boot** — Confirm Phase 5 extracts systemd-boot from Dakota, registers NVRAM, and VM boots into composefs on next reboot
2. **Exercise `commit` subcommand** — After verified composefs boot, run `bootc-migrate-composefs commit` and confirm the OSTree fallback is removed from the ESP cleanly
3. **Realistic Bluefin user setup in E2E** — Add a primary `bluefin` user via useradd inside the VM pre-migration, drop `gnome-initial-setup-done` markers, populate dconf/.local/share to mirror a real first-boot state
4. **Post-reboot validation** — Verify /var, /etc, /home persistence after successful composefs boot
