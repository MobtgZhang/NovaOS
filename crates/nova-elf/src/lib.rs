//! ELF64 parsing and validation.
//!
//! This crate is the kernel's only view of a user program's image. It parses
//! the ELF64 header and program headers, and refuses anything it cannot reason
//! about — a loader that maps a malformed image is how a user process ends up
//! owning kernel memory.
//!
//! # Why the validation lives here and not in the loader
//!
//! The rules are *about the image*, not about the machine: "the entry point is
//! inside a `PT_LOAD`", "`p_filesz` does not exceed `p_memsz`", "no segment
//! reaches into the kernel half". A loader that checked them inline would have
//! to be re-checked by every future loader, and the second one would check a
//! slightly different subset. Putting them here means one implementation, and
//! the host test suite exercises exactly the code the kernel runs.
//!
//! # `no_std`, and no allocator
//!
//! The crate is `no_std` and uses no heap: every view into the image is a
//! borrowed slice, and the parser never copies. That is not an optimisation —
//! the kernel has no allocator at all until M3, and a parser that needed one
//! could not be used to load the *first* user process, which is the one that
//! has to exist before any heap could be brought up.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod builder;

pub use builder::ElfBuilder;

/// The four bytes every ELF file starts with.
pub const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];

/// `e_ident[EI_CLASS]`: 64-bit.
pub const ELFCLASS64: u8 = 2;
/// `e_ident[EI_DATA]`: little-endian.
pub const ELFDATA2LSB: u8 = 1;
/// `e_ident[EI_VERSION]`: the current version.
pub const EV_CURRENT: u8 = 1;

/// `e_type`: a non-relocatable executable.
pub const ET_EXEC: u16 = 2;
/// `e_type`: a position-independent executable (a PIE).
pub const ET_DYN: u16 = 3;

/// `e_machine`: x86-64.
pub const EM_X86_64: u16 = 62;

/// `p_type`: a segment loaded into memory.
pub const PT_LOAD: u32 = 1;

/// `p_flags`: executable.
pub const PF_X: u32 = 1;
/// `p_flags`: writable.
pub const PF_W: u32 = 2;
/// `p_flags`: readable.
pub const PF_R: u32 = 4;

/// Size of one 64-bit program header, in bytes.
pub const PROGRAM_HEADER_SIZE: usize = 56;

/// Size of the 64-bit ELF header, in bytes.
pub const ELF_HEADER_SIZE: usize = 64;

/// The largest number of program headers this parser will accept.
///
/// The kernel has no heap, so the program-header table is viewed in place
/// rather than copied. The bound exists because `e_phnum` is a 16-bit field
/// from an untrusted file: a value of 65535 would describe a table larger than
/// any real image, and a caller that trusted it would walk off the end.
pub const MAX_PROGRAM_HEADERS: usize = 128;

/// Why an image was rejected.
///
/// Every variant is a *distinct* rule, not a catch-all, so the kernel can log
/// exactly which invariant an image broke and the host test suite can assert
/// on one variant per rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElfError {
    /// The file is shorter than an ELF header.
    TruncatedHeader,
    /// The first four bytes are not `\x7fELF`.
    BadMagic,
    /// `e_ident[EI_CLASS]` is not `ELFCLASS64`.
    Not64Bit,
    /// `e_ident[EI_DATA]` is not `ELFDATA2LSB`.
    NotLittleEndian,
    /// `e_ident[EI_VERSION]` is not the current version.
    BadVersion,
    /// `e_type` is neither `ET_EXEC` nor `ET_DYN`.
    UnsupportedType,
    /// `e_machine` is not `EM_X86_64`.
    WrongMachine,
    /// `e_ehsize` does not match the 64-bit header size.
    BadHeaderSize,
    /// `e_phentsize` does not match the 64-bit program header size.
    BadProgramHeaderSize,
    /// `e_phnum` is zero, so the image has no loadable segment.
    NoProgramHeaders,
    /// `e_phnum` exceeds [`MAX_PROGRAM_HEADERS`].
    TooManyProgramHeaders,
    /// The program header table lies outside the file.
    ProgramHeadersOutOfBounds,
    /// The file is shorter than the program header table it declares.
    TruncatedProgramHeaders,
    /// The image has no `PT_LOAD` segment.
    NoLoadableSegment,
    /// A `PT_LOAD` segment's `p_memsz` is zero.
    EmptySegment,
    /// A `PT_LOAD` segment's `p_filesz` exceeds its `p_memsz`.
    FileSizeExceedsMemorySize,
    /// A segment's file range extends past the end of the file.
    SegmentPastEndOfFile,
    /// A segment's `p_offset` is not congruent to its `p_vaddr` modulo `p_align`.
    MisalignedSegment,
    /// A segment's `p_align` is not a power of two.
    BadSegmentAlignment,
    /// A segment's virtual address is not canonical.
    NonCanonicalAddress,
    /// A segment reaches into the kernel's higher half.
    SegmentInKernelSpace,
    /// Two `PT_LOAD` segments overlap.
    OverlappingSegments,
    /// The entry point lies outside every `PT_LOAD` segment.
    EntryPointOutsideSegments,
}

impl ElfError {
    /// A stable, lowercase tag for the serial log.
    pub const fn tag(self) -> &'static str {
        match self {
            ElfError::TruncatedHeader => "truncated-header",
            ElfError::BadMagic => "bad-magic",
            ElfError::Not64Bit => "not-64-bit",
            ElfError::NotLittleEndian => "not-little-endian",
            ElfError::BadVersion => "bad-version",
            ElfError::UnsupportedType => "unsupported-type",
            ElfError::WrongMachine => "wrong-machine",
            ElfError::BadHeaderSize => "bad-header-size",
            ElfError::BadProgramHeaderSize => "bad-program-header-size",
            ElfError::NoProgramHeaders => "no-program-headers",
            ElfError::TooManyProgramHeaders => "too-many-program-headers",
            ElfError::ProgramHeadersOutOfBounds => "program-headers-out-of-bounds",
            ElfError::TruncatedProgramHeaders => "truncated-program-headers",
            ElfError::NoLoadableSegment => "no-loadable-segment",
            ElfError::EmptySegment => "empty-segment",
            ElfError::FileSizeExceedsMemorySize => "filesz-exceeds-memsz",
            ElfError::SegmentPastEndOfFile => "segment-past-end-of-file",
            ElfError::MisalignedSegment => "misaligned-segment",
            ElfError::BadSegmentAlignment => "bad-segment-alignment",
            ElfError::NonCanonicalAddress => "non-canonical-address",
            ElfError::SegmentInKernelSpace => "segment-in-kernel-space",
            ElfError::OverlappingSegments => "overlapping-segments",
            ElfError::EntryPointOutsideSegments => "entry-point-outside-segments",
        }
    }
}

impl core::fmt::Display for ElfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.tag())
    }
}

/// One program header.
///
/// The field names mirror the ELF specification's, so the code and the
/// specification can be read side by side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProgramHeader {
    /// Segment type; [`PT_LOAD`] is the only one the loader acts on.
    pub p_type: u32,
    /// Segment flags: any of [`PF_R`], [`PF_W`], [`PF_X`].
    pub p_flags: u32,
    /// Offset of the segment's contents within the file.
    pub p_offset: u64,
    /// Virtual address the segment is loaded at.
    pub p_vaddr: u64,
    /// Physical address; meaningless for a user program, kept for completeness.
    pub p_paddr: u64,
    /// Number of bytes read from the file.
    pub p_filesz: u64,
    /// Number of bytes occupied in memory. May exceed `p_filesz` (a `.bss`).
    pub p_memsz: u64,
    /// Required alignment of `p_offset` and `p_vaddr`.
    pub p_align: u64,
}

impl ProgramHeader {
    /// Whether this segment is loaded into memory.
    pub const fn is_load(&self) -> bool {
        self.p_type == PT_LOAD
    }

    /// Whether the segment is executable.
    pub const fn is_executable(&self) -> bool {
        self.p_flags & PF_X != 0
    }

    /// Whether the segment is writable.
    pub const fn is_writable(&self) -> bool {
        self.p_flags & PF_W != 0
    }

    /// Whether the segment is readable.
    pub const fn is_readable(&self) -> bool {
        self.p_flags & PF_R != 0
    }

    /// One past the last virtual byte the segment occupies.
    ///
    /// Saturating rather than wrapping: a segment whose end overflows is
    /// rejected by validation, and a wrapped value here would be *smaller* than
    /// the start, which is the direction that hides the problem.
    pub const fn vaddr_end(&self) -> u64 {
        self.p_vaddr.saturating_add(self.p_memsz)
    }
}

/// The ELF header, as parsed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ElfHeader {
    /// `e_type`.
    pub e_type: u16,
    /// `e_machine`.
    pub e_machine: u16,
    /// `e_version`.
    pub e_version: u32,
    /// `e_entry`: where execution starts.
    pub e_entry: u64,
    /// `e_phoff`: offset of the program header table.
    pub e_phoff: u64,
    /// `e_shoff`: offset of the section header table.
    pub e_shoff: u64,
    /// `e_flags`.
    pub e_flags: u32,
    /// `e_ehsize`.
    pub e_ehsize: u16,
    /// `e_phentsize`.
    pub e_phentsize: u16,
    /// `e_phnum`.
    pub e_phnum: u16,
    /// `e_shentsize`.
    pub e_shentsize: u16,
    /// `e_shnum`.
    pub e_shnum: u16,
    /// `e_shstrndx`.
    pub e_shstrndx: u16,
}

/// A parsed, borrowed ELF64 image.
#[derive(Clone, Copy)]
pub struct Elf<'a> {
    header: ElfHeader,
    /// The raw program header table, borrowed from the file.
    ///
    /// Kept as bytes rather than as `&[ProgramHeader]` because the table's
    /// alignment is not guaranteed: `e_phoff` is a file offset the linker
    /// chooses, and a table at an odd offset cannot be viewed as a slice of a
    /// `#[repr(C)]` struct without violating the type's alignment requirement.
    /// Decoding on demand costs a handful of loads and keeps the crate free of
    /// `unsafe` entirely.
    program_header_bytes: &'a [u8],
    data: &'a [u8],
}

impl<'a> Elf<'a> {
    /// Parse the header and program headers of `data`.
    ///
    /// This checks only what can be checked without knowing where the image will
    /// be loaded. The rules that need the load address — canonicality, the
    /// kernel-half exclusion, segment overlap — live in [`Elf::validate`].
    ///
    /// # Errors
    ///
    /// Returns the first rule the image breaks. A truncated file, a foreign
    /// architecture, and a program header table that runs off the end of the
    /// file are all distinguished, because the kernel's log is the only place
    /// that information exists.
    pub fn parse(data: &'a [u8]) -> Result<Self, ElfError> {
        if data.len() < ELF_HEADER_SIZE {
            return Err(ElfError::TruncatedHeader);
        }

        if data[0..4] != ELF_MAGIC {
            return Err(ElfError::BadMagic);
        }

        if data[4] != ELFCLASS64 {
            return Err(ElfError::Not64Bit);
        }

        if data[5] != ELFDATA2LSB {
            return Err(ElfError::NotLittleEndian);
        }

        if data[6] != EV_CURRENT {
            return Err(ElfError::BadVersion);
        }

        let e_type = u16::from_le_bytes([data[16], data[17]]);
        let e_machine = u16::from_le_bytes([data[18], data[19]]);
        let e_version = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
        let e_entry = u64::from_le_bytes([
            data[24], data[25], data[26], data[27], data[28], data[29], data[30], data[31],
        ]);
        let e_phoff = u64::from_le_bytes([
            data[32], data[33], data[34], data[35], data[36], data[37], data[38], data[39],
        ]);
        let e_shoff = u64::from_le_bytes([
            data[40], data[41], data[42], data[43], data[44], data[45], data[46], data[47],
        ]);
        let e_flags = u32::from_le_bytes([data[48], data[49], data[50], data[51]]);
        let e_ehsize = u16::from_le_bytes([data[52], data[53]]);
        let e_phentsize = u16::from_le_bytes([data[54], data[55]]);
        let e_phnum = u16::from_le_bytes([data[56], data[57]]);
        let e_shentsize = u16::from_le_bytes([data[58], data[59]]);
        let e_shnum = u16::from_le_bytes([data[60], data[61]]);
        let e_shstrndx = u16::from_le_bytes([data[62], data[63]]);

        if e_type != ET_EXEC && e_type != ET_DYN {
            return Err(ElfError::UnsupportedType);
        }

        if e_machine != EM_X86_64 {
            return Err(ElfError::WrongMachine);
        }

        if e_ehsize as usize != ELF_HEADER_SIZE {
            return Err(ElfError::BadHeaderSize);
        }

        if e_phentsize as usize != PROGRAM_HEADER_SIZE {
            return Err(ElfError::BadProgramHeaderSize);
        }

        if e_phnum == 0 {
            return Err(ElfError::NoProgramHeaders);
        }

        if e_phnum as usize > MAX_PROGRAM_HEADERS {
            return Err(ElfError::TooManyProgramHeaders);
        }

        let header = ElfHeader {
            e_type,
            e_machine,
            e_version,
            e_entry,
            e_phoff,
            e_shoff,
            e_flags,
            e_ehsize,
            e_phentsize,
            e_phnum,
            e_shentsize,
            e_shnum,
            e_shstrndx,
        };

        let table_bytes = (e_phnum as usize) * PROGRAM_HEADER_SIZE;
        let table_start = e_phoff as usize;
        let table_end = match table_start.checked_add(table_bytes) {
            Some(end) => end,
            None => return Err(ElfError::ProgramHeadersOutOfBounds),
        };

        if table_end > data.len() {
            return Err(ElfError::TruncatedProgramHeaders);
        }

        Ok(Self {
            header,
            program_header_bytes: &data[table_start..table_end],
            data,
        })
    }

    /// The parsed header.
    pub const fn header(&self) -> &ElfHeader {
        &self.header
    }

    /// The number of program headers.
    pub const fn program_header_count(&self) -> usize {
        self.program_header_bytes.len() / PROGRAM_HEADER_SIZE
    }

    /// The program header at `index`.
    ///
    /// Returns `None` when `index` is past the end of the table.
    pub fn program_header(&self, index: usize) -> Option<ProgramHeader> {
        let start = index.checked_mul(PROGRAM_HEADER_SIZE)?;
        let bytes = self
            .program_header_bytes
            .get(start..start + PROGRAM_HEADER_SIZE)?;
        Some(decode_program_header(bytes))
    }

    /// Every program header, in file order.
    pub fn program_headers(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        let count = self.program_header_count();
        (0..count).filter_map(|index| self.program_header(index))
    }

    /// The `PT_LOAD` segments, in file order.
    pub fn loadable_segments(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        self.program_headers().filter(|ph| ph.is_load())
    }

    /// The bytes of the image.
    pub const fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The entry point.
    pub const fn entry_point(&self) -> u64 {
        self.header.e_entry
    }

    /// Whether the image declares itself position-independent.
    pub const fn is_position_independent(&self) -> bool {
        self.header.e_type == ET_DYN
    }

    /// Validate the image against the address space it is about to enter.
    ///
    /// `kernel_half_start` is the first address of the kernel's higher half,
    /// taken from the architecture's address-space description rather than
    /// written down here — the same number the rest of the kernel uses to
    /// decide what "kernel space" means.
    ///
    /// # Errors
    ///
    /// The first rule the image breaks, in the order the checks are performed.
    pub fn validate(&self, kernel_half_start: u64) -> Result<(), ElfError> {
        if self.loadable_segments().next().is_none() {
            return Err(ElfError::NoLoadableSegment);
        }

        // Every loadable segment is checked against the file and the address
        // space before any of them is compared with any other, so a malformed
        // segment is reported as such rather than as an overlap.
        for segment in self.loadable_segments() {
            validate_segment(&segment, self.data.len(), kernel_half_start)?;
        }

        check_overlap(self.loadable_segments())?;

        if !self.entry_point_in_load() {
            return Err(ElfError::EntryPointOutsideSegments);
        }

        Ok(())
    }

    /// Whether the entry point falls inside a loadable segment.
    pub fn entry_point_in_load(&self) -> bool {
        let entry = self.header.e_entry;
        self.loadable_segments()
            .any(|segment| entry >= segment.p_vaddr && entry < segment.vaddr_end())
    }
}

/// Decode one program header from its raw bytes.
fn decode_program_header(bytes: &[u8]) -> ProgramHeader {
    let word = |offset: usize| {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[offset..offset + 8]);
        u64::from_le_bytes(raw)
    };
    let half = |offset: usize| {
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&bytes[offset..offset + 4]);
        u32::from_le_bytes(raw)
    };
    // An ELF64 program header has no 16-bit fields: `p_type` and `p_flags`
    // are both 32-bit. A 16-bit reader would be dead code, so there is none.

    ProgramHeader {
        p_type: half(0),
        p_flags: half(4),
        p_offset: word(8),
        p_vaddr: word(16),
        p_paddr: word(24),
        p_filesz: word(32),
        p_memsz: word(40),
        p_align: word(48),
    }
}

/// Check one segment against the file and the address space.
fn validate_segment(
    segment: &ProgramHeader,
    file_len: usize,
    kernel_half_start: u64,
) -> Result<(), ElfError> {
    if segment.p_memsz == 0 {
        return Err(ElfError::EmptySegment);
    }

    if segment.p_filesz > segment.p_memsz {
        return Err(ElfError::FileSizeExceedsMemorySize);
    }

    // The file range must be inside the file. Both ends are checked: an
    // offset past the end is caught by the addition, and an addition that
    // overflows is caught by the checked form.
    let file_end = segment
        .p_offset
        .checked_add(segment.p_filesz)
        .ok_or(ElfError::SegmentPastEndOfFile)?;
    if file_end > file_len as u64 {
        return Err(ElfError::SegmentPastEndOfFile);
    }

    // `p_align` is either zero (no constraint) or a power of two. The
    // congruence rule only applies when it is a power of two, which is what
    // the ELF specification says.
    if segment.p_align != 0 {
        if !segment.p_align.is_power_of_two() {
            return Err(ElfError::BadSegmentAlignment);
        }
        if segment.p_offset % segment.p_align != segment.p_vaddr % segment.p_align {
            return Err(ElfError::MisalignedSegment);
        }
    }

    if !is_canonical(segment.p_vaddr) {
        return Err(ElfError::NonCanonicalAddress);
    }

    // The segment's *memory* range must not reach into the kernel half. The
    // check is on the end of the range, not the start, because a segment that
    // starts in user space and runs into the kernel half is exactly the case
    // that matters.
    let end = segment.vaddr_end();
    if end > kernel_half_start {
        return Err(ElfError::SegmentInKernelSpace);
    }

    Ok(())
}

/// Check that no two loadable segments overlap in memory.
///
/// The comparison is between *every* pair, not only consecutive ones. Two
/// segments can overlap while a third sits between them in the file, and a
/// consecutive-only comparison would miss that — which is the case an ELF
/// file with a deliberately reordered program header table exists to
/// produce.
///
/// The pairs are held in a fixed-size array rather than a sorted copy: the
/// segment count is bounded by [`MAX_PROGRAM_HEADERS`], and sorting would need
/// an allocator the kernel does not have.
fn check_overlap(mut segments: impl Iterator<Item = ProgramHeader>) -> Result<(), ElfError> {
    let mut ranges: [(u64, u64); MAX_PROGRAM_HEADERS] = [(0, 0); MAX_PROGRAM_HEADERS];
    let mut count = 0usize;

    while let Some(segment) = segments.next() {
        if count >= MAX_PROGRAM_HEADERS {
            // Unreachable: `parse` already refuses an image with more than
            // `MAX_PROGRAM_HEADERS` program headers, so the iterator cannot
            // yield more than that many loadable segments.
            break;
        }

        let range = (segment.p_vaddr, segment.vaddr_end());

        for &(start, end) in ranges[..count].iter() {
            if range.0 < end && start < range.1 {
                return Err(ElfError::OverlappingSegments);
            }
        }

        ranges[count] = range;
        count += 1;
    }

    Ok(())
}

/// Whether `address` is canonical on a 48-bit virtual address space.
///
/// Bit 47 is the last addressable bit, so the upper sixteen bits must either
/// all replicate it or all be zero. A segment whose address is not canonical
/// would fault the moment it was mapped, so this is checked before anything is
/// mapped at all.
pub const fn is_canonical(address: u64) -> bool {
    let sign = (address >> 48) & 0xFFFF;
    sign == 0 || sign == 0xFFFF
}
