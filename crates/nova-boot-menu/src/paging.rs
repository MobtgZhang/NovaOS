//! Early page tables for the NovaBoot hand-off.
//!
//! # Layout
//!
//! A single PML4 with the upper-half entries (256..512) populated:
//!
//! - `PML4[0]` -> identity-map of the low 4 GiB: enough that the boot manager's
//!   own code, stack, and the UEFI runtime remain reachable after CR3 is
//!   reloaded.
//! - `PML4[256]` -> HHDM window: every physical byte the firmware reported is
//!   mapped at `virtual = physical + 0xFFFF_8000_0000_0000`. This is the
//!   direct map the kernel's frame allocator and the snapshot use. It is
//!   built from the *largest leaves the alignment allows* — 1 GiB where the
//!   CPU supports `PDPE1GB`, 2 MiB for the rest of every aligned body, and
//!   4 KiB pages for each region's unaligned head and tail — because a 4
//!   KiB-only window over a 192 GiB machine would need on the order of a
//!   hundred thousand page-table frames, which the loader cannot allocate.
//! - `PML4[511]` -> kernel higher-half window: the kernel image loaded at
//!   physical `0x00100000` is reachable at virtual
//!   `0xFFFFFFFF80100000` (i.e. `physical + 0xFFFFFFFF80000000`), as are the
//!   bootstrap stack and the BootProtocol snapshot page. This window stays
//!   4 KiB-only: no PS bit may be set anywhere in it, because it covers the
//!   kernel image page by page and the kernel's own builder replaces it
//!   wholesale rather than walking it.

use core::ptr::write_volatile;

use nova_boot::layout::{AddressParts, P2_PAGE, P3_PAGE, PS, chunk_size, has_1gib_pages};
use uefi::table::boot::{AllocateType, BootServices, MemoryType};

/// Page size in bytes.
pub use nova_boot::layout::PAGE_SIZE;

/// Number of entries in one page table page.
const ENTRIES: usize = 512;

/// PML4 index of the HHDM window.
const HHDM_PML4_IDX: usize = 256;

/// PML4 index of the kernel higher-half window.
const KERNEL_PML4_IDX: usize = 511;

/// Page table entry flags. Matches the kernel's
/// `nova_core::paging::PageFlags`: PRESENT (bit 0), WRITABLE (bit 1), GLOBAL
/// (bit 8). No NX bit: all leaves are executable.
const PRESENT: u64 = 0x1;
const WRITABLE: u64 = 0x2;
const GLOBAL: u64 = 0x100;

/// Table entry (points at another table): present + writable.
const TABLE: u64 = PRESENT | WRITABLE;
/// Leaf entry (points at a 4 KiB frame): present + writable + global.
const LEAF: u64 = PRESENT | WRITABLE | GLOBAL;

/// The direct-map offset: `virtual = physical + HHDM_OFFSET`.
pub const HHDM_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// The kernel's higher-half offset: `vma = physical + KERNEL_OFFSET`.
pub const KERNEL_OFFSET: u64 = 0xFFFF_FFFF_8000_0000;

/// The kernel's physical load address.
pub const KERNEL_LMA: u64 = 0x0010_0000;

/// The bootstrap stack's size in bytes.
pub const BOOT_STACK_SIZE: u64 = 32 * PAGE_SIZE;

/// A page-aligned physical frame, zeroed on allocation.
pub struct Frame {
    phys: u64,
}

impl Frame {
    /// Allocate one page of the given UEFI memory type, zeroed.
    pub fn allocate_one(bs: &BootServices, ty: MemoryType) -> Result<Self, uefi::Status> {
        let addr = bs
            .allocate_pages(AllocateType::AnyPages, ty, 1)
            .map_err(|error| error.status())?;
        unsafe {
            let slice = core::slice::from_raw_parts_mut(addr as *mut u8, PAGE_SIZE as usize);
            for byte in slice.iter_mut() {
                *byte = 0;
            }
        }
        Ok(Self { phys: addr })
    }

    /// Wrap a raw physical address as a `Frame`.
    pub fn from_phys(phys: u64) -> Self {
        Self { phys }
    }

    /// Physical address, page aligned.
    pub fn phys(&self) -> u64 {
        self.phys
    }
}

/// The page tables: one PML4 plus every table it references.
///
/// Kept alive for the duration of the hand-off; the kernel clones the upper
/// half and then rebuilds its own, so these pages can be reclaimed after the
/// jump.
pub struct Tables {
    /// The PML4 page itself.
    pub pml4: Frame,
    /// PML4[0] -> PDPT; PDPT[i] -> PD[i]; PD[i][j] -> 2 MiB identity leaf.
    ///
    /// These fields are not read by Rust after construction, but the CPU walks
    /// through them via the PML4, so they must not be dropped.
    #[allow(dead_code)]
    low_pdpt: Frame,
    /// The 512 PD pages of the low identity map, retained for the same reason
    /// as `low_pdpt`.
    #[allow(dead_code)]
    low_pds: alloc::vec::Vec<Frame>,
    /// HHDM window tables (populated by [`Tables::map_hhdm`]).
    hhdm_pdpt: Option<Frame>,
    /// Kernel window tables (populated by [`Tables::map_kernel_window`]).
    kernel_pdpt: Option<Frame>,
}

impl Tables {
    /// Build page tables that identity-map the whole low 4 GiB through
    /// PML4[0], leaving PML4[256] and PML4[511] empty until [`map_hhdm`] and
    /// [`map_kernel_window`] fill them.
    pub fn new(bs: &BootServices) -> Result<Self, uefi::Status> {
        let pml4 = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;
        let low_pdpt = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;

        // PML4[0] -> low PDPT.
        Self::write_entry(pml4.phys() as *mut u64, 0, TABLE | low_pdpt.phys());

        // PDPT[i] -> PD[i], and every PD[i][j] is a 2 MiB identity leaf: 512
        // PDs of 512 leaves each cover the entire low 4 GiB. The boot
        // manager keeps executing its own code on its own stack across the
        // CR3 reload — wherever UEFI loaded them below 4 GiB — so the
        // identity map must reach all of it. (An earlier 512-PT build
        // covered only the first 1 GiB while its comment claimed 4 GiB; with
        // the boot manager at ~2 GiB the first fetch after `mov cr3`
        // page-faulted and triple-faulted, and the kernel never printed.)
        let mut low_pds = alloc::vec::Vec::with_capacity(ENTRIES);
        for pd_index in 0..ENTRIES {
            let pd = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;
            for leaf_index in 0..ENTRIES {
                let physical = (pd_index as u64 * ENTRIES as u64 + leaf_index as u64) * P2_PAGE;
                Self::write_entry(pd.phys() as *mut u64, leaf_index, LEAF | PS | physical);
            }
            Self::write_entry(low_pdpt.phys() as *mut u64, pd_index, TABLE | pd.phys());
            low_pds.push(pd);
        }

        // PML4[256] and PML4[511] are intentionally left zero here.
        Self::write_entry(pml4.phys() as *mut u64, HHDM_PML4_IDX, 0);
        Self::write_entry(pml4.phys() as *mut u64, KERNEL_PML4_IDX, 0);

        Ok(Self {
            pml4,
            low_pdpt,
            low_pds,
            hhdm_pdpt: None,
            kernel_pdpt: None,
        })
    }

    /// Map every reported memory region at `virtual = physical + HHDM_OFFSET`.
    ///
    /// The window is built from the largest leaves the alignment allows:
    /// [`chunk_size`] picks 1 GiB (when the CPU supports it), 2 MiB, or 4 KiB
    /// for each step through each region. Every leaf in the window carries the
    /// same flags — present, writable, global — and the mapping is identity,
    /// so a leaf that covers bytes an earlier 4 KiB mapping already touched
    /// maps exactly the same physical memory with exactly the same rights:
    /// overwriting it is not a change of meaning.
    pub fn map_hhdm(
        &mut self,
        bs: &BootServices,
        regions: &[(u64, u64)],
    ) -> Result<(), uefi::Status> {
        let one_gib = has_1gib_pages();

        let hhdm_pdpt = match &self.hhdm_pdpt {
            Some(frame) => frame.phys(),
            None => {
                let frame = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;
                let phys = frame.phys();
                self.hhdm_pdpt = Some(frame);
                Self::write_entry(self.pml4.phys() as *mut u64, HHDM_PML4_IDX, TABLE | phys);
                phys
            }
        };

        for &(start, end) in regions {
            if start >= end {
                continue;
            }
            let mut addr = start & !(PAGE_SIZE - 1);
            let last = (end - 1) & !(PAGE_SIZE - 1);
            loop {
                // Bytes still to map from `addr`, counting the partially
                // covered final page: this is what stops a 1 GiB or 2 MiB
                // leaf from running past the region's end into memory the
                // firmware described as something else.
                let left = last + PAGE_SIZE - addr;

                let size = chunk_size(addr, left, one_gib);
                match size {
                    P3_PAGE => {
                        // A 1 GiB leaf lives directly in the PDPT. Whatever
                        // the entry held before mapped a subset of this same
                        // 1 GiB identity — see the function comment — so the
                        // overwrite is safe.
                        Self::write_entry(
                            hhdm_pdpt as *mut u64,
                            ((addr.wrapping_add(HHDM_OFFSET)) >> 30) as usize & 0x1FF,
                            LEAF | PS | addr,
                        );
                    }
                    P2_PAGE => {
                        let pd = self.hhdm_pd(bs, addr)?;
                        // `pd == 0` means a 1 GiB leaf already covers this
                        // address: nothing to build.
                        if pd != 0 {
                            Self::write_entry(
                                pd as *mut u64,
                                ((addr >> 21) & 0x1FF) as usize,
                                LEAF | PS | addr,
                            );
                        }
                    }
                    _ => {
                        self.map_hhdm_page(bs, addr)?;
                    }
                }

                if addr >= last {
                    break;
                }
                addr = addr
                    .checked_add(size)
                    .ok_or(uefi::Status::INVALID_PARAMETER)?;
            }
        }
        Ok(())
    }

    /// The PD table of the HHDM window that holds `addr`'s directory entry,
    /// allocating one if the PDPT entry is still empty.
    ///
    /// Returns `0` when a 1 GiB leaf already covers `addr`, which absolves the
    /// caller of mapping anything below it.
    fn hhdm_pd(&mut self, bs: &BootServices, addr: u64) -> Result<u64, uefi::Status> {
        let Some(pdpt) = self.hhdm_pdpt.as_ref().map(Frame::phys) else {
            return Err(uefi::Status::NOT_FOUND);
        };
        let p3 = ((addr.wrapping_add(HHDM_OFFSET)) >> 30) as usize & 0x1FF;
        let entry = Self::read_entry(pdpt as *const u64, p3);
        if entry & PS != 0 {
            // A 1 GiB leaf: the whole directory is subsumed by this entry.
            return Ok(0);
        }
        let pd = entry & !(PAGE_SIZE - 1);
        if pd != 0 {
            return Ok(pd);
        }
        let frame = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;
        let phys = frame.phys();
        Self::write_entry(pdpt as *mut u64, p3, TABLE | phys);
        Ok(phys)
    }

    /// Map the kernel image at `physical + KERNEL_OFFSET`, plus the bootstrap
    /// stack and the snapshot page at their `+ KERNEL_OFFSET` aliases.
    ///
    /// `stack_phys` is the physical address of the allocated bootstrap stack
    /// (`BOOT_STACK_SIZE` bytes), and `snapshot_phys` is the physical address
    /// of one page holding the BootProtocol snapshot.
    pub fn map_kernel_window(
        &mut self,
        bs: &BootServices,
        kernel_pages: u64,
        stack_phys: u64,
        snapshot_phys: u64,
    ) -> Result<(), uefi::Status> {
        let kernel_pdpt = match &self.kernel_pdpt {
            Some(frame) => frame.phys(),
            None => {
                let frame = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;
                let phys = frame.phys();
                self.kernel_pdpt = Some(frame);
                Self::write_entry(self.pml4.phys() as *mut u64, KERNEL_PML4_IDX, TABLE | phys);
                phys
            }
        };

        // Kernel image: [KERNEL_LMA, KERNEL_LMA + kernel_pages * PAGE_SIZE).
        let kernel_end = KERNEL_LMA + kernel_pages * PAGE_SIZE;
        let mut addr = KERNEL_LMA;
        while addr < kernel_end {
            let virt = addr.wrapping_add(KERNEL_OFFSET);
            self.map_one(bs, kernel_pdpt, virt, addr)?;
            addr += PAGE_SIZE;
        }

        // Bootstrap stack: identity-mapped too (it must be reachable before and
        // after CR3 reload), plus its KERNEL_OFFSET alias for the kernel's own
        // use after it switches to KERNEL_STACK.
        let stack_end = stack_phys + BOOT_STACK_SIZE;
        let mut addr = stack_phys & !(PAGE_SIZE - 1);
        while addr < stack_end {
            let virt_low = addr; // identity
            let virt_high = addr.wrapping_add(KERNEL_OFFSET);
            self.map_one(bs, kernel_pdpt, virt_high, addr)?;
            self.map_one(bs, kernel_pdpt, virt_low, addr)?;
            addr += PAGE_SIZE;
        }

        // Snapshot page: only the KERNEL_OFFSET alias is needed (RDI carries it).
        self.map_one(
            bs,
            kernel_pdpt,
            snapshot_phys.wrapping_add(KERNEL_OFFSET),
            snapshot_phys,
        )?;

        Ok(())
    }

    /// The physical address of the PML4, to load into CR3.
    pub fn pml4_phys(&self) -> u64 {
        self.pml4.phys()
    }

    /// Map one physical page at its HHDM alias (`physical + HHDM_OFFSET`),
    /// allocating intermediate HHDM-window tables as needed.
    ///
    /// # Huge leaves
    ///
    /// The window is mostly 1 GiB and 2 MiB leaves, so most calls land inside
    /// a leaf that already covers the address — identity mapping means the
    /// leaf maps exactly the requested page — and the call is a no-op. Only a
    /// page in a region's unaligned head or tail, where 4 KiB leaves live,
    /// reaches the fourth level here.
    pub fn map_hhdm_page(&mut self, bs: &BootServices, physical: u64) -> Result<(), uefi::Status> {
        let Some(pdpt) = self.hhdm_pdpt.as_ref().map(Frame::phys) else {
            return Err(uefi::Status::NOT_FOUND);
        };
        let virt = physical.wrapping_add(HHDM_OFFSET);
        let parts = AddressParts::from(virt);

        // PDPT level: a 1 GiB leaf covers the page already.
        let pdpt_entry = Self::read_entry(pdpt as *const u64, parts.p3);
        if pdpt_entry & PS != 0 {
            return Ok(());
        }
        if pdpt_entry & PRESENT == 0 {
            let frame = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;
            let pd = frame.phys();
            Self::write_entry(pdpt as *mut u64, parts.p3, TABLE | pd);
        }

        // PD level: a 2 MiB leaf covers the page already.
        let pd = Self::read_entry(pdpt as *const u64, parts.p3) & !(PAGE_SIZE - 1);
        let pd_entry = Self::read_entry(pd as *const u64, parts.p2);
        if pd_entry & PS != 0 {
            return Ok(());
        }
        if pd_entry & PRESENT == 0 {
            let frame = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;
            let pt = frame.phys();
            Self::write_entry(pd as *mut u64, parts.p2, TABLE | pt);
        }

        // PT level: write the 4 KiB leaf.
        let pt = Self::read_entry(pd as *const u64, parts.p2) & !(PAGE_SIZE - 1);
        Self::write_entry(pt as *mut u64, parts.p1, LEAF | physical);
        Ok(())
    }

    /// Map one virtual page to one physical page, allocating intermediate
    /// tables as needed.
    pub fn map_one(
        &mut self,
        bs: &BootServices,
        pdpt_phys: u64,
        virt: u64,
        physical: u64,
    ) -> Result<(), uefi::Status> {
        let parts = AddressParts::from(virt);

        // PDPT[parts.p3] -> PD.
        let pd_phys = Self::read_entry(pdpt_phys as *const u64, parts.p3) & !(PAGE_SIZE - 1);
        let pd = if pd_phys == 0 {
            let frame = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;
            let phys = frame.phys();
            Self::write_entry(pdpt_phys as *mut u64, parts.p3, TABLE | phys);
            phys
        } else {
            pd_phys
        };

        // PD[parts.p2] -> PT.
        let pt_phys = Self::read_entry(pd as *const u64, parts.p2) & !(PAGE_SIZE - 1);
        let pt = if pt_phys == 0 {
            let frame = Frame::allocate_one(bs, MemoryType::LOADER_DATA)?;
            let phys = frame.phys();
            Self::write_entry(pd as *mut u64, parts.p2, TABLE | phys);
            phys
        } else {
            pt_phys
        };

        // PT[parts.p1] = leaf.
        Self::write_entry(pt as *mut u64, parts.p1, LEAF | physical);
        Ok(())
    }

    /// Write one page table entry.
    fn write_entry(table: *mut u64, index: usize, entry: u64) {
        debug_assert!(index < ENTRIES);
        unsafe {
            write_volatile(table.add(index), entry);
        }
    }

    /// Read one page table entry.
    fn read_entry(table: *const u64, index: usize) -> u64 {
        debug_assert!(index < ENTRIES);
        unsafe { *table.add(index) }
    }
}

/// Reload CR3 with the new PML4's physical address and jump to `entry` with
/// `rsp` and `rdi` set.
///
/// # Safety
///
/// The caller must guarantee that `entry` and the stack at `rsp` are mapped
/// identically in both the old and new page tables, that interrupts are
/// masked, and that `entry` never returns.
pub unsafe fn enter_new_tables(pml4_phys: u64, rsp: u64, rdi: u64, entry: u64) -> ! {
    unsafe {
        core::arch::asm!(
            // Interrupts off: after CR3 is reloaded the firmware's IDT is no
            // longer reachable, and an NMI (or a spurious interrupt) landing
            // between the reload and the jump would fault on its own fetch.
            "cli",
            "mov cr3, {pml4}",
            "mov rsp, {rsp}",
            "mov rdi, {rdi}",
            "jmp {entry}",
            pml4 = in(reg) pml4_phys,
            rsp = in(reg) rsp,
            rdi = in(reg) rdi,
            entry = in(reg) entry,
            options(noreturn, nostack),
        );
    }
}
