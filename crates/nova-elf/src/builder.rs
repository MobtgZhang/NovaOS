//! A minimal ELF64 image builder.
//!
//! This exists so the validation rules can be tested one at a time. Each rule
//! needs an image that is *valid in every respect except that one*, and
//! producing such an image by hand means writing 64 bytes of header and 56
//! bytes per program header in the right order — which is exactly the kind of
//! work that produces a test that passes for the wrong reason.
//!
//! # No heap, anywhere
//!
//! The builder uses a fixed-capacity byte array rather than a `Vec`. That is
//! not a stylistic choice: the crate is `no_std` and is compiled *into the
//! kernel*, where no allocator exists. A builder that needed one could not be
//! part of the same crate as the parser, and splitting it into a second crate
//! would mean the kernel's dependency graph grew a crate that exists only for
//! its tests.

use crate::{
    ELF_HEADER_SIZE, ELF_MAGIC, ELFCLASS64, ELFDATA2LSB, EM_X86_64, ET_EXEC, EV_CURRENT, PF_R,
    PF_W, PF_X, PROGRAM_HEADER_SIZE,
};

/// Maximum size of a built image, in bytes.
pub const MAX_IMAGE_BYTES: usize = 64 * 1024;

/// Maximum number of segments in a built image.
pub const MAX_SEGMENTS: usize = 16;

/// One segment to place in the image.
#[derive(Clone, Copy)]
pub struct Segment {
    /// Virtual address the segment is loaded at.
    pub vaddr: u64,
    /// Bytes read from the file.
    pub bytes: &'static [u8],
    /// Bytes occupied in memory. Zero means "the same as `bytes`".
    pub memsz: u64,
    /// Segment flags.
    pub flags: u32,
    /// Alignment. Zero means page-aligned.
    pub align: u64,
}

impl Segment {
    /// A read-only segment whose memory size matches its file size.
    pub const fn ro(vaddr: u64, bytes: &'static [u8]) -> Self {
        Self {
            vaddr,
            bytes,
            memsz: 0,
            flags: PF_R,
            align: 0,
        }
    }

    /// A read-write segment whose memory size matches its file size.
    pub const fn rw(vaddr: u64, bytes: &'static [u8]) -> Self {
        Self {
            vaddr,
            bytes,
            memsz: 0,
            flags: PF_R | PF_W,
            align: 0,
        }
    }

    /// A read-execute segment whose memory size matches its file size.
    pub const fn rx(vaddr: u64, bytes: &'static [u8]) -> Self {
        Self {
            vaddr,
            bytes,
            memsz: 0,
            flags: PF_R | PF_X,
            align: 0,
        }
    }

    /// Override the in-memory size, to model a `.bss`.
    pub const fn with_memsz(mut self, memsz: u64) -> Self {
        self.memsz = memsz;
        self
    }

    /// Override the alignment.
    pub const fn with_align(mut self, align: u64) -> Self {
        self.align = align;
        self
    }

    /// The effective in-memory size.
    pub const fn effective_memsz(&self) -> u64 {
        if self.memsz == 0 {
            self.bytes.len() as u64
        } else {
            self.memsz
        }
    }
}

/// A built image: bytes plus the number of them that are meaningful.
#[derive(Clone)]
pub struct BuiltElf {
    /// The image's bytes.
    pub bytes: [u8; MAX_IMAGE_BYTES],
    /// How many of those bytes are part of the image.
    pub len: usize,
}

impl BuiltElf {
    /// The image as a slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Builds an ELF64 image.
///
/// The builder is deliberately dumb: it lays out the header, the program
/// header table, and the segment contents, and lets the caller corrupt any
/// field afterwards. That is what makes "one test per rejection rule" possible
/// — the image is valid, and then exactly one thing is changed.
pub struct ElfBuilder {
    /// `e_type`.
    pub e_type: u16,
    /// `e_entry`.
    pub e_entry: u64,
    /// `e_phentsize`.
    pub e_phentsize: u16,
    /// `e_ehsize`.
    pub e_ehsize: u16,
    /// `e_machine`.
    pub e_machine: u16,
    /// `e_version`.
    pub e_version: u32,
    /// `EI_CLASS`.
    pub ei_class: u8,
    /// `EI_DATA`.
    pub ei_data: u8,
    /// `EI_VERSION`.
    pub ei_version: u8,
    /// The magic number, so a test can corrupt it.
    pub magic: [u8; 4],
    /// The program headers.
    pub segments: [Option<Segment>; MAX_SEGMENTS],
}

impl Default for ElfBuilder {
    fn default() -> Self {
        Self {
            e_type: ET_EXEC,
            e_entry: 0,
            e_phentsize: PROGRAM_HEADER_SIZE as u16,
            e_ehsize: ELF_HEADER_SIZE as u16,
            e_machine: EM_X86_64,
            e_version: 1,
            ei_class: ELFCLASS64,
            ei_data: ELFDATA2LSB,
            ei_version: EV_CURRENT,
            magic: ELF_MAGIC,
            segments: [None; MAX_SEGMENTS],
        }
    }
}

impl ElfBuilder {
    /// Start a new image.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the entry point.
    pub fn entry(mut self, entry: u64) -> Self {
        self.e_entry = entry;
        self
    }

    /// Set the image type.
    pub fn kind(mut self, kind: u16) -> Self {
        self.e_type = kind;
        self
    }

    /// Add a segment.
    ///
    /// Panics if more than [`MAX_SEGMENTS`] are added, which a test never does
    /// and which would otherwise silently drop one.
    pub fn segment(mut self, segment: Segment) -> Self {
        let slot = self
            .segments
            .iter()
            .position(|existing| existing.is_none())
            .expect("an image may not have more segments than MAX_SEGMENTS");
        self.segments[slot] = Some(segment);
        self
    }

    /// Build the image.
    pub fn build(&self) -> BuiltElf {
        let phnum = self.segments.iter().filter(|slot| slot.is_some()).count() as u16;
        let phoff = ELF_HEADER_SIZE as u64;
        let table_end = phoff + (phnum as u64) * PROGRAM_HEADER_SIZE as u64;

        // The segment contents start on the next page boundary, which is where a
        // real linker's `.text` starts and what keeps every segment's
        // `p_offset` congruent to its `p_vaddr` modulo the page size.
        let contents_start = align_up(table_end, 0x1000);

        let mut image = [0u8; MAX_IMAGE_BYTES];
        let mut len = contents_start as usize;

        // Header.
        image[0..4].copy_from_slice(&self.magic);
        image[4] = self.ei_class;
        image[5] = self.ei_data;
        image[6] = self.ei_version;
        image[7] = 0; // `EI_OSABI`: System V.
        image[16..18].copy_from_slice(&self.e_type.to_le_bytes());
        image[18..20].copy_from_slice(&self.e_machine.to_le_bytes());
        image[20..24].copy_from_slice(&self.e_version.to_le_bytes());
        image[24..32].copy_from_slice(&self.e_entry.to_le_bytes());
        image[32..40].copy_from_slice(&phoff.to_le_bytes());
        image[40..48].copy_from_slice(&0u64.to_le_bytes()); // `e_shoff`
        image[48..52].copy_from_slice(&0u32.to_le_bytes()); // `e_flags`
        image[52..54].copy_from_slice(&self.e_ehsize.to_le_bytes());
        image[54..56].copy_from_slice(&self.e_phentsize.to_le_bytes());
        image[56..58].copy_from_slice(&phnum.to_le_bytes());
        image[58..60].copy_from_slice(&0u16.to_le_bytes()); // `e_shentsize`
        image[60..62].copy_from_slice(&0u16.to_le_bytes()); // `e_shnum`
        image[62..64].copy_from_slice(&0u16.to_le_bytes()); // `e_shstrndx`

        // Program headers and contents.
        let mut cursor = contents_start;
        let mut index = 0usize;
        while index < MAX_SEGMENTS {
            let Some(segment) = self.segments[index] else {
                index += 1;
                continue;
            };

            let offset = cursor;
            let memsz = segment.effective_memsz();
            let align = if segment.align == 0 {
                0x1000
            } else {
                segment.align
            };

            let header_start = ELF_HEADER_SIZE + index * PROGRAM_HEADER_SIZE;
            image[header_start..header_start + 4].copy_from_slice(&1u32.to_le_bytes()); // `PT_LOAD`
            image[header_start + 4..header_start + 8].copy_from_slice(&segment.flags.to_le_bytes());
            image[header_start + 8..header_start + 16].copy_from_slice(&offset.to_le_bytes());
            image[header_start + 16..header_start + 24]
                .copy_from_slice(&segment.vaddr.to_le_bytes());
            image[header_start + 24..header_start + 32]
                .copy_from_slice(&segment.vaddr.to_le_bytes());
            image[header_start + 32..header_start + 40]
                .copy_from_slice(&(segment.bytes.len() as u64).to_le_bytes());
            image[header_start + 40..header_start + 48].copy_from_slice(&memsz.to_le_bytes());
            image[header_start + 48..header_start + 56].copy_from_slice(&align.to_le_bytes());

            let content_end = (cursor as usize) + segment.bytes.len();
            image[(cursor as usize)..content_end].copy_from_slice(segment.bytes);
            len = len.max(content_end);

            cursor = align_up(cursor + segment.bytes.len() as u64, 0x1000);
            index += 1;
        }

        BuiltElf { bytes: image, len }
    }
}

/// Round `value` up to a multiple of `align`.
fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}
