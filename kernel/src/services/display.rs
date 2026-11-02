//! The kernel-side display service.
//!
//! # What it serves
//!
//! The display service answers one question: what mode is the framebuffer? It
//! does not accept pixels and does not hand out the framebuffer address. The
//! compositor is the only code that draws.
//!
//! # Protocol handling
//!
//! Requests and replies are the fixed `nova-protocols` display layouts, carried
//! in a service request's inline payload. The version is checked first with
//! `negotiate` — an unknown version is refused rather than interpreted under a
//! nearby one.
//!
//! # State
//!
//! The service keeps the mode it learned at bring-up. It also falls back to the
//! driver's live mode when its own copy is absent, which keeps the kernel test
//! suites from depending on a second copy of a boot fact.

use nova_abi::error::ErrorCode;
use nova_ipc::Message;
use nova_protocols::display::{
    DISPLAY_OP_QUERY, DISPLAY_QUERY_REPLY_SIZE, DISPLAY_QUERY_REQUEST_SIZE, DISPLAY_VERSION,
};
use nova_protocols::negotiate;
use nova_protocols::surface::PIXEL_FORMAT_XRGB8888;

use crate::drivers::display::DisplayMode;

/// The display service's own copy of the mode.
#[derive(Clone, Copy)]
pub struct DisplayService {
    mode: Option<DisplayMode>,
}

impl DisplayService {
    /// An empty display service.
    pub const fn new() -> Self {
        DisplayService { mode: None }
    }

    /// Record the mode the driver brought up.
    pub fn set_mode(&mut self, mode: Option<DisplayMode>) {
        self.mode = mode;
    }

    /// The mode to answer with.
    ///
    /// The service's stored copy wins; if it is absent, the driver's live mode
    /// is read. That fallback is deliberate: a test state reset should not make
    /// a hardware fact disappear.
    pub fn mode(&self) -> Option<DisplayMode> {
        self.mode.or_else(|| crate::drivers::display::mode())
    }

    /// Drop the service's copy. The driver's live mode remains available.
    pub fn reset(&mut self) {
        self.mode = None;
    }

    /// Handle one display-protocol request.
    pub fn handle_message(&self, request: &Message) -> Message {
        if request.opcode() != DISPLAY_OP_QUERY {
            return reply_error(ErrorCode::Unsupported);
        }

        let payload = request.payload();
        if payload.len() != DISPLAY_QUERY_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid);
        }

        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(DISPLAY_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }

        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Some(reserved) = read_u32(payload, 8) else {
            return reply_error(ErrorCode::Invalid);
        };
        if flags != 0 || reserved != 0 {
            return reply_error(ErrorCode::Invalid);
        }

        let Some(mode) = self.mode() else {
            return reply_error(ErrorCode::Unsupported);
        };

        let mut bytes = [0u8; DISPLAY_QUERY_REPLY_SIZE];
        write_version(&mut bytes, DISPLAY_VERSION);
        bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&mode.width.to_le_bytes());
        bytes[12..16].copy_from_slice(&mode.height.to_le_bytes());
        bytes[16..20].copy_from_slice(&mode.pitch.to_le_bytes());
        bytes[20..24].copy_from_slice(&PIXEL_FORMAT_XRGB8888.to_le_bytes());

        Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
    }
}

impl Default for DisplayService {
    fn default() -> Self {
        Self::new()
    }
}

/// A reply carrying only a status, padded to the reply layout's size.
fn reply_error(code: ErrorCode) -> Message {
    let mut bytes = [0u8; DISPLAY_QUERY_REPLY_SIZE];
    write_version(&mut bytes, DISPLAY_VERSION);
    bytes[4..8].copy_from_slice(&code.code().to_le_bytes());
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

#[cfg(test)]
mod tests {
    use super::DisplayService;
    use crate::drivers::display::DisplayMode;
    use nova_protocols::display::{DISPLAY_OP_QUERY, DISPLAY_QUERY_REQUEST_SIZE, DISPLAY_VERSION};

    fn mode() -> DisplayMode {
        DisplayMode {
            width: 1280,
            height: 800,
            pitch: 5120,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            base_virtual: 0xFFFF_8000_0000_0000,
        }
    }

    fn request(payload: &[u8]) -> nova_ipc::Message {
        nova_ipc::Message::with_payload(DISPLAY_OP_QUERY, payload).unwrap()
    }

    fn query_payload() -> [u8; DISPLAY_QUERY_REQUEST_SIZE] {
        let mut bytes = [0u8; DISPLAY_QUERY_REQUEST_SIZE];
        bytes[0..2].copy_from_slice(&DISPLAY_VERSION.major.to_le_bytes());
        bytes[2..4].copy_from_slice(&DISPLAY_VERSION.minor.to_le_bytes());
        bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
        bytes
    }

    #[test]
    fn a_query_reports_the_mode() {
        let mut service = DisplayService::new();
        service.set_mode(Some(mode()));
        let reply = service.handle_message(&request(&query_payload()));
        let payload = reply.payload();
        assert_eq!(payload.len(), 24);
        assert_eq!(&payload[0..2], &DISPLAY_VERSION.major.to_le_bytes());
        assert_eq!(&payload[4..8], &0i32.to_le_bytes());
        assert_eq!(&payload[8..12], &1280u32.to_le_bytes());
        assert_eq!(&payload[12..16], &800u32.to_le_bytes());
        assert_eq!(&payload[16..20], &5120u32.to_le_bytes());
        assert_eq!(&payload[20..24], &1u32.to_le_bytes());
    }

    #[test]
    fn an_unknown_opcode_is_refused() {
        let service = DisplayService::new();
        let reply = service.handle_message(&nova_ipc::Message::new(9999));
        assert_eq!(reply.payload().len(), 24);
        assert_eq!(
            &reply.payload()[4..8],
            &nova_abi::error::ErrorCode::Unsupported.code().to_le_bytes()
        );
    }

    #[test]
    fn a_wrong_version_is_refused() {
        let mut service = DisplayService::new();
        service.set_mode(Some(mode()));
        let mut payload = query_payload();
        payload[0..2].copy_from_slice(&2u16.to_le_bytes());
        let reply = service.handle_message(&request(&payload));
        assert_eq!(
            &reply.payload()[4..8],
            &nova_abi::error::ErrorCode::Unsupported.code().to_le_bytes()
        );
    }

    #[test]
    fn a_nonzero_flag_is_refused() {
        let mut service = DisplayService::new();
        service.set_mode(Some(mode()));
        let mut payload = query_payload();
        payload[4..8].copy_from_slice(&1u32.to_le_bytes());
        let reply = service.handle_message(&request(&payload));
        assert_eq!(
            &reply.payload()[4..8],
            &nova_abi::error::ErrorCode::Invalid.code().to_le_bytes()
        );
    }
}
