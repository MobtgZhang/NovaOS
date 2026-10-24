//! The IPC message the kernel copies between endpoints.
//!
//! A message is the header the ABI defines plus the two bounded payloads that
//! travel with it: an inline byte buffer and a small array of capability
//! handles.
//!
//! # Why the payload is inline
//!
//! [`MAX_INLINE_PAYLOAD`] is 64 bytes, and that is small on purpose: a single
//! IPC operation must have a bounded, predictable copy cost, because the
//! kernel has no allocator and every rendezvous slot has to be a fixed size.
//! Larger data travels in a shared memory object whose handle goes in the
//! capability array.
//!
//! # Why the capability array is fixed too
//!
//! [`MAX_CAPS_PER_MSG`] is 4. A capability transfer must be atomic — all of
//! them or none of them — and an atomic transfer over an unbounded set would
//! need an unbounded amount of kernel state to stage. Four keeps the staging
//! area inside two cache lines and inside this structure.

use nova_abi::cap::CapHandle;
use nova_abi::error::ErrorCode;
use nova_abi::ipc::IpcHeader;

/// Maximum inline payload bytes, as fixed by the ABI.
///
/// Re-exported rather than re-declared, so there is exactly one number in the
/// tree: a second copy would be free to drift from the ABI's.
pub const MAX_INLINE_PAYLOAD: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD;

/// Maximum capabilities carried by one message, as fixed by the ABI.
pub const MAX_CAPS_PER_MSG: usize = nova_abi::ipc::MAX_CAPS_PER_MSG;

/// One IPC message, with its bounded inline payload and capability list.
///
/// The header is the ABI's contract and is not reinterpreted here; this type
/// exists so the kernel can hold a whole message in one fixed-size slot with
/// no allocation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Message {
    header: IpcHeader,
    payload: [u8; MAX_INLINE_PAYLOAD],
    caps: [CapHandle; MAX_CAPS_PER_MSG],
}

impl Message {
    /// A message with an opcode and nothing else.
    pub const fn new(opcode: u64) -> Self {
        Self {
            header: IpcHeader::new(opcode),
            payload: [0; MAX_INLINE_PAYLOAD],
            caps: [CapHandle::INVALID; MAX_CAPS_PER_MSG],
        }
    }

    /// A message with an inline payload.
    ///
    /// Fails with [`ErrorCode::Invalid`] when the payload is longer than
    /// [`MAX_INLINE_PAYLOAD`], so an oversized message is refused before any
    /// copy happens.
    pub fn with_payload(opcode: u64, bytes: &[u8]) -> Result<Self, ErrorCode> {
        if bytes.len() > MAX_INLINE_PAYLOAD {
            return Err(ErrorCode::Invalid);
        }
        let mut message = Self::new(opcode);
        message.payload[..bytes.len()].copy_from_slice(bytes);
        message.header.payload_len = bytes.len() as u32;
        Ok(message)
    }

    /// A message carrying capabilities.
    ///
    /// Fails with [`ErrorCode::Invalid`] when more than [`MAX_CAPS_PER_MSG`]
    /// handles are supplied.
    pub fn with_caps(opcode: u64, caps: &[CapHandle]) -> Result<Self, ErrorCode> {
        if caps.len() > MAX_CAPS_PER_MSG {
            return Err(ErrorCode::Invalid);
        }
        let mut message = Self::new(opcode);
        message.set_caps(caps);
        Ok(message)
    }

    /// Replace the capability list.
    ///
    /// The count is kept in the header, which is the single source of truth:
    /// a message whose header and array disagree is not a message.
    pub fn set_caps(&mut self, caps: &[CapHandle]) {
        let count = caps.len().min(MAX_CAPS_PER_MSG);
        self.caps[..count].copy_from_slice(&caps[..count]);
        for slot in self.caps.iter_mut().skip(count) {
            *slot = CapHandle::INVALID;
        }
        self.header.cap_count = count as u32;
    }

    /// Replace one capability in the list.
    ///
    /// Used by the transfer path, which mints a *new* handle in the receiving
    /// table and writes it back here.
    pub fn set_cap(&mut self, index: usize, handle: CapHandle) {
        if index >= MAX_CAPS_PER_MSG {
            return;
        }
        self.caps[index] = handle;
    }

    /// The ABI header.
    pub const fn header(&self) -> &IpcHeader {
        &self.header
    }

    /// The ABI header, mutably.
    ///
    /// The kernel uses this to build a message from bytes that arrived from
    /// user space, where the header may have been forged and therefore has to
    /// be validated afterwards.
    pub const fn header_mut(&mut self) -> &mut IpcHeader {
        &mut self.header
    }

    /// The caller-defined opcode.
    pub const fn opcode(&self) -> u64 {
        self.header.opcode
    }

    /// The inline payload, trimmed to its declared length.
    pub fn payload(&self) -> &[u8] {
        &self.payload[..self.header.payload_len as usize]
    }

    /// The capability handles, trimmed to the header's count.
    pub fn caps(&self) -> &[CapHandle] {
        &self.caps[..self.header.cap_count as usize]
    }

    /// How many capabilities the message carries.
    pub const fn cap_count(&self) -> usize {
        self.header.cap_count as usize
    }

    /// Whether the message satisfies every ABI limit.
    ///
    /// The kernel runs this on a message arriving from user space, where the
    /// header may have been forged.
    pub const fn validate(&self) -> bool {
        self.header.validate()
    }

    /// Whether the message carries capabilities, and therefore needs the
    /// atomic transfer path.
    pub const fn has_caps(&self) -> bool {
        self.header.cap_count > 0
    }
}

impl Default for Message {
    fn default() -> Self {
        Self::new(0)
    }
}
