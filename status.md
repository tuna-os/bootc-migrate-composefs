# XFS Migration Status

## Current Blocker: VM boots OSTree instead of ComposeFS after migration

**Root cause**: Migration uses systemd-boot path (ESP), but OVMF/GRUB boots centos shim → GRUB → which reads from `/boot/loader/entries/`. Composefs BLS entry and kernel+initrd are on ESP only.

**Fix needed**: Even when systemd-boot is installed, ALSO write composefs BLS + kernel+initrd to `/boot/` for GRUB fallback. Mirror what the GRUB2 path already does.

## What works
- ✅ Migration Phases 0-5
- ✅ Initrd rebuild (registry streaming, xfs.ko + mount cpio)
- ✅ verify_migration (in-VM)
- ✅ Host-side .raw scan (vmlinuz 19.6MB MZ, initrd 220MB, systemd-boot, .origin, BLS)

## Attempted fixes for composefs boot
1. ❌ efibootmgr BootOrder — OVMF VARS doesn't persist across QEMU restarts
2. ❌ GRUB saved_entry set in migration — GRUB ignores it (blscfg?)
3. ❌ Direct menuentry in /boot/grub2/grub.cfg — GRUB ignores modified config
4. ❌ Direct menuentry in ESP grub.cfg — file write works, but GRUB still shows old blscfg entries

## Next step
EROFS composefs image mounts during boot (`erofs: mounted...`) but is not used as root — system boots OSTree fallback. Likely cause: ext4 loopback at /sysroot/composefs isn't mounted early enough in initrd for ostree-prepare-root to find the composefs images. The xfs-mount.cpio mount unit may need `Before=ostree-prepare-root.service` ordering.

## RESOLVED: Composefs boots on XFS!

**Status**: SUCCESSFUL after hotfixes:
- `bootc-root-setup.service` (from bootc dracut module) mounts composefs as root
- Requires: meta.json in loopback repo, .wants symlink for mount unit, Requires= dependency
- Hostname preserved as "bluefin" from /etc merge (expected)

**Verified working**:
- Migration Phases 0-5 ✅
- Initrd rebuild (xfs.ko + loopback mount unit) ✅
- Registry streaming for kernel modules ✅
- bootc-root-setup.service in initrd ✅
- Composefs EROFS mounts as root ✅
- Dakota userspace (wallpaper, firstboot services) ✅

**Remaining (minor)** :
- SSH not available (e2e-sshd.socket not in composefs /etc)
- dbus failures on first boot
- Need to incorporate all hotfixes into migration code

**Hotfixes applied to checkpoint** :
1. bootc-modules.cpio with bootc-root-setup.service + initramfs-setup
2. xfs-mount.cpio with .wants symlink for auto-mount
3. Drop-in Requires=sysroot-composefs.mount on bootc-root-setup.service
4. meta.json in loopback repo (fsverity-sha512-12, verity:false)
5. BLS entry with /boot/ paths for GRUB
6. grub.cfg with set default=1 for composefs entry
