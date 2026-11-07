//! Loading the kernel ELF and initramfs off the ESP.
//!
//! # The file path
//!
//! The boot manager is itself a UEFI image on the ESP. Its [`LoadedImage`]
//! protocol exposes the device handle it was loaded from; opening
//! [`SimpleFileSystem`] on that handle and walking `\EFI\NovaOS\*` finds the
//! payloads next to the menu binary itself.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::RegularFile;
use uefi::proto::media::file::{File, FileAttribute, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{BootServices, ScopedProtocol};
use uefi::{CString16, Handle, Status};

use nova_elf::Elf;

/// `\EFI\NovaOS\nova-kernel.elf` on the ESP.
#[allow(dead_code)]
pub const KERNEL_PATH: &str = "\\EFI\\NovaOS\\nova-kernel.elf";
/// `\EFI\NovaOS\initramfs` on the ESP.
#[allow(dead_code)]
pub const INITRAMFS_PATH: &str = "\\EFI\\NovaOS\\initramfs";

/// A loaded ELF segment, ready to be copied into place.
pub struct LoadedSegment {
    /// Destination physical address (`p_paddr`).
    pub paddr: u64,
    /// Number of bytes to copy from the file (`p_filesz`).
    pub filesz: u64,
    /// Total memory size (`p_memsz`, >= filesz; the rest is zeroed).
    pub memsz: u64,
    /// Source bytes in the ELF file image.
    pub data: Vec<u8>,
}

/// Page size in bytes.
pub const PAGE_SIZE: u64 = 0x1000;

/// Open the ESP's filesystem from the boot manager's own image handle.
pub fn open_esp<'a>(
    bs: &'a BootServices,
    handle: Handle,
) -> Result<ScopedProtocol<'a, SimpleFileSystem>, Status> {
    let loaded = bs
        .open_protocol_exclusive::<LoadedImage>(handle)
        .map_err(|error| error.status())?;
    let device = loaded.device().ok_or(Status::NOT_FOUND)?;
    let fs = bs
        .open_protocol_exclusive::<SimpleFileSystem>(device)
        .map_err(|error| error.status())?;
    Ok(fs)
}

/// Open a file on the ESP by its UCS-2 path.
fn open_file(fs: &mut SimpleFileSystem, path: &str) -> Result<RegularFile, Status> {
    let cstr = CString16::try_from(path).map_err(|_| Status::INVALID_PARAMETER)?;
    let mut dir = fs.open_volume().map_err(|error| error.status())?;
    let handle = dir
        .open(&cstr, FileMode::Read, FileAttribute::empty())
        .map_err(|error| error.status())?;
    match handle.into_type().map_err(|error| error.status())? {
        uefi::proto::media::file::FileType::Regular(regular) => Ok(regular),
        _ => Err(Status::INVALID_PARAMETER),
    }
}

/// Read an entire file into a `Vec`.
///
/// A generous fixed buffer is used instead of querying the file size first,
/// because the UEFI `File::get_boxed_info` path is not needed here: both the
/// kernel ELF and the initramfs are bounded in size (the initramfs by the
/// kernel's `INITRAMFS_CAPACITY`, the kernel ELF by a few mebibytes).
pub fn read_file(fs: &mut SimpleFileSystem, path: &str) -> Result<Vec<u8>, Status> {
    let mut file = open_file(fs, path)?;

    const MAX_FILE: usize = 32 * 1024 * 1024; // 32 MiB is enough for both files
    let mut data = vec![0u8; MAX_FILE];
    let mut pos = 0usize;
    while pos < MAX_FILE {
        let chunk = &mut data[pos..];
        let n = file.read(chunk).map_err(|error| error.status())?;
        if n == 0 {
            break;
        }
        pos += n;
    }
    data.truncate(pos);
    Ok(data)
}

/// Parse the ELF and collect every `PT_LOAD` segment.
///
/// The kernel ELF is linked at a high VMA but must be copied to its physical
/// LMA (`0x00100000`). Segment `p_paddr` names the physical destination.
pub fn parse_kernel_segments(elf_data: &[u8]) -> Result<Vec<LoadedSegment>, String> {
    let elf = Elf::parse(elf_data).map_err(|error| format!("ELF parse failed: {error}"))?;

    if elf.header().e_machine != 62 {
        return Err(format!(
            "not an x86-64 ELF: e_machine = {}",
            elf.header().e_machine
        ));
    }

    let mut segments = Vec::new();
    for ph in elf.loadable_segments() {
        let paddr = ph.p_paddr;
        let filesz = ph.p_filesz as usize;
        let memsz = ph.p_memsz as usize;
        if memsz < filesz {
            return Err(format!(
                "segment at {paddr:#x}: memsz {memsz} < filesz {filesz}"
            ));
        }
        let offset = ph.p_offset as usize;
        let end = offset
            .checked_add(filesz)
            .ok_or_else(|| String::from("segment offset overflow"))?;
        if end > elf_data.len() {
            return Err(format!(
                "segment at {paddr:#x}: file bytes {offset}..{end} exceed ELF size {}",
                elf_data.len()
            ));
        }
        let data = elf_data[offset..end].to_vec();
        segments.push(LoadedSegment {
            paddr,
            filesz: filesz as u64,
            memsz: memsz as u64,
            data,
        });
    }

    if segments.is_empty() {
        return Err(String::from("ELF has no loadable segments"));
    }
    Ok(segments)
}

/// The entry point virtual address from the ELF header.
pub fn kernel_entry_vaddr(elf_data: &[u8]) -> Result<u64, String> {
    let elf = Elf::parse(elf_data).map_err(|error| format!("ELF parse failed: {error}"))?;
    Ok(elf.header().e_entry)
}

/// Total physical pages spanned by all loadable segments (rounded up).
pub fn kernel_page_count(segments: &[LoadedSegment]) -> u64 {
    let mut last = 0u64;
    for seg in segments {
        let end = seg.paddr + seg.memsz;
        if end > last {
            last = end;
        }
    }
    last.div_ceil(PAGE_SIZE)
}
