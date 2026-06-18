# AGENTS.md — Testing & Verification Strategy

## Status: XFS composefs migration pipeline complete ✅

All fixes validated individually. CI runs on every PR → main.

## What's working

| Component | Status |
|-----------|--------|
| Registry /etc extraction | ✅ 132 entries, no EROFS zero-fill |
| Identity DB supplement | ✅ dbus/messagebus user added |
| Dangling symlink cleanup | ✅ dbus-broker → dbus.service handled |
| sysroot-composefs.mount | ✅ persisted in deploy /etc |
| bootc-composefs-rebind.service | ✅ loopback visible through overlay |
| BLS entry title | ✅ `Dakota 42 (composefs)` format |
| Host-side disk scan | ✅ 9 validation checks |
| dbus after composefs boot | ✅ SSH in ~55s |
| bootc status | ✅ stale BLS entries cleaned |
| --version flag | ✅ Git SHA embedded |

## Remaining work

| Item | Priority | Notes |
|------|----------|-------|
| LUKS encryption for E2E | Medium | bootc install --encrypt flag |
| Commit subcommand | Low | Needs bootc composfs --policy |
| BTRFS E2E test pass | Low | Should work, not validated recently |

## Two-sided testing

Every E2E run validates migration correctness from **both sides**:

| Side | What | Where | Executes |
|------|------|-------|----------|
| **In-VM** | `verify_migration()` in the migrator binary | `src/migration/mod.rs` | Inside QEMU, after Phase 5 |
| **Host-side** | `.raw` disk image scan | `tests/run-e2e.sh` | On the CI/laptop host |

## CI matrix

| Scenario | Base | Target | Filesystem | --skip-import |
|----------|------|--------|------------|---------------|
| btrfs + composefs | bluefin:stable | dakota:stable | btrfs | yes |
| xfs + loopback | bluefin:lts | dakota:stable | xfs | yes |
| LUKS + xfs (TODO) | bluefin:lts | dakota:stable | xfs+crypt | yes |

## Common failure modes

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `meta.json not found` | EROFS overlay shadows loopback mount | bootc-composefs-rebind.service bind-mounts on top |
| `bootc status` fails with stale entry | OSTree BLS entry referenced by bootc state | Clean /run/composefs/staged-deployment |
| `dbus.service` 217/USER | Missing `dbus` user in merged passwd | supplement_identity_dbs_from_registry |
| `system.conf` not well-formed | EROFS zero-fills past inline threshold | Registry streaming for /etc extraction |
| Zero-byte initrd on ESP | VFAT writeback cache unsynced | `unsafe { libc::sync() }` |
