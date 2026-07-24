# Filesystem Support: btrfs vs XFS

This document explains how the migration tool handles the two most common root
filesystem types it encounters — btrfs (Bluefin stable) and XFS (Bluefin LTS) —
and where the two paths diverge.

## Background: what composefs requires

The composefs object store (`/sysroot/composefs/`) holds content-addressed EROFS
images and their backing file objects. When `bootc` pulls an OCI image into this
store, it seals each file object with **fs-verity** — a kernel feature that computes
and stores a Merkle-tree hash over a file's contents, allowing the kernel to detect
on-the-fly corruption or tampering.

**fs-verity requires kernel + filesystem support.** As of Linux 6.12:

| Filesystem | fs-verity | reflink (CoW copy) |
|------------|-----------|--------------------|
| btrfs      | ✅ yes    | ✅ yes             |
| ext4       | ✅ yes    | ❌ no              |
| XFS        | ❌ no     | ✅ yes (on some)   |

Bluefin **stable** (Fedora-based) ships btrfs by default.  
Bluefin **LTS** (CentOS Stream 10-based) ships XFS by default.

---

## btrfs path (Bluefin stable → Dakota stable)

The happy path. No workarounds needed.

### Phase 0 — preflight

`preflight.rs` reads `/proc/mounts` to confirm the root filesystem type and probes
for reflink support by attempting an actual copy-on-write clone under `/sysroot`.
On btrfs both checks pass.

### Phase 1 — OSTree object import (optional)

OSTree file objects are reflinked (copy-on-write cloned) from the existing OSTree
repo at `/sysroot/ostree/repo` into the composefs object store at
`/sysroot/composefs/objects`. Reflink is ~instant and uses no extra disk space on
btrfs, so this phase is cheap.

`check_free_space` uses a **1.1× multiplier** when reflink is available (vs 1.5×
without), since a reflink share means almost no additional blocks are consumed.

### Phase 3 — EROFS seal

`bootc internals cfs seal` calls `ioctl(FS_IOC_ENABLE_VERITY)` on each object file.
btrfs handles this natively; no extra setup.

### /var migration

On btrfs, `/var` is typically a subvolume mounted with `subvol=/` or a named
subvolume. The tool copies `/var` data into `/sysroot/state/os/default/var` —
the composefs state path — via `copy_dir_all_with_xattrs`. The composefs initramfs
bind-mounts this path at `/var` post-pivot, so user data is preserved.

---

## XFS path (Bluefin LTS → Dakota stable)

XFS does not support fs-verity, so a workaround is needed before any composefs
objects can be sealed. Additionally, Bluefin LTS uses LVM for the root volume,
which requires initrd changes before the composefs boot can succeed.

### Phase 0 — preflight

Same fs-type detection. `is_btrfs = false`, `fs_type = "xfs"`. Reflink is probed
separately (XFS supports reflink on many configurations but not all).

### Pre-Phase 1 — ext4 loopback for fs-verity

`setup_composefs_loopback_if_needed()` runs before Phase 1 when `fs_type == "xfs"`:

1. Creates a sparse file at `/sysroot/composefs-loopback.ext4`.  
   Size = `clamp(ceil(ostree_repo_GB × 1.5 + 5), 10, 30)` GB.
2. Formats it as ext4 with `-O verity` (`mkfs.ext4 -F -O verity`).
3. Loop-mounts it at `/sysroot/composefs` with `-o loop`.

All subsequent composefs operations (object store writes, EROFS image creation,
fs-verity sealing) target `/sysroot/composefs`, which is now an ext4 filesystem
living inside a sparse file on the XFS root — sidestepping XFS's lack of verity
support entirely.

The loopback image is idempotent: if it already exists and is mounted, the tool
detects this via `findmnt` and skips recreation.

### Phase 1 — OSTree object import

On XFS without reflink, `check_free_space` uses a **1.5× multiplier** and performs
full copies rather than reflink clones. This is slower and uses more space. If
reflink is available on the XFS volume, the tool uses it (the probe is runtime, not
hardcoded to XFS=no-reflink).

### Phase 3 — EROFS seal

`bootc internals cfs seal` runs against the composefs store on the ext4 loopback,
where `ioctl(FS_IOC_ENABLE_VERITY)` succeeds normally.

### Phase 5 — LVM initrd rebuild

Bluefin LTS installs root on LVM (`/dev/mapper/<vg>-<lv>`). The Dakota initrd
fetched from the OCI registry was built without LVM dracut modules, so it cannot
activate the volume group at boot and drops to an emergency shell.

`phase5_setup_bootloader` detects this and rebuilds the initrd:

1. **`detect_lvm()`** — checks `/dev/mapper` for entries other than `control`.
   If none, the rebuild step is a no-op.

2. **`rebuild_initrd_with_lvm_if_needed(kver, mount_path, initrd_dst)`**:
   - Locates `dracut` on the running host (`/usr/bin/dracut` or `/usr/sbin/dracut`).
     Bluefin LTS ships dracut; Bluefin stable does not need this step.
   - The Dakota kernel modules are available at
     `<composefs_mount>/usr/lib/modules/<kver>/` via the bootc composefs overlay
     mount (real file content, not zero-filled, because the composefs object store
     is populated by Phase 2).
   - Creates a temporary symlink: `/lib/modules/<kver>` → composefs mount path.
     On modern Fedora/CentOS, `/lib` → `/usr/lib`, so this resolves to the
     right location for dracut's module search.
   - Runs: `dracut --kver <kver> --add "lvm dm" --force <initrd_dst>`
   - Removes the symlink unconditionally (even if dracut fails).

3. The LVM-enabled initrd **replaces** the OCI-fetched one in place. Because this
   runs before `patch_origin_boot_digest`, the sha256(vmlinuz ‖ initrd) hash in
   the `.origin` file covers the final LVM-enabled bytes.

If dracut is unavailable or exits non-zero, the tool warns with the exact manual
command and continues — the migration completes and the user can fix the initrd
from the OSTree fallback entry.

---

## Dedicated `/var` volume (separate partition or LV)

Anaconda's default partitioning — and many real-world installs — put `/var` on
its **own filesystem** (a separate partition or LVM logical volume), mounted via
an `/etc/fstab` entry, distinct from the root volume. This is orthogonal to
btrfs-vs-XFS: it can occur on either.

### The problem

bootc's composefs boot bind-mounts the **per-stateroot var**
(`/sysroot/state/os/default/var`, which lives on the root filesystem) onto `/var`
and **ignores the `/var` fstab entry entirely**. (Traditional OSTree does the
opposite — the fstab `var.mount` overmounts the stateroot bind, so the dedicated
volume wins.) On a composefs boot this means a dedicated `/var` volume is left
unmounted and `/var` silently falls back to the empty stateroot var — losing the
user's home directories, flatpaks, container storage, Tailscale state, etc. The
data is safe (the volume is untouched), but the system boots unusable.

Two things are needed to fix this, both handled automatically:

### 1. Activate every LV backing a mounted filesystem

The source OSTree cmdline typically lists only the root LV
(`rd.lvm.lv=<vg>/root`); non-root volumes like a dedicated `/var` auto-activate
post-switchroot on the source distro, so they never appear on the cmdline. The
composefs target may lack that auto-activation path.

`get_kernel_options` discovers every LV currently backing a mount
(`findmnt` → `lvs`) and emits `rd.lvm.lv=<vg>/<lv>` for each, so the initrd
activates them before their mounts run.

### 2. Mount the dedicated `/var` at the stateroot var path

Activation alone isn't enough, because bootc still binds the stateroot var onto
`/var` and ignores fstab. `phase5_setup_bootloader` therefore:

1. **`detect_separate_var()`** — uses `findmnt -o SOURCE,FSTYPE,FSROOT /var`; a
   dedicated volume has `FSROOT == "/"` (a whole filesystem), versus a subtree
   bind (btrfs `subvol=` or the ostree `…/var` bind) whose FSROOT is a subpath.
   Returns the volume's `(uuid, fstype)`.

2. **`prepare_stateroot_var_include(uuid, fstype)`** — injects a
   `sysroot-state-os-default-var.mount` unit into the rebuilt initrd (via
   `dracut --include`, mirroring the composefs loopback mount). It mounts the
   dedicated volume at `/sysroot/state/os/default/var`, ordered
   `After=sysroot.mount Before=bootc-root-setup.service`.

`bootc-root-setup` then binds **that path** (now the real `/var` volume) onto
`/var`, so the user's data appears at `/var` as expected.

This path is exercised by the `xfs+lvm+crypt` e2e scenario (LVM-on-LUKS with
separate `root` + `var` LVs); its `/var`-persistence assertions verify the
dedicated volume's data survives the migration.

---

## Re-running after a failed composefs boot

If the first migration attempt produced a non-LVM initrd and the system is now stuck
in a dracut emergency shell, boot back to the OSTree fallback (hold ESC at POST,
select the centos/OSTree GRUB entry) and re-run:

```bash
bootc-migrate \
  --target-image ghcr.io/projectbluefin/dakota:stable \
  --force --skip-import
```

- `--skip-import` reuses existing composefs objects from Phase 1 (skips the slow
  object copy/reflink phase).
- `--force` bypasses the Phase 5 idempotency check: the existing `bootc_*` BLS
  entries are removed and Phase 5 re-runs in full, including the LVM initrd rebuild.

---

## Summary table

| Concern                   | btrfs (stable)             | XFS (LTS)                        |
|---------------------------|----------------------------|----------------------------------|
| fs-verity support         | native                     | ext4 loopback at /sysroot/composefs |
| composefs store location  | /sysroot/composefs (btrfs) | /sysroot/composefs (ext4 loop)   |
| Phase 1 object copy       | reflink (instant, ~0 extra space) | full copy or XFS reflink    |
| Free-space multiplier     | 1.1×                       | 1.5× (without reflink)           |
| LVM root                  | uncommon                   | typical (Bluefin LTS default)    |
| initrd rebuild needed     | no                         | yes — dracut --add "lvm dm"      |
| dracut on source system   | not present                | present (CentOS Stream 10 base)  |

Dedicated `/var` (separate partition/LV) is handled independently of the root
filesystem type — see [Dedicated `/var` volume](#dedicated-var-volume-separate-partition-or-lv).
