//! The fixed-layout IPC message header.
//!
//! # Layout is the contract
//!
//! [`IpcHeader`] is `#[repr(C)]` and exactly 24 bytes. Both sides compute nothing:
//! the kernel copies these bytes, the sender fills them in, the receiver reads
//! them. A field that moved, grew, or changed type would be interpreted as a
//! different message by the other side — not a compile error, because they are
//! separate binaries. Hence the `const _: () = assert!(size_of::<IpcHeader>() == 24)`
//! below, which makes an accidental size change a *build* failure.
//!
//! # What `reserved` is for
//!
//! The 8 reserved bytes are zero. They exist so a future revision can add a field
//! without moving the existing ones, and an old receiver skips them by name. A
//! non-zero value is a protocol violation, not an extension point: the kernel
//! rejects it, because silently ignoring a byte the sender thought was meaningful
//! is how two sides end up disagreeing about a message.

/// Maximum payload bytes carried inside a message.
///
/// Large data does not go in the message: it goes in a shared
/// [`MemoryObject`](cap::Rights::MAP) whose handle travels in `cap_count`. The
/// limit exists so a single IPC operation has a bounded, predictable copy cost and
/// so a fixed-size kernel message queue slot is sufficient.
pub const MAX_INLINE_PAYLOAD: usize = 64;

/// Maximum capability handles carried in one message.
///
/// Four is chosen so that a message stays inside two cache lines and so that
/// transferring a capability — which must be atomic — cannot require an unbounded
/// allocation in the kernel.
pub const MAX_CAPS_PER_MSG: usize = 4;

/// Size of the fixed header, in bytes.
pub const IPC_HEADER_SIZE: usize = 24;

/// The header every IPC message begins with.
///
/// `#[repr(C)]` is load-bearing: it makes the field order the memory order, which
/// is the only thing two independently-compiled binaries can agree on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct IpcHeader {
    /// Number of meaningful bytes in `payload`, at most [`MAX_INLINE_PAYLOAD`].
    pub payload_len: u32,
    /// Number of valid handles in `caps`, at most [`MAX_CAPS_PER_MSG`].
    pub cap_count: u32,
    /// Caller-defined opcode. Its meaning belongs to the protocol in
    /// `nova-protocols`, not to the kernel.
    pub opcode: u64,
    /// Must be zero. Non-zero is rejected as [`Invalid`](error::ErrorCode::Invalid).
    pub reserved: u64,
}

impl IpcHeader {
    /// A header for an opcode with no payload and no capabilities.
    pub const fn new(opcode: u64) -> Self {
        Self {
            payload_len: 0,
            cap_count: 0,
            opcode,
            reserved: 0,
        }
    }

    /// Build a header with a payload length.
    ///
    /// Returns `None` if the length exceeds [`MAX_INLINE_PAYLOAD`]. Callers use
    /// `ok_or(ErrorCode::Invalid)` so an oversized message is refused before any
    /// copy happens.
    pub const fn with_payload(opcode: u64, payload_len: usize) -> Option<Self> {
        if payload_len > MAX_INLINE_PAYLOAD {
            return None;
        }
        Some(Self {
            payload_len: payload_len as u32,
            cap_count: 0,
            opcode,
            reserved: 0,
        })
    }

    /// Build a header carrying capabilities.
    ///
    /// Returns `None` if the count exceeds [`MAX_CAPS_PER_MSG`].
    pub const fn with_caps(opcode: u64, cap_count: usize) -> Option<Self> {
        if cap_count > MAX_CAPS_PER_MSG {
            return None;
        }
        Some(Self {
            payload_len: 0,
            cap_count: cap_count as u32,
            opcode,
            reserved: 0,
        })
    }

    /// Whether every field satisfies the ABI limits.
    ///
    /// This is the check the kernel runs on a message arriving from user space.
    /// It is a method rather than inline checks so that the sender, the receiver,
    /// and the tests all apply *identical* validation.
    pub const fn validate(&self) -> bool {
        self.payload_len as usize <= MAX_INLINE_PAYLOAD
            && self.cap_count as usize <= MAX_CAPS_PER_MSG
            && self.reserved == 0
    }
}

// The size is a cross-binary contract. If this fails to compile, the header
// layout changed and every already-built NovaOS binary is now incompatible.
const _: () = assert!(size_of::<IpcHeader>() == IPC_HEADER_SIZE);

// Likewise the field offsets: a reordered struct is still 24 bytes, so the size
// assertion alone would not catch it. `offset_of!` is stable in a `const`
// context, which is the only place these can be checked.
const _: () = assert!(core::mem::offset_of!(IpcHeader, payload_len) == 0);
const _: () = assert!(core::mem::offset_of!(IpcHeader, cap_count) == 4);
const _: () = assert!(core::mem::offset_of!(IpcHeader, opcode) == 8);
const _: () = assert!(core::mem::offset_of!(IpcHeader, reserved) == 16);

#[cfg(test)]
mod tests {
    use super::{IpcHeader, IPC_HEADER_SIZE, MAX_CAPS_PER_MSG, MAX_INLINE_PAYLOAD};

    #[test]
    fn the_header_is_exactly_the_documented_size() {
        assert_eq!(size_of::<IpcHeader>(), IPC_HEADER_SIZE);
        assert_eq!(IPC_HEADER_SIZE, 24);
    }

    #[test]
    fn field_offsets_are_where_the_other_side_expects_them() {
        assert_eq!(core::mem::offset_of!(IpcHeader, payload_len), 0);
        assert_eq!(core::mem::offset_of!(IpcHeader, cap_count), 4);
        assert_eq!(core::mem::offset_of!(IpcHeader, opcode), 8);
        assert_eq!(core::mem::offset_of!(IpcHeader, reserved), 16);
    }

    #[test]
    fn an_empty_header_validates() {
        let header = IpcHeader::new(0x1234);
        assert!(header.validate());
        assert_eq!(header.opcode, 0x1234);
        assert_eq!(header.payload_len, 0);
        assert_eq!(header.cap_count, 0);
    }

    #[test]
    fn the_limits_are_accepted_and_one_more_is_refused() {
        // The boundary is inclusive on the Rust side and exclusive in the
        // constructor, which is where the off-by-one would otherwise hide.
        assert!(IpcHeader::with_payload(1, MAX_INLINE_PAYLOAD).is_some());
        assert!(IpcHeader::with_payload(1, MAX_INLINE_PAYLOAD + 1).is_none());

        assert!(IpcHeader::with_caps(1, MAX_CAPS_PER_MSG).is_some());
        assert!(IpcHeader::with_caps(1, MAX_CAPS_PER_MSG + 1).is_none());
    }

    #[test]
    fn a_non_zero_reserved_field_is_a_protocol_violation() {
        // A sender that sets it believes a field exists that this kernel does not
        // implement; accepting the message would silently drop it.
        let mut header = IpcHeader::new(1);
        header.reserved = 1;
        assert!(!header.validate());
    }

    #[test]
    fn validation_catches_a_forged_oversized_header() {
        // The kernel validates headers that arrive as raw bytes, so it must
        // reject what the constructors would never build.
        let mut header = IpcHeader::new(1);
        header.payload_len = u32::MAX;
        assert!(!header.validate());

        let mut header = IpcHeader::new(1);
        header.cap_count = u32::MAX;
        assert!(!header.validate());
    }

    #[test]
    fn a_default_header_is_valid_and_means_opcode_zero() {
        let header = IpcHeader::default();
        assert!(header.validate());
        assert_eq!(header.opcode, 0);
    }

    #[test]
    fn the_header_is_plain_old_data() {
        // `Copy` plus `#[repr(C)]` means the sender can build the header into a
        // shared buffer with a single 24-byte store, which is what keeps the
        // IPC fast path free of per-field writes.
        let header = IpcHeader::new(0xDEAD_BEEF);
        let copy = header;
        assert_eq!(copy, header);
    }
}