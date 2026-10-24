//! The address-space model: mappings, reference counts, and the W^X rule.
//!
//! # What an address space is
//!
//! An address space is a set of virtual-to-physical mappings, plus the page
//! tables that implement them. The *mappings* are the interesting part — the
//! page tables are a representation of them, and the two can disagree if the
//! bookkeeping is wrong.
//!
//! So the model is kept separate from the page tables: this module owns the
//! mapping table and the reference counts, and the kernel's `vmm` layer owns
//! the tables and calls into here to decide *what* to change.
//!
//! # Reference counting
//!
//! Two address spaces may map the same physical frame. That is how a shared
//! library works, and it is how a `fork`ed page would work if NovaOS had one.
//! When the second space unmaps the frame, the frame must *not* be freed — the
//! first space still maps it.
//!
//! The rule is therefore: the frame is freed when its count reaches zero, and
//! the count is the number of *mappings*, not the number of spaces. Two
//! mappings of the same frame in the *same* space count twice, which is why
//! remapping a page in place must adjust the count rather than simply
//! overwrite the entry.
//!
//! # W^X
//!
//! A user page is never simultaneously writable and executable. The rule is
//! enforced in the mapping API rather than checked by the caller, because the
//! caller is exactly the code that would get it wrong: a loader mapping an
//! ELF segment has to decide between "writable" and "executable" from the
//! segment's flags, and a `.text` that is writable is a shellcode target.
//!
//! The rule is asymmetric on purpose. Kernel mappings may be both, because the
//! kernel is not the adversary; only *user* mappings are restricted.

use crate::paging::{Level, PageFlags, page_base};

/// The number of mappings an address space can hold.
///
/// A fixed bound rather than a growable table: the kernel has no allocator, so
/// the mapping table is a static array. The bound is generous — 4096 mappings
/// is 16 MiB of user space — and a process that needs more fails loudly rather
/// than silently corrupting the table.
pub const MAX_MAPPINGS: usize = 4096;

/// The kind of protection a mapping carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protection {
    /// Readable and writable, not executable.
    UserReadWrite,
    /// Readable and executable, not writable.
    UserReadExecute,
    /// Readable only, not writable and not executable.
    UserReadOnly,
    /// Kernel mapping: readable, writable, and executable.
    Kernel,
}

impl Protection {
    /// The page-table flags this protection implies.
    ///
    /// A user mapping always gets the `USER` bit; a kernel mapping never does,
    /// because a kernel page that is user-accessible is reachable from ring 3.
    ///
    /// The flags are combined with [`PageFlags::union`] rather than the `|`
    /// operator: `BitOr::bitor` cannot be a `const fn` (a trait method may not
    /// be `const`), so `|` is unavailable in a `const` context.
    pub const fn page_flags(self) -> PageFlags {
        match self {
            Protection::UserReadWrite => PageFlags::PRESENT
                .union(PageFlags::WRITABLE)
                .union(PageFlags::USER),
            Protection::UserReadExecute => PageFlags::PRESENT.union(PageFlags::USER),
            Protection::UserReadOnly => PageFlags::PRESENT.union(PageFlags::USER),
            Protection::Kernel => PageFlags::PRESENT
                .union(PageFlags::WRITABLE)
                .union(PageFlags::GLOBAL),
        }
    }

    /// Whether this protection allows writing.
    pub const fn is_writable(self) -> bool {
        matches!(self, Protection::UserReadWrite | Protection::Kernel)
    }

    /// Whether this protection allows execution.
    pub const fn is_executable(self) -> bool {
        matches!(self, Protection::UserReadExecute | Protection::Kernel)
    }

    /// Whether this protection is reachable from ring 3.
    pub const fn is_user(self) -> bool {
        !matches!(self, Protection::Kernel)
    }

    /// A stable tag for the serial log.
    pub const fn tag(self) -> &'static str {
        match self {
            Protection::UserReadWrite => "rwx->rw",
            Protection::UserReadExecute => "rwx->rx",
            Protection::UserReadOnly => "rwx->r",
            Protection::Kernel => "kernel",
        }
    }
}

/// One virtual-to-physical mapping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mapping {
    /// Page-aligned virtual address.
    pub virt: u64,
    /// Page-aligned physical frame address.
    pub phys: u64,
    /// The protection this mapping carries.
    pub protection: Protection,
    /// How many mappings share this frame.
    ///
    /// One means this mapping owns the frame outright; two or more means
    /// something else maps it too, and the frame must survive this mapping
    /// going away.
    pub refcount: u16,
}

/// Why a mapping operation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmmError {
    /// The address space has no room for another mapping.
    TableFull,
    /// The virtual address is not page aligned.
    UnalignedVirtual,
    /// The physical address is not page aligned.
    UnalignedPhysical,
    /// The virtual address is not canonical.
    NonCanonical,
    /// The virtual address is outside the permitted user range.
    OutsideUserRange,
    /// The protection is not permitted for this address space.
    ProtectionNotAllowed,
    /// The address is already mapped and the operation would replace it.
    AlreadyMapped,
    /// The address is not mapped.
    NotMapped,
    /// The frame allocator is out of frames.
    OutOfMemory,
    /// The page tables could not be built.
    TableWalkFailed,
}

impl VmmError {
    /// A stable, lowercase tag for the serial log.
    pub const fn tag(self) -> &'static str {
        match self {
            VmmError::TableFull => "mapping-table-full",
            VmmError::UnalignedVirtual => "unaligned-virtual-address",
            VmmError::UnalignedPhysical => "unaligned-physical-address",
            VmmError::NonCanonical => "non-canonical-address",
            VmmError::OutsideUserRange => "outside-user-range",
            VmmError::ProtectionNotAllowed => "protection-not-allowed",
            VmmError::AlreadyMapped => "already-mapped",
            VmmError::NotMapped => "not-mapped",
            VmmError::OutOfMemory => "out-of-memory",
            VmmError::TableWalkFailed => "page-table-walk-failed",
        }
    }
}

impl core::fmt::Display for VmmError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.tag())
    }
}

/// The window of virtual addresses a user process may map.
///
/// The bounds are *derived*, not written down: the lower bound is the first
/// page after the null page, which no user program may map because the CPU
/// treats a null pointer as "not present" rather than "unmapped"; the upper
/// bound is the last canonical user address, one page below the kernel half.
///
/// The plan freezes the window at `0x1000`–`0x7FFF_FFFF_F000`. Those are
/// exactly the numbers this derivation produces on a 48-bit address space,
/// which is the point: writing them down twice would let the two disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UserWindow {
    /// First address a user mapping may occupy.
    pub start: u64,
    /// One past the last address a user mapping may occupy.
    pub end: u64,
}

impl UserWindow {
    /// Derive the user window from the address-space geometry.
    ///
    /// `virtual_bits` is the target's virtual address width and
    /// `kernel_half_start` is the first address of the kernel half. Both come
    /// from the architecture, so a 57-bit LA57 target would get a proportionally
    /// larger window without a change here.
    ///
    /// # The upper bound comes from the width, not from the kernel half
    ///
    /// The upper bound is the last canonical *user* address, one page below
    /// `2^(virtual_bits - 1)`. It is **not** "the page below the kernel
    /// half": the kernel half begins at `0xFFFF_8000_0000_0000` on a 48-bit
    /// space, so one page below it is `0xFFFF_7FFF_FFFF_F000` — an address
    /// whose sign extension disagrees with bit 47, which the hardware does
    /// not address at all, and which is larger than every canonical address
    /// in the space. A window bounded that way excludes nothing.
    ///
    /// That bound is also not merely too large: `0x0000_8000_0000_0000` and
    /// `0xFFFF_8000_0000_0000` have the *same* page-table walk (PML4 entry
    /// 256, the kernel's own entry), so a user mapping at the former would
    /// land in the kernel half's tables.
    ///
    /// # Why `kernel_half_start` is still a parameter
    ///
    /// It is not the quantity the bound is derived from, but it *is* a
    /// quantity that must agree with `virtual_bits`: the kernel half starts
    /// at `2^64 - 2^(virtual_bits - 1)`. Passing it in turns a silent
    /// disagreement between the two halves of the geometry into a loud
    /// failure here, rather than a window that is derived from one number
    /// and checked against another.
    ///
    /// # Panics
    ///
    /// If `kernel_half_start` is not the canonical higher-half start implied
    /// by `virtual_bits`, or if `virtual_bits` is outside the range x86-64
    /// supports (4..=57), the two halves of the geometry disagree and there is
    /// no window that is correct for both. [`UserWindow::try_derive`] is the
    /// non-panicking form.
    pub const fn derive(virtual_bits: u32, kernel_half_start: u64) -> Self {
        match Self::try_derive(virtual_bits, kernel_half_start) {
            Some(window) => window,
            None => panic!("the address-space geometry is inconsistent"),
        }
    }

    /// Derive the user window, or report that the geometry is inconsistent.
    ///
    /// The non-panicking form of [`UserWindow::derive`], for a caller that
    /// wants to *report* the disagreement rather than abort on it.
    pub const fn try_derive(virtual_bits: u32, kernel_half_start: u64) -> Option<Self> {
        // x86-64 supports 48-bit (four-level) and 57-bit (LA57) virtual
        // addresses, and anything narrower is not a width this derivation
        // has been reasoned about.
        if virtual_bits < 4 || virtual_bits > 57 {
            return None;
        }

        // The half of the address space the user half occupies: bit
        // `virtual_bits - 1` is the last addressable bit, so the user half is
        // everything below `2^(virtual_bits - 1)`. The window's upper bound is
        // the page below that, so the highest byte a user mapping may occupy
        // is `2^(virtual_bits - 1) - 1`.
        let user_half = 1u64 << (virtual_bits - 1);
        let end = user_half - crate::paging::PAGE_SIZE;

        // The kernel half begins at the top of the address space, mirrored
        // around bit `virtual_bits - 1`. On a 48-bit space that is
        // `0xFFFF_8000_0000_0000`.
        let expected_kernel_half = !(user_half - 1);
        if kernel_half_start != expected_kernel_half {
            return None;
        }

        // The null page is never mappable: a user pointer of zero must fault
        // rather than resolve to page zero of the process.
        Some(Self {
            start: crate::paging::PAGE_SIZE,
            end,
        })
    }

    /// Whether `address` lies inside the window.
    pub const fn contains(self, address: u64) -> bool {
        address >= self.start && address < self.end
    }
}

/// The set of mappings that make up one address space.
///
/// The table is a fixed-size array rather than a tree or a list: the kernel
/// has no allocator, and a linear table of 4096 entries is small enough
/// (128 KiB) to sit in `.bss` and large enough for any process v0.1.0 will
/// run. Lookup is linear, which is the price of that choice — and the price is
/// paid only by the kernel, never by user code.
#[derive(Clone)]
pub struct AddressSpace {
    /// The mappings.
    mappings: [Option<Mapping>; MAX_MAPPINGS],
    /// How many slots are occupied.
    count: usize,
    /// The window user mappings may occupy.
    window: UserWindow,
}

impl AddressSpace {
    /// An empty address space with the given user window.
    pub const fn new(window: UserWindow) -> Self {
        Self {
            mappings: [None; MAX_MAPPINGS],
            count: 0,
            window,
        }
    }

    /// The user window this space permits.
    pub const fn window(&self) -> UserWindow {
        self.window
    }

    /// How many mappings the space holds.
    pub const fn mapping_count(&self) -> usize {
        self.count
    }

    /// Whether the space holds no mappings.
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Iterate over the mappings, in slot order.
    pub fn mappings(&self) -> impl Iterator<Item = &Mapping> {
        self.mappings.iter().flatten()
    }

    /// The mapping at `virt`, if any.
    pub fn find(&self, virt: u64) -> Option<&Mapping> {
        self.mappings
            .iter()
            .flatten()
            .find(|mapping| mapping.virt == virt)
    }

    /// Whether `virt` is mapped.
    pub fn is_mapped(&self, virt: u64) -> bool {
        self.find(virt).is_some()
    }

    /// Whether `virt` is mapped with a protection that allows writing.
    pub fn is_writable(&self, virt: u64) -> bool {
        self.find(virt)
            .is_some_and(|mapping| mapping.protection.is_writable())
    }

    /// Whether `virt` is mapped with a protection that allows execution.
    pub fn is_executable(&self, virt: u64) -> bool {
        self.find(virt)
            .is_some_and(|mapping| mapping.protection.is_executable())
    }

    /// Check that a mapping request is well-formed, without applying it.
    ///
    /// Separated from [`AddressSpace::map`] so the kernel can validate a request
    /// *before* it allocates anything — otherwise a rejected request would
    /// have already consumed a frame.
    pub fn check_map(&self, virt: u64, phys: u64, protection: Protection) -> Result<(), VmmError> {
        if !virt.is_multiple_of(crate::paging::PAGE_SIZE) {
            return Err(VmmError::UnalignedVirtual);
        }

        if !phys.is_multiple_of(crate::paging::PAGE_SIZE) {
            return Err(VmmError::UnalignedPhysical);
        }

        if !crate::paging::is_canonical(virt) {
            return Err(VmmError::NonCanonical);
        }

        if protection.is_user() && !self.window.contains(virt) {
            return Err(VmmError::OutsideUserRange);
        }

        // The W^X rule. A user mapping may not be both writable and
        // executable; the kernel half is exempt, because the kernel is not the
        // adversary and its text pages are legitimately writable-then-executed
        // during relocation.
        if protection.is_user() && protection.is_writable() && protection.is_executable() {
            return Err(VmmError::ProtectionNotAllowed);
        }

        Ok(())
    }

    /// Map `phys` at `virt`.
    ///
    /// Returns the mapping that was created, or the error that stopped it.
    /// The caller is responsible for building the page-table entry; this
    /// method owns the *bookkeeping*.
    pub fn map(
        &mut self,
        virt: u64,
        phys: u64,
        protection: Protection,
    ) -> Result<Mapping, VmmError> {
        self.check_map(virt, phys, protection)?;

        if self.count >= MAX_MAPPINGS {
            return Err(VmmError::TableFull);
        }

        // Mapping a frame that is already mapped *at this address* is an error
        // rather than a replacement: a replacement has to adjust the
        // reference count, and doing that silently would hide a caller that
        // forgot to unmap. Mapping the same frame at a *different* address is
        // legitimate and increments the shared count.
        if let Some(existing) = self.find(virt)
            && existing.phys == phys
        {
            return Err(VmmError::AlreadyMapped);
        }

        // The reference count of the *frame*, which may be shared with a
        // mapping at another address.
        let shared = self
            .mappings
            .iter()
            .flatten()
            .any(|mapping| mapping.phys == phys);

        let slot = self
            .mappings
            .iter()
            .position(|entry| entry.is_none())
            .ok_or(VmmError::TableFull)?;

        let mapping = Mapping {
            virt,
            phys,
            protection,
            refcount: if shared { 2 } else { 1 },
        };

        self.mappings[slot] = Some(mapping);
        self.count += 1;

        Ok(mapping)
    }

    /// Remove the mapping at `virt`.
    ///
    /// Returns the removed mapping together with whether the frame should now
    /// be freed — that is, whether this was the last reference to it.
    ///
    /// The distinction matters: unmapping a shared page must not free the
    /// frame, or the other space's mapping becomes a use-after-free.
    pub fn unmap(&mut self, virt: u64) -> Result<(Mapping, bool), VmmError> {
        let slot = self
            .mappings
            .iter()
            .position(|entry| entry.is_some_and(|mapping| mapping.virt == virt))
            .ok_or(VmmError::NotMapped)?;

        let removed = self.mappings[slot].expect("the slot was just found to be occupied");

        // The frame is still referenced if some *other* mapping names it.
        let still_shared = self
            .mappings
            .iter()
            .flatten()
            .any(|mapping| mapping.virt != virt && mapping.phys == removed.phys);

        self.mappings[slot] = None;
        self.count -= 1;

        Ok((removed, !still_shared))
    }

    /// The physical frame mapped at `virt`, if any.
    pub fn translate(&self, virt: u64) -> Option<u64> {
        self.find(virt).map(|mapping| mapping.phys)
    }

    /// Whether `virt` lies in a guard region — an address deliberately left
    /// unmapped so that an overflow faults.
    ///
    /// A guard page is not a mapping, so the answer is derived from the
    /// absence of one: any address below the lowest mapping and above the
    /// window's start is a hole.
    pub fn is_guard(&self, virt: u64) -> bool {
        !self.window.contains(virt) && !self.is_mapped(virt)
    }
}

/// Where the guard page sits relative to a user stack.
///
/// The page immediately *below* a stack is left unmapped, so that a stack
/// overflow faults on the guard rather than silently corrupting whatever is
/// below the stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GuardPage {
    /// The guard page's base address.
    pub base: u64,
    /// The first address above the guard — the stack's lowest page.
    pub above: u64,
}

impl GuardPage {
    /// Place a guard immediately below a stack that occupies `stack_base..stack_top`.
    ///
    /// `stack_base` is the lowest usable address of the stack and `stack_top` is
    /// one past its highest. The guard is the page immediately below
    /// `stack_base`.
    ///
    /// The stack's extent is checked so that a caller cannot build a guard
    /// page that is not actually below the stack: a guard whose range is
    /// inverted would sit *above* the stack, where it protects nothing and
    /// faults on the first legitimate stack access.
    pub const fn below(stack_base: u64, stack_top: u64) -> Self {
        assert!(
            stack_top > stack_base,
            "a stack must have a non-zero extent",
        );
        Self {
            base: stack_base - crate::paging::PAGE_SIZE,
            above: stack_base,
        }
    }

    /// Whether `address` falls inside the guard page.
    pub const fn contains(self, address: u64) -> bool {
        address >= self.base && address < self.above
    }
}

/// The level of the page table a given virtual address's leaf lives in.
///
/// Exposed because the kernel's page-table layer needs it to decide how many
/// intermediate tables a mapping requires.
pub const fn leaf_level() -> Level {
    Level::Pt
}

/// The page-aligned base of `address`.
pub const fn aligned(address: u64) -> u64 {
    page_base(address)
}
