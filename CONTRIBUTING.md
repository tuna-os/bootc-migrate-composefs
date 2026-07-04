# Contributing

Thanks for your interest in improving `bootc-migrate-composefs`.

> **Note:** this tool performs an in-place, hard-to-reverse migration of a real
> system. Treat changes to the migration phases (`src/migration/`) with extra
> care and exercise them through the end-to-end suite before merging.

## Development setup

You need a recent stable Rust toolchain (the crate targets edition 2024,
`rust-version = 1.88.0`) and [`just`](https://github.com/casey/just).

```console
$ cargo build
$ just check        # clippy + rustfmt + unit tests + shellcheck — run before every PR
```

### IDE setup

Standard `rust-analyzer` works out of the box. The crate uses `clippy` with
several extra lints enabled (see `Cargo.toml`); run `just check` rather than
`cargo clippy` alone to get the full lint set.

---

## Running the end-to-end tests

The E2E harness boots a real QEMU VM, runs the full migration, reboots, and
validates the result. It needs additional host packages:

```bash
# Fedora/RHEL
sudo dnf install qemu-system-x86_64 edk2-ovmf podman cryptsetup lvm2 swtpm

# Ubuntu/Debian
sudo apt install qemu-system-x86 ovmf podman cryptsetup-bin lvm2 swtpm
```

You also need root (for loop mounts and pflash) and outbound registry access
(`ghcr.io`) to pull the Bluefin/Dakota images. On a fresh machine, seed the
local registry cache first (saves ~8 GB of re-pulls on every run):

```console
$ just registry-start   # start a local OCI registry on localhost:5000
$ just registry-cache   # pull Bluefin + Dakota; push to local registry
```

### E2E scenarios

| Recipe | What it tests | Disk | Notes |
|--------|--------------|------|-------|
| `just e2e` | Bluefin stable → Dakota (btrfs, x86_64) | 20 GB | Default; fastest |
| `just e2e-lts` | Bluefin LTS → Dakota (XFS + ext4 loopback) | 20 GB | LTS base |
| `just e2e-luks` | Bluefin LTS → Dakota (XFS + LUKS + swtpm) | 40 GB | Encrypted root |
| `just e2e-lvm` | Bluefin LTS → Dakota (LVM-on-LUKS, separate `/var`) | 40 GB | Most complex |

Run the default scenario:

```console
$ just e2e
```

Watch progress in another terminal:

```console
$ just watch          # tails the latest .log; exits on errors or idle timeout
```

Or ssh into the running VM to poke around:

```console
$ just e2e-ssh        # opens an interactive SSH session to port 2222
```

### Debugging a failed E2E run

```console
$ just e2e-failures   # grep log for failures/errors
$ just e2e-composefs  # grep for composefs-related boot messages
$ just e2e-tail       # tail the QEMU serial console (high-signal lines only)
$ just e2e-status     # show disk.raw status + QEMU/SSH availability
```

To reproduce a failure starting from after the migration (skipping setup):

```console
$ SKIP_SETUP=1 just e2e-reboot-test
```

### Cleaning up after E2E

```console
$ just cleanup        # kill QEMU, prune podman, remove disk.raw and .log files
```

---

## Adding a new E2E scenario

1. Add a new recipe in `justfile` modelled on `e2e-luks` or `e2e-lvm`.
2. Add the scenario to the CI matrix in `.github/workflows/e2e-tests.yml`
   (follow the existing `include:` pattern, set `name`, `filesystem`, `disk-size`, and any env overrides).
3. Update the CI matrix table in [AGENTS.md](AGENTS.md).
4. Document the scenario in [docs/filesystem-support.md](docs/filesystem-support.md).

---

## Before you open a PR

- `just check` passes (clippy + rustfmt + unit tests + shellcheck — this is
  what CI's `validate` job runs).
- `cargo deny check` passes if you touched dependencies.
- Commits follow the `component: Summary` convention described in
  [REVIEW.md](REVIEW.md); fixups are squashed before merge.
- New non-trivial logic has unit tests (prefer table-driven, per REVIEW.md),
  and migration-path changes are exercised by at least the default E2E scenario.
- If your change affects the kernel command line, boot artifacts, or any phase
  output, run the full E2E matrix locally or wait for CI to do it on your PR.

## Code review

Please read [REVIEW.md](REVIEW.md) — it describes the testing, code-quality, and
commit-message expectations applied here. AI-assisted contributions must follow
[AGENTS.md](AGENTS.md) (no automatic `Signed-off-by`; add an `Assisted-by:`
trailer).

---

## Dependency update policy

Dependency updates come through [Renovate](https://docs.renovatebot.com/) (see
`renovate.json`). Patch-level updates are auto-merged if CI is green. Minor and
major updates get a PR for human review. When reviewing Renovate PRs:

- Check the changelog / release notes for breaking changes.
- Verify `cargo deny check` still passes.
- Run `just check` locally if the changed crate is a key dependency (`rustix`,
  `clap`, `anyhow`, `serde_json`).

---

## License

By contributing, you agree that your contributions are dual-licensed under the
[MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licenses.
