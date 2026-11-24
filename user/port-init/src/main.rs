#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(target_os = "none")]
use nova_abi::{ABI_VERSION, MAX_INLINE_PAYLOAD, Syscall, error::ErrorCode, nr};

#[cfg(target_os = "none")]
static mut DATA: u64 = 0x4e4f_5641;
#[cfg(target_os = "none")]
static mut BSS: u64 = 0;

#[cfg(all(target_os = "none", not(target_arch = "x86_64")))]
#[unsafe(link_section = ".rodata.page_probe")]
static READ_ONLY: u32 = 0x4e4f_5641;

#[cfg(all(target_os = "none", not(target_arch = "x86_64")))]
#[repr(align(4096))]
struct PageProbe([u8; 4096 + MAX_INLINE_PAYLOAD]);

#[cfg(all(target_os = "none", not(target_arch = "x86_64")))]
static mut MULTIPAGE: PageProbe = PageProbe([0xa5; 4096 + MAX_INLINE_PAYLOAD]);

#[cfg(all(target_os = "none", not(target_arch = "x86_64")))]
fn split_buffer() -> *mut u8 {
    unsafe { (&raw mut MULTIPAGE.0).cast::<u8>().add(4092) }
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text._start")]
extern "C" fn _start(role: u64, endpoint: u64) -> ! {
    const INIT: u64 = 16;
    const ECHO_SERVER: u64 = 17;
    let slot = role & 0xff;
    if slot == 3 {
        loop {
            core::hint::spin_loop();
        }
    }
    if slot == 2 {
        write_code_page();
    }
    #[cfg(target_arch = "x86_64")]
    if slot == 5 {
        unsafe { core::arch::asm!("jmp {data}", data = in(reg) 0x13000_usize, options(noreturn)) };
    }
    #[cfg(target_arch = "x86_64")]
    if slot == 6 {
        probe_page_downgrade(endpoint as usize);
    }
    #[cfg(not(target_arch = "x86_64"))]
    if slot == 4 {
        read_guard_page();
    }
    #[cfg(not(target_arch = "x86_64"))]
    if slot == 5 || slot == 6 {
        probe_segment_permissions(slot);
    }
    let status = if !check_image(slot) {
        1
    } else if slot == INIT || slot == ECHO_SERVER {
        resident(slot == INIT, endpoint)
    } else if (role & (1 << 9) != 0 && !contended_mutex(slot, 128))
        || !contended_mutex(slot, if role & (1 << 8) != 0 { 64 } else { 0 })
    {
        8
    } else if !exchange(slot, endpoint) {
        2
    } else if !rpc(slot, endpoint) {
        3
    } else {
        0
    };
    syscall(nr::THREAD_EXIT, status, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
fn probe_page_downgrade(address: usize) {
    use core::sync::atomic::{AtomicU64, Ordering};

    let target = address as *mut u64;
    let ready = unsafe { &*target.add(1).cast::<AtomicU64>() };
    let resume = unsafe { &*target.add(2).cast::<AtomicU64>() };
    unsafe { target.write_volatile(1) };
    ready.store(1, Ordering::Release);
    while resume.load(Ordering::Acquire) == 0 {
        core::hint::spin_loop();
    }
    unsafe { target.write_volatile(2) };
}

#[cfg(target_os = "none")]
fn contended_mutex(slot: u64, offset: u64) -> bool {
    use core::sync::atomic::{AtomicU32, Ordering};

    const CONTENDED: u32 = 2;
    const COOKIE: u32 = 0x4e4f_5641;
    let address = 0x14000 + offset;
    let word = unsafe { &*(address as *const AtomicU32) };
    let ready = unsafe { &*((address + 4) as *const AtomicU32) };
    let payload = unsafe { &*((address + 8) as *const AtomicU32) };
    let would_block = ErrorCode::WouldBlock.negated() as u64;
    if slot == 1 {
        if word.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err()
            || syscall(nr::FUTEX_WAIT, address, 0, 0, 0) != would_block
        {
            return false;
        }
        payload.store(COOKIE, Ordering::Relaxed);
        ready.store(1, Ordering::Release);
        loop {
            match syscall(nr::FUTEX_WAKE, address, 1, 0, 0) {
                1 => break,
                0 => { syscall(nr::YIELD, 0, 0, 0, 0); }
                _ => return false,
            }
        }
        word.swap(0, Ordering::Release) == CONTENDED && syscall(nr::FUTEX_WAKE, address, 1, 0, 0) <= 1
    } else {
        while ready.load(Ordering::Acquire) == 0 {
            syscall(nr::YIELD, 0, 0, 0, 0);
        }
        while word.swap(CONTENDED, Ordering::Acquire) != 0 {
            let result = syscall(nr::FUTEX_WAIT, address, CONTENDED as u64, 0, 0);
            if result != 0 && result != would_block {
                return false;
            }
        }
        let visible = payload.load(Ordering::Relaxed) == COOKIE;
        word.store(0, Ordering::Release);
        visible
    }
}

#[cfg(all(target_os = "none", not(target_arch = "x86_64")))]
fn read_guard_page() {
    let guard = 0x12000_usize;
    unsafe {
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("ldr xzr, [{guard}]", guard = in(reg) guard, options(nostack));
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!("lw zero, 0({guard})", guard = in(reg) guard, options(nostack));
        #[cfg(target_arch = "loongarch64")]
        core::arch::asm!("ld.w $zero, {guard}, 0", guard = in(reg) guard, options(nostack));
    }
}

#[cfg(all(target_os = "none", not(target_arch = "x86_64")))]
fn probe_segment_permissions(slot: u64) {
    let data = &raw mut MULTIPAGE as usize;
    let read_only = &raw const READ_ONLY as usize;
    unsafe {
        if slot == 5 {
            #[cfg(target_arch = "aarch64")]
            core::arch::asm!("br {address}", address = in(reg) data, options(noreturn));
            #[cfg(target_arch = "riscv64")]
            core::arch::asm!("jr {address}", address = in(reg) data, options(noreturn));
            #[cfg(target_arch = "loongarch64")]
            core::arch::asm!("jirl $zero, {address}, 0", address = in(reg) data, options(noreturn));
        }
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("str wzr, [{address}]", address = in(reg) read_only, options(nostack));
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!("sw zero, 0({address})", address = in(reg) read_only, options(nostack));
        #[cfg(target_arch = "loongarch64")]
        core::arch::asm!("st.w $zero, {address}, 0", address = in(reg) read_only, options(nostack));
    }
}

#[cfg(target_os = "none")]
fn resident(is_init: bool, endpoint: u64) -> u64 {
    const ECHO: u64 = 0x51;
    #[cfg(target_arch = "x86_64")]
    let mut payload = [0_u8; MAX_INLINE_PAYLOAD];
    #[cfg(target_arch = "x86_64")]
    let buffer = payload.as_mut_ptr();
    #[cfg(not(target_arch = "x86_64"))]
    let buffer = split_buffer();
    let address = buffer as u64;
    let capacity = MAX_INLINE_PAYLOAD as u64;
    let length = 8;
    let mut sequence = 0_u64;
    let mut token = 0_u64;
    loop {
        if is_init {
            let value = sequence as u8;
            for offset in 0..length {
                unsafe { buffer.add(offset as usize).write_volatile(value.wrapping_add(offset as u8)) };
            }
            if syscall6(nr::CALL, endpoint, ECHO, address, length, address, capacity) != 0
                || (0..length).any(|offset| unsafe { buffer.add(offset as usize).read_volatile() }
                    != !value.wrapping_add(offset as u8))
            {
                return 4;
            }
            let line: &[u8] = match sequence {
                0 => b"[NOVA-INIT] echo service ready\r\n",
                127 => b"[NOVA-INIT] 128 RPCs complete\r\n",
                _ => &[],
            };
            if !line.is_empty()
                && syscall(nr::WRITE, 1, line.as_ptr() as u64, line.len() as u64, 0) != line.len() as u64
            {
                return 5;
            }
            sequence = sequence.wrapping_add(1);
        } else {
            if syscall6(nr::RECV, endpoint, 0, address, capacity, &raw mut token as u64, 0) != ECHO {
                return 6;
            }
            let reply = unsafe { (&raw const token).read_volatile() };
            for offset in 0..length {
                let byte = unsafe { buffer.add(offset as usize) };
                unsafe { byte.write_volatile(!byte.read_volatile()) };
            }
            if reply == 0 || syscall(nr::REPLY, endpoint, reply, address, length) != 0 {
                return 7;
            }
        }
    }
}

#[cfg(target_os = "none")]
fn write_code_page() {
    let entry = _start as *const ();
    unsafe {
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("mov byte ptr [{entry}], 0", entry = in(reg) entry, options(nostack));
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("str wzr, [{entry}]", entry = in(reg) entry, options(nostack));
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!("sw zero, 0({entry})", entry = in(reg) entry, options(nostack));
        #[cfg(target_arch = "loongarch64")]
        core::arch::asm!("st.w $zero, {entry}, 0", entry = in(reg) entry, options(nostack));
    }
}

#[cfg(target_os = "none")]
fn check_image(slot: u64) -> bool {
    let version = syscall(nr::GET_VERSION, 0, 0, 0, 0);
    let expected = (u64::from(ABI_VERSION.major) << 16) | u64::from(ABI_VERSION.minor);
    let mut stack = version;
    let data = &raw mut DATA;
    let bss = &raw mut BSS;
    #[cfg(not(target_arch = "x86_64"))]
    unsafe {
        let pages = (&raw mut MULTIPAGE.0).cast::<u8>();
        if (&raw const READ_ONLY).read_volatile() != 0x4e4f_5641
            || pages.read_volatile() != 0xa5 || pages.add(4103).read_volatile() != 0xa5
        {
            return false;
        }
        pages.add(4103).write_volatile(slot as u8);
        if pages.add(4103).read_volatile() != slot as u8 {
            return false;
        }
    }
    unsafe {
        let initial = data.read_volatile();
        let zero = bss.read_volatile();
        data.write_volatile(zero);
        bss.write_volatile(slot);
        initial == 0x4e4f_5641 && zero == 0 && (&raw mut stack).read_volatile() == expected
            && data.read_volatile() == 0
    }
}

#[cfg(target_os = "none")]
fn exchange(slot: u64, endpoint: u64) -> bool {
    const REQUEST: u64 = 0x31;
    const RESPONSE: u64 = 0x32;
    const VALUE: u8 = 0x5a;
    let mut payload = [0_u8; MAX_INLINE_PAYLOAD];
    let payload = payload.as_mut_ptr();
    let buffer = payload as u64;
    let capacity = MAX_INLINE_PAYLOAD as u64;
    let rx_address = _start as *const () as u64;
    if syscall(nr::RECV, 0, 0, buffer, capacity) != ErrorCode::BadHandle.negated() as u64
        || syscall(nr::RECV, endpoint, 0, rx_address, capacity) != ErrorCode::Invalid.negated() as u64
    {
        return false;
    }

    match slot {
        0 => {
            unsafe { payload.write_volatile(VALUE) };
            syscall(nr::SEND, endpoint, REQUEST, buffer, 1) == 0
                && syscall(nr::RECV, endpoint, 0, buffer, capacity) == RESPONSE
                && unsafe { payload.read_volatile() } == !VALUE
                && unsafe { (&raw const BSS).read_volatile() } == slot
        }
        1 => {
            if syscall(nr::RECV, endpoint, 0, buffer, capacity) != REQUEST
                || unsafe { payload.read_volatile() } != VALUE
                || syscall(nr::YIELD, 0, 0, 0, 0) != 0
                || unsafe { (&raw const BSS).read_volatile() } != slot
            {
                return false;
            }
            unsafe { payload.write_volatile(!VALUE) };
            syscall(nr::SEND, endpoint, RESPONSE, buffer, 1) == 0
        }
        _ => false,
    }
}

#[cfg(target_os = "none")]
fn rpc(slot: u64, endpoint: u64) -> bool {
    const REQUEST: u64 = 0x41;
    const ABANDONED: u64 = 0x42;
    const VALUE: u8 = 0xc7;
    let mut payload = [0_u8; MAX_INLINE_PAYLOAD];
    let buffer = payload.as_mut_ptr();
    #[cfg(target_arch = "x86_64")]
    let mut token = 0_u64;
    #[cfg(target_arch = "x86_64")]
    let token = &raw mut token;
    #[cfg(not(target_arch = "x86_64"))]
    let token = split_buffer().cast::<u64>();
    let token_out = token as u64;
    let read_token = || u64::from_le(unsafe { token.read_unaligned() });
    let capacity = MAX_INLINE_PAYLOAD as u64;
    let address = buffer as u64;
    let code = _start as *const () as u64;
    match slot {
        0 => {
            unsafe { buffer.write_volatile(VALUE) };
            let call = |opcode, reply| syscall6(nr::CALL, endpoint, opcode, address, 1, reply, capacity);
            #[cfg(not(target_arch = "x86_64"))]
            if call(REQUEST, 0x11ffc) != ErrorCode::Invalid.negated() as u64 {
                return false;
            }
            call(REQUEST, code) == ErrorCode::Invalid.negated() as u64
                && call(REQUEST, address) == 0
                && unsafe { buffer.read_volatile() } == !VALUE
                && call(ABANDONED, address) == ErrorCode::PeerDied.negated() as u64
                && call(ABANDONED, address) == ErrorCode::PeerDied.negated() as u64
                && unsafe { buffer.read_volatile() } == !VALUE
        }
        1 => {
            let receive = |out| syscall6(nr::RECV, endpoint, 0, address, capacity, out, 0);
            if receive(code) != ErrorCode::Invalid.negated() as u64 || receive(token_out) != REQUEST
                || unsafe { buffer.read_volatile() } != VALUE
            {
                return false;
            }
            let reply_token = read_token();
            unsafe { buffer.write_volatile(!VALUE) };
            let reply = || syscall(nr::REPLY, endpoint, reply_token, address, 1);
            reply_token != 0
                && reply() == 0
                && reply() == ErrorCode::Invalid.negated() as u64
                && receive(token_out) == ABANDONED
                && read_token() != reply_token
        }
        _ => false,
    }
}

#[cfg(target_os = "none")]
fn syscall(number: Syscall, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    syscall6(number, arg0, arg1, arg2, arg3, 0, 0)
}

#[cfg(target_arch = "aarch64")]
#[cfg(target_os = "none")]
fn syscall6(number: Syscall, mut arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    unsafe {
        core::arch::asm!("svc #0", in("x8") u64::from(number.number()), inout("x0") arg0,
            in("x1") arg1, in("x2") arg2, in("x3") arg3, in("x4") arg4, in("x5") arg5, options(nostack));
    }
    arg0
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn syscall6(number: Syscall, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    let result;
    unsafe {
        core::arch::asm!("syscall", inlateout("rax") number.raw() => result,
            in("rdi") arg0, in("rsi") arg1, in("rdx") arg2, in("r10") arg3,
            in("r8") arg4, in("r9") arg5, lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    result
}

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
#[cfg(target_os = "none")]
fn syscall6(number: Syscall, mut arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    unsafe {
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!("ecall", in("a7") u64::from(number.number()), inout("a0") arg0,
            in("a1") arg1, in("a2") arg2, in("a3") arg3, in("a4") arg4, in("a5") arg5, options(nostack));
        #[cfg(target_arch = "loongarch64")]
        core::arch::asm!("syscall 0", in("$a7") u64::from(number.number()), inout("$a0") arg0,
            in("$a1") arg1, in("$a2") arg2, in("$a3") arg3, in("$a4") arg4, in("$a5") arg5, options(nostack));
    }
    arg0
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop { core::hint::spin_loop(); }
}
