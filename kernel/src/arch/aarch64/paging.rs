//! Identity map for the early kernel and the two isolated EL0 probe pages.

use core::arch::asm;
use core::ptr::{addr_of, addr_of_mut};

use super::{GICD, UART};
use crate::arch_bringup::EarlyFrames;
use crate::early_init;

const PAGE: usize = 4096;
const ENTRIES: usize = 512;
const BLOCK: usize = PAGE * ENTRIES;
const TABLE: u64 = 0b11;
const VALID_BLOCK: u64 = 0b01;
const ACCESS: u64 = 1 << 10;
const INNER_SHAREABLE: u64 = 3 << 8;
const EL0_ACCESS: u64 = 1 << 6;
const READ_ONLY: u64 = 1 << 7;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const DEVICE: u64 = 1 << 2;

#[repr(C, align(4096))]
struct Table([u64; ENTRIES]);

struct KernelTables {
    root: Table,
    level1: Table,
    device_l2: Table,
    ram_l2: Table,
    ram_l3: Table,
    second_root: Table,
    second_level1: Table,
    second_ram_l2: Table,
    second_ram_l3: Table,
    init_ram_l2: Table,
    init_ram_l3: Table,
    pool_l3: [Table; 2],
}

const EMPTY: Table = Table([0; ENTRIES]);
static mut KERNEL_TABLES: [KernelTables; crate::arch::virt_smp::MAX_CPUS] = [const {
    KernelTables {
        root: EMPTY, level1: EMPTY, device_l2: EMPTY, ram_l2: EMPTY, ram_l3: EMPTY,
        second_root: EMPTY, second_level1: EMPTY, second_ram_l2: EMPTY, second_ram_l3: EMPTY,
        init_ram_l2: EMPTY, init_ram_l3: EMPTY,
        pool_l3: [EMPTY, EMPTY],
    }
}; crate::arch::virt_smp::MAX_CPUS];

fn local_tables() -> *mut KernelTables {
    let cpu = crate::arch::virt_smp::current_slot();
    unsafe { addr_of_mut!(KERNEL_TABLES).cast::<KernelTables>().add(cpu) }
}

#[derive(Clone, Copy)]
pub(super) struct AddressSpace([usize; 4]);

impl AddressSpace {
    pub(super) fn activate(self) {
        activate(self.0[0] as *const Table);
    }

    pub(super) fn retire(self, frames: &mut EarlyFrames) -> Result<(), &'static str> {
        select(false);
        frames.release(self.0)
    }
}

unsafe extern "C" {
    static __text_start: u8;
    static __rodata_start: u8;
    static __data_start: u8;
    static __user_text_start: u8;
    static __user_stack_start: u8;
    static __image_end_vma: u8;
}

pub(super) fn enable_and_verify(
    frames: &mut EarlyFrames,
    stack_pages: Option<[usize; 2]>, images: &[early_init::Image],
) -> Result<[AddressSpace; early_init::PROCESSES], &'static str> {
    let kernel = addr_of!(__text_start) as usize;
    let rodata = addr_of!(__rodata_start) as usize;
    let data = addr_of!(__data_start) as usize;
    let user_text = addr_of!(__user_text_start) as usize;
    let user_stack = addr_of!(__user_stack_start) as usize;
    let image_end = addr_of!(__image_end_vma) as usize;
    let tables = local_tables();
    let KernelTables {
        root, level1, device_l2, ram_l2, ram_l3, second_root, second_level1,
        second_ram_l2, second_ram_l3, init_ram_l2, init_ram_l3, pool_l3,
    } = unsafe { &mut *tables };
    let [root, level1, device_l2, ram_l2, ram_l3, second_root, second_level1,
        second_ram_l2, second_ram_l3, init_ram_l2, init_ram_l3] =
        [root, level1, device_l2, ram_l2, ram_l3, second_root, second_level1,
            second_ram_l2, second_ram_l3, init_ram_l2, init_ram_l3].map(core::ptr::from_mut);
    let mut spaces = [AddressSpace([0; 4]); early_init::PROCESSES];
    let pool = frames.identity_range();
    if pool.start >> 30 != kernel >> 30 || (pool.end - 1) >> 30 != kernel >> 30 {
        return Err("AArch64 early pool is outside the kernel L1 window");
    }
    for space in &mut spaces[..images.len()] {
        *space = AddressSpace(unsafe { frames.allocate::<4>()? });
    }

    unsafe {
        (*root).0[0] = level1 as u64 | TABLE;
        (*level1).0[UART >> 30] = device_l2 as u64 | TABLE;
        (*level1).0[kernel >> 30] = ram_l2 as u64 | TABLE;
        for device in [UART, GICD] {
            let offset = (device / BLOCK) % ENTRIES;
            let descriptor = device as u64 | VALID_BLOCK | ACCESS | DEVICE | PXN | UXN;
            (*device_l2).0[offset] = descriptor;
        }
        (*init_ram_l2).0[(kernel / BLOCK) % ENTRIES] = init_ram_l3 as u64 | TABLE;
        let user_flags = TABLE | ACCESS | INNER_SHAREABLE | EL0_ACCESS | PXN;
        for (space, image) in spaces.into_iter().zip(images) {
            let [init_root, init_level1, init_device_l2, init_low_l3] =
                space.0.map(|address| address as *mut Table);
            (*init_device_l2).0 = (*device_l2).0;
            (*init_root).0[0] = init_level1 as u64 | TABLE;
            (*init_level1).0[UART >> 30] = init_device_l2 as u64 | TABLE;
            (*init_level1).0[kernel >> 30] = init_ram_l2 as u64 | TABLE;
            (*init_device_l2).0[0] = init_low_l3 as u64 | TABLE;
            for page in image.pages() {
                let permission = match page.access {
                    early_init::Access::Read => READ_ONLY | UXN,
                    early_init::Access::ReadWrite => UXN,
                    early_init::Access::ReadExecute => READ_ONLY,
                };
                (*init_low_l3).0[page.address / PAGE] = page.frame as u64 | user_flags | permission;
            }
        }
        (*ram_l2).0[(kernel / BLOCK) % ENTRIES] = ram_l3 as u64 | TABLE;
        (*second_root).0[0] = second_level1 as u64 | TABLE;
        (*second_level1).0[UART >> 30] = device_l2 as u64 | TABLE;
        (*second_level1).0[kernel >> 30] = second_ram_l2 as u64 | TABLE;
        (*second_ram_l2).0[(kernel / BLOCK) % ENTRIES] = second_ram_l3 as u64 | TABLE;

        if image_end != user_stack + PAGE {
            return Err("AArch64 probe stack layout changed");
        }
        for address in (kernel..user_stack + PAGE).step_by(PAGE) {
            let permissions = if address == user_text {
                EL0_ACCESS | READ_ONLY | PXN
            } else if address == user_stack {
                EL0_ACCESS | PXN | UXN
            } else if address < rodata {
                READ_ONLY | UXN
            } else if address < data {
                READ_ONLY | PXN | UXN
            } else {
                PXN | UXN
            };
            let offset = (address / PAGE) % ENTRIES;
            let flags = TABLE | ACCESS | INNER_SHAREABLE | permissions;
            let [first_physical, second_physical] = if address == user_stack {
                stack_pages.unwrap_or([address; 2])
            } else {
                [address; 2]
            };
            (*ram_l3).0[offset] = first_physical as u64 | flags;
            (*second_ram_l3).0[offset] = second_physical as u64 | flags;
            if address != user_text && address != user_stack {
                (*init_ram_l3).0[offset] = address as u64 | flags;
            }
        }

        for address in pool.clone().step_by(PAGE) {
            let block = address / BLOCK;
            let l2_index = block % ENTRIES;
            let l3_index = address / PAGE % ENTRIES;
            let entry = address as u64 | TABLE | ACCESS | INNER_SHAREABLE | PXN | UXN;
            if block == kernel / BLOCK {
                for table in [ram_l3, second_ram_l3, init_ram_l3] {
                    (*table).0[l3_index] = entry;
                }
            } else {
                let table = &mut pool_l3[block - pool.start / BLOCK];
                table.0[l3_index] = entry;
                for level2 in [ram_l2, second_ram_l2, init_ram_l2] {
                    (*level2).0[l2_index] = core::ptr::from_ref(table) as u64 | TABLE;
                }
            }
        }

        let cache_type: u64;
        asm!("mrs {cache_type}, ctr_el0", cache_type = out(reg) cache_type, options(nostack));
        let cache_line = 4 << ((cache_type >> 16) & 0xf);
        for image in images {
            for page in image.pages().filter(|page| page.access == early_init::Access::ReadExecute) {
                for address in (page.frame..page.frame + PAGE).step_by(cache_line) {
                    asm!("dc cvau, {address}", address = in(reg) address, options(nostack));
                }
            }
        }
        const TCR: u64 = 16 | (1 << 8) | (1 << 10) | (3 << 12) | (1 << 23) | (2 << 32);
        const MAIR: u64 = 0x04ff;
        asm!(
            "dsb sy",
            "ic iallu",
            "dsb sy",
            "isb",
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {root}",
            "isb",
            "tlbi vmalle1",
            "dsb sy",
            "isb",
            "mrs {control}, sctlr_el1",
            "orr {control}, {control}, #1",
            "msr sctlr_el1, {control}",
            "isb",
            mair = in(reg) MAIR,
            tcr = in(reg) TCR,
            root = in(reg) root,
            control = out(reg) _,
            options(nostack),
        );
    }

    for (root, stack_physical) in [root, second_root].into_iter().zip(stack_pages.into_iter().flatten()) {
        activate(root);
        if el0_address(kernel, false).is_some()
            || el0_address(data, false).is_some()
            || el0_address(UART, false).is_some()
            || el0_address(GICD, false).is_some()
            || el0_address(user_text, false) != Some(user_text)
            || el0_address(user_text, true).is_some()
            || el0_address(user_stack, true) != Some(stack_physical)
            || el0_address(user_stack + PAGE, false).is_some()
            || el0_address(early_init::CODE, false).is_some()
            || el0_address(early_init::SHARED, false).is_some()
        {
            return Err("EL0 translation permissions do not match the probe layout");
        }
    }
    for (space, image) in spaces.into_iter().zip(images) {
        space.activate();
        if el0_address(kernel, false).is_some()
            || el0_address(user_text, false).is_some()
            || el0_address(user_stack, false).is_some()
            || el0_address(early_init::STACK_TOP, false).is_some()
            || image.pages().any(|page| {
                el0_address(page.address, false) != Some(page.frame)
                    || el0_address(page.address, true)
                        != (page.access == early_init::Access::ReadWrite).then_some(page.frame)
            })
        {
            return Err("EL0 translation permissions do not match the init layout");
        }
    }
    Ok(spaces)
}

pub(super) fn select(second: bool) {
    let tables = local_tables();
    activate(unsafe { if second { addr_of!((*tables).second_root) } else { addr_of!((*tables).root) } });
}

pub(super) fn disable_early_mmu() {
    unsafe {
        asm!(
            "dsb sy",
            "mrs {control}, sctlr_el1",
            "bic {control}, {control}, #1",
            "msr sctlr_el1, {control}",
            "isb",
            control = out(reg) _,
            options(nostack),
        );
    }
}

pub(super) fn contains_user_range(address: usize, length: usize, write: bool) -> bool {
    if length == 0 {
        return true;
    }
    let Some(last) = address.checked_add(length - 1) else {
        return false;
    };
    let first_page = address & !(PAGE - 1);
    let last_page = last & !(PAGE - 1);
    (first_page..=last_page)
        .step_by(PAGE)
        .all(|page| el0_address(page, write).is_some())
}

fn activate(root: *const Table) {
    unsafe {
        asm!(
            "dsb ishst",
            "msr ttbr0_el1, {root}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            root = in(reg) root,
            options(nostack),
        );
    }
}

fn el0_address(address: usize, write: bool) -> Option<usize> {
    let result: u64;
    unsafe {
        if write {
            asm!("at s1e0w, {address}", address = in(reg) address, options(nostack));
        } else {
            asm!("at s1e0r, {address}", address = in(reg) address, options(nostack));
        }
        asm!("isb", "mrs {result}, par_el1", result = out(reg) result, options(nostack));
    }
    (result & 1 == 0).then_some((result as usize & 0xff_ffff_f000) | (address & (PAGE - 1)))
}
