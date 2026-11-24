//! Allocation-free discovery of RAM and fixed reservations in a version 17 DTB.

use core::{num::NonZeroU32, ops::Range};

pub const HEADER_SIZE: usize = 40;
pub const MAX_SIZE: usize = 2 * 1024 * 1024;
const MAX_RANGES: usize = super::MAX_MEMORY_REGIONS;
const MAX_CPUS: usize = 64;

pub struct MemoryMap {
    ram: Ranges,
    reserved: Ranges,
    allocated: Ranges,
    initrd: Option<Range<u64>>,
    timebase_frequency: Option<NonZeroU32>,
    cpu_ids: [u64; MAX_CPUS],
    cpu_count: usize,
}

struct Ranges {
    entries: [Option<(u64, u64)>; MAX_RANGES],
    len: usize,
}

impl Ranges {
    const fn new() -> Self {
        Self {
            entries: [None; MAX_RANGES],
            len: 0,
        }
    }

    fn push(&mut self, start: u64, size: u64) -> Option<()> {
        if size != 0 {
            *self.entries.get_mut(self.len)? = Some((start, start.checked_add(size)?));
            self.len += 1;
        }
        Some(())
    }

    fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.entries[..self.len].iter().flatten().copied()
    }

    fn remove(&mut self, start: u64, end: u64) -> Option<()> {
        let index = self.entries[..self.len].iter().position(|entry| *entry == Some((start, end)))?;
        self.len -= 1;
        self.entries[index] = self.entries[self.len].take();
        self.entries[self.len] = None;
        Some(())
    }
}

/// Validate the magic, version and bounded length before opening the full blob.
pub fn total_size(header: &[u8]) -> Option<usize> {
    let size = word(header, 4)? as usize;
    (word(header, 0)? == 0xd00d_feed
        && word(header, 20)? >= 17
        && word(header, 24)? <= 17
        && (HEADER_SIZE..=MAX_SIZE).contains(&size))
    .then_some(size)
}

impl MemoryMap {
    /// Parse a DTB and reserve the DTB itself. Returns `None` on malformed
    /// input, overflowing ranges, unsupported cell widths or exhausted storage.
    pub fn from_dtb(bytes: &[u8], physical: u64) -> Option<Self> {
        let bytes = bytes.get(..total_size(bytes)?)?;
        let structure = header_range(bytes, 8, 36)?;
        let strings = header_range(bytes, 12, 32)?;
        let mut offset = word(bytes, 16)? as usize;
        if offset < HEADER_SIZE
            || offset % 8 != 0
            || structure.start % 4 != 0
            || offset > structure.start
            || structure.end > strings.start
        {
            return None;
        }
        let mut map = Self {
            ram: Ranges::new(),
            reserved: Ranges::new(),
            allocated: Ranges::new(),
            initrd: None,
            timebase_frequency: None,
            cpu_ids: [0; MAX_CPUS],
            cpu_count: 0,
        };
        map.reserved.push(physical, bytes.len() as u64)?;
        let reservations = bytes.get(..structure.start)?;
        loop {
            let start = wide(reservations, offset)?;
            let size = wide(reservations, offset + 8)?;
            if start == 0 && size == 0 {
                break;
            }
            map.reserved.push(start, size)?;
            offset += 16;
        }
        map.read_nodes(bytes.get(structure)?, bytes.get(strings)?)?;
        (map.ram.len != 0).then_some(map)
    }

    /// Return the firmware-provided initrd span when it lies entirely in RAM.
    pub fn initrd(&self) -> Option<Range<u64>> {
        let initrd = self.initrd.clone()?;
        self.ram.iter().any(|(start, end)| start <= initrd.start && initrd.end <= end).then_some(initrd)
    }

    pub fn timebase_frequency(&self) -> Option<NonZeroU32> {
        self.timebase_frequency
    }

    pub fn cpu_ids(&self) -> &[u64] {
        &self.cpu_ids[..self.cpu_count]
    }

    /// Register an initrd supplied separately from the DTB.
    pub fn reserve_initrd(&mut self, span: Range<u64>) -> Option<()> {
        if span.is_empty() || self.initrd.is_some()
            || !self.ram.iter().any(|(start, end)| start <= span.start && span.end <= end)
            || self.allocated.iter().any(|(start, end)| start < span.end && span.start < end)
        {
            return None;
        }
        self.reserved.push(span.start, span.end - span.start)?;
        self.initrd = Some(span);
        Some(())
    }

    /// Allocate and reserve a power-of-two-aligned span in the RAM bank
    /// containing `image`. Keep early allocations above the loaded image.
    pub fn allocate_after_image(
        &mut self,
        image: Range<u64>,
        size: u64,
        align: u64,
    ) -> Option<u64> {
        if image.is_empty() || size == 0 || !align.is_power_of_two() {
            return None;
        }
        let (_, bank_end) = self
            .ram
            .iter()
            .find(|&(start, end)| start <= image.start && image.end <= end)?;
        let mut candidate = image.end.checked_next_multiple_of(align)?;
        loop {
            let end = candidate.checked_add(size)?;
            if end > bank_end {
                return None;
            }
            let overlaps = self
                .reserved
                .iter()
                .chain(self.allocated.iter())
                .filter(|&(start, stop)| start < end && candidate < stop);
            if let Some(stop) = overlaps.map(|(_, stop)| stop).max() {
                candidate = stop.checked_next_multiple_of(align)?;
                continue;
            }
            self.allocated.push(candidate, size)?;
            return Some(candidate);
        }
    }

    /// Release exactly one earlier allocation. Firmware reservations cannot be released.
    pub fn release_boot_allocation(&mut self, start: u64, size: u64) -> Option<()> {
        self.allocated.remove(start, start.checked_add(size)?)
    }

    fn read_nodes(&mut self, mut structure: &[u8], strings: &[u8]) -> Option<()> {
        let mut depth = 0_usize;
        let mut saw_root = false;
        let mut root_cells = (2, 1);
        let mut reserved_cells = (2, 1);
        let mut reserved_node = false;
        let mut node = Node::default();
        let mut chosen = false;
        let mut cpus = false;
        let mut initrd = (None, None);
        while !structure.is_empty() {
            let token = word(structure, 0)?;
            structure = structure.get(4..)?;
            match token {
                1 => {
                    let name = cstr(structure)?;
                    structure = structure.get((name.len() + 1).next_multiple_of(4)..)?;
                    depth += 1;
                    match depth {
                        1 if !saw_root && name.is_empty() => saw_root = true,
                        1 => return None,
                        2 => {
                            reserved_node = name == b"reserved-memory";
                            chosen = name == b"chosen";
                            cpus = name == b"cpus";
                            reserved_cells = root_cells;
                            node = Node::default();
                        }
                        3 if reserved_node || cpus => node = Node::default(),
                        _ => {}
                    }
                }
                2 => {
                    match depth {
                        0 => return None,
                        3 if reserved_node && node.enabled => {
                            if let Some(reg) = node.reg {
                                read_reg(reg, reserved_cells, &mut self.reserved)?;
                            } else if node.dynamic {
                                return None;
                            }
                        }
                        3 if cpus && node.enabled && node.cpu => {
                            let id = cells_value(node.reg?)?;
                            if self.cpu_ids().contains(&id) {
                                return None;
                            }
                            *self.cpu_ids.get_mut(self.cpu_count)? = id;
                            self.cpu_count += 1;
                        }
                        2 if !reserved_node && node.enabled && node.memory => {
                            read_reg(node.reg?, root_cells, &mut self.ram)?;
                        }
                        _ => {}
                    }
                    depth -= 1;
                }
                3 => {
                    if depth == 0 {
                        return None;
                    }
                    let len = word(structure, 0)? as usize;
                    let name = cstr(strings.get(word(structure, 4)? as usize..)?)?;
                    structure = structure.get(8..)?;
                    let value = structure.get(..len)?;
                    structure = structure.get(len.checked_next_multiple_of(4)?..)?;
                    if depth == 1 || (depth == 2 && reserved_node) {
                        let cells = if depth == 1 {
                            &mut root_cells
                        } else {
                            &mut reserved_cells
                        };
                        match name {
                            b"#address-cells" => cells.0 = word(value, 0)? as usize,
                            b"#size-cells" => cells.1 = word(value, 0)? as usize,
                            b"ranges" if depth == 2 && !value.is_empty() => return None,
                            _ => {}
                        }
                    } else if depth == 2 || (depth == 3 && (reserved_node || cpus)) {
                        match name {
                            b"device_type" => {
                                let kind = cstr(value)?;
                                node.memory = kind == b"memory";
                                node.cpu = kind == b"cpu";
                            }
                            b"status" => node.enabled = matches!(cstr(value)?, b"ok" | b"okay"),
                            b"reg" => node.reg = Some(value),
                            b"size" => node.dynamic = true,
                            b"linux,initrd-start" if depth == 2 && chosen => {
                                initrd.0 = Some(cells_value(value)?)
                            }
                            b"linux,initrd-end" if depth == 2 && chosen => {
                                initrd.1 = Some(cells_value(value)?)
                            }
                            b"timebase-frequency" if depth == 2 && cpus => {
                                if value.len() != 4 || self.timebase_frequency.is_some() {
                                    return None;
                                }
                                self.timebase_frequency = Some(NonZeroU32::new(word(value, 0)?)?);
                            }
                            _ => {}
                        }
                    }
                }
                4 => {}
                9 if depth == 0 && saw_root => {
                    match initrd {
                        (Some(start), Some(end)) if start < end => {
                            self.reserved.push(start, end - start)?;
                            self.initrd = Some(start..end);
                        }
                        (None, None) => {}
                        _ => return None,
                    }
                    return Some(());
                }
                _ => return None,
            }
        }
        None
    }
}

struct Node<'a> {
    enabled: bool,
    memory: bool,
    cpu: bool,
    dynamic: bool,
    reg: Option<&'a [u8]>,
}

impl Default for Node<'_> {
    fn default() -> Self {
        Self {
            enabled: true,
            memory: false,
            cpu: false,
            dynamic: false,
            reg: None,
        }
    }
}

fn read_reg(
    reg: &[u8],
    (address_cells, size_cells): (usize, usize),
    output: &mut Ranges,
) -> Option<()> {
    if !matches!(address_cells, 1 | 2) || !matches!(size_cells, 1 | 2) {
        return None;
    }
    let address_bytes = address_cells * 4;
    let mut ranges = reg.chunks_exact(address_bytes + size_cells * 4);
    if !ranges.remainder().is_empty() {
        return None;
    }
    ranges.try_for_each(|row| {
        let (address, size) = row.split_at(address_bytes);
        output.push(cells_value(address)?, cells_value(size)?)
    })
}

fn cstr(bytes: &[u8]) -> Option<&[u8]> {
    bytes
        .split_at_checked(bytes.iter().position(|byte| *byte == 0)?)
        .map(|(name, _)| name)
}

fn word(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..)?.first_chunk().copied()?,
    ))
}

fn wide(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        bytes.get(offset..)?.first_chunk().copied()?,
    ))
}

fn cells_value(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        4 => word(bytes, 0).map(u64::from),
        8 => wide(bytes, 0),
        _ => None,
    }
}

fn header_range(bytes: &[u8], offset: usize, length: usize) -> Option<Range<usize>> {
    let start = word(bytes, offset)? as usize;
    let end = start.checked_add(word(bytes, length)? as usize)?;
    Some(start..end)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    fn append_word(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend(value.to_be_bytes());
    }

    fn begin_node(bytes: &mut Vec<u8>, name: &[u8]) {
        append_word(bytes, 1);
        bytes.extend(name);
        bytes.push(0);
        bytes.resize(bytes.len().next_multiple_of(4), 0);
    }

    fn prop(bytes: &mut Vec<u8>, name: u32, value: &[u8]) {
        append_word(bytes, 3);
        append_word(bytes, value.len() as u32);
        append_word(bytes, name);
        bytes.extend(value);
        bytes.resize(bytes.len().next_multiple_of(4), 0);
    }

    fn tree() -> Vec<u8> {
        tree_with_timebase(None)
    }

    fn tree_with_timebase(frequency: Option<&[u8]>) -> Vec<u8> {
        let names = b"device_type\0reg\0linux,initrd-start\0linux,initrd-end\0timebase-frequency\0status\0";
        let mut nodes = Vec::new();
        begin_node(&mut nodes, b"");
        begin_node(&mut nodes, b"memory@8000");
        prop(&mut nodes, 0, b"memory\0");
        prop(&mut nodes, 12, &[0, 0, 0, 0, 0, 0, 0x80, 0, 0, 1, 0, 0]);
        append_word(&mut nodes, 2);
        begin_node(&mut nodes, b"reserved-memory");
        begin_node(&mut nodes, b"hole@b000");
        prop(&mut nodes, 12, &[0, 0, 0, 0, 0, 0, 0xb0, 0, 0, 0, 0x20, 0]);
        append_word(&mut nodes, 2);
        append_word(&mut nodes, 2);
        begin_node(&mut nodes, b"chosen");
        prop(&mut nodes, 16, &0xd000_u32.to_be_bytes());
        prop(&mut nodes, 35, &0xd100_u32.to_be_bytes());
        append_word(&mut nodes, 2);
        if let Some(frequency) = frequency {
            begin_node(&mut nodes, b"cpus");
            prop(&mut nodes, 52, frequency);
            for (name, id, disabled) in [(b"cpu@3", 3_u64, false), (b"cpu@7", 7, true)] {
                begin_node(&mut nodes, name);
                prop(&mut nodes, 0, b"cpu\0");
                prop(&mut nodes, 12, &id.to_be_bytes());
                if disabled {
                    prop(&mut nodes, 71, b"disabled\0");
                }
                append_word(&mut nodes, 2);
            }
            append_word(&mut nodes, 2);
        }
        append_word(&mut nodes, 2);
        append_word(&mut nodes, 9);
        let structure_offset = HEADER_SIZE + 32;
        let strings_offset = structure_offset + nodes.len();
        let mut result = Vec::new();
        for value in [0xd00d_feed, (strings_offset + names.len()) as u32, structure_offset as u32,
            strings_offset as u32, HEADER_SIZE as u32, 17, 16, 0, names.len() as u32,
            nodes.len() as u32] {
            append_word(&mut result, value);
        }
        result.extend(0xa000_u64.to_be_bytes());
        result.extend(0x1000_u64.to_be_bytes());
        result.extend([0; 16]);
        result.extend(nodes);
        result.extend(names);
        result
    }

    #[test]
    fn reads_nonzero_single_cell_cpu_timebase() {
        let bytes = tree_with_timebase(Some(&12_500_000_u32.to_be_bytes()));
        assert_eq!(MemoryMap::from_dtb(&bytes, 0).unwrap().timebase_frequency(), NonZeroU32::new(12_500_000));
        assert_eq!(MemoryMap::from_dtb(&bytes, 0).unwrap().cpu_ids(), &[3]);
        assert_eq!(MemoryMap::from_dtb(&tree(), 0).unwrap().timebase_frequency(), None);
        for invalid in [&[0; 4][..], &[1; 3], &[1; 8]] {
            assert!(MemoryMap::from_dtb(&tree_with_timebase(Some(invalid)), 0).is_none());
        }
    }

    #[test]
    fn skips_header_reservations_reserved_memory_and_initrd() {
        let bytes = tree();
        let mut map = MemoryMap::from_dtb(&bytes, 0xe000).unwrap();
        assert_eq!(map.initrd(), Some(0xd000..0xd100));
        assert_eq!(map.allocate_after_image(0x8000..0xa000, 0x1000, 0x1000), Some(0xf000));
        assert_eq!(map.allocate_after_image(0x8000..0xa000, 0x1000, 0x1000), Some(0x10000));
        assert_eq!(map.allocate_after_image(0x7000..0xa000, 0x1000, 0x1000), None);
    }

    #[test]
    fn reuses_released_boot_frames_and_preserves_firmware_reservations() {
        let bytes = tree();
        let mut map = MemoryMap::from_dtb(&bytes, 0xe000).unwrap();
        let kernel = 0x8000..0xa000;
        assert_eq!(map.allocate_after_image(kernel.clone(), 0x1000, 0x1000), Some(0xf000));
        assert_eq!(map.allocate_after_image(kernel.clone(), 0x1000, 0x1000), Some(0x10000));
        assert_eq!(map.release_boot_allocation(0xa000, 0x1000), None);
        assert_eq!(map.release_boot_allocation(0xf000, 0x2000), None);
        assert_eq!(map.release_boot_allocation(0xf000, 0x1000), Some(()));
        assert_eq!(map.release_boot_allocation(0xf000, 0x1000), None);
        assert_eq!(map.allocate_after_image(kernel, 0x1000, 0x1000), Some(0xf000));
        assert_eq!(map.release_boot_allocation(0x10000, 0x1000), Some(()));
    }

    #[test]
    fn rejects_truncated_and_unterminated_sections() {
        let mut bytes = tree();
        for length in 0..bytes.len() {
            assert!(MemoryMap::from_dtb(&bytes[..length], 0).is_none());
        }
        bytes[HEADER_SIZE + 16..HEADER_SIZE + 32].copy_from_slice(&[0xff; 16]);
        assert!(MemoryMap::from_dtb(&bytes, 0).is_none());
    }
}
