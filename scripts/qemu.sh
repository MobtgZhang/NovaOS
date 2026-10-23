#!/usr/bin/env bash
#
# Boot the NovaOS image in QEMU with a serial console you can type into.
#
# # Why this script exists when `cargo xtask run` already boots the image
#
# `cargo xtask run` is for CI: it is non-interactive, it imposes a timeout, and
# it decides pass or fail. This script is for the two things that cannot be done
# through that interface:
#
#   1. **Looking at a hang.** `cargo xtask run` kills the VM after N seconds and
#      prints the last 40 lines. That is often not enough to see where the kernel
#      stopped, and there is no way to single-step from there.
#   2. **Changing a register.** A fault handler that prints the wrong register
#      cannot be inspected after the fact.
#
# Everything that must agree with the automated path is asserted from this
# script rather than duplicated: see the DEBUG_EXIT_IOBASE check below.
#
# # Usage
#
#   scripts/qemu.sh                 # boot the release image, interactive serial
#   scripts/qemu.sh --keep-image    # leave nova.iso on disk
#   scripts/qemu.sh --no-build      # boot whatever image is already there
#   scripts/qemu.sh --timeout 120   # for a slow machine (default: none)
#
# Interactive mode needs `-nographic`, which cannot be combined with `-serial
# file:`. That is the whole difference between this script and `xtask run`.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
readonly ISO="$TARGET_DIR/novaos/nova.iso"
readonly SERIAL_LOG="$TARGET_DIR/novaos/serial-interactive.log"

# ---------------------------------------------------------------------------
# Constants that must match the rest of the tree
# ---------------------------------------------------------------------------
#
# These are duplicated from `tools/xtask/src/qemu.rs` and
# `kernel/src/arch/x86_64/exit.rs`. Duplication across a shell script and two
# Rust files is normally a mistake; here it is unavoidable, so the script
# *asserts* against the Rust sources rather than trusting its own copy. A change
# to either that is not made here fails loudly at startup instead of hanging a
# VM whose tests all passed.
readonly DEBUG_EXIT_IOBASE="0xf4"
readonly EXIT_PORT_EXPECTATION='const DEBUG_EXIT_PORT: u16 = 0xF4;'
readonly MEMORY_MIB=256

BUILD=true
KEEP_IMAGE=false
TIMEOUT=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build)   BUILD=false ;;
        --keep-image) KEEP_IMAGE=true ;;
        --timeout)    TIMEOUT="$2"; shift ;;
        -h|--help)    sed -n '4,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)            echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

# Fail before spending a build on a mismatch that would hang the VM.
#
# `isa-debug-exit` listens on whatever `iobase=` says — it has no fixed port. If
# this script's copy and the kernel's disagree, the kernel finishes its tests,
# writes to a port nothing is listening on, and QEMU keeps running. The
# symptom is indistinguishable from a hung kernel, which is why this is checked
# rather than documented.
if ! grep -q "$EXIT_PORT_EXPECTATION" \
        "$REPO_ROOT/kernel/src/arch/x86_64/exit.rs"; then
    cat >&2 <<EOF
error: scripts/qemu.sh writes the exit port $DEBUG_EXIT_IOBASE, but
       kernel/src/arch/x86_64/exit.rs no longer contains:

           $EXIT_PORT_EXPECTATION

QEMU would never see the kernel's exit status, and a passing test suite would
look like a hang. Update all three together:
  - kernel/src/arch/x86_64/exit.rs   (DEBUG_EXIT_PORT)
  - tools/xtask/src/qemu.rs          (DEBUG_EXIT_IOBASE)
  - scripts/qemu.sh                  (DEBUG_EXIT_IOBASE, EXIT_PORT_EXPECTATION)
EOF
    exit 1
fi

if [[ "$BUILD" == true ]]; then
    # `cargo xtask image` builds the kernel and packages the ISO. Using it rather
    # than `cargo build` here means this script and the automated path produce
    # byte-identical images, so "it works interactively" implies "it works in CI".
    ( cd "$REPO_ROOT" && cargo xtask image )
fi

if [[ ! -f "$ISO" ]]; then
    echo "error: $ISO not found; run 'cargo xtask image' first" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Firmware
# ---------------------------------------------------------------------------
#
# Two pflash drives, not `-bios`: OVMF's variable store has to be writable, and
# pflash is how it is made so. The second drive is a *copy* of a template —
# pointing it at the system's template directly would let one run's NVRAM writes
# corrupt every later run, including other projects on the machine.

find_ovmf() {
    local kind="$1"; shift
    local name
    for dir in /usr/share/OVMF /usr/share/ovmf /usr/share/edk2/ovmf /usr/local/share/ovmf; do
        for name in "$@"; do
            if [[ -f "$dir/$name" ]]; then
                printf '%s\n' "$dir/$name"
                return 0
            fi
        done
    done
    echo "error: OVMF $kind image not found. Install an edk2/ovmf package." >&2
    echo "       tried: $*" >&2
    return 1
}

OVMF_CODE="$(find_ovmf code OVMF_CODE_4M.fd OVMF_CODE.fd OVMF.fd)"
OVMF_VARS_TEMPLATE="$(find_ovmf variables OVMF_VARS_4M.fd OVMF_VARS.fd OVMF_VARS.ms.fd)"

# A fresh variable store per run, in a directory that is ours rather than one
# shared with the system's OVMF installation.
readonly VARS_COPY="$TARGET_DIR/novaos/OVMF_VARS-interactive.fd"
mkdir -p "$(dirname "$VARS_COPY")"
cp "$OVMF_VARS_TEMPLATE" "$VARS_COPY"

# ---------------------------------------------------------------------------
# Launch
# ---------------------------------------------------------------------------

# The exit device exists even in interactive mode so that a kernel which reaches
# its tests still shuts the VM down. Without it, an interactive run would need a
# manual power-off every time, and the most common interactive run *is* a
# successful one.
#
# `-no-reboot` matters here more than in CI: a triple fault with reboot enabled
# puts the machine in a reset loop that looks like a hang and can only be
# escaped from outside the window.
readonly -a QEMU_ARGS=(
    -machine q35,accel=tcg
    -cpu qemu64
    -smp 1
    -m "${MEMORY_MIB}M"
    -drive "if=pflash,format=raw,unit=0,readonly=on,file=$OVMF_CODE"
    -drive "if=pflash,format=raw,unit=1,file=$VARS_COPY"
    -drive "file=$ISO,format=raw,media=cdrom"
    -device "isa-debug-exit,iobase=$DEBUG_EXIT_IOBASE,iosize=0x04"
    -no-reboot
)

echo "image:    $ISO"
echo "firmware: $OVMF_CODE"
echo "vars:     $VARS_COPY  (a copy; the system template is never written)"
echo

if [[ -n "$TIMEOUT" ]]; then
    # `-serial stdio` is deliberately absent here: with a timeout there is nobody
    # to type, so the log is more useful than an interactive prompt that the
    # timeout will kill anyway.
    echo "serial:   $SERIAL_LOG (timeout ${TIMEOUT}s)"
    timeout --foreground "${TIMEOUT}s" \
        qemu-system-x86_64 "${QEMU_ARGS[@]}" \
            -display none \
            -serial "file:$SERIAL_LOG"
    status=$?
    echo
    if [[ $status -eq 124 ]]; then
        echo "timed out after ${TIMEOUT}s. This is a hang, not a test failure:"
        echo "the kernel most likely never reached isa-debug-exit."
    else
        # `isa-debug-exit` maps a guest status of 0 to 1, and any failure to 3.
        echo "QEMU exited with $status; the log is in $SERIAL_LOG"
    fi
else
    echo "serial:   interactive (Ctrl-A X to quit)"
    echo
    qemu-system-x86_64 "${QEMU_ARGS[@]}" \
        -nographic
fi

if [[ "$KEEP_IMAGE" == false && "$BUILD" == true ]]; then
    # Only remove what this script built. If the caller passed --no-build, the ISO
    # was theirs and deleting it would be destroying something they asked to keep.
    rm -f "$ISO"
fi