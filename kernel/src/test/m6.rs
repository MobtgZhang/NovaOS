//! The M6 suites: display, compositor surfaces, focus routing, input, buffer
//! validation, GUI crash isolation, surface lifecycle, and idle scheduling.
//!
//! # What the cases are testing
//!
//! The compositor and input service are kernel-side services. A user process can
//! ask them, but the *display* is only drawn by the kernel. These cases therefore
//! drive the same `CompositorService` and `InputService` the syscall dispatcher
//! uses: create a surface, create and attach a `MemoryObject` buffer, paint the
//! buffer through the direct map, commit, present, and read the framebuffer back.
//!
//! That order matters. A test that only checked the protocol structs would prove
//! the wire layout, not the pixel path. A test that only checked the compositor's
//! state would prove the bookkeeping, not that two windows reach the display.
//!
//! # The named failures
//!
//! - `display_two_windows`: two surfaces must produce two distinct pixel regions.
//! - `move_window`: a moved surface must draw at its new position and not at the
//!   old one.
//! - `focus_follows_click`: focus is decided by the compositor's hit test, not by
//!   the surface named in a client request.
//! - `key_only_focused_surface`: a key to an unfocused surface is refused, and a
//!   refusal does not count as a delivered key.
//! - `invalid_surface_buffer_denied`: a handle without `MAP`, a short buffer,
//!   and a malformed stride are refused before any byte is copied.
//! - `gui_app_crash_isolated`: retiring one window owner removes only that
//!   owner's surface and leaves the other window present.
//! - `window_open_close_100`: one hundred create/attach/destroy cycles must
//!   return every frame to the allocator.
//! - `idle_gui_cpu_no_busy_spin`: a compositor with no pending frame presents
//!   nothing and does not increment its frame counter.
//!
//! # State discipline
//!
//! Every case starts from a fresh test state. The state is a fixed-size static;
//! resetting it clears surface and object metadata but does not itself return
//! frames, so each case destroys the surfaces it creates. The lifecycle case
//! additionally snapshots the frame allocator's free count and checks it is
//! unchanged at the end.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_abi::{ObjectType, nr};
use nova_ipc::Message;
use nova_protocols::input::{
    INPUT_BUTTON_PRIMARY, INPUT_OP_KEY, INPUT_OP_POINTER, INPUT_VERSION, InputKeyRequest,
    InputPointerRequest, KeyState,
};
use nova_protocols::surface::PixelFormat;

use super::harness::TestSuite;
use crate::drivers::display;
use crate::mm::direct_map;
use crate::services::KernelState;
use crate::services::dispatch::{SyscallContext, dispatch_syscall_in};
use crate::services::object::ObjectKind;

/// The M6 suites, in registration order.
///
/// The names are the milestone's exit gate (`reports/NovaOS.md` §20.8) and must
/// not be renamed: the shell harness asserts on exactly these strings.
pub static M6_SUITES: &[TestSuite] = &[
    TestSuite {
        name: "display_two_windows",
        cases: &[
            (
                "two_surfaces_present_distinct_pixels",
                two_surfaces_present_distinct_pixels,
            ),
            (
                "present_counts_one_frame_for_two_windows",
                present_counts_one_frame_for_two_windows,
            ),
        ],
    },
    TestSuite {
        name: "move_window",
        cases: &[
            (
                "a_moved_surface_draws_at_the_new_position",
                a_moved_surface_draws_at_the_new_position,
            ),
            (
                "an_owner_cannot_move_another_task_surface",
                an_owner_cannot_move_another_task_surface,
            ),
        ],
    },
    TestSuite {
        name: "focus_follows_click",
        cases: &[
            (
                "a_primary_click_focuses_the_topmost_surface",
                a_primary_click_focuses_the_topmost_surface,
            ),
            (
                "a_click_outside_every_window_clears_focus",
                a_click_outside_every_window_clears_focus,
            ),
            (
                "a_named_surface_does_not_claim_focus",
                a_named_surface_does_not_claim_focus,
            ),
        ],
    },
    TestSuite {
        name: "key_only_focused_surface",
        cases: &[
            (
                "a_key_to_the_focused_surface_is_accepted",
                a_key_to_the_focused_surface_is_accepted,
            ),
            (
                "a_key_to_an_unfocused_surface_is_denied",
                a_key_to_an_unfocused_surface_is_denied,
            ),
            (
                "a_denied_key_does_not_increment_the_delivered_count",
                a_denied_key_does_not_increment_the_delivered_count,
            ),
        ],
    },
    TestSuite {
        name: "invalid_surface_buffer_denied",
        cases: &[
            (
                "a_handle_without_map_is_denied",
                a_handle_without_map_is_denied,
            ),
            (
                "a_buffer_too_small_for_the_surface_is_refused",
                a_buffer_too_small_for_the_surface_is_refused,
            ),
            (
                "a_stride_not_a_multiple_of_four_is_refused",
                a_stride_not_a_multiple_of_four_is_refused,
            ),
            (
                "two_surfaces_cannot_share_one_buffer",
                two_surfaces_cannot_share_one_buffer,
            ),
        ],
    },
    TestSuite {
        name: "gui_app_crash_isolated",
        cases: &[
            (
                "a_crashed_window_owner_leaves_the_other_window",
                a_crashed_window_owner_leaves_the_other_window,
            ),
            (
                "focus_moves_to_the_surviving_window",
                focus_moves_to_the_surviving_window,
            ),
        ],
    },
    TestSuite {
        name: "window_open_close_100",
        cases: &[
            (
                "one_hundred_cycles_release_every_frame",
                one_hundred_cycles_release_every_frame,
            ),
            (
                "the_surface_and_object_pools_return_to_empty",
                the_surface_and_object_pools_return_to_empty,
            ),
        ],
    },
    TestSuite {
        name: "idle_gui_cpu_no_busy_spin",
        cases: &[
            (
                "an_idle_compositor_presents_nothing",
                an_idle_compositor_presents_nothing,
            ),
            (
                "no_damage_does_not_increment_the_frame_count",
                no_damage_does_not_increment_the_frame_count,
            ),
        ],
    },
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fresh test state.
fn fresh_state() -> crate::sync::SpinLockGuard<'static, crate::services::KernelState> {
    // SAFETY: the suites run one at a time in ring 0 with interrupts masked.
    let state = crate::services::test_state();
    let mut state = state;
    state.reset();
    state
}

/// Create a task and return its slot.
fn new_task(state: &mut KernelState) -> Result<usize, &'static str> {
    let id = state.create_task().ok_or("the task pool was empty")?;
    state.slot_of_task(id).ok_or("the new task has no slot")
}

/// Create a contiguous `MemoryObject` and return its handle, base, and size.
fn create_memory_object(
    state: &mut KernelState,
    slot: usize,
    size: u64,
    rights: Rights,
) -> Result<(CapHandle, u64, u64), &'static str> {
    let context = SyscallContext::new(slot, 0);
    let outcome = dispatch_syscall_in(
        state,
        &context,
        nr::OBJECT_CREATE.number() as u64,
        [
            ObjectType::MemoryObject as u64,
            rights.bits() as u64,
            size,
            0,
            0,
            0,
        ],
    );

    let raw = match outcome {
        crate::services::dispatch::SyscallOutcome::Value(value) => value as u64,
        _ => return Err("the memory object was refused"),
    };
    let handle = CapHandle::from_raw(raw);

    let table = state
        .caps_at(slot)
        .ok_or("the task has no capability table")?;
    let resolved = crate::services::cap::resolve_any(table, &state.objects, handle)
        .map_err(|_| "the memory object did not resolve")?;
    let ObjectKind::MemoryObject { base, size } = resolved.kind else {
        return Err("the object was not a memory object");
    };
    Ok((handle, base, size))
}

/// Create a surface owned by `slot`.
fn create_surface(
    state: &mut KernelState,
    slot: usize,
    width: u32,
    height: u32,
) -> Result<u32, &'static str> {
    let id = state
        .compositor
        .create(width, height, PixelFormat::Xrgb8888)
        .map_err(|_| "the surface was refused")?;
    state
        .compositor
        .set_owner(id, slot)
        .map_err(|_| "the surface owner was refused")?;
    Ok(id)
}

/// Attach a buffer to a surface using the task's capability table.
fn attach_surface(
    state: &mut KernelState,
    slot: usize,
    id: u32,
    buffer: CapHandle,
    stride: u32,
) -> Result<(), &'static str> {
    let parts = state.split();
    let table = &parts.caps[slot];
    let objects = &*parts.objects;
    parts
        .compositor
        .attach(
            id,
            slot,
            table,
            objects,
            stride,
            PixelFormat::Xrgb8888,
            buffer,
        )
        .map_err(|code| match code {
            ErrorCode::Denied => "the buffer attachment was denied",
            ErrorCode::Invalid => "the buffer attachment was invalid",
            _ => "the buffer attachment was refused",
        })
}

/// Destroy a surface, releasing its buffer.
fn destroy_surface(state: &mut KernelState, slot: usize, id: u32) -> Result<(), &'static str> {
    let parts = state.split();
    parts
        .compositor
        .destroy(id, slot, parts.objects)
        .map_err(|_| "the surface destroy was refused")
}

/// Paint a whole surface buffer with one colour.
fn paint(
    base: u64,
    size: u64,
    width: u32,
    height: u32,
    red: u8,
    green: u8,
    blue: u8,
) -> Result<(), &'static str> {
    let Some(mode) = display::mode() else {
        return Err("the framebuffer is not mapped");
    };
    let word = ((red as u32) << mode.red_shift)
        | ((green as u32) << mode.green_shift)
        | ((blue as u32) << mode.blue_shift);
    let bytes = match usize::try_from(size) {
        Ok(bytes) => bytes,
        Err(_) => return Err("the buffer length is too large"),
    };
    let needed = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("the buffer size overflowed")?;
    if needed > bytes {
        return Err("the buffer is shorter than the surface");
    }

    // SAFETY: the buffer is a contiguous kernel-owned frame region and the
    // direct map is live for the kernel's lifetime.
    let buffer = unsafe { direct_map::frame_bytes(base, bytes) };
    for pixel in 0..needed / 4 {
        let offset = pixel * 4;
        buffer[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    Ok(())
}

/// The colour word the framebuffer records.
fn colour_word(red: u8, green: u8, blue: u8) -> Result<u32, &'static str> {
    let Some(mode) = display::mode() else {
        return Err("the framebuffer is not mapped");
    };
    Ok(((red as u32) << mode.red_shift)
        | ((green as u32) << mode.green_shift)
        | ((blue as u32) << mode.blue_shift))
}

/// Read one framebuffer pixel.
fn pixel(x: u32, y: u32) -> Result<u32, &'static str> {
    display::get_pixel(x, y).map_err(|_| "the pixel could not be read")
}

/// The status in a service reply.
fn reply_status(message: &Message) -> Result<(), ErrorCode> {
    let payload = message.payload();
    if payload.len() < 8 {
        return Err(ErrorCode::Invalid);
    }
    let status = i32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    if status == 0 {
        return Ok(());
    }
    match ErrorCode::from_result(-(status as i64)) {
        Some(code) => Err(code),
        None => Err(ErrorCode::Unsupported),
    }
}

/// Encode a `#[repr(C)]` protocol value as bytes.
fn encoded<T>(value: &T) -> &[u8] {
    // SAFETY: each protocol value is `#[repr(C)]` and its asserted wire size
    // is exactly `size_of::<T>()`.
    unsafe {
        core::slice::from_raw_parts(value as *const T as *const u8, core::mem::size_of::<T>())
    }
}

/// Build a pointer message for the input service.
fn pointer_message(surface: u32, x: i32, y: i32, buttons: u32) -> Result<Message, &'static str> {
    let request = InputPointerRequest {
        version: INPUT_VERSION,
        surface,
        x,
        y,
        buttons,
    };
    Message::with_payload(INPUT_OP_POINTER, encoded(&request))
        .map_err(|_| "the pointer request was malformed")
}

/// Build a key message for the input service.
fn key_message(surface: u32, scancode: u16, state: KeyState) -> Result<Message, &'static str> {
    let request = InputKeyRequest {
        version: INPUT_VERSION,
        surface,
        scancode,
        state,
    };
    Message::with_payload(INPUT_OP_KEY, encoded(&request))
        .map_err(|_| "the key request was malformed")
}

// ---------------------------------------------------------------------------
// display_two_windows
// ---------------------------------------------------------------------------

fn two_surfaces_present_distinct_pixels() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let first_slot = new_task(&mut state)?;
    let second_slot = new_task(&mut state)?;

    let first = create_surface(&mut state, first_slot, 80, 60)?;
    let second = create_surface(&mut state, second_slot, 80, 60)?;

    let first_bytes = 80 * 60 * 4;
    let second_bytes = 80 * 60 * 4;
    let (first_buffer, first_base, first_size) = create_memory_object(
        &mut state,
        first_slot,
        first_bytes as u64,
        Rights::MAP | Rights::CONTROL,
    )?;
    let (second_buffer, second_base, second_size) = create_memory_object(
        &mut state,
        second_slot,
        second_bytes as u64,
        Rights::MAP | Rights::CONTROL,
    )?;

    paint(first_base, first_size, 80, 60, 0xFF, 0x00, 0x00)?;
    paint(second_base, second_size, 80, 60, 0x00, 0xFF, 0x00)?;

    attach_surface(&mut state, first_slot, first, first_buffer, 80 * 4)?;
    attach_surface(&mut state, second_slot, second, second_buffer, 80 * 4)?;

    state
        .compositor
        .commit(first, first_slot, 1)
        .map_err(|_| "first commit refused")?;
    state
        .compositor
        .commit(second, second_slot, 1)
        .map_err(|_| "second commit refused")?;

    state
        .compositor
        .present()
        .map_err(|_| "presentation failed")?;

    let red = colour_word(0xFF, 0x00, 0x00)?;
    let green = colour_word(0x00, 0xFF, 0x00)?;
    if pixel(60, 60)? != red {
        return Err("the first window's pixels did not reach the display");
    }
    if pixel(100, 90)? != green {
        return Err("the second window's pixels did not reach the display");
    }
    Ok(())
}

fn present_counts_one_frame_for_two_windows() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let first_slot = new_task(&mut state)?;
    let second_slot = new_task(&mut state)?;

    let first = create_surface(&mut state, first_slot, 40, 40)?;
    let second = create_surface(&mut state, second_slot, 40, 40)?;
    let (first_buffer, first_base, first_size) = create_memory_object(
        &mut state,
        first_slot,
        40 * 40 * 4,
        Rights::MAP | Rights::CONTROL,
    )?;
    let (second_buffer, second_base, second_size) = create_memory_object(
        &mut state,
        second_slot,
        40 * 40 * 4,
        Rights::MAP | Rights::CONTROL,
    )?;
    paint(first_base, first_size, 40, 40, 0x10, 0x20, 0x30)?;
    paint(second_base, second_size, 40, 40, 0x30, 0x20, 0x10)?;

    attach_surface(&mut state, first_slot, first, first_buffer, 40 * 4)?;
    attach_surface(&mut state, second_slot, second, second_buffer, 40 * 4)?;
    state
        .compositor
        .commit(first, first_slot, 1)
        .map_err(|_| "first commit refused")?;
    state
        .compositor
        .commit(second, second_slot, 1)
        .map_err(|_| "second commit refused")?;

    if state.compositor.frame_count() != 0 {
        return Err("a frame was presented before a commit");
    }
    let drawn = state
        .compositor
        .present()
        .map_err(|_| "presentation failed")?;
    if drawn != 2 {
        return Err("presentation did not draw both surfaces");
    }
    if state.compositor.frame_count() != 1 {
        return Err("presentation did not record exactly one frame");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// move_window
// ---------------------------------------------------------------------------

fn a_moved_surface_draws_at_the_new_position() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;
    let id = create_surface(&mut state, slot, 60, 40)?;
    let (buffer, base, size) =
        create_memory_object(&mut state, slot, 60 * 40 * 4, Rights::MAP | Rights::CONTROL)?;
    paint(base, size, 60, 40, 0xFF, 0x80, 0x00)?;
    attach_surface(&mut state, slot, id, buffer, 60 * 4)?;
    state
        .compositor
        .commit(id, slot, 1)
        .map_err(|_| "commit refused")?;
    state
        .compositor
        .present()
        .map_err(|_| "first presentation failed")?;

    let old = pixel(48, 48)?;
    let orange = colour_word(0xFF, 0x80, 0x00)?;
    if old != orange {
        return Err("the surface did not draw at its original position");
    }

    state
        .compositor
        .move_surface(id, slot, 300, 100)
        .map_err(|_| "move refused")?;
    state
        .compositor
        .present()
        .map_err(|_| "second presentation failed")?;

    let background = colour_word(24, 24, 28)?;
    if pixel(48, 48)? != background {
        return Err("the old position was not cleared");
    }
    if pixel(320, 110)? != orange {
        return Err("the surface did not draw at its new position");
    }
    Ok(())
}

fn an_owner_cannot_move_another_task_surface() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let first_slot = new_task(&mut state)?;
    let second_slot = new_task(&mut state)?;
    let id = create_surface(&mut state, first_slot, 40, 40)?;

    match state.compositor.move_surface(id, second_slot, 10, 10) {
        Err(ErrorCode::Denied) => {}
        Err(_) => return Err("the move failed, but not as an ownership denial"),
        Ok(_) => return Err("another task moved a surface it did not own"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// focus_follows_click
// ---------------------------------------------------------------------------

fn a_primary_click_focuses_the_topmost_surface() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let first_slot = new_task(&mut state)?;
    let second_slot = new_task(&mut state)?;
    let first = create_surface(&mut state, first_slot, 70, 50)?;
    let second = create_surface(&mut state, second_slot, 70, 50)?;

    let (first_buffer, _, _) = create_memory_object(
        &mut state,
        first_slot,
        70 * 50 * 4,
        Rights::MAP | Rights::CONTROL,
    )?;
    let (second_buffer, _, _) = create_memory_object(
        &mut state,
        second_slot,
        70 * 50 * 4,
        Rights::MAP | Rights::CONTROL,
    )?;
    attach_surface(&mut state, first_slot, first, first_buffer, 70 * 4)?;
    attach_surface(&mut state, second_slot, second, second_buffer, 70 * 4)?;

    // The second surface is created later, so it starts on top, but focus is
    // still the first surface until a click decides otherwise.
    let message = pointer_message(first, 120, 100, INPUT_BUTTON_PRIMARY)?;
    let reply = state.input.handle_message(&message, &mut state.compositor);
    if reply_status(&reply).is_err() {
        return Err("the input request was refused");
    }

    if state.compositor.focused() != Some(second) {
        return Err("the click did not focus the topmost surface under the pointer");
    }
    if state.compositor.pointer_at(120, 100) != Some(second) {
        return Err("the hit test did not select the surface at the click");
    }
    Ok(())
}

fn a_click_outside_every_window_clears_focus() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;
    let id = create_surface(&mut state, slot, 40, 40)?;
    let (buffer, _, _) =
        create_memory_object(&mut state, slot, 40 * 40 * 4, Rights::MAP | Rights::CONTROL)?;
    attach_surface(&mut state, slot, id, buffer, 40 * 4)?;

    if state.compositor.focused() != Some(id) {
        return Err("the first surface was not focused");
    }

    let message = pointer_message(id, 5, 5, INPUT_BUTTON_PRIMARY)?;
    let reply = state.input.handle_message(&message, &mut state.compositor);
    if reply_status(&reply).is_err() {
        return Err("the input request was refused");
    }

    if state.compositor.focused().is_some() {
        return Err("a click outside every buffer left focus set");
    }
    Ok(())
}

fn a_named_surface_does_not_claim_focus() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let first_slot = new_task(&mut state)?;
    let second_slot = new_task(&mut state)?;
    let first = create_surface(&mut state, first_slot, 70, 50)?;
    let second = create_surface(&mut state, second_slot, 70, 50)?;

    let (first_buffer, _, _) = create_memory_object(
        &mut state,
        first_slot,
        70 * 50 * 4,
        Rights::MAP | Rights::CONTROL,
    )?;
    let (second_buffer, _, _) = create_memory_object(
        &mut state,
        second_slot,
        70 * 50 * 4,
        Rights::MAP | Rights::CONTROL,
    )?;
    attach_surface(&mut state, first_slot, first, first_buffer, 70 * 4)?;
    attach_surface(&mut state, second_slot, second, second_buffer, 70 * 4)?;

    // The client names `first`, but the click lands on `second`. The request's
    // surface field is not trusted for focus.
    let message = pointer_message(first, 120, 100, INPUT_BUTTON_PRIMARY)?;
    let reply = state.input.handle_message(&message, &mut state.compositor);
    if reply_status(&reply).is_err() {
        return Err("the input request was refused");
    }

    if state.compositor.focused() != Some(second) {
        return Err("the named surface won focus without a hit");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// key_only_focused_surface
// ---------------------------------------------------------------------------

fn a_key_to_the_focused_surface_is_accepted() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;
    let id = create_surface(&mut state, slot, 40, 40)?;
    let (buffer, _, _) =
        create_memory_object(&mut state, slot, 40 * 40 * 4, Rights::MAP | Rights::CONTROL)?;
    attach_surface(&mut state, slot, id, buffer, 40 * 4)?;

    let message = key_message(id, 0x1E, KeyState::Pressed)?;
    let reply = state.input.handle_message(&message, &mut state.compositor);
    if reply_status(&reply).is_err() {
        return Err("the input request was refused");
    }

    if state.input.key_count() != 1 {
        return Err("the accepted key was not recorded");
    }
    if state.input.last_key().map(|event| event.surface) != Some(id) {
        return Err("the accepted key was not routed to the focused surface");
    }
    Ok(())
}

fn a_key_to_an_unfocused_surface_is_denied() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let first_slot = new_task(&mut state)?;
    let second_slot = new_task(&mut state)?;
    let first = create_surface(&mut state, first_slot, 40, 40)?;
    let second = create_surface(&mut state, second_slot, 40, 40)?;

    // The first surface is focused by creation. A key naming the second surface
    // is a refusal, not a focus change.
    let message = key_message(second, 0x1E, KeyState::Pressed)?;
    let reply = state.input.handle_message(&message, &mut state.compositor);
    match reply_status(&reply) {
        Err(ErrorCode::Denied) => {}
        Err(_) => return Err("the key failed, but not as a focus denial"),
        Ok(_) => return Err("an unfocused surface accepted a key"),
    }

    if state.compositor.focused() != Some(first) {
        return Err("the denied key moved focus");
    }
    Ok(())
}

fn a_denied_key_does_not_increment_the_delivered_count() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let first_slot = new_task(&mut state)?;
    let second_slot = new_task(&mut state)?;
    let first = create_surface(&mut state, first_slot, 40, 40)?;
    let second = create_surface(&mut state, second_slot, 40, 40)?;

    let focused = key_message(first, 0x1E, KeyState::Pressed)?;
    let reply = state.input.handle_message(&focused, &mut state.compositor);
    if reply_status(&reply).is_err() {
        return Err("the input request was refused");
    }

    let denied = key_message(second, 0x1F, KeyState::Pressed)?;
    let reply = state.input.handle_message(&denied, &mut state.compositor);
    if reply_status(&reply).is_ok() {
        return Err("the second key was accepted");
    }

    if state.input.key_count() != 1 {
        return Err("the denied key was counted as delivered");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// invalid_surface_buffer_denied
// ---------------------------------------------------------------------------

fn a_handle_without_map_is_denied() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;
    let id = create_surface(&mut state, slot, 40, 40)?;
    let (buffer, _, _) = create_memory_object(&mut state, slot, 40 * 40 * 4, Rights::CONTROL)?;

    match attach_surface(&mut state, slot, id, buffer, 40 * 4) {
        Err("the buffer attachment was denied") => {}
        Err(_) => return Err("the attach failed, but not as a rights denial"),
        Ok(_) => return Err("a buffer without MAP was attached"),
    }
    Ok(())
}

fn a_buffer_too_small_for_the_surface_is_refused() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;
    let id = create_surface(&mut state, slot, 80, 60)?;
    let (buffer, _, _) =
        create_memory_object(&mut state, slot, 128, Rights::MAP | Rights::CONTROL)?;

    match attach_surface(&mut state, slot, id, buffer, 80 * 4) {
        Err("the buffer attachment was invalid") => {}
        Err(_) => return Err("the attach failed, but not as an invalid buffer"),
        Ok(_) => return Err("a short buffer was attached"),
    }
    Ok(())
}

fn a_stride_not_a_multiple_of_four_is_refused() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;
    let id = create_surface(&mut state, slot, 40, 40)?;
    let (buffer, _, _) =
        create_memory_object(&mut state, slot, 40 * 40 * 4, Rights::MAP | Rights::CONTROL)?;

    match attach_surface(&mut state, slot, id, buffer, 40 * 4 + 1) {
        Err("the buffer attachment was invalid") => {}
        Err(_) => return Err("the attach failed, but not as an invalid stride"),
        Ok(_) => return Err("a stride that does not end on a pixel was attached"),
    }
    Ok(())
}

fn two_surfaces_cannot_share_one_buffer() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;
    let first = create_surface(&mut state, slot, 40, 40)?;
    let second = create_surface(&mut state, slot, 40, 40)?;
    let (buffer, _, _) =
        create_memory_object(&mut state, slot, 40 * 40 * 4, Rights::MAP | Rights::CONTROL)?;

    attach_surface(&mut state, slot, first, buffer, 40 * 4)?;
    match attach_surface(&mut state, slot, second, buffer, 40 * 4) {
        Err("the buffer attachment was denied") => {}
        Err(_) => return Err("the second attach failed, but not as a denial"),
        Ok(_) => return Err("two surfaces shared one buffer"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// gui_app_crash_isolated
// ---------------------------------------------------------------------------

fn build_two_windows(state: &mut KernelState) -> Result<(usize, u32, usize, u32), &'static str> {
    let first_slot = new_task(&mut *state)?;
    let second_slot = new_task(&mut *state)?;
    let first = create_surface(&mut *state, first_slot, 60, 40)?;
    let second = create_surface(&mut *state, second_slot, 60, 40)?;

    let (first_buffer, first_base, first_size) = create_memory_object(
        &mut *state,
        first_slot,
        60 * 40 * 4,
        Rights::MAP | Rights::CONTROL,
    )?;
    let (second_buffer, second_base, second_size) = create_memory_object(
        &mut *state,
        second_slot,
        60 * 40 * 4,
        Rights::MAP | Rights::CONTROL,
    )?;
    paint(first_base, first_size, 60, 40, 0x20, 0x60, 0xC0)?;
    paint(second_base, second_size, 60, 40, 0xC0, 0x60, 0x20)?;

    attach_surface(&mut *state, first_slot, first, first_buffer, 60 * 4)?;
    attach_surface(&mut *state, second_slot, second, second_buffer, 60 * 4)?;
    state
        .compositor
        .commit(first, first_slot, 1)
        .map_err(|_| "first commit refused")?;
    state
        .compositor
        .commit(second, second_slot, 1)
        .map_err(|_| "second commit refused")?;
    state
        .compositor
        .present()
        .map_err(|_| "presentation failed")?;

    if let Some(task) = state.task_at_slot_mut(first_slot) {
        let name = b"terminal";
        task.service_name[..name.len()].copy_from_slice(name);
        task.service_name_len = name.len();
    }
    if let Some(task) = state.task_at_slot_mut(second_slot) {
        let name = b"editor";
        task.service_name[..name.len()].copy_from_slice(name);
        task.service_name_len = name.len();
    }

    Ok((first_slot, first, second_slot, second))
}

fn a_crashed_window_owner_leaves_the_other_window() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let (first_slot, first, second_slot, second) = build_two_windows(&mut state)?;

    if state.compositor.surface_count() != 2 {
        return Err("the two windows were not both present");
    }
    if state.compositor.surface(first).is_none() || state.compositor.surface(second).is_none() {
        return Err("a window was missing before the crash");
    }

    // Retire the first GUI task, exactly as the fault path does for an unhandled
    // ring-3 fault.
    if !state.retire_task(first_slot) {
        return Err("the first task could not be retired");
    }

    if state.task_at_slot(first_slot).is_some() {
        return Err("the retired task still occupies its slot");
    }
    if state.compositor.surface(first).is_some() {
        return Err("the crashed task's window survived");
    }
    if state.compositor.surface(second).is_none() {
        return Err("the crash removed the other application's window");
    }
    if state.compositor.surface_count() != 1 {
        return Err("the crash left more than one window");
    }
    if state.service_is_alive(b"editor") == false {
        return Err("the surviving window's service identity was lost");
    }
    Ok(())
}

fn focus_moves_to_the_surviving_window() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let (first_slot, first, _second_slot, second) = build_two_windows(&mut state)?;

    // Make the first window the focused one, then crash its owner.
    state
        .compositor
        .set_focus(first)
        .map_err(|_| "focus refused")?;
    if state.compositor.focused() != Some(first) {
        return Err("the first window was not focused");
    }

    if !state.retire_task(first_slot) {
        return Err("the first task could not be retired");
    }

    if state.compositor.focused() != Some(second) {
        return Err("focus did not move to the surviving window");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// window_open_close_100
// ---------------------------------------------------------------------------

fn one_hundred_cycles_release_every_frame() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;

    let allocator = unsafe { crate::mm::frame_allocator() };
    let free_before = allocator.free_count();

    for serial in 0..100u64 {
        let id = create_surface(&mut state, slot, 32, 32)?;
        let (buffer, base, size) =
            create_memory_object(&mut state, slot, 32 * 32 * 4, Rights::MAP | Rights::CONTROL)?;
        paint(base, size, 32, 32, (serial % 255) as u8, 0x20, 0x40)?;
        attach_surface(&mut state, slot, id, buffer, 32 * 4)?;
        state
            .compositor
            .commit(id, slot, serial)
            .map_err(|_| "commit refused")?;
        destroy_surface(&mut state, slot, id)?;
    }

    let free_after = allocator.free_count();
    if free_after != free_before {
        return Err("the open/close cycles leaked frames");
    }
    Ok(())
}

fn the_surface_and_object_pools_return_to_empty() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;

    for serial in 0..100u64 {
        let id = create_surface(&mut state, slot, 24, 24)?;
        let (buffer, _, _) =
            create_memory_object(&mut state, slot, 24 * 24 * 4, Rights::MAP | Rights::CONTROL)?;
        attach_surface(&mut state, slot, id, buffer, 24 * 4)?;
        state
            .compositor
            .commit(id, slot, serial)
            .map_err(|_| "commit refused")?;
        destroy_surface(&mut state, slot, id)?;
    }

    if state.compositor.surface_count() != 0 {
        return Err("surfaces remained after every destroy");
    }
    // The compositor destroys each buffer object when its surface is destroyed.
    // The task's root capability names a TaskFactory object, which surface
    // lifecycle does not touch, so exactly that one object remains.
    if state.objects.count() != 1 {
        return Err("buffer objects remained in the registry");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// idle_gui_cpu_no_busy_spin
// ---------------------------------------------------------------------------

fn an_idle_compositor_presents_nothing() -> Result<(), &'static str> {
    let mut compositor = crate::services::compositor::CompositorService::new();
    if compositor.frame_pending() {
        return Err("an empty compositor reported a pending frame");
    }
    let drawn = compositor.present().map_err(|_| "presentation failed")?;
    if drawn != 0 {
        return Err("an empty compositor drew a frame");
    }
    if compositor.frame_count() != 0 {
        return Err("an empty compositor incremented the frame counter");
    }
    Ok(())
}

fn no_damage_does_not_increment_the_frame_count() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = new_task(&mut state)?;
    let id = create_surface(&mut state, slot, 40, 40)?;
    let (buffer, base, size) =
        create_memory_object(&mut state, slot, 40 * 40 * 4, Rights::MAP | Rights::CONTROL)?;
    paint(base, size, 40, 40, 0x50, 0x80, 0xA0)?;
    attach_surface(&mut state, slot, id, buffer, 40 * 4)?;
    state
        .compositor
        .commit(id, slot, 1)
        .map_err(|_| "commit refused")?;

    let first = state
        .compositor
        .present()
        .map_err(|_| "first presentation failed")?;
    if first != 1 || state.compositor.frame_count() != 1 {
        return Err("the first present did not draw the committed frame");
    }

    if state.compositor.frame_pending() {
        return Err("the compositor still reported pending content");
    }
    let second = state
        .compositor
        .present()
        .map_err(|_| "second presentation failed")?;
    if second != 0 {
        return Err("an idle present drew a frame");
    }
    if state.compositor.frame_count() != 1 {
        return Err("an idle present incremented the frame counter");
    }
    Ok(())
}
