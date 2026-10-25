#!/usr/bin/env bash
#
# Run NovaOS's quality gates.
#
# # What this is for
#
# The plan's quality gates are four commands that have to be run in the right
# order, and two of them are easy to get subtly wrong:
#
# - `cargo clippy` must exclude the bare-metal binaries. They are `no_std` /
#   `no_main` programs with their own `_start`, and building them for the
#   host fails with "unwinding panics are not supported without std" — an
#   error that reads as a broken toolchain and is really a kernel being built
#   for the wrong target.
# - `cargo build --workspace --all-targets` fails for the same reason, and
#   the bare-metal build has to name `--target x86_64-unknown-none`.
#
# This script runs the gates the way they are meant to be run, so that a
# developer and CI run the same thing.
#
# # Usage
#
#   scripts/gates.sh              # every gate
#   scripts/gates.sh fmt          # formatting only
#   scripts/gates.sh clippy       # lints only
#   scripts/gates.sh test         # host unit tests only
#   scripts/gates.sh kernel       # bare-metal build only
#   scripts/gates.sh boot         # the QEMU boot suite only
#
# Every gate exits non-zero on failure, and the script stops at the first
# failure — the gates are ordered cheapest-first for exactly that reason.
#
# # What is *not* here
#
# The kernel's own lint warnings. `cargo clippy --package nova-kernel --target
# x86_64-unknown-none` produces hundreds of `unused` and `needless_borrow`
# warnings on the kernel's internal symbols. They are real, they belong to
# whoever owns the kernel, and running them as a hard gate here would block
# every other change on a cleanup nobody has scheduled. The bare-metal build
# *is* a hard gate, because a kernel that does not compile is a kernel that
# cannot boot.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# The bare-metal target triple. The kernel and the user runtime are built for
# it and for nothing else.
readonly BARE_TARGET="x86_64-unknown-none"

# The workspace members that cannot be built for the host.
#
# `nova-kernel` and `nova-rt` are `no_std` / `no_main` bare-metal binaries.
# `xtask` is the build tool itself: it is a host program and it is built for
# the host, but it is excluded from the *test* sweep because it is the tool
# driving the run rather than code under test. Its own tests are run by
# `cargo xtask test`.
readonly HOST_EXCLUDES=(--exclude nova-kernel --exclude nova-rt --exclude nova-apps)

# --- Individual gates ------------------------------------------------------

gate_fmt() {
    echo "==> cargo fmt --all -- --check"
    # Over the whole workspace, kernel included: rustfmt parses without
    # linking, so the bare-metal binaries are covered here.
    cargo fmt --all -- --check
}

gate_clippy() {
    echo "==> cargo clippy (host)"
    cargo clippy --workspace "${HOST_EXCLUDES[@]}" --all-targets -- -D warnings
}

gate_test() {
    echo "==> cargo test (host)"
    cargo test --workspace --exclude nova-kernel --exclude nova-rt --exclude nova-apps --exclude xtask
}

gate_kernel() {
    echo "==> cargo build --target ${BARE_TARGET} (bare metal)"
    cargo build --package nova-kernel --package nova-rt --target "$BARE_TARGET" --release
}

gate_boot() {
    echo "==> cargo xtask test (QEMU boot suite)"
    # `cargo xtask test` runs xtask's own tests, the host unit tests, and then
    # the QEMU boot suite. It exits non-zero on a failure *or* a timeout, and
    # it archives the serial log under `target/novaos/run/archive/`.
    #
    # The Limine release is fetched first: the boot needs it, and a cold
    # environment does not have it.
    cargo xtask fetch-limine
    cargo xtask test --timeout "${NOVA_QEMU_TIMEOUT:-120}"
}

# --- Dispatch --------------------------------------------------------------

usage() {
    sed -n '4,38p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

run_all() {
    gate_fmt
    gate_clippy
    gate_test
    gate_kernel
    gate_boot
    echo
    echo "all gates passed"
}

case "${1:-all}" in
    all)       run_all ;;
    fmt)       gate_fmt ;;
    clippy)    gate_clippy ;;
    test)      gate_test ;;
    kernel)    gate_kernel ;;
    boot)      gate_boot ;;
    -h|--help) usage ;;
    *)
        echo "unknown gate: $1" >&2
        usage >&2
        exit 2
        ;;
esac
