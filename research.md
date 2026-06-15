# Research: Bluefin LTS vs Bluefin Stable — SSH/E2E Boot Failure Analysis

## Summary

Bluefin LTS (`ghcr.io/projectbluefin/bluefin:lts`) fails to boot at all in the E2E QEMU environment — the VM drops to a GRUB rescue prompt with `"no such device: UUID"` before the kernel even loads. This is **not an SSH configuration issue**; it is a **GRUB/boot chain failure** — GRUB cannot locate the root filesystem by UUID on the virtio block device. Bluefin stable (Fedora-based) works perfectly; Bluefin LTS (CentOS Stream 10-based) does not. The most likely root cause is that the CentOS Stream 10 GRUB build lacks btrfs module support for the virtio disk, or `bootc install to-disk` on LTS produces GRUB configuration incompatible with QEMU's virtio-blk device enumeration.

## Findings

1. **GRUB rescue prompt — kernel never loads.** The qemu.log from the LTS run shows the VM enters UEFI, launches GRUB, and GRUB immediately errors: `error: ../../grub-core/commands/search.c:470:no such device: 5feade0d-fe38-4f5b-b8c1-375401f2607c.` followed by `error: ../../grub-core/commands/boot.c:196:you need to load the kernel first.` The VM drops to a `grub>` rescue prompt. This happens before any systemd unit or SSH service could start. [Source: local qemu.log from LTS E2E run]

2. **Bluefin LTS is CentOS Stream 10-based; stable is Fedora-based.** Bluefin `:stable` tracks Fedora releases (currently Fedora 41). Bluefin `:lts` is based on CentOS Stream 10, which is a fundamentally different OS base with a different kernel, GRUB package, and init system configuration. The CentOS/EPEL ecosystem historically uses different GRUB module configurations than Fedora. [Source: Bluefin project documentation at github.com/ublue-os/bluefin — LTS image uses CentOS Stream 10 base]

3. **GRUB UUID search failure points to device enumeration or module issue.** The GRUB `search --fs-uuid` command fails to find the btrfs root filesystem. In QEMU with `-drive file=disk.raw,format=raw,if=virtio`, the block device appears as a virtio-blk device (`/dev/vda`). If the CentOS Stream 10 GRUB image lacks the `btrfs` module or the `virtio` disk driver module, GRUB can't read the filesystem to find the kernel. Fedora's GRUB build typically includes both `btrfs.mod` and `virtio.mod`; CentOS Stream 10 may not. [Source: qemu.log error pattern; known GRUB/QEMU compatibility issue with virtio + missing modules]

4. **`bootc install to-disk` may produce different GRUB config on LTS.** The `bootc install to-disk` command writes GRUB configuration referencing the root filesystem UUID. If `bootc` on the LTS image (potentially a different version) writes the GRUB config with a UUID that doesn't survive the install process, or references a device path that QEMU's OVMF doesn't expose the same way, GRUB can't resolve it. [Source: bootc install documentation; E2E test script shows `bootc install to-disk --generic-image --filesystem btrfs`]

5. **Even if GRUB boot worked, SSH enablement may also differ.** The CentOS Stream base likely has different systemd preset policies (default-disabled services), and the `sshd` package may not be installed by default in the same way. The E2E derived image's `systemctl enable sshd.service` inside the Containerfile may behave differently on a CentOS-based system due to different preset files or systemd version. [Source: analysis of E2E Containerfile and CentOS/RHEL vs Fedora systemd preset differences]

6. **Kernel version difference.** Bluefin LTS ships an older LTS kernel (likely 6.12.x from CentOS Stream 10). Bluefin stable ships a newer Fedora kernel (6.13+). While kernel age alone shouldn't prevent boot, driver differences (particularly for virtio-blk in the initramfs) could contribute to the boot failure after GRUB hands off to the kernel. [Source: CentOS Stream 10 kernel policy vs Fedora tracking latest kernels]

## Root Cause Assessment (most likely)

The **primary issue** is GRUB's inability to read the btrfs root filesystem on the virtio block device. This is almost certainly caused by the CentOS Stream 10 GRUB image either:

- **Missing `btrfs.mod`**: GRUB can't read btrfs filesystems without this module
- **Missing `virtio.mod` or `virtio_blk.mod`**: GRUB can't see the virtio block device
- **Different `grub-mkconfig` / `grub2-mkconfig` behavior**: The UUID search path generated on install doesn't match the exposed device

This is confirmed by the qemu.log showing GRUB reaches the rescue prompt — meaning UEFI successfully hands off to GRUB, but GRUB's filesystem/driver stack can't reach the root partition.

## Recommendations for Fixing LTS E2E

### Option A: Pre-load GRUB modules in the derived image (fastest)
Add a step to the E2E Containerfile that ensures the GRUB image on the ESP includes btrfs and virtio modules:
```dockerfile
RUN grub2-mkimage -O x86_64-efi -o /tmp/grubx64.efi \
    btrfs virtio part_gpt ext2 fat search search_fs_uuid \
    normal linux configfile && \
    cp /tmp/grubx64.efi /boot/efi/EFI/BOOT/BOOTX64.EFI
```
This would rebuild the GRUB EFI binary with the required modules.

### Option B: Use `--filesystem xfs` for LTS (simpler)
If the LTS E2E is testing the XFS path anyway (as implied by the goal), avoid btrfs entirely for the source install. XFS has broader GRUB support:
```bash
bootc install to-disk --generic-image --filesystem xfs ...
```

### Option C: Boot LTS VM with `-device virtio-blk-pci,drive=...` instead of `-drive if=virtio`
Sometimes the explicit PCI device path helps GRUB enumerate devices correctly. This would change the QEMU command line in the test script.

### Option D: Add `root=/dev/vda3` to the GRUB cmdline (workaround)
After `bootc install`, patch the GRUB BLS entry to include a device-path-based `root=` parameter instead of (or in addition to) UUID-based search. This could be done in the BLS injection step of the E2E script.

### Recommended approach
**Start with Option B** since the goal mentions XFS validation, and XFS avoids the btrfs GRUB module dependency. If the test must use btrfs for LTS, then **Option A** (ensuring GRUB modules are present) is the cleanest fix.

## Sources

- Kept: local `qemu.log` from `e2e-lts` run — direct evidence of GRUB rescue prompt with UUID search failure
- Kept: local `e2e-lts.log` — shows E2E configuration and preflight checks completing before stalling
- Kept: local `e2e-run.log` from `e2e` (stable) — confirms stable→Dakota migration works fully with 31/31 assertions green
- Kept: local `justfile` — shows `e2e-lts` target uses `BASE_IMAGE=ghcr.io/projectbluefin/bluefin:lts`
- Kept: local `tests/run-e2e.sh` — shows full E2E workflow including derived image build with sshd enablement and GRUB BLS patching
- Kept: local `HANDOFF.md` — confirms stable E2E is fully green with all persistence and rollback checks passing
- Kept: github.com/ublue-os/bluefin — project documentation confirms LTS is based on CentOS Stream 10
- Dropped: N/A — all sources are local files or primary project documentation

## Gaps

1. **Cannot confirm GRUB module list in LTS image.** Without shell access to pull and inspect the LTS container image (`podman run --rm ghcr.io/projectbluefin/bluefin:lts ls /usr/lib/grub/x86_64-efi/`), the exact missing GRUB module cannot be identified. This should be the first diagnostic step.
2. **Cannot confirm bootc version in LTS.** The bootc version in the CentOS Stream 10 base may differ from Fedora's, potentially producing different GRUB configuration during `bootc install to-disk`.
3. **Cannot check if sshd is installed in LTS base.** The CentOS Stream 10 minimal base may not include openssh-server by default, requiring additional package installation in the derived image.
4. **Firewall state unknown.** CentOS/RHEL derivatives typically ship with firewalld enabled, which blocks SSH by default. Even if GRUB boot is fixed, firewalld may need to be disabled or port 22 opened in the derived image.

## Next Steps

1. Pull and inspect the LTS image: `podman run --rm ghcr.io/projectbluefin/bluefin:lts bash`
   - Check GRUB modules: `ls /usr/lib/grub/x86_64-efi/ | grep -E 'btrfs|virtio'`
   - Check for sshd: `rpm -qa | grep openssh-server`
   - Check kernel version: `rpm -qa | grep kernel-core`
   - Check bootc version: `bootc --version`
2. Test boot with XFS filesystem (Option B) as the goal already targets XFS validation
3. If btrfs is required, rebuild the GRUB EFI image with virtio+btrfs modules (Option A)
4. Verify the existing stable E2E still passes after any changes to the test infrastructure
