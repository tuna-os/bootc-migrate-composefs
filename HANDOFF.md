# OSTree → ComposeFS Migration Tool — Handoff

**Repository:** `hanthor/ostree-composefs-rebase`  
**Goal:** In-place migration from OSTree-booted Bluefin:stable to ComposeFS-booted Dakota:stable  
**Last updated:** 2026-06-14 (post grub-fix iteration)  

---

## Architecture

```
bootc-migrate-composefs (Rust CLI)
├── main.rs          — CLI entrypoint, preflight, orchestration
├── preflight.rs     — System checks (OSTree? UEFI? ESP? Btrfs? reflink?)
├── reflink.rs       — FICLONE ioctl for zero-copy block cloning
├── ostree.rs        — Scan OSTree repo objects, compute SHA-512
├── composefs.rs     — pull_image, create_image, seal_image wrappers
└── migration.rs     — 5-phase migration pipeline + /var migration
```

## 5-Phase Migration Pipeline

| Phase | What | Status |
|-------|------|--------|
| **1** | Import OSTree file objects → ComposeFS object store (SHA-512 keyed) | ✅ Working |
| **2** | Pull target OCI image via `bootc internals cfs oci pull` | ✅ Working |
| **3** | Create EROFS image via `bootc internals cfs oci create-image` + seal | ✅ Working |
| **4** | Stage deployment: copy /etc, write .origin, .imginfo, var symlink | ✅ Working |
| **5** | Mount EROFS, extract kernel/initrd, write bootloader entries | ✅ GRUB now selects the composefs entry; kernel boot fails downstream (see Current Blocker) |

## What Works

All migration phases complete without errors. The tool successfully:

1. **Detects** the system is OSTree-booted, UEFI-capable, Btrfs-backed
2. **Remounts** `/sysroot` and `/boot` read-write
3. **Imports** ~30,900 OSTree file objects into ComposeFS store
4. **Pulls** the target OCI image into the ComposeFS repository
5. **Creates** an EROFS filesystem image from the pulled layers
6. **Seals** the image (generates verity metadata)
7. **Mounts** the raw EROFS backing file directly (`mount -t erofs`) to extract kernel/initrd
8. **Copies** `/etc` configuration to the new deployment
9. **Writes** `.origin` and `.imginfo` metadata files
10. **Migrates** `/var` data from OSTree deployment to ComposeFS state directory

## Verified Artifacts (confirmed in CI)

After migration completes, the following exist on disk:

- **Deployment dir:** `/sysroot/state/deploy/<sha512_hash>/`
- **BLS entry:** `/boot/loader/entries/bootc_bluefin_dakota-<hash>.conf`
- **Kernel:** `/boot/bootc_composefs-<hash>/vmlinuz` (~18 MB)
- **Initrd:** `/boot/bootc_composefs-<hash>/initrd` (~120 MB)
- **EROFS image:** `/sysroot/composefs/images/<hash>` → objects symlink
- **.origin file:** Points to target OCI image with composefs digest
- **.imginfo file:** OCI config JSON for `bootc status`

## Current Blocker: ComposeFS Kernel Boots but Doesn't Reach SSH

**Symptom:** After reboot, GRUB correctly selects and starts the Dakota
(composefs) entry — the boot menu transcript shows `*Dakota` highlighted
and "Booting `Dakota'" printed — but the VM never becomes reachable via
SSH and the watch-for-boot loop times out. Without a serial console on
the composefs kernel cmdline we could not see what happened next.

**Why we believe this is the actual point of failure:**
- The previous-stage blocker (GRUB picking `ostree-1.conf` instead of our
  entry) was conclusively fixed: grubenv now holds the correct
  `saved_entry`, grub.cfg has `set default="${saved_entry}"`, and the
  serial dump of the GRUB TUI shows `*Dakota` as the selected default.
- The new failure happens after the kernel handoff: kernel/initrd are
  read off `/boot/bootc_composefs-<hash>/`, but the system does not come
  up — strongly suggests composefs-setup-root in initrd is failing, or
  rootfs mounts but networking/sshd does not start.

**In-flight diagnostic work (already pushed):**
- `tests/run-e2e.sh` patches `console=ttyS0,115200n8 console=tty0` into
  every BLS entry on the base disk *before first boot*. Since
  `get_kernel_options()` in `migration.rs` inherits cmdline from
  `/proc/cmdline`, the composefs entry written during migration will
  also get the serial console — so the post-reboot boot is visible.
- A `tail -F qemu.log` runs during both boot-waits and streams kernel
  output into the CI log as `[vm-serial] …`.

**Next steps once serial output lands:**
- Inspect kernel/initrd output from the failing Dakota boot.
- Likely candidates: composefs= argument format wrong (we currently
  emit `composefs=sha512:<hash>`; initrd might expect the bare hex),
  missing modules in the extracted initrd, or `/var` symlink not
  resolving in early boot.
- Confirm `bootc internals cleanup` is not needed pre-reboot to detach
  the OSTree deployment before composefs takes over.

## Previously Solved (in this session)

| Issue | Fix | Commit |
|-------|-----|--------|
| Raw `fs::write` to grubenv corrupting 1024-byte block | Use `grub2-editenv` (fall back to `grub2-set-default`); also set `GRUB_DEFAULT=saved` in `/etc/default/grub` | `6796b99` |
| bootupd grub.cfg never consults `saved_entry` (no `set default=` line) so blscfg picked its own default | Inject `set default="${saved_entry}"` before the `blscfg` command, idempotently | `a82a043` |
| No coverage that `/etc` state survives migration; `/home` symlink untested | Live-inject `/etc/migration-test/*`, in-place edit `/etc/hostname`, /etc symlink, and a real `useradd realuser` + `/home/realuser` content check | `6938d37` |
| No kernel-level visibility — qemu.log empty past GRUB | Patch BLS entries on disk to add `console=ttyS0,115200n8 console=tty0`; force `systemctl enable sshd` + direct multi-user symlink in derived image; stream qemu.log via tail -F with ANSI strip into CI log | `1a4c986` |
| Bluefin had no TCP sshd listener (uses systemd-ssh-generator → Unix-local + vsock only); local test reached login prompt but `ssh root@localhost:2222` never connected | Drop a `e2e-sshd.socket` (`ListenStream=22`, `Accept=yes`) + `e2e-sshd@.service` (`/usr/sbin/sshd -i`) into the derived Containerfile and pre-link it into `sockets.target.wants` | TBD next run |
| CI only ran fedora-bootc self-migration | Matrix the workflow over `fedora-bootc -> fedora-bootc` and `bluefin -> dakota` with `fail-fast: false` | `fafd0b9` |

## Previously Solved (earlier sessions)

| Issue | Fix | Commit |
|-------|-----|--------|
| `/sysroot` read-only | `mount -o remount,rw /sysroot` | `356ab23` |
| Missing `docker://` scheme | Auto-prepend transport prefix | `a7bfd38` |
| Multi-line pull output parsing | Parse manifest/config from labeled lines | `f981019`, `ce25a83` |
| Manifest vs config digest confusion | Separate manifest_digest from config_digest | `ff25839` |
| `/usr` read-only (`scp` destination) | Copy binary to `/var/tmp` | `4386461` |
| `/etc` copy fails on symlinks/sockets | Handle symlinks, skip special files | `53f2cd4` |
| `bootc internals cfs oci mount` needs sealed | Call `seal` after `create-image` | `6327e54` |
| Still "Can only mount sealed containers" | Mount raw EROFS file directly | `c0b8978` |
| `/boot` read-only (OSTree default) | `mount -o remount,rw /boot` | `18c7f85` |
| Colon in hash dir names confuses GRUB | Strip `sha512:` prefix from paths | `ab9ae0d` |
| `grub2-mkconfig` breaks on composefs= | Use grub2-set-default + grubenv | `bb55ed2`, `35899f9` |

## E2E Testing

### CI (GitHub Actions)
- **Workflow:** `.github/workflows/e2e-tests.yml`
- **Matrix** (`fail-fast: false`):
  - `fedora-bootc -> fedora-bootc` — self-migration smoke test
  - `bluefin:stable -> dakota:stable` — the real target scenario
- **Runner:** `ubuntu-latest` with QEMU + KVM + OVMF
- **Status:** fedora-bootc reaches the composefs entry post-reboot; the
  Dakota kernel boots but SSH never comes up (current blocker). Bluefin
  matrix leg still fails earlier — base image's sshd / kernel-console
  fix is in place but unverified.

### Local (Bluefin → Dakota)
- **Script:** `tests/run-e2e.sh` — same script as CI, just invoked
  directly via sudo. Use this to iterate without waiting for CI.
- Status mirrors CI: Bluefin base image historically didn't boot to SSH;
  visibility commit (`1a4c986`) should make any remaining issue
  observable on next run.

### Test Fixtures (injected before migration, verified after)
The test script creates these and verifies them post-migration:
- `/var/lib/migration-test/data` — basic persistence
- `/var/home/testuser/` — user home with dotfiles
- `/var/home/devuser/` — nested project structure + SSH keys
- `/var/lib/systemd/timers/` — system state
- `/var/lib/alternatives/` — symlinks
- `/var/cache/.hidden-dir/` — hidden directories
- `/etc/migration-test/{marker,nested}.conf` — custom /etc state
- `/etc/migration-test/marker.link` — symlink inside /etc
- `/etc/hostname` (in-place append) — verifies edits to existing files
- `realuser` (via `useradd`) + `/home/realuser/home-marker.txt` —
  verifies `/home -> /var/home` symlink resolves and `/etc/passwd`
  edits survive

### Maintaining this file

Keep `HANDOFF.md` current after each meaningful change. When fixing or
discovering a blocker:

1. Move the resolved item from "Current Blocker" into the appropriate
   "Previously Solved" table with its short commit SHA.
2. Replace the Current Blocker section with the *new* failing stage —
   include the symptom, why we believe that's the actual point of
   failure, what's already in-flight to diagnose it, and the next
   candidate fixes.
3. Bump the "Last updated" line.
4. Don't grow the doc indefinitely; trim stale "next steps" once they
   no longer apply.

## Key Design Decisions

1. **EROFS direct mount** instead of `bootc internals cfs oci mount` — avoids the "sealed container" requirement and works more reliably
2. **Clean hex hashes** for directory names — stripped `sha512:` prefix to avoid GRUB parsing issues
3. **Manual `/boot` management** — writing kernel/initrd and BLS entries directly rather than delegating to `prepare-boot` (which has caveats about bootc compatibility)
4. **Remounting `/sysroot` and `/boot` read-write** — required on OSTree systems where both are mounted read-only by default

## Files

```
.
├── SPECIFICATION.md              # Detailed technical specification
├── HANDOFF.md                    # This file
├── Cargo.toml
├── src/
│   ├── main.rs                   # CLI entrypoint
│   ├── migration.rs              # 5-phase pipeline orchestrator
│   ├── composefs.rs              # bootc internals cfs wrappers
│   ├── ostree.rs                 # OSTree repo scanner
│   ├── preflight.rs              # System preflight checks
│   └── reflink.rs                # FICLONE ioctl
├── tests/
│   └── run-e2e.sh                # QEMU-based E2E test script
└── .github/workflows/
    └── e2e-tests.yml             # CI workflow
```
