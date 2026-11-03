//! The kernel-side input service.
//!
//! # What it routes
//!
//! A key event is delivered only to the surface the compositor currently owns
//! focus. A pointer event is routed by the compositor's own hit test: the
//! request's `surface` field is deliberately not consulted for focus, because
//! an application naming a surface must not be able to move the desktop's
//! focus. That is §15.3 rule 4 and §15.5's boundary.
//!
//! # Why the service keeps a small history
//!
//! The kernel has no allocator, so the last key and pointer event are stored in
//! fixed structs. The counts are the machine-greppable evidence the M6 suites
//! use; a denied key does not increment the count, which is what makes
//! `key_only_focused_surface` meaningful.
//!
//! # Reading events is not injecting them
//!
//! Two entry points exist and they are not interchangeable. [`InputService::handle_message`]
//! is the *trusted* path: it is what the PS/2 driver and the kernel test
//! suites use to record a hardware event. [`InputService::handle_client_message`]
//! is the ring-3 path reached through `SERVICE_REQUEST`, and it answers only
//! `INPUT_OP_GET` — it reads the queue. A user-space caller that tried to
//! inject a key with `INPUT_OP_KEY` gets `E_UNSUPPORTED`, so an application
//! cannot forge a keystroke into a window it does not own.
//!
//! # Source discipline
//!
//! The real PS/2 driver records events through the same routing method. A
//! user-space caller can ask the service, but the service refuses a key aimed
//! at a surface outside the compositor's focus. It does not accept a client's
//! claim about where the pointer landed.

use nova_abi::error::ErrorCode;
use nova_ipc::Message;
use nova_protocols::input::{
    INPUT_EVENT_KEY, INPUT_EVENT_NONE, INPUT_EVENT_POINTER, INPUT_GET_REPLY_SIZE,
    INPUT_GET_REQUEST_SIZE, INPUT_KEY_REQUEST_SIZE, INPUT_OP_GET, INPUT_OP_KEY, INPUT_OP_POINTER,
    INPUT_POINTER_REQUEST_SIZE, INPUT_REPLY_SIZE, INPUT_VERSION, KeyState,
};
use nova_protocols::negotiate;

use crate::services::compositor::CompositorService;

/// How many events may wait for their window at once.
///
/// The kernel has no allocator, so the queue is a fixed array. It is a ring of
/// whole events rather than a per-surface table: one surface that is not
/// polling must not cost the others their input, and a full queue drops the
/// oldest event for *that* surface rather than blocking the driver.
pub const MAX_QUEUED_EVENTS: usize = 32;

/// One event waiting to be read by the window it belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct QueuedEvent {
    /// The surface the event is addressed to.
    surface: u32,
    /// `INPUT_EVENT_KEY` or `INPUT_EVENT_POINTER`.
    kind: u32,
    /// Key state, zero for a pointer event.
    state: u16,
    /// Scancode, zero for a pointer event.
    scancode: u16,
    /// Pointer x, zero for a key event.
    x: i32,
    /// Pointer y, zero for a key event.
    y: i32,
    /// Button mask, zero for a key event.
    buttons: u32,
}

impl QueuedEvent {
    /// One key event for `surface`.
    const fn key(surface: u32, scancode: u16, state: KeyState) -> Self {
        Self {
            surface,
            kind: INPUT_EVENT_KEY,
            state: state.as_u16(),
            scancode,
            x: 0,
            y: 0,
            buttons: 0,
        }
    }

    /// One pointer event for `surface`.
    const fn pointer(surface: u32, x: i32, y: i32, buttons: u32) -> Self {
        Self {
            surface,
            kind: INPUT_EVENT_POINTER,
            state: 0,
            scancode: 0,
            x,
            y,
            buttons,
        }
    }
}

/// One key event the service accepted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyEvent {
    /// The focused surface that received the key.
    pub surface: u32,
    /// The scancode from the device.
    pub scancode: u16,
    /// Whether the key was pressed or released.
    pub state: KeyState,
}

/// One pointer event the service accepted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PointerEvent {
    /// The surface the compositor found under the pointer, if any.
    pub hit: Option<u32>,
    /// The focus after the event.
    pub focused: Option<u32>,
    /// X coordinate, signed.
    pub x: i64,
    /// Y coordinate, signed.
    pub y: i64,
    /// Button mask.
    pub buttons: u32,
}

/// The input service's state.
#[derive(Clone, Copy, Debug)]
pub struct InputService {
    last_key: Option<KeyEvent>,
    last_pointer: Option<PointerEvent>,
    key_count: u64,
    pointer_count: u64,
    /// Events waiting for the window they belong to, oldest first.
    queue: [Option<QueuedEvent>; MAX_QUEUED_EVENTS],
    /// How many entries of `queue` are in use.
    queue_len: usize,
}

impl InputService {
    /// An empty input service.
    pub const fn new() -> Self {
        Self {
            last_key: None,
            last_pointer: None,
            key_count: 0,
            pointer_count: 0,
            queue: [None; MAX_QUEUED_EVENTS],
            queue_len: 0,
        }
    }

    /// Drop the recorded events.
    pub fn reset(&mut self) {
        self.last_key = None;
        self.last_pointer = None;
        self.key_count = 0;
        self.pointer_count = 0;
        self.queue = [None; MAX_QUEUED_EVENTS];
        self.queue_len = 0;
    }

    /// The last accepted key event.
    pub fn last_key(&self) -> Option<KeyEvent> {
        self.last_key
    }

    /// The last accepted pointer event.
    pub fn last_pointer(&self) -> Option<PointerEvent> {
        self.last_pointer
    }

    /// How many key events were accepted.
    pub fn key_count(&self) -> u64 {
        self.key_count
    }

    /// How many pointer events were accepted.
    pub fn pointer_count(&self) -> u64 {
        self.pointer_count
    }

    /// Handle one input-protocol request from a ring-3 caller.
    ///
    /// This is the path `SERVICE_REQUEST` takes. It answers `INPUT_OP_GET`
    /// only: a client reads the events its own surface was sent, and nothing
    /// else. Injection stays on [`InputService::handle_message`], which the
    /// driver and the kernel suites call from ring 0.
    pub fn handle_client_message(
        &mut self,
        request: &Message,
        compositor: &CompositorService,
        task_slot: usize,
    ) -> Message {
        match request.opcode() {
            INPUT_OP_GET => self.handle_get(request.payload(), compositor, task_slot),
            _ => reply_error(ErrorCode::Unsupported),
        }
    }

    /// Read the oldest event addressed to a surface the caller owns.
    fn handle_get(
        &mut self,
        payload: &[u8],
        compositor: &CompositorService,
        task_slot: usize,
    ) -> Message {
        if payload.len() != INPUT_GET_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(INPUT_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        let Some(surface) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid);
        };

        // Ownership, not focus: a window may read its own queue whether or not
        // it is focused, and it may not read another window's queue at all.
        match compositor.surface(surface) {
            None => return reply_error(ErrorCode::BadHandle),
            Some(owned) if owned.owner != task_slot => return reply_error(ErrorCode::Denied),
            Some(_) => {}
        }

        match self.pop_event(surface) {
            Some(event) => reply_event(&event),
            None => reply_event_empty(surface),
        }
    }

    /// Handle one input-protocol request from a trusted producer.
    pub fn handle_message(
        &mut self,
        request: &Message,
        compositor: &mut CompositorService,
    ) -> Message {
        match request.opcode() {
            INPUT_OP_KEY => self.handle_key(request.payload(), compositor),
            INPUT_OP_POINTER => self.handle_pointer(request.payload(), compositor),
            _ => reply_error(ErrorCode::Unsupported),
        }
    }

    /// Append `event` to the queue, dropping the oldest entry when full.
    fn push_event(&mut self, event: QueuedEvent) {
        if self.queue_len == MAX_QUEUED_EVENTS {
            // Shift down by one: the oldest event is the one that goes, and
            // every later event keeps its place. A window that stopped
            // polling must not grow the kernel's memory, and it must not be
            // able to stall the device by never reading.
            for index in 1..MAX_QUEUED_EVENTS {
                self.queue[index - 1] = self.queue[index];
            }
            self.queue[MAX_QUEUED_EVENTS - 1] = None;
            self.queue_len -= 1;
        }
        self.queue[self.queue_len] = Some(event);
        self.queue_len += 1;
    }

    /// Remove the oldest event addressed to `surface`.
    fn pop_event(&mut self, surface: u32) -> Option<QueuedEvent> {
        let position = (0..self.queue_len).find(|index| {
            self.queue[*index]
                .as_ref()
                .is_some_and(|event| event.surface == surface)
        })?;
        let taken = self.queue[position];
        for index in position + 1..self.queue_len {
            self.queue[index - 1] = self.queue[index];
        }
        self.queue_len -= 1;
        self.queue[self.queue_len] = None;
        taken
    }

    /// How many events are waiting.
    pub fn queued_events(&self) -> usize {
        self.queue_len
    }

    fn handle_key(&mut self, payload: &[u8], compositor: &CompositorService) -> Message {
        if payload.len() != INPUT_KEY_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(INPUT_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        let Some(surface) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Some(scancode) = read_u16(payload, 8) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Some(state_word) = read_u16(payload, 10) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Some(state) = KeyState::from_u16(state_word) else {
            return reply_error(ErrorCode::Invalid);
        };

        // A key is delivered only to the focused surface. Naming another
        // surface does not move focus; it is a refusal.
        if compositor.focused() != Some(surface) {
            return reply_error(ErrorCode::Denied);
        }
        if compositor.surface(surface).is_none() {
            return reply_error(ErrorCode::BadHandle);
        }

        self.record_key(surface, scancode, state);
        reply_status(0)
    }

    /// Record one hardware key for the surface the compositor has focused.
    ///
    /// The PS/2 driver calls this directly; it does not carry a client's
    /// surface name, because the device has no idea which window is focused.
    pub fn route_key(
        &mut self,
        scancode: u16,
        state: KeyState,
        compositor: &CompositorService,
    ) -> Result<(), ErrorCode> {
        let Some(surface) = compositor.focused() else {
            return Err(ErrorCode::Denied);
        };
        if compositor.surface(surface).is_none() {
            return Err(ErrorCode::BadHandle);
        }
        self.record_key(surface, scancode, state);
        Ok(())
    }

    fn record_key(&mut self, surface: u32, scancode: u16, state: KeyState) {
        self.last_key = Some(KeyEvent {
            surface,
            scancode,
            state,
        });
        self.key_count = self.key_count.saturating_add(1);
        self.push_event(QueuedEvent::key(surface, scancode, state));
    }

    fn handle_pointer(&mut self, payload: &[u8], compositor: &mut CompositorService) -> Message {
        if payload.len() != INPUT_POINTER_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(INPUT_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        // The request names a surface, but focus is the compositor's decision.
        let _surface = read_u32(payload, 4);
        let Some(x) = read_i32(payload, 8) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Some(y) = read_i32(payload, 12) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Some(buttons) = read_u32(payload, 16) else {
            return reply_error(ErrorCode::Invalid);
        };
        if buttons & !0x7 != 0 {
            return reply_error(ErrorCode::Invalid);
        }

        match self.route_pointer(x as i64, y as i64, buttons, compositor) {
            Ok(_) => reply_status(0),
            Err(code) => reply_error(code),
        }
    }

    /// Route one hardware pointer event through the compositor's hit test.
    ///
    /// The event is queued for the surface actually under the pointer, with
    /// coordinates made *local* to that surface: a button at (10, 40) inside
    /// its window is at (10, 40) wherever the window sits on the display, and
    /// the application never has to know the desktop's layout to hit-test its
    /// own widgets.
    pub fn route_pointer(
        &mut self,
        x: i64,
        y: i64,
        buttons: u32,
        compositor: &mut CompositorService,
    ) -> Result<Option<u32>, ErrorCode> {
        if buttons & !0x7 != 0 {
            return Err(ErrorCode::Invalid);
        }

        let hit = compositor.pointer_event(x, y, buttons);
        self.last_pointer = Some(PointerEvent {
            hit,
            focused: compositor.focused(),
            x,
            y,
            buttons,
        });
        self.pointer_count = self.pointer_count.saturating_add(1);

        if let Some(target) = compositor.pointer_at(x, y) {
            if let Some(surface) = compositor.surface(target) {
                let local_x = clamp_i32(x.saturating_sub(surface.x));
                let local_y = clamp_i32(y.saturating_sub(surface.y));
                self.push_event(QueuedEvent::pointer(target, local_x, local_y, buttons));
            }
        }
        Ok(hit)
    }
}

/// Narrow an `i64` coordinate to the protocol's `i32` without wrapping.
fn clamp_i32(value: i64) -> i32 {
    if value < i32::MIN as i64 {
        i32::MIN
    } else if value > i32::MAX as i64 {
        i32::MAX
    } else {
        value as i32
    }
}

impl Default for InputService {
    fn default() -> Self {
        Self::new()
    }
}

/// A reply carrying only a status.
fn reply_status(status: i32) -> Message {
    let mut bytes = [0u8; INPUT_REPLY_SIZE];
    write_version(&mut bytes, INPUT_VERSION);
    bytes[4..8].copy_from_slice(&status.to_le_bytes());
    bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
    Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
}

fn reply_error(code: ErrorCode) -> Message {
    reply_status(code.code())
}

/// A poll reply carrying one queued event.
fn reply_event(event: &QueuedEvent) -> Message {
    let mut bytes = [0u8; INPUT_GET_REPLY_SIZE];
    write_version(&mut bytes, INPUT_VERSION);
    bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
    bytes[8..12].copy_from_slice(&event.kind.to_le_bytes());
    bytes[12..14].copy_from_slice(&event.state.to_le_bytes());
    bytes[14..16].copy_from_slice(&event.scancode.to_le_bytes());
    bytes[16..20].copy_from_slice(&event.x.to_le_bytes());
    bytes[20..24].copy_from_slice(&event.y.to_le_bytes());
    bytes[24..28].copy_from_slice(&event.buttons.to_le_bytes());
    bytes[28..32].copy_from_slice(&event.surface.to_le_bytes());
    Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
}

/// A poll reply that found nothing to read: not an error, an empty queue.
fn reply_event_empty(surface: u32) -> Message {
    let mut bytes = [0u8; INPUT_GET_REPLY_SIZE];
    write_version(&mut bytes, INPUT_VERSION);
    bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
    bytes[8..12].copy_from_slice(&INPUT_EVENT_NONE.to_le_bytes());
    bytes[28..32].copy_from_slice(&surface.to_le_bytes());
    Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
}

fn write_version(bytes: &mut [u8], version: nova_protocols::ProtocolVersion) {
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
}

fn read_version(payload: &[u8], offset: usize) -> Option<nova_protocols::ProtocolVersion> {
    let major = read_u16(payload, offset)?;
    let minor = read_u16(payload, offset.checked_add(2)?)?;
    Some(nova_protocols::ProtocolVersion::new(major, minor))
}

fn read_u16(payload: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let bytes = payload.get(offset..end)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let bytes = payload.get(offset..end)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_i32(payload: &[u8], offset: usize) -> Option<i32> {
    let end = offset.checked_add(4)?;
    let bytes = payload.get(offset..end)?;
    Some(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::InputService;
    use crate::services::compositor::CompositorService;
    use nova_ipc::Message;
    use nova_protocols::input::{
        INPUT_KEY_REQUEST_SIZE, INPUT_OP_KEY, INPUT_OP_POINTER, INPUT_POINTER_REQUEST_SIZE,
        INPUT_VERSION,
    };
    use nova_protocols::surface::PixelFormat;

    fn key_payload(surface: u32, scancode: u16) -> [u8; INPUT_KEY_REQUEST_SIZE] {
        let mut bytes = [0u8; INPUT_KEY_REQUEST_SIZE];
        bytes[0..2].copy_from_slice(&INPUT_VERSION.major.to_le_bytes());
        bytes[2..4].copy_from_slice(&INPUT_VERSION.minor.to_le_bytes());
        bytes[4..8].copy_from_slice(&surface.to_le_bytes());
        bytes[8..10].copy_from_slice(&scancode.to_le_bytes());
        bytes[10..12].copy_from_slice(&1u16.to_le_bytes());
        bytes
    }

    fn pointer_payload(surface: u32, x: i32, y: i32) -> [u8; INPUT_POINTER_REQUEST_SIZE] {
        let mut bytes = [0u8; INPUT_POINTER_REQUEST_SIZE];
        bytes[0..2].copy_from_slice(&INPUT_VERSION.major.to_le_bytes());
        bytes[2..4].copy_from_slice(&INPUT_VERSION.minor.to_le_bytes());
        bytes[4..8].copy_from_slice(&surface.to_le_bytes());
        bytes[8..12].copy_from_slice(&x.to_le_bytes());
        bytes[12..16].copy_from_slice(&y.to_le_bytes());
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
        bytes
    }

    #[test]
    fn a_key_is_refused_outside_the_focused_surface() {
        let mut compositor = CompositorService::new();
        let first = compositor.create(10, 10, PixelFormat::Xrgb8888).unwrap();
        let second = compositor.create(10, 10, PixelFormat::Xrgb8888).unwrap();

        let mut input = InputService::new();
        let refused = input.handle_message(
            &Message::with_payload(INPUT_OP_KEY, &key_payload(first, 0x1E)).unwrap(),
            &mut compositor,
        );
        assert_eq!(input.key_count(), 0);
        assert_eq!(
            &refused.payload()[4..8],
            &nova_abi::error::ErrorCode::Denied.code().to_le_bytes()
        );

        compositor.set_focus(first).unwrap();
        let accepted = input.handle_message(
            &Message::with_payload(INPUT_OP_KEY, &key_payload(first, 0x1E)).unwrap(),
            &mut compositor,
        );
        assert_eq!(input.key_count(), 1);
        assert_eq!(input.last_key().unwrap().surface, first);
        assert_eq!(&accepted.payload()[4..8], &0i32.to_le_bytes());
    }

    #[test]
    fn a_pointer_click_follows_the_topmost_surface() {
        let mut compositor = CompositorService::new();
        let first = compositor.create(100, 100, PixelFormat::Xrgb8888).unwrap();
        let second = compositor.create(100, 100, PixelFormat::Xrgb8888).unwrap();
        compositor.set_focus(first).unwrap();

        let mut input = InputService::new();
        let _ = input.handle_message(
            &Message::with_payload(INPUT_OP_POINTER, &pointer_payload(first, 100, 90)).unwrap(),
            &mut compositor,
        );
        assert_eq!(compositor.focused(), Some(second));
        assert_eq!(input.last_pointer().unwrap().hit, Some(second));
    }
}
