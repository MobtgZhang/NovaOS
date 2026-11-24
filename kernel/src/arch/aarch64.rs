//! Early AArch64 backend for the QEMU `virt` board.

use core::arch::asm;
use core::ptr::{addr_of, read_volatile, write_volatile};

use super::capabilities::{AddressSpace, Console, InterruptControl, MachineExit};

mod paging;
mod user;

pub const NAME: &str = "aarch64";
pub const ADDRESS_SPACE: AddressSpace = match AddressSpace::new(4096, Some(40), Some(48)) {
    Some(space) => space,
    None => panic!("AArch64 requires a valid page size"),
};

const UART: usize = 0x0900_0000;
const UART_DATA: usize = 0x00;
const UART_FLAGS: usize = 0x18;
const UART_CONTROL: usize = 0x30;
const UART_TX_FULL: u32 = 1 << 5;
const UART_RX_EMPTY: u32 = 1 << 4;
const GICD: usize = 0x0800_0000;
const GICC: usize = 0x0801_0000;
const VIRTUAL_TIMER_IRQ: u32 = 27;

pub struct ConsoleImpl;

impl Console for ConsoleImpl {
    fn init(&self) {
        unsafe { write_volatile((UART + UART_CONTROL) as *mut u32, 0x301) };
    }

    fn send_byte(&self, byte: u8) {
        unsafe {
            while read_volatile((UART + UART_FLAGS) as *const u32) & UART_TX_FULL != 0 {
                core::hint::spin_loop();
            }
            write_volatile((UART + UART_DATA) as *mut u32, u32::from(byte));
        }
    }

    fn try_recv(&self) -> Option<u8> {
        unsafe {
            if read_volatile((UART + UART_FLAGS) as *const u32) & UART_RX_EMPTY != 0 {
                None
            } else {
                Some(read_volatile((UART + UART_DATA) as *const u32) as u8)
            }
        }
    }
}

pub struct Interrupts;

impl InterruptControl for Interrupts {
    fn mask(&self) -> bool {
        let masked = self.is_masked();
        unsafe { asm!("msr daifset, #2", options(nomem, nostack, preserves_flags)) };
        masked
    }

    fn restore(&self, was_masked: bool) {
        if !was_masked {
            unsafe { asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags)) };
        }
    }

    fn is_masked(&self) -> bool {
        let daif: u64;
        unsafe { asm!("mrs {daif}, daif", daif = out(reg) daif, options(nomem, nostack)) };
        daif & (1 << 7) != 0
    }
}

pub struct ExitImpl;

impl MachineExit for ExitImpl {
    unsafe fn exit(_status: u32) -> ! {
        loop {
            unsafe { asm!("wfi", options(nomem, nostack)) };
        }
    }
}

pub fn cycles() -> u64 {
    let counter: u64;
    unsafe { asm!("mrs {counter}, cntvct_el0", counter = out(reg) counter, options(nomem, nostack)) };
    counter
}

unsafe extern "C" {
    static nova_aarch64_vectors: u8;
}

pub unsafe fn arch_init() {
    unsafe {
        asm!(
            "msr vbar_el1, {vectors}",
            "msr cntkctl_el1, {user_counter}",
            "isb",
            vectors = in(reg) addr_of!(nova_aarch64_vectors),
            user_counter = in(reg) 2_u64,
            options(nostack),
        );
    }
}

pub(super) fn verify_privilege_transition(
    frames: &mut crate::arch_bringup::EarlyFrames,
    stack_pages: Option<[usize; 2]>, init: &[crate::early_init::Image],
) -> Result<(), &'static str> {
    let result = (|| {
        let spaces = paging::enable_and_verify(frames, stack_pages, init)?;
        let timer = Timer::init()?;
        user::run_init(frames, spaces, init, &timer, stack_pages.is_some())?;
        if stack_pages.is_some() { user::run(&timer) } else { Ok(()) }
    })();
    paging::disable_early_mmu();
    result
}

const GICD_SET_ENABLE: usize = 0x100;
const GICD_CLEAR_ENABLE: usize = 0x180;
const GICD_CLEAR_PENDING: usize = 0x280;
const GICD_PRIORITY: usize = 0x400;
const TIMER_MASK: u32 = 1 << VIRTUAL_TIMER_IRQ;

struct Timer {
    frequency: u64,
}

impl Timer {
    fn init() -> Result<Self, &'static str> {
        let frequency: u64;
        unsafe {
            asm!("mrs {frequency}, cntfrq_el0", frequency = out(reg) frequency, options(nomem, nostack));
        }
        if frequency < 100 {
            return Err("invalid AArch64 generic timer frequency");
        }
        unsafe {
            write_volatile((GICD + GICD_CLEAR_ENABLE) as *mut u32, TIMER_MASK);
            write_volatile((GICD + GICD_CLEAR_PENDING) as *mut u32, TIMER_MASK);
            write_volatile((GICD + GICD_PRIORITY + VIRTUAL_TIMER_IRQ as usize) as *mut u8, 0x80);
            write_volatile((GICD + GICD_SET_ENABLE) as *mut u32, TIMER_MASK);
            write_volatile((GICC + 4) as *mut u32, 0xff);
            write_volatile(GICC as *mut u32, 1);
        }
        Ok(Self { frequency })
    }

    fn arm(&self) {
        unsafe {
            asm!(
                "dsb sy",
                "msr cntv_tval_el0, {ticks}",
                "msr cntv_ctl_el0, {enable}",
                "isb",
                ticks = in(reg) self.frequency / 100,
                enable = in(reg) 1_u64,
                options(nostack),
            );
        }
    }

    fn stop() {
        unsafe { asm!("msr cntv_ctl_el0, xzr", "isb", "dsb sy", options(nostack)) };
    }

    fn acknowledge(&self) -> u32 {
        let ack = unsafe { read_volatile((GICC + 0x0c) as *const u32) };
        let irq = ack & 0x3ff;
        Self::stop();
        if irq < 1020 {
            unsafe { write_volatile((GICC + 0x10) as *mut u32, ack) };
        }
        irq
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        Self::stop();
        unsafe {
            write_volatile((GICD + GICD_CLEAR_ENABLE) as *mut u32, TIMER_MASK);
            write_volatile((GICD + GICD_CLEAR_PENDING) as *mut u32, TIMER_MASK);
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn nova_aarch64_fatal(syndrome: u64, pc: u64, address: u64) -> ! {
    panic!("AArch64 exception at {pc:#x}: ESR={syndrome:#x}, FAR={address:#x}");
}

core::arch::global_asm!(
    include_str!("aarch64/entry.S"),
    stack_size = const super::virt_smp::STACK_SIZE,
);
