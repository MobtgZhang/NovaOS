//! Interrupt routing: 8259 PIC remapping and PIC-masked spurious-IRQ defence.
//!
//! The 8259 PICs default to vectors 0x08-0x0F and 0x70-0x77, which collide
//! with the CPU exception vectors. Remapping them to 0x20/0x28 is what makes
//! "vector >= 32 means hardware interrupt" a safe assumption in
//! [`crate::arch::idt`].

use crate::arch::x86_64::port_io::{inb, outb};
use crate::klog;
use crate::logger::Level;

/// Master PIC command port.
const PIC1_COMMAND: u16 = 0x20;
/// Master PIC data port.
const PIC1_DATA: u16 = 0x21;
/// Slave PIC command port.
const PIC2_COMMAND: u16 = 0xA0;
/// Slave PIC data port.
const PIC2_DATA: u16 = 0xA1;

/// ICW1: begin initialisation, ICW4 expected.
const ICW1_INIT: u8 = 0x11;
/// ICW4: 8086 mode.
const ICW4_8086: u8 = 0x01;
/// Base vector for master IRQs.
pub const IRQ_BASE_MASTER: u8 = 0x20;
/// Base vector for slave IRQs.
pub const IRQ_BASE_SLAVE: u8 = 0x28;
/// Cascade line on the master, routed to IRQ2.
const CASCADE_IRQ: u8 = 2;

/// Remap the 8259 PICs so hardware IRQs start at vector 32.
///
/// # Safety
///
/// Writes to the PIC's ICW sequence, which briefly leaves the controller
/// uninitialised. Must not race with real interrupt activity; called once from
/// `_start` while interrupts are masked.
pub unsafe fn remap_pic() {
    // SAFETY: delegated to this function's contract.
    unsafe {
        let mask1 = inb(PIC1_DATA);
        let mask2 = inb(PIC2_DATA);

        // ICW1: start init.
        outb(PIC1_COMMAND, ICW1_INIT);
        outb(PIC2_COMMAND, ICW1_INIT);

        // ICW2: vector offsets. These are what push the IRQ range clear of the
        // exception vectors.
        outb(PIC1_DATA, IRQ_BASE_MASTER);
        outb(PIC2_DATA, IRQ_BASE_SLAVE);

        // ICW3: tell the master the slave is on IRQ2, and the master its ID.
        outb(PIC1_DATA, 1 << CASCADE_IRQ);
        outb(PIC2_DATA, CASCADE_IRQ);

        // ICW4: 8086/88 mode.
        outb(PIC1_DATA, ICW4_8086);
        outb(PIC2_DATA, ICW4_8086);

        // Restore the interrupt masks that were set before remapping.
        outb(PIC1_DATA, mask1);
        outb(PIC2_DATA, mask2);
    }

    klog!(
        Level::Debug,
        "pic: remapped to vectors {IRQ_BASE_MASTER:#04x}..{:#04x} and {IRQ_BASE_SLAVE:#04x}..{:#04x}",
        IRQ_BASE_MASTER + 7,
        IRQ_BASE_SLAVE + 7
    );
}

/// Mask every hardware IRQ line.
///
/// NovaOS v0.1.0 drives no devices, and the PIT in M2 is the first consumer. An
/// unmasked line with no handler is the classic source of a machine that boots
/// and then dies on the first spurious interrupt, so both PICs start fully
/// masked.
pub fn mask_all_irqs() {
    // SAFETY: writing PIC data registers to mask lines is always safe.
    unsafe {
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
    }
}

/// Unmask a single IRQ line on the master PIC.
///
/// # Safety
///
/// Must only be called once a real handler exists for `irq`, otherwise the line
/// fires into a stub and halts the kernel.
pub unsafe fn unmask_irq(irq: u8) {
    // SAFETY: delegated to this function's contract.
    unsafe {
        if irq < 8 {
            let mask = inb(PIC1_DATA);
            outb(PIC1_DATA, mask & !(1 << irq));
        } else {
            // A slave IRQ also needs the master's cascade line unmasked.
            let master_mask = inb(PIC1_DATA);
            outb(PIC1_DATA, master_mask & !(1 << CASCADE_IRQ));

            let slave_mask = inb(PIC2_DATA);
            outb(PIC2_DATA, slave_mask & !(1 << (irq - 8)));
        }
    }
}

/// Send an end-of-interrupt to both PICs.
///
/// # Safety
///
/// Only valid while servicing an IRQ; an EOI without a matching in-service
/// interrupt desynchronises the controller.
pub unsafe fn send_eoi() {
    // SAFETY: delegated to this function's contract.
    unsafe {
        outb(PIC2_COMMAND, 0x20);
        outb(PIC1_COMMAND, 0x20);
    }
}

/// Install the interrupt routing and leave all lines masked.
pub fn init() {
    // SAFETY: called once from `_start` with interrupts masked, so no IRQ can
    // arrive mid-sequence.
    unsafe { remap_pic() };
    mask_all_irqs();

    klog!(Level::Debug, "pic: all IRQ lines masked");
}
