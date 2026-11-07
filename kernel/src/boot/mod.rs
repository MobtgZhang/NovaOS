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
//! - [`novaboot`] — the NovaBoot protocol reader that fills [`info::BootInfo`]
//!   from the snapshot the UEFI boot manager (`nova-boot-menu`) published. The
//!   protocol itself is architecture-neutral (`crates/nova-boot`), so this
//!   module is shared rather than per-architecture; only the entry stub and the
//!   linker script that places it differ per target.

pub mod info;
pub mod novaboot;
pub mod smp;

pub use info::{BootInfo, MAX_CPUS, MAX_PHYSICAL_ADDRESS, PAGE_SIZE};

pub use novaboot::{
    boot_info_is_ready, capture, check_protocol_revision, initramfs, read_boot_info,
};

/// The bootloader name as a NUL-trimmed string, or `"unknown"`.
pub fn bootloader_name(info: &BootInfo) -> &str {
    info.bootloader_name()
}

/// The bootloader version as a NUL-trimmed string, or `"unknown"`.
pub fn bootloader_version(info: &BootInfo) -> &str {
    info.bootloader_version()
}
