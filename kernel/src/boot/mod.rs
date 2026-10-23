//! Boot-time protocol handling.
//!
//! The bootloader is a *source of untrusted facts*, not a service the kernel can
//! rely on. This module's job is to validate everything the firmware says and
//! copy it into kernel-owned storage before the firmware is free to reclaim its
//! own pages.
//!
//! # Layout
//!
//! - [`info`] — `BootInfo`, `MemoryRegion`, validation. Fully architecture-neutral:
//!   it is a struct with bounds checks, no pointers, no asm, no hardware access.
//! - [`limine`] — the Limine protocol requests and the adapter that fills
//!   [`info::BootInfo`]. Limine supports both x86-64 and AArch64 behind one boot
//!   protocol, so this module is shared rather than per-architecture; only the
//!   `.boot.rodata` section name and the linker script that places it differ, and
//!   both are set per target in the build configuration.

pub mod info;
pub mod limine;

pub use info::{BootInfo, MAX_PHYSICAL_ADDRESS, PAGE_SIZE};

pub use limine::{boot_info_is_ready, capture, check_protocol_revision, read_boot_info};

/// The bootloader name as a NUL-trimmed string, or `"unknown"`.
pub fn bootloader_name(info: &BootInfo) -> &str {
    info.bootloader_name()
}

/// The bootloader version as a NUL-trimmed string, or `"unknown"`.
pub fn bootloader_version(info: &BootInfo) -> &str {
    info.bootloader_version()
}
