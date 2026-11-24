use nova_abi::cap::CapHandle;
use nova_abi::nr;
use nova_ipc::{ReplyToken, message::MAX_INLINE_PAYLOAD};

use crate::syscall;

pub const MAX_PAYLOAD: usize = MAX_INLINE_PAYLOAD;

pub fn endpoint_create() -> Option<CapHandle> {
    syscall::call(nr::ENDPOINT_CREATE.raw(), [0; 6])
        .ok()
        .map(|raw| CapHandle::from_raw(raw as u64))
}

pub fn endpoint_peer_create() -> Option<CapHandle> {
    syscall::call(nr::ENDPOINT_PEER_CREATE.raw(), [0; 6])
        .ok()
        .map(|raw| CapHandle::from_raw(raw as u64))
}

pub fn endpoint_self() -> Option<CapHandle> {
    syscall::call(nr::ENDPOINT_SELF.raw(), [0; 6])
        .ok()
        .map(|raw| CapHandle::from_raw(raw as u64))
}

pub fn send(endpoint: CapHandle, opcode: u64, payload: &[u8]) -> Result<(), syscall::SyscallError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(syscall::SyscallError::ShortWrite);
    }
    syscall::call(
        nr::SEND.raw(),
        [
            endpoint.raw(),
            opcode,
            payload.as_ptr() as u64,
            payload.len() as u64,
            0,
            0,
        ],
    )
    .map(|_| ())
}

pub fn call(
    endpoint: CapHandle,
    opcode: u64,
    payload: &[u8],
) -> Result<[u8; MAX_PAYLOAD], syscall::SyscallError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(syscall::SyscallError::ShortWrite);
    }
    let mut reply = [0; MAX_PAYLOAD];
    syscall::call(
        nr::CALL.raw(),
        [
            endpoint.raw(),
            opcode,
            payload.as_ptr() as u64,
            payload.len() as u64,
            reply.as_mut_ptr() as u64,
            reply.len() as u64,
        ],
    )
    .map(|_| reply)
}

pub fn recv(endpoint: CapHandle) -> Option<(u64, [u8; MAX_PAYLOAD], Option<ReplyToken>)> {
    let mut payload = [0u8; MAX_PAYLOAD];
    let mut token = 0u64;
    let raw = syscall::call(
        nr::RECV.raw(),
        [
            endpoint.raw(),
            0,
            payload.as_mut_ptr() as u64,
            payload.len() as u64,
            &mut token as *mut u64 as u64,
            0,
        ],
    )
    .ok()?;
    let opcode = raw as u64;
    let token = (token != 0).then(|| ReplyToken::from_raw(token));
    Some((opcode, payload, token))
}

pub fn reply(
    endpoint: CapHandle,
    token: ReplyToken,
    payload: &[u8],
) -> Result<(), syscall::SyscallError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(syscall::SyscallError::ShortWrite);
    }
    syscall::call(
        nr::REPLY.raw(),
        [
            endpoint.raw(),
            token.to_raw(),
            payload.as_ptr() as u64,
            payload.len() as u64,
            0,
            0,
        ],
    )
    .map(|_| ())
}
