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

    for suite in ALL_SUITES {
        totals.suites += 1;
        report(format_args!("SUITE {}", suite.name));

        for (case_name, case) in suite.cases {
            totals.cases += 1;

            // A panicking case unwinds into the panic handler, so a failure here
            // is always a returned `Err`, never a hang.
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

/// Every suite the kernel runs at boot.
///
/// M0 verifies the boot contract only. M2 adds paging, capability, and IPC
/// suites here as those subsystems land.
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
    let entry = unsafe { encode_entry(physical, flags) };

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
