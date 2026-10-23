//! In-kernel test harness for M0 boot verification.
//!
//! Each suite prints a machine-greppable line so `scripts/qemu.sh` can assert on
//! the serial output without parsing free-form prose:
//!
//! ```text
//! [NOVA-TEST] SUITE boot_smoke
//! [NOVA-TEST]   case <name> ... ok|fail
//! [NOVA-TEST] RESULT suites=1 cases=12 passed=12 failed=0
//! ```
//!
//! A failing case is reported but does **not** halt the kernel: the remaining
//! suites still run so one CI log shows every regression at once. Only after
//! all suites finish does the harness exit with [`crate::EXIT_TEST_FAILURE`].
//!
//! # Suite names are the milestone contract
//!
//! Each suite is named after an acceptance criterion from the milestone plan
//! (`boot_smoke`, `memory_map_sanity`, `divide_by_zero_fault`,
//! `invalid_opcode_fault`, `serial_log_format`), and `scripts/qemu.sh` asserts on
//! exactly those strings. A renamed suite silently drops a gate, so the names
//! live here as a list the shell script and the kernel both check.

pub mod harness;

/// Every suite name the kernel runs, in order.
///
/// Duplicated deliberately rather than derived: this is the list the milestone's
/// exit gate is written against, so it has to be checkable as a literal. A host
/// test below asserts this matches the harness's registration list.
pub const SUITE_NAMES: &[&str] = &[
    "boot_smoke",
    "memory_map_sanity",
    "divide_by_zero_fault",
    "invalid_opcode_fault",
    "serial_log_format",
    "page_tables",
    "console",
];

/// Whether `address` falls inside the kernel image's virtual range.
///
/// Used by the fault tests to check that a reported instruction pointer is real
/// kernel text. A handler that read its frame from the wrong place produces an
/// address that is not in this range, which is precisely the bug this catches,
/// so the test does not need a symbol table to notice it.
///
/// The bounds come from the linker script's own symbols, so a change to the
/// link-time VMA needs no change here.
pub fn kernel_image_contains(address: u64) -> bool {
    unsafe extern "C" {
        static __kernel_vma_base: u8;
        static __image_end_vma: u8;
    }

    let start = core::ptr::addr_of!(__kernel_vma_base) as u64;
    let end = core::ptr::addr_of!(__image_end_vma) as u64;

    address >= start && address < end
}
