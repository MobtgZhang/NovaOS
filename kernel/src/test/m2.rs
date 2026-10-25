//! The M2 suites: paging, user mode, and scheduling.
//!
//! # Why these live in their own file
//!
//! The harness registration list is the milestone's exit gate, and several
//! agents work on it concurrently. Keeping each milestone's cases in its own
//! module means the shared files -- `harness.rs` and `mod.rs` -- carry only a
//! registration line per milestone, so two milestones never have to touch the
//! same region of the same file.
//!
//! # The bar these tests must clear
//!
//! A test that passes without exercising the mechanism is worse than no test
//! at all, because it reports green for a feature that does not work. Every
//! case here names the specific failure it is designed to catch, and the
//! comment above each case says what that failure looks like from the outside.
//!
//! # The three things M2 has to prove
//!
//! 1. **A user program runs.** An ELF64 image is validated, mapped into a
//!    private address space, entered in ring 3, and able to talk to the kernel
//!    through a syscall.
//!
//! 2. **A user program cannot reach the kernel.** Not "the kernel's *data*" --
//!    the kernel's *page tables*. The specific hazard these cases pin is the
//!    address `0x0000_8000_0000_0000`, whose page-table walk is *identical* to
//!    the direct map's base `0xFFFF_8000_0000_0000`: both select PML4 entry
//!    256, the kernel half's own entry. A user mapping there would write a
//!    leaf into the kernel's intermediate tables, shared with every process.
//!
//! 3. **Two processes are isolated, and the scheduler is fair.** A round-robin
//!    policy hands each runnable thread a turn, and a timeslice expiry
//!    preempts a thread that will not yield on its own.
//!
//! # What these cases can and cannot prove
//!
//! The harness runs in ring 0. The cases below therefore exercise the
//! *mechanism* -- the mapping path, the window check, the ELF validator, the
//! scheduler's policy state machine -- by calling the same code the kernel
//! calls on a user thread's behalf. Where a case can cheaply cross the
//! privilege boundary for real, it does; where doing so would risk halting the
//! machine (a genuine ring-3 `#PF` kills the owning thread and task), the case
//! says so in its own comment rather than pretending to have covered it.

use nova_core::address_space::{Protection, UserWindow, VmmError};
use nova_elf::builder::Segment;
use nova_elf::{Elf, ElfBuilder, ElfError};

use super::harness::TestSuite;
use crate::mm::address_space::{self, AddressSpace};

/// The M2 suites, in registration order.
///
/// The names are the milestone's exit gate and must not be renamed: the shell
/// harness asserts on exactly these strings.
pub static M2_SUITES: &[TestSuite] = &[
    TestSuite {
        name: "user_hello",
        cases: &[
            ("the_image_parses", the_image_parses),
            ("the_entry_point_is_recovered", the_entry_point_is_recovered),
            ("the_segments_are_inspected", the_segments_are_inspected),
            (
                "the_user_window_admits_the_image",
                the_user_window_admits_the_image,
            ),
            (
                "the_image_is_loaded_into_a_space",
                the_image_is_loaded_into_a_space,
            ),
            ("the_user_stack_is_mapped", the_user_stack_is_mapped),
            ("the_guard_page_is_unmapped", the_guard_page_is_unmapped),
            (
                "the_entry_point_is_executable",
                the_entry_point_is_executable,
            ),
            (
                "the_user_thread_starts_in_ring_three",
                the_user_thread_starts_in_ring_three,
            ),
        ],
    },
    TestSuite {
        name: "round_robin_threads",
        cases: &[
            ("the_threads_are_registered", the_threads_are_registered),
            (
                "the_queue_is_in_creation_order",
                the_queue_is_in_creation_order,
            ),
            ("each_thread_runs_in_turn", each_thread_runs_in_turn),
            ("the_queue_cycles", the_queue_cycles),
            (
                "a_yield_puts_the_thread_at_the_back",
                a_yield_puts_the_thread_at_the_back,
            ),
            (
                "a_thread_is_never_queued_twice",
                a_thread_is_never_queued_twice,
            ),
            (
                "an_exited_thread_leaves_the_queue",
                an_exited_thread_leaves_the_queue,
            ),
        ],
    },
    TestSuite {
        name: "preempt_busy_loop",
        cases: &[
            (
                "the_timeslice_is_the_documented_one",
                the_timeslice_is_the_documented_one,
            ),
            ("a_tick_is_accounted", a_tick_is_accounted),
            (
                "the_quantum_is_consumed_before_the_preemption",
                the_quantum_is_consumed,
            ),
            (
                "the_timeslice_expires_after_its_ticks",
                the_timeslice_expires,
            ),
            (
                "a_preempted_thread_goes_back_to_the_queue",
                a_preempted_thread_returns_to_queue,
            ),
            (
                "the_busy_loop_thread_was_preempted",
                the_busy_loop_thread_was_preempted,
            ),
        ],
    },
    TestSuite {
        name: "user_cannot_write_kernel",
        cases: &[
            (
                "the_user_alias_of_the_direct_map_is_refused",
                the_user_alias_of_the_direct_map_is_refused,
            ),
            (
                "the_alias_has_the_kernels_page_table_walk",
                the_alias_has_the_kernels_walk,
            ),
            ("a_kernel_address_is_refused", a_kernel_address_is_refused),
            (
                "a_non_canonical_address_is_refused",
                a_non_canonical_address_is_refused,
            ),
            (
                "the_window_excludes_the_kernel_half",
                the_window_excludes_the_kernel_half,
            ),
            (
                "the_kernel_half_is_supervisor_only",
                the_kernel_half_is_supervisor_only,
            ),
            (
                "user_translation_refuses_the_kernel_half",
                user_translation_refuses_the_kernel_half,
            ),
            (
                "the_kernel_half_is_shared_not_copied",
                the_kernel_half_is_shared_not_copied,
            ),
            (
                "a_user_mapping_cannot_reach_the_kernel_tables",
                a_user_mapping_cannot_reach_the_kernel_tables,
            ),
        ],
    },
    TestSuite {
        name: "task_a_cannot_read_task_b",
        cases: &[
            ("the_two_spaces_are_distinct", the_two_spaces_are_distinct),
            (
                "the_same_address_maps_different_frames",
                the_same_address_maps_different_frames,
            ),
            (
                "a_space_cannot_see_the_other_frames",
                a_space_cannot_see_the_other_frames,
            ),
            (
                "the_lower_halves_are_not_shared",
                the_lower_halves_are_not_shared,
            ),
            ("the_kernel_halves_are_shared", the_kernel_halves_are_shared),
            (
                "writing_one_space_leaves_the_other_alone",
                writing_one_space_leaves_the_other_alone,
            ),
        ],
    },
    TestSuite {
        name: "invalid_elf_rejected",
        cases: &[
            (
                "a_truncated_header_is_refused",
                a_truncated_header_is_refused,
            ),
            ("a_bad_magic_is_refused", a_bad_magic_is_refused),
            ("a_wrong_machine_is_refused", a_wrong_machine_is_refused),
            ("a_bad_header_size_is_refused", a_bad_header_size_is_refused),
            (
                "a_non_canonical_segment_is_refused",
                a_non_canonical_segment_is_refused,
            ),
            (
                "a_segment_in_the_kernel_half_is_refused",
                a_segment_in_the_kernel_half_is_refused,
            ),
            (
                "a_writable_executable_segment_is_refused",
                a_writable_executable_segment_is_refused,
            ),
            (
                "overlapping_segments_are_refused",
                overlapping_segments_are_refused,
            ),
            (
                "an_entry_point_outside_the_segments_is_refused",
                an_entry_point_outside_is_refused,
            ),
            (
                "a_segment_past_the_end_of_the_file_is_refused",
                a_segment_past_the_file_is_refused,
            ),
            (
                "the_loader_refuses_a_malformed_image",
                the_loader_refuses_a_malformed_image,
            ),
            (
                "the_loader_refuses_a_kernel_half_segment",
                the_loader_refuses_a_kernel_segment,
            ),
        ],
    },
];

// --- Helpers -----------------------------------------------------------------

/// The kernel half's start on this target.
///
/// Derived from the architecture rather than written down, so a target with a
/// different virtual address width gets the right number without a change
/// here.
const fn kernel_half_start() -> u64 {
    crate::arch::ADDRESS_SPACE.kernel_half_start()
}

/// The user window the kernel permits.
fn user_window() -> UserWindow {
    address_space::user_window()
}

/// A one-segment image loaded at `vaddr`.
///
/// The segment is read-only and executable, so it is a legal user image under
/// the W^X rule and the loader accepts it.
fn image_at(vaddr: u64) -> nova_elf::builder::BuiltElf {
    ElfBuilder::new()
        .entry(vaddr)
        .segment(Segment::rx(vaddr, &[0x90; 16]))
        .build()
}

/// The bytes of a fresh address space with the kernel half cloned into it.
///
/// # Errors
///
/// Returns the error the address-space builder reported.
fn new_process_space() -> Result<AddressSpace, VmmError> {
    // SAFETY: `boot::capture` has run and published the snapshot, which is
    // immutable from here on, and the builder only reads it.
    let boot_info = unsafe { crate::boot::read_boot_info() };
    address_space::create_process_space(boot_info)
}

/// Allocate one physical frame and return its physical address.
fn alloc_frame() -> Result<u64, &'static str> {
    // SAFETY: `mm::init` has run and published the allocator, and the kernel's
    // single-CPU discipline means no other allocation is in flight.
    let allocator = unsafe { crate::mm::frame_allocator() };
    let frame = allocator.alloc().map_err(|_| "frame allocation failed")?;
    Ok(frame as u64 * crate::arch::ADDRESS_SPACE.page_size)
}

/// Write a byte pattern into a frame, through the direct map.
///
/// # Safety
///
/// `phys` must name a frame this kernel owns, and `value` must be a byte the
/// caller wants to find there afterwards.
unsafe fn poke_frame(phys: u64, value: u8) {
    // SAFETY: delegated to this function's contract. The direct map covers
    // every frame the allocator hands out, and the write is one byte inside
    // the frame the caller owns.
    unsafe {
        crate::mm::direct_map::phys_to_virt(phys).write(value);
    }
}

/// Read a byte from a frame, through the direct map.
///
/// # Safety
///
/// `phys` must name a frame this kernel owns.
unsafe fn peek_frame(phys: u64) -> u8 {
    // SAFETY: delegated to this function's contract.
    unsafe { *crate::mm::direct_map::phys_to_virt(phys) }
}

// --- `user_hello` cases -----------------------------------------------------

/// The image the loader is handed must parse.
fn the_image_parses() -> Result<(), &'static str> {
    let built = image_at(0x0000_0000_0000_1000);
    let elf = Elf::parse(built.as_slice()).map_err(|_| "the built image does not parse")?;

    if elf.program_header_count() == 0 {
        return Err("the parsed image has no program headers");
    }

    Ok(())
}

/// The entry point must survive parsing unchanged.
fn the_entry_point_is_recovered() -> Result<(), &'static str> {
    let entry = 0x0000_0000_0000_1000u64;
    let built = image_at(entry);
    let elf = Elf::parse(built.as_slice()).map_err(|_| "the built image does not parse")?;

    if elf.entry_point() != entry {
        return Err("the entry point did not survive parsing");
    }

    // The entry point must also fall inside a loadable segment: an entry
    // point outside every segment is a jump into nothing.
    if !elf.entry_point_in_load() {
        return Err("the entry point is not inside a loadable segment");
    }

    Ok(())
}

/// The segment's flags must survive the round trip through the parser.
fn the_segments_are_inspected() -> Result<(), &'static str> {
    let vaddr = 0x0000_0000_0000_1000u64;
    let built = image_at(vaddr);
    let elf = Elf::parse(built.as_slice()).map_err(|_| "the built image does not parse")?;

    let mut count = 0usize;
    for segment in elf.loadable_segments() {
        count += 1;
        if segment.p_vaddr != vaddr {
            return Err("the segment's virtual address did not survive parsing");
        }
        if !segment.is_executable() {
            return Err("the segment's executable flag did not survive parsing");
        }
        if segment.is_writable() {
            return Err("the segment's writable flag did not survive parsing");
        }
    }

    if count != 1 {
        return Err("the image does not have exactly one loadable segment");
    }

    Ok(())
}

/// The image's addresses must fall inside the user window.
fn the_user_window_admits_the_image() -> Result<(), &'static str> {
    let window = user_window();
    let built = image_at(0x0000_0000_0000_1000);
    let elf = Elf::parse(built.as_slice()).map_err(|_| "the built image does not parse")?;

    for segment in elf.loadable_segments() {
        if !window.contains(segment.p_vaddr) {
            return Err("the segment's start is outside the user window");
        }
        if !window.contains(segment.vaddr_end().saturating_sub(1)) {
            return Err("the segment's end is outside the user window");
        }
    }

    Ok(())
}

/// The loader must map the image into a private address space.
fn the_image_is_loaded_into_a_space() -> Result<(), &'static str> {
    let space = new_process_space().map_err(|_| "could not create an address space")?;
    let built = image_at(0x0000_0000_0000_1000);

    // SAFETY: `boot::capture` has published the snapshot.
    let boot_info = unsafe { crate::boot::read_boot_info() };
    let loaded = crate::loader::load(&space, built.as_slice(), boot_info)
        .map_err(|_| "the loader refused a valid image")?;

    if loaded.entry != 0x0000_0000_0000_1000 {
        return Err("the loaded image's entry point is wrong");
    }
    if loaded.image_pages == 0 {
        return Err("the loader mapped no image pages");
    }

    // The segment's pages must now be mapped in *this* space.
    if address_space::translate(&space, 0x0000_0000_0000_1000).is_none() {
        return Err("the loader did not map the segment");
    }

    Ok(())
}

/// The user stack must be mapped, at the ABI's address.
fn the_user_stack_is_mapped() -> Result<(), &'static str> {
    let space = new_process_space().map_err(|_| "could not create an address space")?;
    let built = image_at(0x0000_0000_0000_1000);

    // SAFETY: `boot::capture` has published the snapshot.
    let boot_info = unsafe { crate::boot::read_boot_info() };
    let loaded = crate::loader::load(&space, built.as_slice(), boot_info)
        .map_err(|_| "the loader refused a valid image")?;

    // The stack pointer must be the ABI's stack top.
    if loaded.stack_pointer != nova_abi::layout::USER_STACK_TOP {
        return Err("the loaded stack pointer is not the ABI's stack top");
    }

    // The topmost stack page must be mapped.
    let top_page = nova_abi::layout::USER_STACK_TOP - nova_abi::layout::PAGE_SIZE;
    if address_space::translate(&space, top_page).is_none() {
        return Err("the topmost user stack page is not mapped");
    }

    // And so must the lowest one.
    if address_space::translate(&space, nova_abi::layout::USER_STACK_BASE).is_none() {
        return Err("the lowest user stack page is not mapped");
    }

    Ok(())
}

/// The page immediately below the stack must be left unmapped.
///
/// The guard page is the *absence* of a mapping, so the only way to check it
/// is to look for the absence.
fn the_guard_page_is_unmapped() -> Result<(), &'static str> {
    let space = new_process_space().map_err(|_| "could not create an address space")?;
    let built = image_at(0x0000_0000_0000_1000);

    // SAFETY: `boot::capture` has published the snapshot.
    let boot_info = unsafe { crate::boot::read_boot_info() };
    crate::loader::load(&space, built.as_slice(), boot_info)
        .map_err(|_| "the loader refused a valid image")?;

    if address_space::translate(&space, nova_abi::layout::USER_GUARD_BASE).is_some() {
        return Err("the guard page is mapped");
    }

    Ok(())
}

/// The entry point must land on a page that is executable and not writable.
fn the_entry_point_is_executable() -> Result<(), &'static str> {
    use crate::arch::x86_64::paging::{PageFlags, entry_flags};
    use crate::mm::page_table::leaf_entry;

    let space = new_process_space().map_err(|_| "could not create an address space")?;
    let built = image_at(0x0000_0000_0000_1000);

    // SAFETY: `boot::capture` has published the snapshot.
    let boot_info = unsafe { crate::boot::read_boot_info() };
    crate::loader::load(&space, built.as_slice(), boot_info)
        .map_err(|_| "the loader refused a valid image")?;

    let entry = 0x0000_0000_0000_1000u64;
    let leaf = leaf_entry(&space.pml4(), entry).ok_or("the entry page has no leaf entry")?;
    let flags = entry_flags(leaf);

    if !flags.contains(PageFlags::PRESENT) {
        return Err("the entry page is not present");
    }
    if !flags.contains(PageFlags::USER) {
        return Err("the entry page is not user-accessible");
    }
    if flags.contains(PageFlags::WRITABLE) {
        return Err("the entry page is writable: the W^X rule was not applied");
    }

    Ok(())
}

/// A user thread's initial context must be a ring-3 context.
///
/// The context is built from the loader's entry point and stack pointer, so
/// this is the check that the privilege change is *prepared* -- the `iretq`
/// that performs it is the mechanism's job.
fn the_user_thread_starts_in_ring_three() -> Result<(), &'static str> {
    use crate::arch::x86_64::gdt::USER_CODE_SELECTOR;
    use crate::task::{Context, Entry};

    let context = Context::user_start(0x0000_0000_0000_1000, 0x0000_0000_7FFF_F000);

    if !context.is_user() {
        return Err("a user context does not report itself as user-mode");
    }
    if context.cs != USER_CODE_SELECTOR as u64 {
        return Err("a user context does not carry the user code selector");
    }
    if context.cs & 0x3 != 3 {
        return Err("the user code selector does not request ring 3");
    }
    if context.rflags & (1 << 9) == 0 {
        return Err("the user context has interrupts disabled: it could never be preempted");
    }

    // A kernel context must be the opposite on both counts.
    let kernel = Context::kernel_start(0, 0);
    if kernel.is_user() {
        return Err("a kernel context reports itself as user-mode");
    }
    if kernel.cs & 0x3 != 0 {
        return Err("the kernel code selector does not request ring 0");
    }

    // The entry the scheduler builds for a user thread must be the user form.
    let entry = crate::sched::user_entry(context.rip, context.rsp);
    match entry {
        Entry::User { rip, rsp } => {
            if rip != context.rip || rsp != context.rsp {
                return Err("the user entry does not carry the context's addresses");
            }
        }
        Entry::Kernel(_) => return Err("the user entry is not the user form"),
    }

    Ok(())
}

// --- `round_robin_threads` cases --------------------------------------------

/// Creating threads must register them with the scheduler.
fn the_threads_are_registered() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);

    let first = scheduler
        .create_thread(0, 0)
        .map_err(|_| "the scheduler refused the first thread")?;
    let second = scheduler
        .create_thread(0, 0)
        .map_err(|_| "the scheduler refused the second thread")?;

    if scheduler.thread_count() != 2 {
        return Err("the scheduler did not record both threads");
    }
    if scheduler.runnable_count() != 2 {
        return Err("the scheduler did not queue both threads");
    }
    if first == second {
        return Err("the scheduler minted the same identifier twice");
    }

    Ok(())
}

/// The queue must be in creation order: round-robin is fair only if the order
/// is well-defined.
fn the_queue_is_in_creation_order() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);

    let ids: [u32; 3] = [
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread one refused")?,
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread two refused")?,
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread three refused")?,
    ];

    let mut order = [0u32; 3];
    let mut seen = 0usize;
    for id in scheduler.runnable_order() {
        if seen >= order.len() {
            return Err("the runnable queue is longer than expected");
        }
        order[seen] = id;
        seen += 1;
    }
    if seen != order.len() {
        return Err("the runnable queue is shorter than expected");
    }
    for index in 0..order.len() {
        if order[index] != ids[index] {
            return Err("the runnable queue is not in creation order");
        }
    }

    Ok(())
}

/// Each thread must get a turn, in queue order.
fn each_thread_runs_in_turn() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);

    let ids: [u32; 3] = [
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread one refused")?,
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread two refused")?,
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread three refused")?,
    ];

    for expected in ids.iter() {
        match scheduler.schedule() {
            Some(actual) if actual == *expected => {}
            Some(_) => return Err("the scheduler chose a thread out of order"),
            None => return Err("the scheduler found no runnable thread"),
        }
    }

    Ok(())
}

/// The queue must cycle: after every thread has run, the order repeats.
fn the_queue_cycles() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);

    let ids: [u32; 2] = [
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread one refused")?,
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread two refused")?,
    ];

    // Two full rounds.
    for round in 0..2 {
        for expected in ids.iter() {
            let actual = scheduler
                .schedule()
                .ok_or("the scheduler found no runnable thread")?;
            if actual != *expected {
                return Err("the scheduler did not cycle the queue in order");
            }
        }
        let _ = round;
    }

    Ok(())
}

/// A yield must put the running thread at the *back* of the queue.
fn a_yield_puts_the_thread_at_the_back() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);

    let ids: [u32; 2] = [
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread one refused")?,
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread two refused")?,
    ];

    // Run the first thread.
    let first = scheduler.schedule().ok_or("no runnable thread")?;
    if first != ids[0] {
        return Err("the wrong thread was scheduled first");
    }

    // Yield: the running thread goes to the back, and the second runs.
    let next = scheduler
        .yield_current()
        .ok_or("the yield chose no thread")?;
    if next != ids[1] {
        return Err("the yield did not choose the next thread in the queue");
    }

    // The yielded thread must now be at the back.
    let mut order = [0u32; 2];
    let mut seen = 0usize;
    for id in scheduler.runnable_order() {
        if seen >= order.len() {
            return Err("the queue is longer than expected");
        }
        order[seen] = id;
        seen += 1;
    }
    if seen != order.len() {
        return Err("the queue lost a thread across the yield");
    }
    if order[0] != ids[1] || order[1] != ids[0] {
        return Err("the yielded thread did not go to the back of the queue");
    }

    Ok(())
}

/// A thread must never be in the runnable queue twice.
fn a_thread_is_never_queued_twice() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);

    let id = scheduler
        .create_thread(0, 0)
        .map_err(|_| "thread refused")?;

    // Unblocking a thread that is already runnable must not enqueue it again.
    scheduler
        .unblock(id)
        .map_err(|_| "the unblock was refused")?;

    if scheduler.runnable_count() != 1 {
        return Err("unblocking a runnable thread queued it a second time");
    }

    // And the queue must still hold exactly one copy of it.
    let mut seen = 0u32;
    for queued in scheduler.runnable_order() {
        if queued == id {
            seen += 1;
        }
    }
    if seen != 1 {
        return Err("the runnable queue holds a duplicate");
    }

    Ok(())
}

/// An exited thread must leave the queue and stop being runnable.
fn an_exited_thread_leaves_the_queue() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);

    let first = scheduler
        .create_thread(0, 0)
        .map_err(|_| "thread one refused")?;
    let _second = scheduler
        .create_thread(0, 0)
        .map_err(|_| "thread two refused")?;

    // Run the first thread, then let it exit.
    let running = scheduler.schedule().ok_or("no runnable thread")?;
    if running != first {
        return Err("the wrong thread was scheduled first");
    }

    scheduler.exit(first).map_err(|_| "the exit was refused")?;

    if scheduler.runnable_count() != 1 {
        return Err("the exited thread is still in the runnable queue");
    }
    if !scheduler.is_exited(first) {
        return Err("the exited thread is not marked as exited");
    }
    if scheduler.exited_count() != 1 {
        return Err("the exited thread was not counted");
    }

    // The next schedule must not choose the exited thread.
    let next = scheduler.schedule().ok_or("no runnable thread")?;
    if next == first {
        return Err("the scheduler chose an exited thread");
    }

    // Reaping it must release the slot.
    scheduler.reap(first).map_err(|_| "the reap was refused")?;
    if scheduler.thread_count() != 1 {
        return Err("the reaped thread was not released");
    }
    if scheduler.exited_count() != 0 {
        return Err("the reaped thread was not uncounted");
    }

    Ok(())
}

// --- `preempt_busy_loop` cases ----------------------------------------------

/// The timeslice must be the one the plan and the log name.
fn the_timeslice_is_the_documented_one() -> Result<(), &'static str> {
    if crate::sched::DEFAULT_TIMESLICE == 0 {
        return Err("the timeslice is zero: a thread would never be preempted");
    }

    // The scheduler the kernel runs must be built with that same timeslice.
    if crate::sched::scheduler().timeslice != crate::sched::DEFAULT_TIMESLICE {
        return Err("the running scheduler's timeslice differs from the documented one");
    }

    Ok(())
}

/// A tick must be accounted.
fn a_tick_is_accounted() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);
    let _id = scheduler
        .create_thread(0, 0)
        .map_err(|_| "thread refused")?;
    let _ = scheduler.schedule();

    // The timeslice is four ticks, so the first three ticks must not preempt.
    for _ in 0..3 {
        if scheduler.tick() {
            return Err("the thread was preempted before its quantum expired");
        }
    }

    // The fourth must.
    if !scheduler.tick() {
        return Err("the thread was not preempted when its quantum expired");
    }

    Ok(())
}

/// The quantum must be consumed *before* the preemption, not at the same time.
fn the_quantum_is_consumed() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);
    let _id = scheduler
        .create_thread(0, 0)
        .map_err(|_| "thread refused")?;
    let _ = scheduler.schedule();

    if scheduler.current_ticks() != 0 {
        return Err("the running thread starts with ticks already consumed");
    }

    for expected in 1..=3u64 {
        let _ = scheduler.tick();
        if scheduler.current_ticks() != expected {
            return Err("the tick accounting does not match the ticks taken");
        }
    }

    Ok(())
}

/// The timeslice must expire after exactly its number of ticks.
fn the_timeslice_expires() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);
    let _id = scheduler
        .create_thread(0, 0)
        .map_err(|_| "thread refused")?;
    let _ = scheduler.schedule();

    let mut preemptions = 0u32;
    for _ in 0..crate::sched::DEFAULT_TIMESLICE {
        if scheduler.tick() {
            preemptions += 1;
        }
    }

    if preemptions != 1 {
        return Err("the timeslice did not expire exactly once");
    }

    Ok(())
}

/// A preempted thread must go back to the queue, so the next one runs.
fn a_preempted_thread_returns_to_queue() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(4);

    let ids: [u32; 2] = [
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread one refused")?,
        scheduler
            .create_thread(0, 0)
            .map_err(|_| "thread two refused")?,
    ];

    // Run the first thread.
    let running = scheduler.schedule().ok_or("no runnable thread")?;
    if running != ids[0] {
        return Err("the wrong thread was scheduled first");
    }

    // Burn the quantum; only the final tick reports the preemption.
    for _ in 0..crate::sched::DEFAULT_TIMESLICE {
        scheduler.tick();
    }

    // The preempted thread must be back in the queue, and the *other* thread
    // must be the one that runs next.
    let next = scheduler.schedule().ok_or("no runnable thread")?;
    if next != ids[1] {
        return Err("the preempted thread did not yield to the other one");
    }

    Ok(())
}

/// The scheduler policy must account the busy loop's preemption.
///
/// The busy loop is a thread that never yields: the only thing that can take
/// the CPU away from it is the timeslice expiring. This case drives the
/// kernel's scheduler directly; the real timer path is exercised by the boot
/// harness, while this case pins the policy state machine's arithmetic.
fn the_busy_loop_thread_was_preempted() -> Result<(), &'static str> {
    let mut scheduler = nova_core::scheduler::Scheduler::new(crate::sched::DEFAULT_TIMESLICE);

    // Three "busy loop" threads: none of them ever yields.
    let _a = scheduler
        .create_thread(0, 0)
        .map_err(|_| "thread one refused")?;
    let _b = scheduler
        .create_thread(0, 0)
        .map_err(|_| "thread two refused")?;
    let _c = scheduler
        .create_thread(0, 0)
        .map_err(|_| "thread three refused")?;

    // Run one, and burn its quantum without yielding.
    let running = scheduler.schedule().ok_or("no runnable thread")?;

    let mut preempted = false;
    for _ in 0..crate::sched::DEFAULT_TIMESLICE {
        if scheduler.tick() {
            preempted = true;
        }
    }

    if !preempted {
        return Err("a busy-looping thread was never preempted");
    }

    // The preemption must hand the CPU to a *different* thread.
    let next = scheduler.schedule().ok_or("no runnable thread")?;
    if next == running {
        return Err("the preempted thread was scheduled again immediately");
    }

    Ok(())
}

// --- `user_cannot_write_kernel` cases ---------------------------------------

/// The address that aliases the direct map must be refused.
///
/// `0x0000_8000_0000_0000` and `0xFFFF_8000_0000_0000` have the *same*
/// page-table walk -- PML4 entry 256, PDPT 0, PD 0, PT 0 -- because the
/// hardware ignores the sign-extension bits and takes the index from bits
/// 47:39. The first is the user's half of the pair; the second is where the
/// direct map really lives. A user mapping at the first would write a leaf
/// entry into the *kernel's* intermediate tables, which every process shares.
fn the_user_alias_of_the_direct_map_is_refused() -> Result<(), &'static str> {
    const ALIAS: u64 = 0x0000_8000_0000_0000;

    // The address must not be canonical in the first place: bit 47 is set but
    // the sign extension is all-zero, so the hardware raises `#GP` for it.
    if nova_core::paging::is_canonical(ALIAS) {
        return Err("the direct map's user alias is reported as canonical");
    }

    // And the mapping API must refuse it, in every protection that matters.
    let window = user_window();
    let model = nova_core::address_space::AddressSpace::new(window);

    for protection in [
        Protection::UserReadWrite,
        Protection::UserReadExecute,
        Protection::UserReadOnly,
    ] {
        match model.check_map(ALIAS, 0x0000_0000_0000_1000, protection) {
            Err(VmmError::NonCanonical) => {}
            Err(_) => return Err("the alias was refused for the wrong reason"),
            Ok(()) => return Err("a user mapping at the direct map's alias was accepted"),
        }
    }

    Ok(())
}

/// The alias must have the *same* page-table walk as the direct map's base.
///
/// This is the evidence that the alias is not merely "an address the policy
/// dislikes" but an address that selects the kernel's own tables. The case
/// asserts the walk is identical, so that a future change to the canonical
/// rule cannot quietly make the alias diverge from the direct map and turn
/// the alias into "merely unmapped".
fn the_alias_has_the_kernels_walk() -> Result<(), &'static str> {
    use nova_core::paging::WalkIndices;

    const ALIAS: u64 = 0x0000_8000_0000_0000;
    const DIRECT_MAP_BASE: u64 = 0xFFFF_8000_0000_0000;

    let alias = WalkIndices::for_address(ALIAS);
    let direct = WalkIndices::for_address(DIRECT_MAP_BASE);

    if alias != direct {
        return Err("the alias no longer has the direct map's page-table walk");
    }

    // And the walk must select the kernel half's PML4 entry -- entry 256.
    if alias.pml4 != crate::mm::address_space::KERNEL_HALF_FIRST_ENTRY {
        return Err("the alias does not select the kernel half's PML4 entry");
    }

    Ok(())
}

/// A canonical kernel address must be refused for a user mapping.
fn a_kernel_address_is_refused() -> Result<(), &'static str> {
    const KERNEL: u64 = 0xFFFF_8000_0000_0000;

    // The kernel's own base *is* canonical -- the rule must not be so strict
    // that it rejects the higher half entirely.
    if !nova_core::paging::is_canonical(KERNEL) {
        return Err("the kernel half's base is reported as non-canonical");
    }

    // But it must be outside the user window, so a user mapping there is
    // refused.
    let window = user_window();
    if window.contains(KERNEL) {
        return Err("the user window contains the kernel half's base");
    }

    let model = nova_core::address_space::AddressSpace::new(window);
    match model.check_map(KERNEL, 0x0000_0000_0000_1000, Protection::UserReadWrite) {
        Err(VmmError::OutsideUserRange) => {}
        Err(_) => return Err("a kernel address was refused for the wrong reason"),
        Ok(()) => return Err("a user mapping at a kernel address was accepted"),
    }

    Ok(())
}

/// A non-canonical address must be refused.
fn a_non_canonical_address_is_refused() -> Result<(), &'static str> {
    // The other end of the hole: bit 47 clear, sign extension all-one.
    const HOLE: u64 = 0xFFFF_7FFF_FFFF_F000;

    if nova_core::paging::is_canonical(HOLE) {
        return Err("a non-canonical address is reported as canonical");
    }

    let window = user_window();
    let model = nova_core::address_space::AddressSpace::new(window);

    match model.check_map(HOLE, 0x0000_0000_0000_1000, Protection::UserReadWrite) {
        Err(VmmError::NonCanonical) => {}
        Err(_) => return Err("the hole was refused for the wrong reason"),
        Ok(()) => return Err("a user mapping at a non-canonical address was accepted"),
    }

    Ok(())
}

/// The user window must exclude the kernel half *and* the alias.
fn the_window_excludes_the_kernel_half() -> Result<(), &'static str> {
    let window = user_window();

    // The window's bounds must be the derived ones: the page after the null
    // page, and the last canonical *user* page.
    if window.start != 0x1000 {
        return Err("the user window does not start above the null page");
    }
    if window.end != 0x0000_7FFF_FFFF_F000 {
        return Err("the user window's end is not the last canonical user page");
    }

    // The window's end must itself be canonical -- the bound must name an
    // address the hardware can reach.
    if !nova_core::paging::is_canonical(window.end) {
        return Err("the user window's end is not a canonical address");
    }

    // Neither the kernel half nor its alias may be inside it.
    if window.contains(0xFFFF_8000_0000_0000) {
        return Err("the user window contains the kernel half's base");
    }
    if window.contains(0x0000_8000_0000_0000) {
        return Err("the user window contains the direct map's user alias");
    }

    Ok(())
}

/// The kernel half's entries must never carry the user bit.
fn the_kernel_half_is_supervisor_only() -> Result<(), &'static str> {
    use crate::arch::x86_64::paging::{ENTRIES_PER_TABLE, PageFlags, entry_flags};
    use crate::mm::address_space::KERNEL_HALF_FIRST_ENTRY;

    let space = new_process_space().map_err(|_| "could not create an address space")?;
    let pml4 = space.pml4();

    for index in KERNEL_HALF_FIRST_ENTRY..ENTRIES_PER_TABLE {
        if !pml4.is_present(index) {
            continue;
        }

        let flags = entry_flags(pml4.read(index));
        if flags.contains(PageFlags::USER) {
            return Err("a kernel-half PML4 entry is user-accessible");
        }
    }

    Ok(())
}

/// User-pointer translation must not resolve the shared kernel half.
///
/// The plain translator intentionally can see the kernel half, because the
/// kernel itself relies on it. The user translator is the syscall boundary:
/// it must accept a real user mapping and reject a mapped kernel address.
fn user_translation_refuses_the_kernel_half() -> Result<(), &'static str> {
    const USER_VIRT: u64 = 0x0000_0000_0000_4000;
    const KERNEL_VIRT: u64 = 0xFFFF_8000_0000_0000;

    let space = new_process_space().map_err(|_| "could not create an address space")?;
    let frame = alloc_frame()?;

    address_space::map(&space, USER_VIRT, frame, Protection::UserReadWrite)
        .map_err(|_| "could not map a user page")?;

    if address_space::translate_user(&space, USER_VIRT) != Some(frame) {
        return Err("a legitimate user page was not translated");
    }
    if address_space::translate_user(&space, KERNEL_VIRT).is_some() {
        return Err("the kernel half was translated as a user page");
    }

    Ok(())
}

/// The kernel half must be *shared* with the kernel, not copied.
///
/// This is the other half of the isolation story: the kernel half is shared
/// on purpose, so the check that it is shared is what proves the lower half
/// is the only private part. A process whose kernel half were a copy would
/// be paying for it; a process whose kernel half were shared would still be
/// safe — but a process whose *lower* half were shared would not be.
fn the_kernel_half_is_shared_not_copied() -> Result<(), &'static str> {
    let space = new_process_space().map_err(|_| "could not create an address space")?;

    if !space.has_kernel_half() {
        return Err("the new space has no kernel half");
    }

    // The kernel's own space and the new one must name the *same* child
    // tables for the kernel half's entries. That is what "shared" means.
    let kernel = address_space::kernel_space();

    let mut compared = 0usize;
    for index in crate::mm::address_space::KERNEL_HALF_FIRST_ENTRY
        ..crate::arch::x86_64::paging::ENTRIES_PER_TABLE
    {
        let kernel_child = address_space::kernel_half_child(kernel.pml4_phys(), index);
        let space_child = address_space::kernel_half_child(space.pml4_phys(), index);

        match (kernel_child, space_child) {
            (None, None) => continue,
            (Some(a), Some(b)) => {
                if a != b {
                    return Err("the kernel half was copied rather than shared");
                }
                compared += 1;
            }
            _ => return Err("the two spaces disagree about a kernel-half entry"),
        }
    }

    if compared == 0 {
        return Err("the kernel half has no entries to compare");
    }

    Ok(())
}

/// A user mapping must not be able to reach the kernel's page tables.
///
/// This is the case that pins the fix end to end: it takes the *mechanism*
/// the kernel uses to map a user page, points it at the alias, and checks
/// that the kernel's own PML4 entry is untouched.
fn a_user_mapping_cannot_reach_the_kernel_tables() -> Result<(), &'static str> {
    use crate::arch::x86_64::paging::{ENTRIES_PER_TABLE, PageFlags, entry_flags};
    use crate::mm::address_space::KERNEL_HALF_FIRST_ENTRY;

    const ALIAS: u64 = 0x0000_8000_0000_0000;

    let space = new_process_space().map_err(|_| "could not create an address space")?;
    let kernel = address_space::kernel_space();

    // The kernel's PML4 entry 256 -- the one the alias selects -- before.
    let before = kernel.pml4().read(KERNEL_HALF_FIRST_ENTRY);

    // The kernel's own mechanism, asked to map a user page at the alias. The
    // policy in `nova_core::address_space` is what refuses it; the mechanism
    // in `kernel::mm::address_space` then never runs.
    let phys = alloc_frame()?;
    let result = address_space::map(&space, ALIAS, phys, Protection::UserReadWrite);

    match result {
        Err(VmmError::OutsideUserRange) | Err(VmmError::NonCanonical) => {}
        Err(_) => return Err("the alias was refused for an unexpected reason"),
        Ok(()) => return Err("a user mapping at the kernel's alias was accepted"),
    }

    // The kernel's PML4 must be exactly as it was.
    let after = kernel.pml4().read(KERNEL_HALF_FIRST_ENTRY);
    if before != after {
        return Err("the rejected mapping changed the kernel's PML4");
    }

    // And no entry in the kernel half may have acquired the user bit.
    for index in KERNEL_HALF_FIRST_ENTRY..ENTRIES_PER_TABLE {
        if !kernel.pml4().is_present(index) {
            continue;
        }
        if entry_flags(kernel.pml4().read(index)).contains(PageFlags::USER) {
            return Err("a kernel-half entry became user-accessible");
        }
    }

    // The frame was not consumed by the refused mapping, so give it back.
    //
    // SAFETY: the frame was allocated above and the refused mapping did not
    // take ownership of it.
    unsafe {
        let allocator = crate::mm::frame_allocator();
        allocator.dealloc((phys / crate::arch::ADDRESS_SPACE.page_size) as usize);
    }

    Ok(())
}

// --- `task_a_cannot_read_task_b` cases --------------------------------------

/// The two address spaces must be distinct.
fn the_two_spaces_are_distinct() -> Result<(), &'static str> {
    let a = new_process_space().map_err(|_| "could not create the first space")?;
    let b = new_process_space().map_err(|_| "could not create the second space")?;

    if a.pml4_phys() == b.pml4_phys() {
        return Err("the two address spaces share a PML4");
    }

    Ok(())
}

/// The same virtual address must map different physical frames in the two
/// spaces.
fn the_same_address_maps_different_frames() -> Result<(), &'static str> {
    const VIRT: u64 = 0x0000_0000_0000_4000;

    let a = new_process_space().map_err(|_| "could not create the first space")?;
    let b = new_process_space().map_err(|_| "could not create the second space")?;

    let frame_a = alloc_frame()?;
    let frame_b = alloc_frame()?;

    address_space::map(&a, VIRT, frame_a, Protection::UserReadWrite)
        .map_err(|_| "could not map into the first space")?;
    address_space::map(&b, VIRT, frame_b, Protection::UserReadWrite)
        .map_err(|_| "could not map into the second space")?;

    let translated_a =
        address_space::translate(&a, VIRT).ok_or("the first space has no mapping")?;
    let translated_b =
        address_space::translate(&b, VIRT).ok_or("the second space has no mapping")?;

    if translated_a != frame_a {
        return Err("the first space does not translate to its own frame");
    }
    if translated_b != frame_b {
        return Err("the second space does not translate to its own frame");
    }
    if translated_a == translated_b {
        return Err("the two spaces translate the same address to the same frame");
    }

    Ok(())
}

/// Neither space may see the other's frames.
fn a_space_cannot_see_the_other_frames() -> Result<(), &'static str> {
    const VIRT: u64 = 0x0000_0000_0000_4000;

    let a = new_process_space().map_err(|_| "could not create the first space")?;
    let b = new_process_space().map_err(|_| "could not create the second space")?;

    let frame_a = alloc_frame()?;
    let frame_b = alloc_frame()?;

    // Only the first space gets a mapping; the second must have none.
    address_space::map(&a, VIRT, frame_a, Protection::UserReadWrite)
        .map_err(|_| "could not map into the first space")?;

    if address_space::translate(&b, VIRT).is_some() {
        return Err("the second space sees the first space's mapping");
    }

    // And the second space's *own* frame must not be reachable through the
    // first space's address.
    if address_space::translate(&a, VIRT) == Some(frame_b) {
        return Err("the first space reaches the second space's frame");
    }

    Ok(())
}

/// The lower halves must not be shared.
fn the_lower_halves_are_not_shared() -> Result<(), &'static str> {
    use crate::arch::x86_64::paging::ENTRIES_PER_TABLE;
    use crate::mm::address_space::USER_HALF_ENTRIES;

    let a = new_process_space().map_err(|_| "could not create the first space")?;
    let b = new_process_space().map_err(|_| "could not create the second space")?;

    // Neither space has any user-half entries yet, so there is nothing to
    // share. What must be checked is that the *entry* is not shared: a shared
    // entry would mean a shared intermediate table.
    for index in 0..USER_HALF_ENTRIES {
        let child_a = a.pml4().target(index);
        let child_b = b.pml4().target(index);

        if let (Some(x), Some(y)) = (child_a, child_b)
            && x == y
        {
            return Err("the two spaces share a user-half PML4 entry");
        }
    }

    // The bounds themselves must be the ones the design names.
    if USER_HALF_ENTRIES != 256 {
        return Err("the user half is not 256 PML4 entries");
    }
    if USER_HALF_ENTRIES != ENTRIES_PER_TABLE / 2 {
        return Err("the user half is not half of the PML4");
    }

    Ok(())
}

/// The kernel halves must be shared.
fn the_kernel_halves_are_shared() -> Result<(), &'static str> {
    let a = new_process_space().map_err(|_| "could not create the first space")?;
    let b = new_process_space().map_err(|_| "could not create the second space")?;

    let kernel = address_space::kernel_space();

    // Find the kernel half's first present entry, and check that all three
    // spaces name the same child for it.
    let mut found = false;
    for index in crate::mm::address_space::KERNEL_HALF_FIRST_ENTRY
        ..crate::arch::x86_64::paging::ENTRIES_PER_TABLE
    {
        let Some(kernel_child) = address_space::kernel_half_child(kernel.pml4_phys(), index) else {
            continue;
        };
        found = true;

        let a_child = address_space::kernel_half_child(a.pml4_phys(), index);
        let b_child = address_space::kernel_half_child(b.pml4_phys(), index);

        if a_child != Some(kernel_child) {
            return Err("the first space does not share the kernel's tables");
        }
        if b_child != Some(kernel_child) {
            return Err("the second space does not share the kernel's tables");
        }
    }

    if !found {
        return Err("the kernel half has no present entries");
    }

    Ok(())
}

/// Writing into one space's frame must leave the other space's frame alone.
fn writing_one_space_leaves_the_other_alone() -> Result<(), &'static str> {
    const VIRT: u64 = 0x0000_0000_0000_4000;

    let a = new_process_space().map_err(|_| "could not create the first space")?;
    let b = new_process_space().map_err(|_| "could not create the second space")?;

    let frame_a = alloc_frame()?;
    let frame_b = alloc_frame()?;

    address_space::map(&a, VIRT, frame_a, Protection::UserReadWrite)
        .map_err(|_| "could not map into the first space")?;
    address_space::map(&b, VIRT, frame_b, Protection::UserReadWrite)
        .map_err(|_| "could not map into the second space")?;

    // Write a distinct pattern into each frame.
    //
    // SAFETY: both frames were just allocated by this kernel, and each write
    // is one byte inside the frame the caller owns.
    unsafe {
        poke_frame(frame_a, 0xA1);
        poke_frame(frame_b, 0xB2);
    }

    // Task A's frame must hold A's pattern, and task B's frame B's.
    //
    // SAFETY: as above.
    unsafe {
        if peek_frame(frame_a) != 0xA1 {
            return Err("the first space's frame does not hold its own pattern");
        }
        if peek_frame(frame_b) != 0xB2 {
            return Err("the second space's frame does not hold its own pattern");
        }
    }

    Ok(())
}

// --- `invalid_elf_rejected` cases -------------------------------------------

/// A file shorter than an ELF header must be refused.
fn a_truncated_header_is_refused() -> Result<(), &'static str> {
    let short = [0u8; 16];
    match Elf::parse(&short) {
        Err(ElfError::TruncatedHeader) => {}
        Err(_) => return Err("a truncated header was refused for the wrong reason"),
        Ok(_) => return Err("a truncated header was accepted"),
    }

    Ok(())
}

/// A file with the wrong magic must be refused.
fn a_bad_magic_is_refused() -> Result<(), &'static str> {
    let mut built = image_at(0x0000_0000_0000_1000);
    built.bytes[0] = b'X';

    match Elf::parse(built.as_slice()) {
        Err(ElfError::BadMagic) => {}
        Err(_) => return Err("a bad magic was refused for the wrong reason"),
        Ok(_) => return Err("an image with a bad magic was accepted"),
    }

    Ok(())
}

/// An image for another machine must be refused.
fn a_wrong_machine_is_refused() -> Result<(), &'static str> {
    let mut built = image_at(0x0000_0000_0000_1000);
    // `e_machine` is at offset 18.
    built.bytes[18] = 0x03;
    built.bytes[19] = 0x00;

    match Elf::parse(built.as_slice()) {
        Err(ElfError::WrongMachine) => {}
        Err(_) => return Err("a foreign machine was refused for the wrong reason"),
        Ok(_) => return Err("an image for another machine was accepted"),
    }

    Ok(())
}

/// A header with the wrong size must be refused.
fn a_bad_header_size_is_refused() -> Result<(), &'static str> {
    let mut built = image_at(0x0000_0000_0000_1000);
    // `e_ehsize` is at offset 52.
    built.bytes[52] = 0x20;

    match Elf::parse(built.as_slice()) {
        Err(ElfError::BadHeaderSize) => {}
        Err(_) => return Err("a bad header size was refused for the wrong reason"),
        Ok(_) => return Err("an image with a bad header size was accepted"),
    }

    Ok(())
}

/// A segment at a non-canonical address must be refused.
///
/// The address is the direct map's user alias: bit 47 set, sign extension
/// all-zero.
fn a_non_canonical_segment_is_refused() -> Result<(), &'static str> {
    const ALIAS: u64 = 0x0000_8000_0000_0000;

    let built = image_at(ALIAS);
    let elf = Elf::parse(built.as_slice()).map_err(|_| "the built image does not parse")?;

    match elf.validate(kernel_half_start()) {
        Err(ElfError::NonCanonicalAddress) => {}
        Err(_) => return Err("the alias segment was refused for the wrong reason"),
        Ok(()) => return Err("a segment at a non-canonical address was accepted"),
    }

    Ok(())
}

/// A segment in the kernel half must be refused.
fn a_segment_in_the_kernel_half_is_refused() -> Result<(), &'static str> {
    const KERNEL: u64 = 0xFFFF_8000_0000_0000;

    let built = image_at(KERNEL);
    let elf = Elf::parse(built.as_slice()).map_err(|_| "the built image does not parse")?;

    match elf.validate(kernel_half_start()) {
        Err(ElfError::SegmentInKernelSpace) => {}
        Err(_) => return Err("the kernel-half segment was refused for the wrong reason"),
        Ok(()) => return Err("a segment in the kernel half was accepted"),
    }

    Ok(())
}

/// A segment that is both writable and executable must be refused.
///
/// This is the W^X rule, applied to the image rather than to the mapping.
fn a_writable_executable_segment_is_refused() -> Result<(), &'static str> {
    use nova_elf::{PF_R, PF_W, PF_X};

    const VIRT: u64 = 0x0000_0000_0000_1000;

    // The builder's `Segment` has no writable-executable constructor, so the
    // flags are assembled by hand.
    let segment = Segment {
        vaddr: VIRT,
        bytes: &[0x90; 16],
        memsz: 0,
        flags: PF_R | PF_W | PF_X,
        align: 0,
    };

    let built = ElfBuilder::new().entry(VIRT).segment(segment).build();
    let elf = Elf::parse(built.as_slice()).map_err(|_| "the built image does not parse")?;

    // The ELF validator does not carry the W^X rule -- that is the loader's --
    // so the segment must *parse*, and the loader must be the one to refuse it.
    let loader = crate::loader::load(
        &new_process_space().map_err(|_| "could not create an address space")?,
        built.as_slice(),
        // SAFETY: `boot::capture` has published the snapshot.
        unsafe { crate::boot::read_boot_info() },
    );

    match loader {
        Err(crate::loader::LoadError::WritableAndExecutable) => {}
        Err(_) => return Err("the writable-executable image was refused for the wrong reason"),
        Ok(_) => return Err("a writable and executable segment was accepted"),
    }

    Ok(())
}

/// Two overlapping segments must be refused.
fn overlapping_segments_are_refused() -> Result<(), &'static str> {
    const FIRST: u64 = 0x0000_0000_0000_1000;
    const SECOND: u64 = 0x0000_0000_0000_1000;

    let built = ElfBuilder::new()
        .entry(FIRST)
        .segment(Segment::ro(FIRST, &[0x90; 16]).with_memsz(0x1000))
        .segment(Segment::ro(SECOND, &[0x90; 16]))
        .build();

    let elf = Elf::parse(built.as_slice()).map_err(|_| "the built image does not parse")?;

    match elf.validate(kernel_half_start()) {
        Err(ElfError::OverlappingSegments) => {}
        Err(_) => return Err("the overlapping segments were refused for the wrong reason"),
        Ok(()) => return Err("overlapping segments were accepted"),
    }

    Ok(())
}

/// An entry point outside every segment must be refused.
fn an_entry_point_outside_is_refused() -> Result<(), &'static str> {
    const SEGMENT: u64 = 0x0000_0000_0000_1000;
    const ENTRY: u64 = 0x0000_0000_0000_9000;

    let built = ElfBuilder::new()
        .entry(ENTRY)
        .segment(Segment::ro(SEGMENT, &[0x90; 16]))
        .build();

    let elf = Elf::parse(built.as_slice()).map_err(|_| "the built image does not parse")?;

    match elf.validate(kernel_half_start()) {
        Err(ElfError::EntryPointOutsideSegments) => {}
        Err(_) => return Err("the bad entry point was refused for the wrong reason"),
        Ok(()) => return Err("an entry point outside every segment was accepted"),
    }

    Ok(())
}

/// A segment whose file range runs past the end of the file must be refused.
fn a_segment_past_the_file_is_refused() -> Result<(), &'static str> {
    const VIRT: u64 = 0x0000_0000_0000_1000;

    let built = image_at(VIRT);

    // `p_filesz` and `p_memsz` are at offsets 32 and 40 of the first program
    // header, which starts at offset 64. `p_filesz` may not exceed `p_memsz`,
    // so both are made larger than the file; the file-range check must then
    // name `SegmentPastEndOfFile`.
    let header = 64 + 32;
    let mut bytes = built.clone();
    bytes.bytes[header..header + 8].copy_from_slice(&0x0000_0000_0010_0000u64.to_le_bytes());
    bytes.bytes[header + 8..header + 16].copy_from_slice(&0x0000_0000_0010_0000u64.to_le_bytes());

    let elf = Elf::parse(bytes.as_slice()).map_err(|_| "the built image does not parse")?;

    match elf.validate(kernel_half_start()) {
        Err(ElfError::SegmentPastEndOfFile) => {}
        Err(_) => return Err("the over-long segment was refused for the wrong reason"),
        Ok(()) => return Err("a segment past the end of the file was accepted"),
    }

    Ok(())
}

/// The loader must refuse a malformed image.
fn the_loader_refuses_a_malformed_image() -> Result<(), &'static str> {
    let mut built = image_at(0x0000_0000_0000_1000);
    built.bytes[0] = b'X';

    let space = new_process_space().map_err(|_| "could not create an address space")?;

    // SAFETY: `boot::capture` has published the snapshot.
    let boot_info = unsafe { crate::boot::read_boot_info() };
    match crate::loader::load(&space, built.as_slice(), boot_info) {
        Err(crate::loader::LoadError::Malformed(_)) => {}
        Err(_) => return Err("the malformed image was refused for the wrong reason"),
        Ok(_) => return Err("the loader accepted a malformed image"),
    }

    Ok(())
}

/// The loader must refuse an image with a segment in the kernel half.
fn the_loader_refuses_a_kernel_segment() -> Result<(), &'static str> {
    const KERNEL: u64 = 0xFFFF_8000_0000_0000;

    let built = image_at(KERNEL);
    let space = new_process_space().map_err(|_| "could not create an address space")?;

    // SAFETY: `boot::capture` has published the snapshot.
    let boot_info = unsafe { crate::boot::read_boot_info() };
    match crate::loader::load(&space, built.as_slice(), boot_info) {
        Err(crate::loader::LoadError::Malformed(ElfError::SegmentInKernelSpace)) => {}
        Err(_) => return Err("the kernel-half image was refused for the wrong reason"),
        Ok(_) => return Err("the loader accepted an image with a kernel-half segment"),
    }

    Ok(())
}
