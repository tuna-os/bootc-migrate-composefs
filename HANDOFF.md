# OSTree → ComposeFS Migration Tool — Handoff

**Repository:** `hanthor/ostree-composefs-rebase`  
**Goal:** In-place migration from OSTree-booted Bluefin:stable to ComposeFS-booted Dakota:stable via systemd-boot  
**Last updated:** 2026-06-14 (sd-boot fallback wired)  

---

## What Works

The migration binary successfully completes all 5 phases end-to-end in E2E testing. The last run produced:

```
=== MIGRATION COMPLETED ===
Primary bootloader: systemd-boot
```

All phases complete without errors:
- **Phase 0**: Free-space check passes
- **Phase 1**: OSTree object import (skipped via --skip-import)
- **Phase 2**: OCI pull from local registry (fast)
- **Phase 3**: ComposeFS EROFS image creation + seal (idempotent on re-runs)
- **Phase 4**: /etc 3-way merge, .origin, .imginfo, /var migration
- **Phase 5**: Bootloader setup (systemd-boot on ESP + GRUB2 fallback on /boot)

## Previously Solved

| Blocker | Resolution | SHA |
|---------|------------|-----|
| Phase 5 silently writes ESP BLS entries with no systemd-boot binary on ESP → VM falls back to OSTree | Preflight now exports `systemd_boot_binaries_present`; phase 5 routes to GRUB2 when `/usr/lib/systemd/boot/efi` is absent and errors loudly if `bootctl install` returns non-zero | _pending commit_ |

## Current Blocker: verify composefs entry actually boots via GRUB2

With the sd-boot fallback wired, Bluefin VMs (no systemd-boot package) should now follow the GRUB2 branch in `phase5_setup_bootloader` and write the composefs entry to `/boot/loader/entries/` with `grub2-reboot` setting it one-shot. Need to re-run the E2E and confirm the next boot lands on the composefs entry, not the OSTree fallback.

### Diagnostics to run

- After migration completes, on the VM: `ls /boot/loader/entries/` (expect `bootc_*.conf` + ostree fallback), `grub2-editenv list` (expect `saved_entry=bootc_…`), `grep blscfg /boot/grub2/grub.cfg`.
- After reboot: `cat /proc/cmdline` should contain `composefs=<hex>`.

### Next candidate fixes (if GRUB2 doesn't pick composefs entry)

1. Verify `blscfg` module is in `grub.cfg`; some Bluefin builds patch it out.
2. Confirm `/etc/default/grub` `GRUB_DEFAULT=saved` survives the /etc 3-way merge — the merge runs in Phase 4 before Phase 5's grub patches.
3. If grub still skips the entry, consider regenerating grub.cfg explicitly with `grub2-mkconfig -o /boot/grub2/grub.cfg` after writing the BLS files.

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

1. **Re-run E2E with sd-boot fallback** — Confirm Phase 5 takes the GRUB2 branch automatically on Bluefin and that VM boots into composefs on next reboot
2. **systemd-boot package in target image** — Dakota should include systemd-boot by default so the primary path actually exercises
3. **Post-reboot validation** — Verify /var, /etc, /home persistence after successful composefs boot
