# OSTree → ComposeFS Migration Tool — just tasks
# https://github.com/hanthor/ostree-composefs-rebase

default:
    @just --list

# === Build & Test ===

# Build (set PROFILE_FLAG=--release for CI)
build:
    cargo build {{env_var_or_default('PROFILE_FLAG', '')}}

# Build in release profile (convenience alias)
build-release:
    cargo build --release

# Run all unit tests (set PROFILE_FLAG=--release for CI)
test:
    cargo test {{env_var_or_default('PROFILE_FLAG', '')}}

# Run tests with output
test-verbose:
    cargo test {{env_var_or_default('PROFILE_FLAG', '')}} -- --nocapture

# Run a single test (usage: just test-one test_name)
test-one test_name:
    cargo test {{test_name}} -- --nocapture

# Check compilation without producing binary (fast)
check:
    cargo check

# === E2E Tests ===

# Run full E2E migration test.
# All parameters are env-var driven; set PROFILE_FLAG=--release for CI.
# Defaults: Bluefin stable → Dakota stable (btrfs, 20G disk).
e2e: build test
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="{{env_var_or_default('BASE_IMAGE', 'ghcr.io/projectbluefin/bluefin:stable')}}" \
      TARGET_IMAGE="{{env_var_or_default('TARGET_IMAGE', 'ghcr.io/projectbluefin/dakota:stable')}}" \
      DISK_SIZE="{{env_var_or_default('DISK_SIZE', '20G')}}" \
      FILESYSTEM="{{env_var_or_default('FILESYSTEM', 'btrfs')}}" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-run.log

# Bluefin LTS → Dakota (xfs + loopback workaround).
e2e-lts: build test
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="ghcr.io/projectbluefin/bluefin:lts" \
      TARGET_IMAGE="ghcr.io/projectbluefin/dakota:stable" \
      DISK_SIZE="20G" \
      FILESYSTEM="xfs" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-lts.log

# Run E2E with composefs boot log_level=debug
e2e-debug: build
    @echo "=== Running E2E with composefs systemd debug logging ==="
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="{{env_var_or_default('BASE_IMAGE', 'ghcr.io/projectbluefin/bluefin:stable')}}" \
      TARGET_IMAGE="{{env_var_or_default('TARGET_IMAGE', 'ghcr.io/projectbluefin/dakota:stable')}}" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-run.log; rc=$$?; \
      echo "=== Quick diagnostics ==="; \
      echo "--- dbus/messagebus/polkit/logind ---"; \
      grep -iE 'dbus|messagebus|polkit|logind|machine.id' e2e-run.log | tail -30; \
      echo "--- all FAILED ---"; \
      grep -E 'FAILED|DEPEND' e2e-run.log | tail -30; \
      exit $$rc

# === Linting ===

# Run all linters (shellcheck, rustfmt, clippy)
lint: lint-shell lint-rust

# Lint shell scripts with shellcheck (warnings + errors only, skip info/style)
lint-shell:
    @echo "=== shellcheck ==="
    shellcheck --severity=warning tests/run-e2e.sh

# Lint Rust code (format + clippy)
lint-rust:
    @echo "=== cargo fmt --check ==="
    cargo fmt --check
    @echo "=== cargo clippy ==="
    cargo clippy -- -D warnings

# === Diagnostics ===

# View the last E2E log
e2e-log:
    @less e2e-run.log

# Grep E2E log for failures
e2e-failures:
    @grep --color=always -E 'FAILED|error|Error|WARNING|panic|blocker' e2e-run.log || echo "No failures found"

# Grep E2E log for dbus-related messages
e2e-dbus:
    @grep --color=always -i 'dbus\|messagebus\|polkit\|logind\|machine.id' e2e-run.log || echo "No dbus messages"

# Grep full QEMU log for dbus failures (unfiltered serial console)
e2e-qemu-dbus:
    @grep --color=always -iE 'dbus|messagebus|polkit|logind|machine.id|FAILED' qemu.log | tail -60 || echo "No qemu.log"

# Grep E2E log for composefs-boot messages (kernel cmdline, mounts, etc.)
e2e-composefs:
    @grep --color=always -E 'composefs=|overlay|erofs|subvol|fstab|BOOT_IMAGE|Kernel command line' e2e-run.log || echo "No composefs messages"

# === Registry (for fast E2E pulls) ===

# Start local OCI registry for E2E
registry-start:
    sudo podman run -d --name e2e-registry --network=host docker.io/library/registry:2

# Cache images in local registry
registry-cache:
    sudo podman tag ghcr.io/projectbluefin/bluefin:stable 127.0.0.1:5000/bluefin:stable
    sudo podman tag ghcr.io/projectbluefin/dakota:stable  127.0.0.1:5000/dakota:stable
    sudo podman push --tls-verify=false 127.0.0.1:5000/bluefin:stable
    sudo podman push --tls-verify=false 127.0.0.1:5000/dakota:stable

# === Cleanup ===

# Kill stale QEMU processes and free disk space
cleanup:
    @echo "Killing stale QEMU processes..."
    -sudo kill $$(pgrep -f 'qemu-system.*disk.raw') 2>/dev/null
    @echo "Freeing podman storage..."
    -sudo podman system prune -af
    @echo "Removing E2E artifacts..."
    -rm -f disk.raw disk.raw.pre-migration qemu.log test_key test_key.pub e2e-run.log
    @echo "Done."

# Clean build artifacts
clean-build:
    cargo clean

# === Git ===

# Commit with conventional commit format
commit msg:
    git add -A && git commit -m "{{msg}}" && git push
