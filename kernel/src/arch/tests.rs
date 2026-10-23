//! Host-side tests for the architecture facade.
//!
//! These compile for the host, not for `x86_64-unknown-none`, so they exercise
//! the *derivation* logic in `arch::shared::hw` — the part that is arithmetic
//! rather than hardware access, and therefore the part most likely to be wrong
//! in a way that only shows up as a mysterious fault at boot.

use super::ADDRESS_SPACE;
use crate::arch::shared::hw::AddressSpace;

#[test]
fn the_compiled_address_space_is_self_consistent() {
    // Every derived value in `AddressSpace` is computed from `page_size` in
    // `new`, so a mismatch here means the derivation is wrong for every caller
    // that trusted `page_shift`.
    assert_eq!(ADDRESS_SPACE.page_shift, ADDRESS_SPACE.page_size.trailing_zeros());
    assert_eq!(1u64 << ADDRESS_SPACE.page_shift, ADDRESS_SPACE.page_size);
    assert_eq!(ADDRESS_SPACE.page_mask, ADDRESS_SPACE.page_size - 1);
}

#[test]
fn x86_64_reports_the_four_level_geometry() {
    // These are architectural facts, not choices: 4 KiB pages, 52 physical
    // bits, 48 virtual bits with LA57 off. A change here would mean the kernel
    // is running on a paging mode it does not implement.
    assert_eq!(ADDRESS_SPACE.page_size, 0x1000);
    assert_eq!(ADDRESS_SPACE.physical_bits, Some(52));
    assert_eq!(ADDRESS_SPACE.virtual_bits, Some(48));
}

#[test]
fn the_canonical_half_boundary_is_derived_not_hardcoded() {
    assert_eq!(
        ADDRESS_SPACE.kernel_half_start(),
        0xFFFF_8000_0000_0000,
        "48-bit virtual addresses split at bit 47",
    );
}

#[test]
fn a_57_bit_space_moves_the_boundary() {
    // LA57 shifts the boundary up; the formula must follow, or a future 5-level
    // target would compute a kernel-half start below its own image.
    let space = AddressSpace::new(0x1000, Some(52), Some(57)).expect("4 KiB pages are a power of two");
    assert_eq!(space.kernel_half_start(), 0xFF00_0000_0000_0000);
}

#[test]
fn a_non_power_of_two_page_size_is_rejected() {
    // Catching it here means the error surfaces at boot rather than as a silent
    // misalignment in `align_up` much later.
    assert!(AddressSpace::new(0x1001, Some(52), Some(48)).is_none());
    assert!(AddressSpace::new(0, Some(52), Some(48)).is_none());
}

#[test]
fn alignment_rounds_up_without_wrapping() {
    let space = ADDRESS_SPACE;
    assert_eq!(space.align_up(0x1000), 0x1000);
    assert_eq!(space.align_up(0x1001), 0x2000);
    assert_eq!(space.align_down(0x1FFF), 0x1000);

    // A wrapped `align_up` would return a length *smaller* than the input, which
    // is the direction that hides a bug instead of exposing it.
    assert_eq!(space.align_up(u64::MAX), u64::MAX);
}

#[test]
fn alignment_recognises_page_boundaries() {
    assert!(ADDRESS_SPACE.is_page_aligned(0x2000));
    assert!(!ADDRESS_SPACE.is_page_aligned(0x2001));
}

#[test]
fn the_physical_limit_follows_the_declared_width() {
    assert_eq!(ADDRESS_SPACE.max_physical_address(), (1u64 << 52) - 1);

    // An unbounded width reports "no limit" rather than panicking, so the caller
    // decides how to treat it.
    let unbounded = AddressSpace::new(0x1000, None, Some(48)).expect("valid page size");
    assert_eq!(unbounded.max_physical_address(), u64::MAX);
}