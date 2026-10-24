//! Host-side tests for the user window's derivation and the canonical rule.
//!
//! # Why these are host tests
//!
//! Both of the rules below are pure arithmetic over the address-space
//! geometry, and both are the kind of rule whose failure is invisible: a
//! window that is too wide accepts mappings it should refuse, and a canonical
//! check that is too loose accepts addresses the hardware raises `#GP` for.
//! Neither shows up until something is mapped at the wrong address, which on
//! bare metal means a corrupted page table rather than a failed assertion.
//!
//! So the numbers are pinned here, on the host, against the same code the
//! kernel runs.

use crate::address_space::{AddressSpace, Protection, UserWindow, VmmError};
use crate::paging::{PAGE_SIZE, is_canonical, is_canonical_in, split_address};

/// The kernel half's start on a 48-bit address space.
const KERNEL_HALF_START_48: u64 = 0xFFFF_8000_0000_0000;

/// The window a 48-bit space derives.
fn window_48() -> UserWindow {
    UserWindow::derive(48, KERNEL_HALF_START_48)
}

#[test]
fn the_window_bounds_are_the_derived_ones() {
    let window = window_48();

    assert_eq!(
        window.start, 0x1000,
        "the window starts one page above the null page",
    );
    assert_eq!(
        window.end, 0x0000_7FFF_FFFF_F000,
        "the window ends one page below the user half's top",
    );
}

#[test]
fn the_window_end_is_canonical() {
    // The bound itself must be an address the hardware can address: a window
    // whose upper bound is non-canonical is a bound that excludes nothing,
    // because it is larger than every address that exists.
    assert!(
        is_canonical(window_48().end),
        "the window's upper bound must itself be canonical",
    );
    assert!(
        is_canonical(window_48().end - 1),
        "the last byte the window admits must be canonical",
    );
}

#[test]
fn the_window_end_is_below_the_kernel_half() {
    let window = window_48();

    assert!(
        window.end < KERNEL_HALF_START_48,
        "the window must end below the kernel half",
    );
    assert!(
        window.end <= 0x0000_7FFF_FFFF_FFFF,
        "the window must end below the user half's top",
    );
}

#[test]
fn the_window_excludes_every_address_that_walks_into_pml4_entry_256() {
    // PML4 entry 256 is the kernel's own entry. Any address whose walk
    // reaches it is in the kernel half, whether or not it looks like one:
    // `0x0000_8000_0000_0000` and `0xFFFF_8000_0000_0000` have the *same*
    // walk, so a user mapping at the former would land in the kernel's
    // tables.
    let window = window_48();

    for address in [0x0000_8000_0000_0000u64, 0xFFFF_8000_0000_0000] {
        assert_eq!(
            split_address(address).p4,
            256,
            "this address is supposed to walk into the kernel's PML4 entry",
        );
        assert!(
            !window.contains(address),
            "the window must exclude an address in the kernel's half",
        );
    }
}

#[test]
fn the_window_excludes_every_address_above_the_user_half() {
    // The window's upper bound is one page below `2^(virtual_bits - 1)`, and
    // the top of the *user* half is PML4 entry 255. Every address that walks
    // into entry 256 or above is therefore outside the window — which is the
    // property that keeps a user mapping out of the kernel's tables.
    let window = window_48();

    for address in [
        0x0000_8000_0000_0000u64,
        0xFFFF_8000_0000_0000,
        0xFFFF_FFFF_FFFF_F000,
        0xFFFF_FFFF_FFFF_FFFF,
    ] {
        assert!(
            split_address(address).p4 >= 256,
            "this address is supposed to walk into the kernel's half",
        );
        assert!(
            !window.contains(address),
            "the window must exclude an address in the kernel's half",
        );
    }
}

#[test]
fn the_window_rejects_the_first_address_above_the_user_half() {
    let window = window_48();

    assert!(
        !window.contains(0x0000_8000_0000_0000),
        "the window must reject the first address above the user half",
    );
    assert!(
        window.contains(0x0000_7FFF_FFFF_F000 - 1),
        "the window must admit the last user page",
    );
}

#[test]
fn the_window_rejects_the_null_page() {
    let window = window_48();

    assert!(!window.contains(0), "the null page is never mappable");
    assert!(
        window.contains(PAGE_SIZE),
        "the page after the null page is"
    );
}

#[test]
fn a_user_mapping_above_the_user_half_is_refused() {
    let window = window_48();
    let space = AddressSpace::new(window);

    assert_eq!(
        space.check_map(0x0000_8000_0000_0000, 0x1000, Protection::UserReadWrite),
        Err(VmmError::NonCanonical),
        "a mapping above the user half is not a canonical address",
    );
    assert_eq!(
        space.check_map(0xFFFF_8000_0000_0000, 0x1000, Protection::UserReadWrite,),
        Err(VmmError::OutsideUserRange),
        "a canonical kernel-half address is outside the user window",
    );
}

#[test]
fn the_canonical_rule_replicates_bit_47() {
    // The four corners of the two canonical ranges.
    for address in [
        0x0000_0000_0000_0000u64,
        0x0000_7FFF_FFFF_FFFF,
        0xFFFF_8000_0000_0000,
        0xFFFF_FFFF_FFFF_FFFF,
    ] {
        assert!(
            is_canonical(address),
            "a corner of a canonical range must be canonical: {address:#018x}",
        );
    }

    // The two holes: an all-zero sign with bit 47 set, and an all-one sign
    // with bit 47 clear.
    for address in [
        0x0000_8000_0000_0000u64,
        0xFFFF_7FFF_FFFF_FFFF,
        0x0001_8000_0000_0000,
        0xFFFE_7FFF_FFFF_FFFF,
    ] {
        assert!(
            !is_canonical(address),
            "an address whose sign disagrees with bit 47 is not canonical: {address:#018x}",
        );
    }
}

#[test]
fn the_canonical_rule_matches_the_hardware_across_the_boundaries() {
    // Every boundary address, checked against the rule stated directly:
    // bits 63:48 must replicate bit 47.
    let cases: [(u64, bool); 12] = [
        (0x0000_0000_0000_0000, true),
        (0x0000_0000_0000_1000, true),
        (0x0000_7FFF_FFFF_F000, true),
        (0x0000_7FFF_FFFF_FFFF, true),
        (0x0000_8000_0000_0000, false),
        (0x0000_FFFF_FFFF_FFFF, false),
        (0x0001_8000_0000_0000, false),
        (0x7FFF_FFFF_FFFF_FFFF, false),
        (0xFFFF_0000_0000_0000, false),
        (0xFFFF_7FFF_FFFF_FFFF, false),
        (0xFFFF_8000_0000_0000, true),
        (0xFFFF_FFFF_FFFF_FFFF, true),
    ];

    for (address, expected) in cases {
        assert_eq!(
            is_canonical(address),
            expected,
            "unexpected canonicality for {address:#018x}",
        );

        // The same answer must come from the general form, which is the one
        // a different address width would use.
        assert_eq!(
            is_canonical_in(48, address),
            expected,
            "the general form disagrees for {address:#018x}",
        );
    }
}

#[test]
fn a_narrower_address_space_moves_the_boundary() {
    // A 57-bit LA57 space: the user half is twice as large, and the kernel
    // half starts correspondingly lower.
    let window = UserWindow::derive(57, 0xFF00_0000_0000_0000);

    assert_eq!(window.start, 0x1000);
    assert_eq!(window.end, 0x00FF_FFFF_FFFF_F000);
    assert!(is_canonical_in(57, window.end));
    assert!(
        !window.contains(0x0100_0000_0000_0000),
        "the LA57 window must exclude an address above the 57-bit user half",
    );
    assert!(
        window.contains(0x0000_8000_0000_0000),
        "the LA57 window covers what the 48-bit one could not",
    );
}

#[test]
fn a_disagreeing_kernel_half_is_refused() {
    // The two halves of the geometry must agree: a kernel half that does not
    // match the address width means the caller derived one of them wrongly.
    // Refusing is the point — a silent disagreement is how a window ends up
    // bounded by the wrong address.
    assert!(
        UserWindow::try_derive(48, 0xFF00_0000_0000_0000).is_none(),
        "a kernel half that disagrees with the address width must be refused",
    );
    assert!(
        UserWindow::try_derive(48, 0).is_none(),
        "a missing kernel half must be refused",
    );
    assert!(
        UserWindow::try_derive(64, 0).is_none(),
        "a 64-bit address space has no canonical split and no user half",
    );
    assert!(
        UserWindow::try_derive(48, KERNEL_HALF_START_48).is_some(),
        "the real geometry must be accepted",
    );
}
