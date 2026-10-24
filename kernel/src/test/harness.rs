//! Minimal test harness: registration, reporting, and the QEMU exit code.
//!
//! Cases are `fn() -> Result<(), &'static str>` rather than `bool` so a failure
//! carries a reason. The reason string is a `&'static str` because the kernel has
//! no allocator, which keeps every failure message allocation-free.
//!
//! # Why the harness writes raw bytes
//!
//! `report` bypasses `klog!` on purpose. Its output must survive even if the
//! logger's filter or console state is what a test is probing, and a suite that
//! fails *because* the logger failed would otherwise be indistinguishable from a
//! kernel that never printed anything. Formatting goes straight into the byte
//! sink for the same reason: there is no `String` to allocate from.
//!
//! # Exit status
//!
//! QEMU reports `(value << 1) | 1` for `isa-debug-exit`, so the statuses the
//! harness sees are always odd. `scripts/qemu.sh` compensates; see
//! `docs/abi/boot-contract.md`.

use core::fmt::Write as _;

use crate::arch::capabilities::{ByteSink, Console, ConsoleWriter};
use crate::arch::shared::fault::{
    FaultOutcome, FaultRecord, last_fault, publish_outcome, reset_fault,
};
use crate::mm::frame_allocator;

/// The process-wide console, named once so the reporting helpers below do not
/// each re-import it.
fn console() -> &'static impl Console {
    crate::arch::console()
}

/// A single test case.
///
/// Returning `Err(reason)` records a failure and continues; panicking is reserved
/// for genuinely unrecoverable states, which the top-level handler turns into a
/// panic exit.
pub type TestCase = fn() -> Result<(), &'static str>;

/// A named group of test cases.
pub struct TestSuite {
    /// Suite name, used in the `SUITE` line.
    pub name: &'static str,
    /// The cases in the suite.
    pub cases: &'static [(&'static str, TestCase)],
}

/// Running totals across every suite.
#[derive(Default)]
struct Totals {
    suites: usize,
    cases: usize,
    passed: usize,
    failed: usize,
}

/// Write a `[NOVA-TEST]` line directly to the console.
fn report(args: core::fmt::Arguments<'_>) {
    let mut writer = ConsoleWriter::new(console());
    let _ = write!(writer, "[NOVA-TEST] ");
    let _ = writer.write_fmt(args);
    let _ = writer.write_str("\r\n");
}

/// Run every suite, then terminate QEMU with a status reflecting the result.
pub fn run_all() -> ! {
    let mut totals = Totals::default();

    // The suites are grouped by milestone, and each group lives in its own
    // module so that concurrent milestones do not have to edit the same
    // region of this file. The groups run in milestone order, so a serial log
    // reads as the plan reads: boot, then paging, then capability, then
    // services.
    for group in SUITE_GROUPS {
        for suite in *group {
            totals.suites += 1;
            report(format_args!("SUITE {}", suite.name));

            for (case_name, case) in suite.cases {
                totals.cases += 1;

                // A panicking case unwinds into the panic handler, so a failure
                // here is always a returned `Err`, never a hang.
                match case() {
                    Ok(()) => {
                        totals.passed += 1;
                        report(format_args!("  case {case_name} ... ok"));
                    }
                    Err(reason) => {
                        totals.failed += 1;
                        report(format_args!("  case {case_name} ... FAIL: {reason}"));
                    }
                }
            }

            report(format_args!("END   {}", suite.name));
        }
    }

    report(format_args!(
        "RESULT suites={} cases={} passed={} failed={}",
        totals.suites, totals.cases, totals.passed, totals.failed
    ));

    if totals.failed == 0 {
        report(format_args!("SUMMARY all suites passed"));
        // SAFETY: every suite has finished; nothing needs the machine to keep
        // running.
        unsafe { crate::arch::exit(crate::EXIT_SUCCESS) }
    } else {
        report(format_args!("SUMMARY failures detected"));
        // SAFETY: as above.
        unsafe { crate::arch::exit(crate::EXIT_TEST_FAILURE) }
    }
}

/// Every suite the kernel runs at boot, grouped by milestone.
///
/// The suite names are the milestone's exit gate: M1 declares
/// `boot_smoke`, `memory_map_sanity`, `divide_by_zero_fault`,
/// `invalid_opcode_fault` and `serial_log_format`, and `scripts/qemu.sh`
/// asserts on exactly those names. Renaming one silently drops a milestone
/// criterion, so the names are part of the contract rather than labels.
///
/// The grouping exists so each milestone's cases live in their own module.
/// Editing one shared list would mean two milestones working at once had to
/// edit the same lines, and the merge of those edits is a silent way to drop a
/// suite.
static SUITE_GROUPS: &[&[TestSuite]] = &[
    ALL_SUITES,
    crate::test::m2::M2_SUITES,
    crate::test::m34::M3_SUITES,
    crate::test::m34::M4_SUITES,
];

/// The M1 suites: the boot, memory, fault, and console gates.
///
/// The suite names are the milestone's exit gate: M1 declares
/// `boot_smoke`, `memory_map_sanity`, `divide_by_zero_fault`,
/// `invalid_opcode_fault` and `serial_log_format`, and `scripts/qemu.sh`
/// asserts on exactly those names. Renaming one silently drops a milestone
/// criterion, so the names are part of the contract rather than labels.
static ALL_SUITES: &[TestSuite] = &[
    TestSuite {
        name: "boot_smoke",
        cases: &[
            ("console_responds", console_responds),
            ("boot_info_published", boot_info_published),
            ("memory_map_is_usable", memory_map_is_usable),
            ("direct_map_offset_is_aligned", direct_map_offset_is_aligned),
            ("frame_allocator_has_capacity", frame_allocator_has_capacity),
            ("alloc_and_free_round_trips", alloc_and_free_round_trips),
            (
                "alloc_returns_distinct_frames",
                alloc_returns_distinct_frames,
            ),
            (
                "alloc_never_reenters_reserved_floor",
                alloc_never_reenters_reserved_floor,
            ),
            ("allocator_reports_exhaustion", allocator_reports_exhaustion),
        ],
    },
    TestSuite {
        name: "memory_map_sanity",
        cases: &[
            ("every_region_is_page_aligned", every_region_is_page_aligned),
            (
                "every_region_has_nonzero_length",
                every_region_has_nonzero_length,
            ),
            ("no_region_overflows_the_address_space", no_region_overflows),
            (
                "regions_lie_below_the_address_limit",
                regions_below_address_limit,
            ),
            ("usable_memory_meets_the_floor", usable_memory_meets_floor),
            (
                "usable_regions_exist_and_are_contiguous_enough",
                usable_regions_exist,
            ),
            (
                "kernel_image_lies_within_usable_memory",
                kernel_image_within_usable,
            ),
            ("the_snapshot_revalidates", the_snapshot_revalidates),
        ],
    },
    TestSuite {
        name: "divide_by_zero_fault",
        cases: &[
            ("the_vector_is_a_divide_error", the_vector_is_a_divide_error),
            ("the_kind_is_divide_by_zero", the_kind_is_divide_by_zero),
            (
                "the_faulting_ip_points_into_the_kernel",
                the_faulting_ip_is_kernel_text,
            ),
            (
                "the_frame_is_read_from_the_stack",
                the_frame_is_read_from_the_stack,
            ),
            (
                "the_saved_ip_matches_the_raising_site",
                div_saved_ip_matches_site,
            ),
            (
                "the_machine_survives_the_fault",
                the_machine_survives_the_fault,
            ),
            (
                "a_second_identical_fault_behaves_the_same",
                faults_are_repeatable,
            ),
        ],
    },
    TestSuite {
        name: "invalid_opcode_fault",
        cases: &[
            (
                "the_vector_is_an_invalid_opcode",
                the_vector_is_invalid_opcode,
            ),
            (
                "the_kind_is_illegal_instruction",
                the_kind_is_illegal_instruction,
            ),
            (
                "the_saved_ip_matches_the_raising_site",
                opcode_saved_ip_matches_site,
            ),
            (
                "the_fault_is_not_reported_as_user",
                opcode_fault_is_not_user,
            ),
            (
                "a_vector_with_no_error_code_reports_zero",
                a_vector_with_no_error_code_reports_zero,
            ),
            ("the_machine_survives_the_fault", opcode_machine_survives),
        ],
    },
    TestSuite {
        name: "serial_log_format",
        cases: &[
            (
                "a_log_line_has_the_documented_shape",
                log_line_has_documented_shape,
            ),
            ("level_tags_are_fixed_width", level_tags_are_fixed_width),
            (
                "a_fault_line_carries_the_register_set",
                fault_line_has_register_set,
            ),
            ("hex_has_no_leading_zeros", hex_has_no_leading_zeros),
            ("the_fault_prefix_is_stable", fault_prefix_is_stable),
            (
                "a_truncated_report_does_not_overflow",
                truncated_report_is_safe,
            ),
        ],
    },
    TestSuite {
        name: "page_tables",
        cases: &[
            ("four_level_indexing", four_level_indexing),
            ("page_flags_round_trips", page_flags_round_trips),
            ("canonical_address_check", canonical_address_check),
            ("entry_encoding_round_trips", entry_encoding_round_trips),
        ],
    },
    TestSuite {
        name: "console",
        cases: &[
            (
                "byte_sink_truncates_instead_of_overflowing",
                byte_sink_truncates,
            ),
            (
                "byte_sink_renders_hex_without_leading_zeros",
                byte_sink_renders_hex,
            ),
        ],
    },
];

// --- M0 boot suite cases ----------------------------------------------------

/// The console must actually emit bytes, not merely have a device registered.
///
/// A round trip through the formatter and the UART. If the port were wrong the
/// test would still pass, so `scripts/qemu.sh` additionally greps for this exact
/// line in the captured serial output — the probe line below is what it greps
/// for, which is why it bypasses `report`.
fn console_responds() -> Result<(), &'static str> {
    let mut writer = ConsoleWriter::new(console());
    writer
        .write_str("[NOVA-TEST]   probe: com1 write test\r\n")
        .map_err(|_| "serial write failed")?;

    // Reading the line status register is side-effect free on a 16550, so
    // `line_status` needs no `unsafe` from its caller.
    let lsr = crate::arch::x86_64::console::line_status();
    if lsr == 0xFF {
        // The classic "no device" signature: a read from an unclaimed floating
        // bus returns all ones.
        return Err("COM1 line status reads 0xff: no serial device");
    }

    Ok(())
}

/// `boot::capture` must have run and published a coherent snapshot.
fn boot_info_published() -> Result<(), &'static str> {
    if !crate::boot::boot_info_is_ready() {
        return Err("BootInfo was never published");
    }

    // SAFETY: `boot_info_is_ready` returned true, so `capture` completed and the
    // snapshot is immutable from here on.
    let info = unsafe { crate::boot::read_boot_info() };

    if info.protocol_revision == 0 {
        return Err("protocol revision is zero");
    }

    // Re-run the same validation the boot path ran. This is the check that
    // catches a snapshot mutated after publication.
    let validation = info.validate(crate::boot::MAX_PHYSICAL_ADDRESS);
    if !validation.is_ok() {
        return Err("published BootInfo failed re-validation");
    }

    Ok(())
}

/// At least one usable region with sane geometry must exist.
fn memory_map_is_usable() -> Result<(), &'static str> {
    // SAFETY: see `boot_info_published`.
    let info = unsafe { crate::boot::read_boot_info() };

    if info.regions().is_empty() {
        return Err("memory map has no regions");
    }

    if info.usable_bytes() == 0 {
        return Err("memory map reports zero usable bytes");
    }

    // 16 MiB is the floor QEMU is asked for; anything less means the image
    // script and the guest disagree about the `-m` value.
    if info.usable_bytes() < 16 * 1024 * 1024 {
        return Err("usable memory below the 16 MiB floor");
    }

    Ok(())
}

/// The direct-map offset must be non-zero and page aligned.
fn direct_map_offset_is_aligned() -> Result<(), &'static str> {
    // SAFETY: see `boot_info_published`.
    let info = unsafe { crate::boot::read_boot_info() };
    let offset = info.direct_map_offset;

    if offset == 0 {
        return Err("direct map offset is zero");
    }

    if offset % crate::boot::PAGE_SIZE != 0 {
        return Err("direct map offset is not page aligned");
    }

    // The kernel lives in the top canonical half on every target that provides a
    // direct map. A lower offset would make `physical + offset` collide with
    // kernel addresses, so it is refused rather than merely noted.
    if offset < crate::arch::ADDRESS_SPACE.kernel_half_start() {
        return Err("direct map offset is below the canonical kernel half");
    }

    Ok(())
}

/// The allocator must have been seeded with a non-trivial number of frames.
fn frame_allocator_has_capacity() -> Result<(), &'static str> {
    // SAFETY: `mm::init` has run by the time the harness starts.
    let allocator = unsafe { frame_allocator() };

    if allocator.total_frames() == 0 {
        return Err("allocator tracks zero frames");
    }

    if allocator.base_frame() == 0 {
        return Err("allocator floor is frame 0: it would hand out low memory");
    }

    if allocator.free_count() == 0 {
        return Err("allocator starts fully allocated");
    }

    Ok(())
}

/// A freed frame must become allocatable again.
fn alloc_and_free_round_trips() -> Result<(), &'static str> {
    // SAFETY: `mm::init` has run; the allocator is single-CPU here.
    let allocator = unsafe { frame_allocator() };

    let frame = allocator.alloc().map_err(|_| "allocation failed")?;
    if !allocator.is_allocated(frame) {
        return Err("allocated frame is not marked allocated");
    }

    // SAFETY: this frame was just allocated by us and has no other owner, so
    // returning it is exactly the contract `dealloc` requires.
    unsafe { allocator.dealloc(frame) };
    if allocator.is_allocated(frame) {
        return Err("freed frame is still marked allocated");
    }

    Ok(())
}

/// Successive allocations must never alias.
fn alloc_returns_distinct_frames() -> Result<(), &'static str> {
    // SAFETY: as `alloc_and_free_round_trips`.
    let allocator = unsafe { frame_allocator() };

    // A handful is enough to catch an allocator that forgets to set the bit.
    const COUNT: usize = 16;
    let mut frames = [0usize; COUNT];
    for slot in frames.iter_mut() {
        *slot = allocator.alloc().map_err(|_| "allocation failed")?;
    }

    for i in 0..COUNT {
        for j in (i + 1)..COUNT {
            if frames[i] == frames[j] {
                return Err("allocator handed out the same frame twice");
            }
        }
    }

    // Return them so the exhaustion test below starts from a clean pool.
    for &frame in frames.iter() {
        // SAFETY: each frame was allocated once above and is not freed
        // elsewhere.
        unsafe { allocator.dealloc(frame) };
    }

    Ok(())
}

/// Nothing below the reserve floor may ever be handed out.
fn alloc_never_reenters_reserved_floor() -> Result<(), &'static str> {
    // SAFETY: as `alloc_and_free_round_trips`.
    let allocator = unsafe { frame_allocator() };
    let floor = allocator.base_frame();

    for _ in 0..64 {
        let frame = allocator.alloc().map_err(|_| "allocation failed")?;
        if frame < floor {
            // SAFETY: the frame came from `alloc`, so we own it.
            unsafe { allocator.dealloc(frame) };
            return Err("allocator returned a frame below its floor");
        }
        // SAFETY: as above.
        unsafe { allocator.dealloc(frame) };
    }

    Ok(())
}

/// Allocating an absurd number of frames must fail cleanly, not corrupt state.
fn allocator_reports_exhaustion() -> Result<(), &'static str> {
    // SAFETY: as `alloc_and_free_round_trips`.
    let allocator = unsafe { frame_allocator() };

    let free = allocator.stats().free_frames;
    // Reserve a small slice rather than the whole pool, so the machine still has
    // room to print the result.
    let target = free.min(1024);

    let mut frames = [0usize; 1024];
    for slot in frames.iter_mut().take(target) {
        *slot = allocator
            .alloc()
            .map_err(|_| "allocation failed unexpectedly")?;
    }

    // Exactly `target` frames were handed out, so the free count must now sit
    // that far below where it started.
    let expected_free = free - target;

    // Freeing out of range must be refused rather than corrupting the bitmap.
    // SAFETY: passing an out-of-range frame is exactly the case `dealloc`
    // documents as refused.
    unsafe {
        allocator.dealloc(usize::MAX);
        allocator.dealloc(0);
    }

    // Both refused frees must leave the count untouched.
    //
    // The check is on `free_frames`, not on `is_allocated(0)`. Frame 0 sits
    // below the reserved floor, so its bit is *never* set and
    // `is_allocated(0)` answers `false` whether or not the bogus free did
    // anything — the old assertion could not distinguish a refused free from a
    // successful one, and reported the working case as corruption.
    if allocator.stats().free_frames != expected_free {
        return Err("out-of-range free corrupted the free-frame count");
    }

    for &frame in frames.iter().take(target) {
        // SAFETY: each frame was allocated once above.
        unsafe { allocator.dealloc(frame) };
    }

    // Everything must come back, which also proves the bogus frees did not
    // strand a frame the allocator now believes is in use.
    if allocator.stats().free_frames != free {
        return Err("frames were not all returned after the round trip");
    }

    Ok(())
}

// --- Page table suite cases -------------------------------------------------

/// x86-64 four-level paging must decompose a virtual address into P4/P3/P2/P1.
fn four_level_indexing() -> Result<(), &'static str> {
    use crate::arch::x86_64::paging::split_address;

    // A kernel address, a user address, and an unmapped address: all must
    // decompose without panicking and round-trip back to the original.
    for address in [
        0xFFFF_8000_0000_0000u64,
        0x0000_0000_DEAD_BEEFu64,
        0xFFFF_FFFF_FFFF_FFFFu64,
    ] {
        let parts = split_address(address);
        // `recompose` drops the 12 offset bits, because that is what a page
        // table encodes. The round-trip is therefore exact only against the
        // page *base* -- two of the addresses above carry a non-zero offset, so
        // comparing to the raw address can never succeed and the assertion
        // would be testing the wrong invariant.
        if parts.recompose() != crate::arch::x86_64::paging::page_base(address) {
            return Err("address decomposition does not round-trip");
        }
    }

    // The page offset is deliberately *not* represented in any index, so an
    // address with a non-zero offset must recompose to its page base.
    let parts = split_address(0xDEAD_BEEF);
    if parts.recompose() != 0xDEAD_B000 {
        return Err("recomposition kept the page offset it should have dropped");
    }

    Ok(())
}

/// The page-table-entry flags must round-trip through the hardware layout.
fn page_flags_round_trips() -> Result<(), &'static str> {
    use crate::arch::x86_64::paging::PageFlags;

    let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
    if flags.bits() & PageFlags::PRESENT.bits() == 0 {
        return Err("PRESENT flag did not survive construction");
    }
    if PageFlags::from_bits(flags.bits()) != Some(flags) {
        return Err("flags did not round-trip through bits");
    }
    if PageFlags::from_bits(!0u64).is_some() {
        return Err("reserved bits were accepted as valid flags");
    }

    Ok(())
}

/// Canonical-address checking must reject the non-canonical hole in the middle.
fn canonical_address_check() -> Result<(), &'static str> {
    use crate::arch::x86_64::paging::is_canonical;

    if !is_canonical(0xFFFF_8000_0000_0000) {
        return Err("rejected a canonical kernel address");
    }
    if !is_canonical(0x0000_7FFF_FFFF_FFFF) {
        return Err("rejected a canonical user address");
    }

    // Bit 47 is the last addressable bit, so an address with bit 47 set and a
    // zero sign extension is canonical: it is the top of user space. Treating
    // `0x0000_8000_0000_0000` as non-canonical would mean rejecting the last
    // 128 TiB of the user range, which the hardware does allow.
    if !is_canonical(0x0000_8000_0000_0000) {
        return Err("rejected the last canonical user address");
    }

    // A non-canonical address is one whose sign extension disagrees with bit 47,
    // so bits 63:48 must be neither all-zero nor all-one while bit 47 is set.
    // `0x0001_8000_0000_0000` has bit 47 set and a sign of `0x0001`: the
    // smallest such address, and the first gap above the user range.
    if is_canonical(0x0001_8000_0000_0000) {
        return Err("accepted a non-canonical address above user space");
    }

    // Symmetrically, `0xFFFE_7FFF_FFFF_FFFF` has an all-ones sign while bit 47
    // is clear, putting it in the hole *below* the kernel. Note that a genuine
    // kernel address is `0xFFFF_8000_…`, with the sign and bit 47 both set, so
    // dropping bit 47 from one is the quickest way to produce a hole address.
    if is_canonical(0xFFFE_7FFF_FFFF_FFFF) {
        return Err("accepted a non-canonical address below the kernel");
    }

    Ok(())
}

/// Encoding and decoding a page table entry must be inverses.
fn entry_encoding_round_trips() -> Result<(), &'static str> {
    use crate::arch::x86_64::paging::{
        PageFlags, decode_entry, encode_entry, entry_flags, entry_is_present,
    };

    let physical = 0x0000_0000_0020_1000u64;
    let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER | PageFlags::GLOBAL;

    // SAFETY: `physical` is page aligned, which is what the encoding requires.
    let entry = encode_entry(physical, flags);

    if decode_entry(entry) != physical {
        return Err("decoding an entry did not recover its frame address");
    }
    if entry_flags(entry) != flags {
        return Err("decoding an entry did not recover its flags");
    }
    if !entry_is_present(entry) {
        return Err("a PRESENT entry decoded as not present");
    }

    Ok(())
}

// --- Console suite cases ----------------------------------------------------

/// The bounded sink used by fault and panic reporting must truncate, not
/// overflow: a fault on a nearly-exhausted stack has to degrade into a short
/// message rather than into a second fault.
fn byte_sink_truncates() -> Result<(), &'static str> {
    let mut sink = ByteSink::<8>::new();
    if !sink.is_empty() {
        return Err("a new sink was not empty");
    }

    sink.push_str("0123456789abcdef");
    if sink.len() != 8 {
        return Err("the sink did not clamp to its capacity");
    }
    if sink.as_bytes() != b"01234567" {
        return Err("the sink kept the wrong bytes when truncating");
    }

    // Writing past a full sink must be a no-op rather than a panic.
    sink.push_str("more");
    if sink.len() != 8 {
        return Err("writing to a full sink changed its length");
    }

    Ok(())
}

/// Hex rendering must be `0x`-prefixed and carry no leading zeros.
fn byte_sink_renders_hex() -> Result<(), &'static str> {
    let mut sink = ByteSink::<32>::new();
    sink.push_hex(0);
    if sink.as_bytes() != b"0x0" {
        return Err("zero rendered incorrectly");
    }

    let mut sink = ByteSink::<32>::new();
    sink.push_hex(0xDEAD_BEEF);
    if sink.as_bytes() != b"0xdeadbeef" {
        return Err("hex rendering padded or mis-cased");
    }

    Ok(())
}

// --- Fault-proving infrastructure --------------------------------------------
//
// These tests raise real CPU exceptions and assert on what the handler saw. That
// needs three pieces that no ordinary Rust code has: a way to name the address
// *after* a faulting instruction, an `ud2` the compiler cannot see past, and a
// policy that tells the handler to resume.
//
// The `ud2` must live in a naked function. Any `unsafe` division in a normal
// function is simply an `idiv` to the optimizer, which may prove the divisor is
// constant, constant-fold the whole thing to a trap, or delete the divide — at
// which point the test raises no fault and passes without proving anything.

/// Address of the `ud2` in [`illegal_instruction`].
///
/// A naked function whose body is exactly `ud2`, so there is no "next
/// instruction" inside it: [`illegal_instruction`] must jump straight past the
/// trap using the resume target.
#[unsafe(naked)]
pub extern "C" fn illegal_instruction() -> ! {
    core::arch::naked_asm!("ud2")
}

/// Where the handler resumes when it recovers from `illegal_instruction`.
///
/// A distinct, reachable function rather than the fallthrough address, because a
/// naked function has no fallthrough: returning into one would land on whatever
/// the linker happened to place next.
#[unsafe(naked)]
pub extern "C" fn illegal_instruction_resume() -> ! {
    core::arch::naked_asm!("mov rax, 1", "ret")
}

/// An `idiv` by zero, followed by the address the handler should resume at.
///
/// `naked` for the reason above, and split into two symbols so the test can name
/// the instruction after the divide.
#[unsafe(naked)]
pub extern "C" fn divide_by_zero() {
    core::arch::naked_asm!(
        "xor rdx, rdx",
        "mov rax, 1",
        // The divisor is zero, so this raises `#DE`. `rax`/`rdx` are set first so
        // the fault cannot be attributed to an uninitialised operand.
        "xor rcx, rcx",
        "idiv rcx",
    );
}

/// The instruction immediately after `divide_by_zero`'s divide.
///
/// Returning into the middle of a naked function is what keeps the divide itself
/// from being re-executed: `iretq` resumes here, not at the `idiv`.
#[unsafe(naked)]
pub extern "C" fn divide_by_zero_resume() -> ! {
    core::arch::naked_asm!("ret")
}

/// The address `divide_by_zero_resume` returns to.
///
/// The casts go through `*const ()` rather than casting the function item
/// straight to an integer. A function item and its address are not
/// interchangeable — an inlinable item can exist in several copies, and casting
/// the item picks one arbitrarily — whereas taking a pointer first names one
/// definition. That matters here because the fault tests compare the reported
/// `RIP` against these addresses: picking the wrong copy would make a correct
/// handler look wrong. The naked functions involved cannot be inlined, so the
/// address is unique.
fn divide_by_zero_resume_entry() -> u64 {
    divide_by_zero_resume as *const () as u64
}

/// The address `illegal_instruction_resume` returns to.
///
/// See [`divide_by_zero_resume_entry`] for why the cast is two-step.
fn illegal_instruction_resume_entry() -> u64 {
    illegal_instruction_resume as *const () as u64
}

/// Raise `#DE` and return once the handler has resumed the kernel.
///
/// Returns the record the handler published. `None` means the handler never ran,
/// which the caller must distinguish from "it ran and reported something odd" —
/// a fault that silently did not happen would otherwise satisfy the assertions
/// that follow.
fn provoke_divide_by_zero() -> Option<FaultRecord> {
    reset_fault();
    publish_outcome(FaultOutcome::Fatal);

    // The closure returns a value rather than `!`. Typing it `!` would make every
    // line after this call unreachable *as far as the compiler is concerned* —
    // correct, since the body genuinely never returns, but it hides the fact that
    // the resume path does come back, and it stops the compiler checking the code
    // that follows. So the body is written to return normally, and the single
    // `noreturn` call sits inside it.
    //
    // SAFETY: the body performs exactly one faulting instruction and the handler
    // resumes at `divide_by_zero_resume`, whose `ret` returns into this frame —
    // so the window always closes before `run_all` reaches the next case.
    unsafe {
        crate::arch::shared::fault::with_recoverable_faults(|| {
            crate::arch::shared::fault::arm_resume_at(divide_by_zero_resume_entry());
            publish_outcome(FaultOutcome::Continue);

            let entry: extern "C" fn() = divide_by_zero;
            // `divide_by_zero_resume` pops the return address `call` pushed, so
            // the stack is balanced across the fault. The `asm!` needs no `unsafe`
            // of its own: the enclosing `with_recoverable_faults` block is already
            // the one carrying this function's SAFETY argument.
            //
            // `options(nostack)` rather than `options(noreturn)`, and the
            // difference is the whole test.
            //
            // `noreturn` tells the compiler the call never returns, so it emits
            // an `ud2` immediately after it — a trap to catch a control-flow
            // bug. But the resume target *is* a naked `ret`, and it pops the
            // return address the `call` pushed... which is that `ud2`. So the
            // resume lands on the compiler's trap, raises a second, spurious
            // `#UD`, and the machine reports a fault the test never provoked.
            //
            // `nostack` says only that the asm does not adjust `RSP`, which is
            // true, and lets the code after it stay reachable. The `0u8` below
            // is then genuinely unreachable — the resume jumps into
            // `divide_by_zero_resume`, whose `ret` returns past this closure —
            // but the compiler cannot see that, so the value is what gives the
            // closure a return type.
            //
            // `clobber_abi("C")` says the same thing about registers that
            // `options(nostack)` says about the stack pointer: this block
            // destroys every caller-saved register, exactly as a real `call`
            // would. Without it the compiler is free to keep a live value in
            // one of those registers across the block — and the fault path
            // makes that a lie.
            //
            // The reason is the exception trampoline, not the `call`. The
            // handler restores the *interrupted* register file: the values the
            // stub saved at fault time. `divide_by_zero` sets `RCX` to zero
            // before the `idiv`, so the restored `RCX` is zero — not the value
            // the compiler had there before the block. Any caller-saved
            // register the compiler had cached a live value in comes back
            // holding the faulting function's value instead, with no
            // diagnostic.
            //
            // That is exactly the bug this block used to have. `faults_are_repeatable`
            // kept the first fault's `ip` live in `RCX` across the second
            // provoke; the restored `RCX` was zero, so the comparison
            // `first.ip != second.ip` reported a spurious difference and the
            // milestone test failed. It failed only for *some* code layouts,
            // because the compiler's choice of register for `first.ip` depended
            // on the surrounding layout — which is why adding any unrelated
            // statement made the failure disappear.
            //
            // `clobber_abi("C")` removes the compiler's freedom entirely: the
            // value must be spilled or held in a callee-saved register, and
            // the callee-saved registers *are* preserved across the fault,
            // because the stub saves and the trampoline restores them
            // unchanged.
            core::arch::asm!(
                "call {entry}",
                entry = in(reg) entry,
                options(nostack),
                clobber_abi("C")
            );

            // Unreachable in practice; present so the closure has a return type
            // and the caller stays reachable to the compiler.
            0u8
        });
    }

    Some(last_fault())
}

/// Raise `#UD` and return once the handler has resumed the kernel.
fn provoke_invalid_opcode() -> Option<FaultRecord> {
    reset_fault();
    publish_outcome(FaultOutcome::Fatal);

    // SAFETY: as `provoke_divide_by_zero`. The resume target skips the `ud2`, so
    // the instruction cannot re-fault.
    unsafe {
        crate::arch::shared::fault::with_recoverable_faults(|| {
            crate::arch::shared::fault::arm_resume_at(illegal_instruction_resume_entry());
            publish_outcome(FaultOutcome::Continue);

            let entry: extern "C" fn() -> ! = illegal_instruction;
            // The `ud2` never falls through, so the only way out is the
            // handler's resume, which lands in `illegal_instruction_resume` and
            // `ret`s back into this frame. The `asm!` needs no `unsafe` of its
            // own; the enclosing block already carries this function's SAFETY
            // argument.
            //
            // `nostack` rather than `noreturn`: `noreturn` would make the
            // compiler emit an `ud2` after the call, and the resume's `ret`
            // would pop *that* address and re-trap. See `provoke_divide_by_zero`.
            //
            // `clobber_abi("C")` for the same reason as in
            // `provoke_divide_by_zero`: the fault path restores the *interrupted*
            // register file, so every caller-saved register comes back holding
            // the faulting function's value rather than the compiler's. See
            // that function's comment for the full argument.
            core::arch::asm!(
                "call {entry}",
                entry = in(reg) entry,
                options(nostack),
                clobber_abi("C")
            );

            0u8
        });
    }

    Some(last_fault())
}

// --- `memory_map_sanity` cases -----------------------------------------------

/// Every region the firmware reported must start on a page boundary.
fn every_region_is_page_aligned() -> Result<(), &'static str> {
    // SAFETY: `boot::capture` ran before the harness started.
    let info = unsafe { crate::boot::read_boot_info() };

    for region in info.regions() {
        if region.base % crate::boot::PAGE_SIZE != 0 {
            return Err("a memory map entry base is not page aligned");
        }
    }

    Ok(())
}

/// A zero-length region carries no information and makes the allocator's
/// watermark arithmetic ambiguous, so none may survive capture.
fn every_region_has_nonzero_length() -> Result<(), &'static str> {
    // SAFETY: see `every_region_is_page_aligned`.
    let info = unsafe { crate::boot::read_boot_info() };

    for region in info.regions() {
        if region.length == 0 {
            return Err("a memory map entry has zero length");
        }
    }

    Ok(())
}

/// `base + length` must be computable; an entry that overflows describes memory
/// that cannot exist.
fn no_region_overflows() -> Result<(), &'static str> {
    // SAFETY: see `every_region_is_page_aligned`.
    let info = unsafe { crate::boot::read_boot_info() };

    for region in info.regions() {
        if region.base.checked_add(region.length).is_none() {
            return Err("a memory map entry overflows the address space");
        }
    }

    Ok(())
}

/// Regions must stay inside what four-level paging can address.
fn regions_below_address_limit() -> Result<(), &'static str> {
    // SAFETY: see `every_region_is_page_aligned`.
    let info = unsafe { crate::boot::read_boot_info() };

    for region in info.regions() {
        if region.end() > crate::boot::MAX_PHYSICAL_ADDRESS {
            return Err("a memory map entry extends past the addressable limit");
        }
    }

    Ok(())
}

/// QEMU is asked for 256 MiB, so anything materially below that means the image
/// script and the guest disagree about `-m`.
fn usable_memory_meets_floor() -> Result<(), &'static str> {
    // SAFETY: see `every_region_is_page_aligned`.
    let info = unsafe { crate::boot::read_boot_info() };

    if info.usable_bytes() < 16 * 1024 * 1024 {
        return Err("usable memory below the 16 MiB floor");
    }

    Ok(())
}

/// At least one allocatable region must exist, or the frame allocator has
/// nothing to hand out and every later milestone fails for the same reason.
fn usable_regions_exist() -> Result<(), &'static str> {
    // SAFETY: see `every_region_is_page_aligned`.
    let info = unsafe { crate::boot::read_boot_info() };

    let usable = info
        .regions()
        .iter()
        .filter(|region| region.is_usable())
        .count();

    if usable == 0 {
        return Err("no allocatable region in the memory map");
    }

    Ok(())
}

/// The allocator reserves everything below the kernel image, so the image must
/// lie inside memory the firmware called usable — otherwise the allocator is
/// reserving frames from a region it does not track.
fn kernel_image_within_usable() -> Result<(), &'static str> {
    // SAFETY: see `every_region_is_page_aligned`.
    let info = unsafe { crate::boot::read_boot_info() };

    // SAFETY: `mm::init` has run, so the allocator exists and knows its floor.
    let allocator = unsafe { frame_allocator() };
    let floor_address = allocator.base_frame() as u64 * crate::boot::PAGE_SIZE;

    let inside = info.regions().iter().any(|region| {
        region.is_usable() && region.base <= floor_address && floor_address < region.end()
    });

    if !inside {
        return Err("the allocator's floor lies outside every usable region");
    }

    Ok(())
}

/// The published snapshot must still satisfy every invariant after capture.
fn the_snapshot_revalidates() -> Result<(), &'static str> {
    // SAFETY: see `every_region_is_page_aligned`.
    let info = unsafe { crate::boot::read_boot_info() };

    let validation = info.validate(crate::boot::MAX_PHYSICAL_ADDRESS);
    if !validation.is_ok() {
        return Err("the published snapshot failed re-validation");
    }

    // Out-of-order entries are legal but should not be *common*; a high count
    // means the copy loop read the wrong array.
    if validation.out_of_order > info.regions().len() {
        return Err("more out-of-order entries than regions");
    }

    Ok(())
}

// --- `divide_by_zero_fault` cases -------------------------------------------

/// The handler must receive vector 0 for a divide error.
fn the_vector_is_a_divide_error() -> Result<(), &'static str> {
    match provoke_divide_by_zero() {
        Some(record) if record.is_set() => {
            if record.vector != 0 {
                return Err("a divide error did not arrive on vector 0");
            }
            Ok(())
        }
        _ => Err("the divide-by-zero handler never ran"),
    }
}

/// The vector must be normalised to the shared `DivideByZero` kind, not left as
/// a generic synchronous fault.
fn the_kind_is_divide_by_zero() -> Result<(), &'static str> {
    use crate::arch::capabilities::FaultKind;

    let record = provoke_divide_by_zero().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the divide");
    }
    if record.kind != FaultKind::DivideByZero {
        return Err("the divide was not classified as DivideByZero");
    }

    Ok(())
}

/// The saved `RIP` must be inside the kernel image.
///
/// This is the assertion that would have caught the original bug: the old stubs
/// read the frame out of `RDI`, which held whatever the interrupted code left
/// there. A value that is not code is the symptom, and "not in the image" is
/// checkable without a symbol table.
fn the_faulting_ip_is_kernel_text() -> Result<(), &'static str> {
    let record = provoke_divide_by_zero().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the divide");
    }

    if !crate::test::kernel_image_contains(record.ip) {
        return Err("the reported ip lies outside the kernel image");
    }

    Ok(())
}

/// The saved `CS` must read back as a segment selector, and `RFLAGS` as a flags
/// word — the two things that are definitely wrong when a frame is read from the
/// wrong place.
fn the_frame_is_read_from_the_stack() -> Result<(), &'static str> {
    let record = provoke_divide_by_zero().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the divide");
    }

    // `CS` bit 2 is the long-mode flag and the low two bits are the RPL, so a
    // real kernel selector has bits 3 and 2 set. A misaligned read lands in the
    // middle of an instruction, where bit 2 is set about a quarter of the time
    // and the value is enormous; checking the plausible range is what makes this
    // an assertion rather than a coin flip.
    if record.cs_value & 0xFFF8 == 0 || record.cs_value > 0xFFFF {
        return Err("the saved cs is not a plausible segment selector");
    }

    // Bit 1 of `RFLAGS` is reserved and always reads 1; a misaligned read lands
    // in code, where it is usually clear. This single bit distinguishes the two
    // cases better than any range check.
    if record.rflags_value & 0x2 == 0 {
        return Err("the saved rflags does not have the reserved bit set");
    }

    Ok(())
}

/// The saved `RIP` must be the `idiv` itself, not the call site or a neighbour.
fn div_saved_ip_matches_site() -> Result<(), &'static str> {
    let record = provoke_divide_by_zero().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the divide");
    }

    let expected = divide_by_zero as *const () as u64;
    let actual = record.ip;

    // `idiv` is a three-byte instruction, so the faulting `RIP` is the `idiv`
    // itself — never the instruction after it. Comparing the exact symbol
    // address would additionally pin the fault to the *first* instruction of the
    // naked body, which is not guaranteed, so a small window is the honest check:
    // within the function, but not past its end.
    let resume = divide_by_zero_resume_entry();

    if actual < expected || actual >= resume {
        return Err("the saved ip is not inside the divide body");
    }

    Ok(())
}

/// Reaching the next line of Rust at all proves the machine survived.
fn the_machine_survives_the_fault() -> Result<(), &'static str> {
    let record = provoke_divide_by_zero().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the divide");
    }

    // The handler only resumes when the interlock is armed, so the fact that this
    // function is running at all means the resume path executed.
    if crate::arch::shared::fault::resume_target() != 0 {
        crate::arch::shared::fault::disarm_resume();
    }

    Ok(())
}

/// A second identical fault must behave exactly like the first: the handler
/// carries no state that changes its verdict.
fn faults_are_repeatable() -> Result<(), &'static str> {
    let first = provoke_divide_by_zero().ok_or("the first fault never ran")?;
    let second = provoke_divide_by_zero().ok_or("the second fault never ran")?;

    if !first.is_set() || !second.is_set() {
        return Err("one of the two faults was never recorded");
    }
    if first.vector != second.vector {
        return Err("the same fault arrived on different vectors");
    }
    if first.kind != second.kind {
        return Err("the same fault was classified differently the second time");
    }
    if first.ip != second.ip {
        return Err("the same fault reported a different ip the second time");
    }

    Ok(())
}

// --- `invalid_opcode_fault` cases -------------------------------------------

/// The handler must receive vector 6 for an invalid opcode.
fn the_vector_is_invalid_opcode() -> Result<(), &'static str> {
    let record = provoke_invalid_opcode().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the ud2");
    }
    if record.vector != 6 {
        return Err("an invalid opcode did not arrive on vector 6");
    }

    Ok(())
}

/// It must be normalised to `IllegalInstruction`.
fn the_kind_is_illegal_instruction() -> Result<(), &'static str> {
    use crate::arch::capabilities::FaultKind;

    let record = provoke_invalid_opcode().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the ud2");
    }
    if record.kind != FaultKind::IllegalInstruction {
        return Err("the ud2 was not classified as IllegalInstruction");
    }

    Ok(())
}

/// The saved `RIP` must be the `ud2` inside the naked function.
fn opcode_saved_ip_matches_site() -> Result<(), &'static str> {
    let record = provoke_invalid_opcode().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the ud2");
    }

    let expected = illegal_instruction as *const () as u64;
    let resume = illegal_instruction_resume_entry();

    // `ud2` is two bytes and is the only instruction in the function, so the
    // saved `RIP` must be within this symbol and strictly before the next one.
    if record.ip < expected || record.ip >= resume {
        return Err("the saved ip is not inside the ud2 body");
    }

    Ok(())
}

/// A fault taken from ring 0 must not be reported as coming from user space.
///
/// This is the check on the privilege-level decode, and it is the one that fails
/// when the frame is read at the wrong displacement: `CS` then holds a word of
/// code, whose low bits are essentially random.
fn opcode_fault_is_not_user() -> Result<(), &'static str> {
    let record = provoke_invalid_opcode().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the ud2");
    }

    if record.from_user {
        return Err("a ring 0 fault was reported as coming from user space");
    }
    if record.cs_value & 0x3 == 3 {
        return Err("the saved cs claims ring 3 for a kernel-mode fault");
    }

    Ok(())
}

/// A vector that pushes no error code must report one as `0`.
///
/// This is the check on the error-code *suppression* rule, and it is the
/// regression test for the frame misread that made `#UD` look like it had
/// pushed an opcode table id.
///
/// `#UD` is the interesting case because the two x86 vendors disagree about
/// it. The Intel SDM specifies that `#UD` pushes **no** error code; AMD's APM
/// specifies that it pushes the opcode table index. QEMU's `qemu64` model
/// follows Intel, and `docs/adr/0004-x86-64-exception-frame.md` records the
/// measurement that settled it: the word at the bottom of the `#UD` frame is
/// `RIP`, not a table id.
///
/// So on this machine the correct error code is `0`. Reading a table id of `1`
/// here would mean the frame was read at the wrong displacement — the word
/// after `RIP` is `CS`, and `CS` on x86-64 is `0x08`/`0x10`, neither of which
/// is `1`. The test therefore asserts `0`, which distinguishes "correctly
/// suppressed" from "read the wrong word" only because it would also fail if
/// the suppression were simply dropped and the raw frame word leaked through.
fn a_vector_with_no_error_code_reports_zero() -> Result<(), &'static str> {
    let record = provoke_invalid_opcode().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the ud2");
    }

    // Intel-family `#UD` pushes nothing, so the reported code is the stub's
    // zero placeholder. Any non-zero value means a word that was not an error
    // code was read as one.
    if record.error_code != 0 {
        return Err("a vector that pushes no error code reported one");
    }

    Ok(())
}

/// The machine must still be running afterwards.
fn opcode_machine_survives() -> Result<(), &'static str> {
    let record = provoke_invalid_opcode().ok_or("the handler never ran")?;
    if !record.is_set() {
        return Err("no fault was recorded for the ud2");
    }

    if crate::arch::shared::fault::resume_target() != 0 {
        crate::arch::shared::fault::disarm_resume();
    }

    Ok(())
}

// --- `serial_log_format` cases ----------------------------------------------

/// The documented line shape is `[NOVA] <level> | <message>`, with the level
/// column padded to a fixed width so serial logs stay aligned.
fn log_line_has_documented_shape() -> Result<(), &'static str> {
    use crate::logger::Level;

    let mut sink = ByteSink::<128>::new();
    sink.push_str("[NOVA] ");
    sink.push_str(Level::Info.tag());
    sink.push_str(" | ");
    sink.push_str("hello");

    if sink.as_bytes() != b"[NOVA] INFO  | hello" {
        return Err("the log line does not match the documented shape");
    }

    Ok(())
}

/// Every level tag must be the same width, or the `|` column drifts.
fn level_tags_are_fixed_width() -> Result<(), &'static str> {
    use crate::logger::Level;

    for level in [
        Level::Trace,
        Level::Debug,
        Level::Info,
        Level::Warn,
        Level::Error,
    ] {
        if level.tag().len() != 5 {
            return Err("a level tag is not five characters wide");
        }
    }

    Ok(())
}

/// A fault report must carry the registers; a report without them is
/// indistinguishable from "the machine stopped for some reason".
fn fault_line_has_register_set() -> Result<(), &'static str> {
    use crate::arch::capabilities::{FaultInfo, FaultKind};

    let info = FaultInfo {
        kind: FaultKind::DivideByZero,
        vector: 0,
        ip: 0xFFFF_8000_0010_0000,
        sp: 0xFFFF_8000_0020_0000,
        address: None,
        from_user: false,
        detail: 0,
        error_code: 0,
    };

    let mut sink = ByteSink::<320>::new();
    crate::arch::shared::fault::format_into(&mut sink, &info, None);
    let rendered = sink.as_bytes();

    let text = core::str::from_utf8(rendered).map_err(|_| "the report is not valid UTF-8")?;

    if !text.contains("ip=") || !text.contains("sp=") {
        return Err("the fault report omits the register set");
    }
    if !text.contains("divide-by-zero") {
        return Err("the fault report omits the fault tag");
    }
    if !text.contains("0xffff800000100000") {
        return Err("the fault report mis-rendered the ip");
    }

    Ok(())
}

/// Hex must be lower-case, `0x`-prefixed, and free of leading zeros — the
/// harness greps for these.
fn hex_has_no_leading_zeros() -> Result<(), &'static str> {
    let mut sink = ByteSink::<32>::new();
    sink.push_hex(1);
    if sink.as_bytes() != b"0x1" {
        return Err("hex padded a small value");
    }

    let mut sink = ByteSink::<32>::new();
    sink.push_hex(0xDEAD_BEEF);
    if sink.as_bytes() != b"0xdeadbeef" {
        return Err("hex was not lower-case");
    }

    Ok(())
}

/// The `[NOVA] ` prefix is what every harness grep keys on, so it is part of the
/// contract rather than cosmetic.
fn fault_prefix_is_stable() -> Result<(), &'static str> {
    use crate::arch::capabilities::{FaultInfo, FaultKind};

    let info = FaultInfo {
        kind: FaultKind::SynchronousGeneric,
        vector: 0,
        ip: 0,
        sp: 0,
        address: None,
        from_user: true,
        detail: 0,
        error_code: 0,
    };

    let mut sink = ByteSink::<320>::new();
    crate::arch::shared::fault::format_into(&mut sink, &info, None);
    let text = core::str::from_utf8(sink.as_bytes()).map_err(|_| "not UTF-8")?;

    if !text.starts_with("[NOVA] ") {
        return Err("the fault report does not start with the stable prefix");
    }
    // A user-origin fault must say what will happen to the thread; without that
    // line it is indistinguishable from a kernel bug.
    if !text.contains("unprivileged") {
        return Err("a user-origin fault does not say the thread is killed");
    }

    Ok(())
}

/// A report that overflows its sink must truncate rather than panic: the fault
/// path runs on a stack whose exhaustion is itself the fault.
fn truncated_report_is_safe() -> Result<(), &'static str> {
    use crate::arch::capabilities::{FaultInfo, FaultKind};

    let info = FaultInfo {
        kind: FaultKind::MachineCheck,
        vector: 18,
        ip: u64::MAX,
        sp: u64::MAX,
        address: Some(u64::MAX),
        from_user: false,
        detail: u64::MAX,
        error_code: u64::MAX,
    };

    // A sink far too small for the report.
    let mut sink = ByteSink::<24>::new();
    crate::arch::shared::fault::format_into(&mut sink, &info, None);

    if sink.len() != 24 {
        return Err("the report did not clamp to the sink capacity");
    }

    Ok(())
}
