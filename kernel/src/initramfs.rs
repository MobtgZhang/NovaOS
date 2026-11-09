//! A deterministic, bounded initramfs archive.

use crate::klog;
use crate::logger::Level;

pub const MAGIC: &[u8; 8] = b"NOVAFS1\0";
pub const HEADER_SIZE: usize = 64;
/// The maximum number of ELFs the archive may hold.
///
/// Each slot is a parsed pointer pair into the loaded initramfs; the boot
/// services plus future desktop apps fit in sixteen, and the nightly kernel
/// test suite asserts the archive it builds stays well under it.
pub const MAX_FILES: usize = 16;

#[derive(Clone, Copy)]
pub struct Entry {
    pub name: &'static [u8],
    pub data: &'static [u8],
}

pub struct Archive {
    entries: [Option<Entry>; MAX_FILES],
    len: usize,
}

impl Archive {
    /// Walk the entries in archive order, for diagnostics and for the
    /// boot-seeded inventory file.
    pub fn iter(&self) -> impl Iterator<Item = Entry> + '_ {
        self.entries[..self.len].iter().flatten().copied()
    }

    pub fn parse(bytes: &'static [u8]) -> Result<Self, &'static str> {
        if bytes.len() < HEADER_SIZE || &bytes[..8] != MAGIC {
            return Err("bad-initramfs-magic");
        }

        let mut archive = Self {
            entries: [None; MAX_FILES],
            len: 0,
        };
        let mut offset = 0usize;

        while offset + HEADER_SIZE <= bytes.len() {
            let header = &bytes[offset..offset + HEADER_SIZE];
            let name_end = header[8..40]
                .iter()
                .position(|byte| *byte == 0)
                .ok_or("unterminated-initramfs-name")?;
            let size = u64::from_le_bytes(header[40..48].try_into().unwrap()) as usize;
            let data_offset = offset
                .checked_add(HEADER_SIZE)
                .ok_or("initramfs-offset-overflow")?;
            let data_end = data_offset
                .checked_add(size)
                .ok_or("initramfs-size-overflow")?;
            if data_end > bytes.len() {
                return Err("initramfs-entry-past-end");
            }
            if archive.len == MAX_FILES {
                return Err("initramfs-file-quota");
            }

            archive.entries[archive.len] = Some(Entry {
                name: &header[8..8 + name_end],
                data: &bytes[data_offset..data_end],
            });
            archive.len += 1;
            offset = data_end;
        }

        if offset != bytes.len() {
            return Err("initramfs-trailing-bytes");
        }

        klog!(Level::Debug, "initramfs: {} entries", archive.len);
        Ok(archive)
    }

    pub fn get(&self, name: &[u8]) -> Option<&'static [u8]> {
        self.entries[..self.len]
            .iter()
            .flatten()
            .find(|entry| entry.name == name)
            .map(|entry| entry.data)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}
