# NovaOS build and run entry points.
#
# The project uses POSIX `make` conventions: `make run` builds the image and
# starts it in QEMU's graphical window. The automated CI path remains
# `cargo xtask test`, which is headless and imposes its own timeout.

CARGO = cargo
TARGET_DIR = target
ISO = $(TARGET_DIR)/novaos/nova.iso

.PHONY: all build image run run-headless test fmt clippy check clean

all: build

# Build the kernel and the bootable ISO, including the two M5 test disks.
build:
	$(CARGO) xtask image

# Alias kept for people who think in terms of the image rather than the build.
image: build

# Build and boot with QEMU GUI plus a serial console in the terminal.
run:
	sh scripts/qemu.sh

# Build and boot without a graphical window. Useful on servers and in SSH.
run-headless:
	sh scripts/qemu.sh --headless

# The full gate: host tests, tool tests, and the QEMU boot suite.
test:
	$(CARGO) xtask test

# Formatting gate.
fmt:
	$(CARGO) nova-fmt

# Lint gate.
clippy:
	$(CARGO) nova-clippy

# Host tests only; the QEMU suite is deliberately a separate run.
check:
	$(CARGO) nova-test-host

# Remove build outputs.
clean:
	$(CARGO) clean
	rm -rf $(TARGET_DIR)/novaos
