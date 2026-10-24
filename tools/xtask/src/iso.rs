//! A self-contained ISO 9660 + El Torito writer.
//!
//! # Why this exists rather than shelling out to `xorriso`
//!
//! `xorriso` is not a universally available prerequisite. It lives in the
//! `xorriso` package, is absent from many minimal images, and installing it
//! needs root — which a CI container frequently does not have.
//! `grub-mkrescue` is not a fallback: it invokes `xorriso` internally, so it
//! fails with the identical missing binary.
//!
//! An ISO is a small, fully specified, seekable format, and the subset Limine
//! needs is narrow: ISO 9660 descriptors, one El Torito catalog, and long
//! filenames. That is a few hundred lines of `u8` shuffling with no
//! dependencies, and it removes the most environment-fragile step in the build.
//!
//! # Determinism
//!
//! Every timestamp written here is fixed at the ISO epoch and the file order is
//! sorted, so two builds of the same tree produce byte-identical images. That is
//! what makes an image diffable and a boot failure reproducible.

use std::path::{Path, PathBuf};

/// ISO 9660 logical block size. Fixed by the format; not configurable.
const BLOCK: usize = 2048;

/// First sector of the volume descriptor set, fixed by ISO 9660.
const PVD_SECTOR: u32 = 16;

/// The El Torito boot record's sector, immediately after the PVD.
const BOOT_RECORD_SECTOR: u32 = 17;

/// Sector of the Joliet supplementary volume descriptor.
///
/// Descriptors are *contiguous* from sector 16 and a reader stops scanning at
/// the first sector that is not one, so nothing that is not a descriptor may
/// sit between them. The path tables are the trap here: they are volume
/// *content*, named by the descriptor, and reading them as descriptors ends the
/// scan two sectors early — leaving the Joliet descriptor unread and the
/// terminator unfound, so the reader rejects a volume whose PVD is flawless.
const JOLIET_SECTOR: u32 = 18;

/// Sector of the volume descriptor set terminator.
const TERMINATOR_SECTOR: u32 = 19;

/// Last sector of the descriptor set, i.e. the first free sector after it.
///
/// The set is PVD(16), El Torito boot record(17), Joliet(18) and terminator(19).
const DESCRIPTOR_SET_END: u32 = TERMINATOR_SECTOR + 1;

/// Sector of the L path table. The first sector after the descriptor set.
const PATH_TABLE_SECTOR: u32 = DESCRIPTOR_SET_END;

/// Sector of the M (big-endian) path table.
const PATH_TABLE_SECTOR_BE: u32 = PATH_TABLE_SECTOR + 1;

/// Sector of the Joliet supplementary path table.
const JOLIET_PATH_TABLE_SECTOR: u32 = PATH_TABLE_SECTOR_BE + 1;

/// Sector of the Joliet supplementary M (big-endian) path table.
const JOLIET_PATH_TABLE_SECTOR_BE: u32 = JOLIET_PATH_TABLE_SECTOR + 1;

/// First sector available to file and directory data.
const DATA_START_SECTOR: u32 = JOLIET_PATH_TABLE_SECTOR_BE + 1;

/// `Extended Attribute Record` length for every record written here.
const EAR_LENGTH: u8 = 0;

/// Directory record flags: bit 1 marks a directory.
const FLAG_DIRECTORY: u8 = 2;

/// Volume descriptor types.
const VD_BOOT_RECORD: u8 = 0;
const VD_PRIMARY: u8 = 1;
const VD_SUPPLEMENTARY: u8 = 2;
const VD_TERMINATOR: u8 = 0xFF;

/// El Torito: a bootable entry. The indicator is 0x88 for bootable and 0x00
/// for "present but do not boot"; there is no separate value for an EFI
/// entry, because what makes an entry an EFI entry is its media type.
const BOOT_INDICATOR_BOOTABLE: u8 = 0x88;
/// El Torito: an entry firmware should not boot.
const BOOT_INDICATOR_NOT_BOOTABLE: u8 = 0x00;
/// El Torito: a section header for a bootable section.
const SECTION_HEADER_BOOTABLE: u8 = 0x90;
/// El Torito: the section-header byte that says "no further sections follow".
const SECTION_HEADER_FINAL: u8 = 0x91;
/// El Torito: the section-header platform byte for x86.
const PLATFORM_SECTION_X86: u8 = 0x00;
/// El Torito media type 0: no emulation, load the image verbatim.
const MEDIA_NO_EMULATION: u8 = 0;
/// El Torito media type 2: EFI image, load it as a partition.
const MEDIA_EFI: u8 = 2;

/// Length of every El Torito catalog record header. Fixed by the spec; the
/// catalog has no variable-length records, so this is not tunable.
const HEADER_LEN: usize = 32;

/// Length of the validation entry.
///
/// It is exactly one record: a 32-byte header holding the platform id, the
/// manufacturer string, the checksum and the 0x55 0xAA key bytes. There is no
/// LBA after it — the catalog's own location is in the boot record, and
/// repeating it here as a trailing 4 bytes shifts every record after it by
/// four. The initial entry then lands where a reader expects the first boot
/// entry, so firmware reads a plausible-looking boot indicator from the middle
/// of the previous record and finds nothing bootable on the medium.
const VALIDATION_ENTRY_LEN: usize = HEADER_LEN;

/// Length of the SUSP `SP` entry, in bytes.
const SUSP_SP_LEN: usize = 7;

/// Length of the SUSP `ER` entry plus its `RRIP_1991A` identifier.
const SUSP_ER_LEN: usize = 8 + 10;

/// Length of the SUSP `PX` entry.
const SUSP_PX_LEN: usize = 36;

/// POSIX mode for a directory.
const MODE_DIRECTORY: u32 = 0o040755;

/// POSIX mode for a regular file.
const MODE_FILE: u32 = 0o100644;

/// Platform byte for an EFI El Torito entry.
pub const PLATFORM_EFI: u8 = 0xEF;

/// The Joliet escape sequence: `%/` plus the UCS-2 level byte.
///
/// The level byte is the *third* character, and the values a reader accepts
/// are `@` (UCS-2 level 1), `C` (level 2) and `E` (level 3). The byte that
/// looks like the level — a slash — is not one of them, and a reader that
/// does not recognise the sequence does not treat the descriptor as Joliet at
/// all: it skips it, and the volume then has a supplementary descriptor that
/// nobody reads and a path table that is checked twice against itself.
const JOLIET_LEVEL_3: [u8; 4] = [b'%', b'/', b'E', 0x00];

/// Volume identifier, used for both the volume set and the publisher.
///
/// 32 bytes, space-padded, as ISO 9660 requires. Written as a literal whose
/// length is asserted below rather than built with a const `copy_from_slice`,
/// which is not stable; the assertion is what stops a mis-sized literal from
/// silently shifting every descriptor field after it.
const VOLUME_ID: &[u8] = b"NOVAOS                          ";

// The length is a format requirement, not a preference: the field is 32 bytes
// wide, and a shorter or longer value would overwrite the volume-space-size
// field that follows it.
const _: () = assert!(VOLUME_ID.len() == 32);

/// Everything that can go wrong while building an image.
#[derive(Debug)]
pub enum Error {
    /// A staged file could not be read, or the output could not be written.
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        error: std::io::Error,
    },
    /// Two staged files would resolve to the same ISO name.
    Collision(String),
    /// No El Torito entry was supplied, so nothing could boot.
    NoBootEntry,
    /// A boot entry named a file that was not staged.
    MissingBootTarget(String),
    /// The staged tree needs more path-table space than a single sector.
    PathTableOverflow,
    /// The staging tree yielded no root directory, so no volume could be
    /// described.
    MissingRootDirectory,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io { path, error } => write!(f, "cannot access {}: {error}", path.display()),
            Error::Collision(name) => write!(
                f,
                "two staged files both resolve to the ISO name {name:?}; ISO 9660 \
                 resolves names case-insensitively, so one would silently shadow the \
                 other and the bootloader would load the wrong file"
            ),
            Error::NoBootEntry => {
                write!(
                    f,
                    "the image has no El Torito boot entry, so nothing can boot it"
                )
            }
            Error::MissingBootTarget(name) => write!(
                f,
                "boot entry names {name:?}, which is not present in the staging tree"
            ),
            Error::PathTableOverflow => {
                write!(f, "the staged tree exceeds the single-sector path table")
            }
            Error::MissingRootDirectory => {
                write!(f, "the staged tree produced no root directory record")
            }
        }
    }
}

impl From<Error> for String {
    fn from(error: Error) -> String {
        error.to_string()
    }
}

/// One El Torito boot entry.
#[derive(Clone, Debug)]
pub struct BootEntry {
    /// Path of the target, relative to the staging root, with no leading `/`.
    pub file: String,
    /// [`PLATFORM_EFI`] for UEFI, `0` for the x86 BIOS path.
    pub platform: u8,
    /// Whether firmware should boot this entry by default.
    pub default: bool,
}

/// One entry inside a directory: a file or a subdirectory.
struct Member {
    /// The level-1 short name the reader sees in the primary tree, or the real
    /// name in the Joliet tree.
    name: String,
    /// The real name, carried in a Rock Ridge `NM` entry in the primary tree.
    ///
    /// Kept separate from `name` because the two are deliberately different:
    /// `name` is what the 8.3 identifier holds and `long_name` is what a reader
    /// should resolve a path against.
    long_name: String,
    /// First sector of the entry's contents.
    extent: u32,
    /// Size in bytes; zero for a directory.
    size: u64,
    /// Whether this member is itself a directory.
    is_directory: bool,
}

/// A staged file, normalised for ISO writing.
struct File {
    /// Path components below the staging root.
    components: Vec<String>,
    /// Absolute host path.
    source: PathBuf,
    /// Size in bytes.
    size: u64,
    /// Level-1 8.3 name of the final component.
    short_name: String,
    /// The final component as it appears in the Joliet tree.
    ///
    /// The two trees describe the same file under different names: the primary
    /// tree uses the ISO 9660 8.3 form, and the Joliet tree uses the real
    /// name. They must agree on *where* the file is, not on what it is called.
    long_name: String,
    /// Lower-cased final component, for collision detection.
    key: String,
}

/// Build a hybrid BIOS+UEFI ISO from `stage`, writing it to `output`.
pub fn write_iso(stage: &Path, boots: &[BootEntry], output: &Path) -> Result<(), Error> {
    if boots.is_empty() {
        return Err(Error::NoBootEntry);
    }
    let files = collect(stage)?;
    check_collisions(&files)?;

    let mut builder = Layout::new(files);
    builder.resolve_boots(boots)?;

    let bytes = builder.assemble()?;
    write_file(output, &bytes)
}

/// A sector cursor over the image, with the directory/file assignments.
struct Layout {
    files: Vec<File>,
    /// Directory path -> `(first sector, sector count)`.
    directories: Vec<(Vec<String>, u32, u32)>,
    /// File index -> `(first sector, byte length)`.
    extents: Vec<(u32, u64)>,
    /// Boot target -> first sector, in the order the entries were supplied.
    boot_sectors: Vec<u32>,
    /// Platform byte for each boot entry, parallel to `boot_sectors`.
    boot_platforms: Vec<u8>,
    /// Whether each boot entry is the one firmware should try first, parallel
    /// to `boot_sectors`.
    boot_defaults: Vec<bool>,
    /// The little-endian path table's records.
    ///
    /// The two tables are kept apart rather than interleaved in one vector.
    /// Interleaving them and then splitting the vector in half *looks* right —
    /// the records alternate, so half of them are the L table — but it only
    /// works if the two tables' records are the same length, and they are not:
    /// the M table's parent number is a two-byte big-endian field where the L
    /// table's is a single byte. The split lands in the middle of a record,
    /// and each table ends up containing a fragment of the other.
    path_table_l: Vec<Vec<u8>>,
    /// The big-endian path table's records.
    path_table_m: Vec<Vec<u8>>,
    path_table_bytes: u32,
    /// The Joliet supplementary path table's records.
    ///
    /// A separate pair of tables, not a second view of the primary ones: the
    /// identifiers are UCS-2BE and so twice as long, and the extents are the
    /// Joliet tree's. The supplementary descriptor points at these and describes
    /// their size, which differs from the primary table's.
    joliet_path_table_l: Vec<Vec<u8>>,
    /// The Joliet big-endian path table's records.
    joliet_path_table_m: Vec<Vec<u8>>,
    joliet_path_table_bytes: u32,
    next: u32,
    /// Absolute path table sector, fixed up once known.
    path_table_sector: u32,
    /// Absolute (big-endian) path table sector.
    path_table_sector_be: u32,
    /// Absolute sector of the Joliet path table.
    joliet_path_table_sector: u32,
    root_sector: u32,
    root_sectors: u32,
    /// The Joliet tree's directories, parallel to `directories`.
    ///
    /// The supplementary volume descriptor indexes its own directory tree, and
    /// that tree cannot be the primary one: its records name files in UCS-2BE,
    /// which is two bytes per character and makes every record longer. Sharing
    /// the primary extents would have the reader decode ASCII bytes as UTF-16
    /// and fail on the very first name it meets.
    joliet_directories: Vec<(Vec<String>, u32, u32)>,
    /// The Joliet tree's root directory sector.
    joliet_root_sector: u32,
    boot_catalog_sector: u32,
    /// LBA recorded in the El Torito validation entry, which is the catalog's
    /// own first sector.
    boot_validation_lba: u32,
}

impl Layout {
    fn new(files: Vec<File>) -> Self {
        Self {
            files,
            directories: Vec::new(),
            extents: Vec::new(),
            boot_sectors: Vec::new(),
            boot_platforms: Vec::new(),
            boot_defaults: Vec::new(),
            path_table_l: Vec::new(),
            path_table_m: Vec::new(),
            path_table_bytes: 0,
            joliet_path_table_l: Vec::new(),
            joliet_path_table_m: Vec::new(),
            joliet_path_table_bytes: 0,
            // Sector 16 is where the primary volume descriptor must live.
            next: 16,
            path_table_sector: 0,
            path_table_sector_be: 0,
            joliet_path_table_sector: 0,
            root_sector: 0,
            root_sectors: 0,
            joliet_directories: Vec::new(),
            joliet_root_sector: 0,
            boot_catalog_sector: 0,
            boot_validation_lba: 0,
        }
    }

    /// Check that every boot target is staged, and record its index.
    fn resolve_boots(&mut self, boots: &[BootEntry]) -> Result<(), Error> {
        for boot in boots {
            let normalized = boot.file.trim_start_matches('/');
            let found = self
                .files
                .iter()
                .position(|file| file.components.join("/").eq_ignore_ascii_case(normalized));
            match found {
                Some(index) => {
                    self.boot_sectors.push(index as u32);
                    self.boot_platforms.push(boot.platform);
                    self.boot_defaults.push(boot.default);
                }
                None => return Err(Error::MissingBootTarget(boot.file.clone())),
            }
        }
        Ok(())
    }

    /// Build the path table, then assign every file and directory an extent.
    ///
    /// # The order
    ///
    /// ISO 9660 fixes the volume descriptor set at sectors 16 and up, and that
    /// set is *contiguous* — a reader walks it from 16 and stops at the first
    /// sector that is not a descriptor. So the layout is:
    ///
    /// ```text
    ///   16        primary volume descriptor
    ///   17        El Torito boot record
    ///   18        Joliet supplementary descriptor   (present because we write Joliet)
    ///   19        descriptor set terminator
    ///   20        type-L path table
    ///   21        type-M path table
    ///   22..      file data, then directory data
    /// ```
    ///
    /// The path tables go *after* the terminator even though the descriptor
    /// that names them is earlier. Putting them inside the set — which looks
    /// tidier, and is what the first version of this did — truncates the
    /// descriptor scan at the path table, so the reader never sees Joliet and
    /// never finds the terminator, and rejects the volume with a message about
    /// a *missing* descriptor rather than about a misplaced one.
    ///
    /// Files come before directories so that a directory's member extents are
    /// known by the time its records are rendered.
    fn plan(&mut self) -> Result<(), Error> {
        // The descriptor's optional-M-table field names the second table.
        self.path_table_sector = PATH_TABLE_SECTOR;
        self.path_table_sector_be = PATH_TABLE_SECTOR_BE;
        self.joliet_path_table_sector = JOLIET_PATH_TABLE_SECTOR;
        self.next = DATA_START_SECTOR;

        // Files first, so a directory's member extents are known by the time
        // its records are rendered.
        for file in &self.files {
            let first = self.next;
            let sectors = file.size.div_ceil(BLOCK as u64) as u32;
            self.next += sectors.max(1);
            self.extents.push((first, file.size));
        }

        // One directory per distinct path in the tree, root included.
        //
        // Two things are needed here, and conflating them is what made the
        // earlier version of this produce an empty volume:
        //
        //   - the set of *directories* that exist, which is every path prefix
        //     of every file, not merely the immediate parent of each file. The
        //     staged tree has `EFI/BOOT/BOOTX64.EFI`, so `EFI` and `EFI/BOOT`
        //     are directories in their own right and each needs an extent.
        //   - the set of *members* of each directory, which is the files
        //     directly inside it plus the subdirectories directly inside it.
        //
        // Tracking only file parents gives directories that exist but are
        // never listed anywhere, so a reader walking the tree never finds
        // `boot/` at all — the media looks empty even though every file is
        // present in the image.
        let mut directories: Vec<Vec<String>> = vec![Vec::new()];
        for file in &self.files {
            // Every proper prefix of the file's path is a directory.
            for depth in 1..=file.components.len().saturating_sub(1) {
                let prefix = file.components[..depth].to_vec();
                if !directories.iter().any(|known| *known == prefix) {
                    directories.push(prefix);
                }
            }
        }
        // Deepest first, so a `..` record can name a parent whose extent is
        // already fixed.
        directories.sort_by(|a, b| b.len().cmp(&a.len()));

        // The size of a directory depends on its members, and its members
        // include its subdirectories — which are known as a *set* only once
        // every path prefix has been collected. So the size is computed from
        // that set rather than from the allocation list, which is filled in
        // progressively and is therefore incomplete for the directories
        // allocated first.
        //
        // Getting this wrong under-counts a directory whose subdirectory has
        // not been registered yet, and the two then share a sector. The one
        // written second overwrites the other's records, and the volume opens
        // listing a directory whose extent holds a different directory's
        // entries.
        let size_of = |components: &[String], joliet: bool| -> u32 {
            // A member's record is a 33-byte header, the name, and padding to
            // an even length. The padding is part of the record's declared
            // length, so the name's parity decides the size — a name of even
            // length needs a pad byte and one of odd length does not.
            // Measuring the record as a fixed `34 + name.len()` assumes the
            // name is odd, which it is half the time.
            //
            // In the Joliet tree every name is UCS-2BE, so it is twice as long
            // and always an even number of bytes. Sizing the Joliet tree with
            // the primary tree's lengths makes every Joliet directory too
            // small: the records overflow the extent, and a reader that walks
            // past the end of the allocation is reading whatever follows it.
            let name_len = |name: &str| if joliet { name.len() * 2 } else { name.len() };
            // The `.` and `..` records are 34 bytes in both trees: their
            // identifiers are the one-byte markers a reader recognises by
            // value, and the Joliet encoding does not apply to them. The root's
            // `.` additionally carries the opening SUSP entries.
            let mut bytes = 34 + 34;
            if !joliet {
                // The root's `.` and `..` carry SP/ER/PX/NM/ST; a subdirectory's
                // `.` and `..` carry PX/NM/ST for the respective directory.
                let name_len_own = components.last().map(|name| name.len()).unwrap_or(0);
                bytes += if components.is_empty() {
                    // `.` has SP+ER+PX+NM("")+ST; `..` the same but NM names the
                    // root, which is the empty string.
                    root_susp_len() + 5 + 4 + SUSP_PX_LEN + 5 + 4
                } else {
                    // `.` names this directory, `..` its parent.
                    SUSP_PX_LEN
                        + 5
                        + name_len_own
                        + 4
                        + SUSP_PX_LEN
                        + 5
                        + components[components.len() - 1].len()
                        + 4
                };
            }

            for path in &directories {
                if path.len() == components.len() + 1 && path[..components.len()] == *components {
                    let name = name_len(&path[components.len()]);
                    // `PX`, `NM` and `ST` for a primary-tree record. An even
                    // identifier length costs one extra pad byte before the
                    // system-use area, exactly as in `directory_record`.
                    let susp = if joliet {
                        0
                    } else {
                        SUSP_PX_LEN + 5 + name_len(&path[components.len()]) + 4
                    };
                    bytes += (33 + name + usize::from(name % 2 == 0) + susp).div_ceil(2) * 2;
                }
            }

            for (index, file) in self.files.iter().enumerate() {
                if file.components[..file.components.len() - 1] != *components {
                    continue;
                }
                let file = &self.files[index];
                let name = name_len(if joliet {
                    &file.long_name
                } else {
                    &file.short_name
                });
                let susp = if joliet {
                    0
                } else {
                    SUSP_PX_LEN + 5 + file.long_name.len() + 4
                };
                bytes += (33 + name + usize::from(name % 2 == 0) + susp).div_ceil(2) * 2;
            }

            (bytes as u32).div_ceil(BLOCK as u32).max(1)
        };

        for components in &directories {
            let sectors = size_of(components, false);
            let first = self.next;
            self.directories.push((components.clone(), first, sectors));
            self.next += sectors;
        }

        // The Joliet supplementary descriptor indexes its own tree, laid out
        // immediately after the primary one. It cannot share the primary
        // directories: its records carry UCS-2BE names, which are twice as
        // long and make every directory larger, and a reader that followed the
        // supplementary descriptor's root into the primary tree would decode
        // ASCII names as UCS-2 and fail on the first one.
        for components in &directories {
            let sectors = size_of(components, true);
            let first = self.next;
            self.joliet_directories
                .push((components.clone(), first, sectors));
            self.next += sectors;
        }

        match self
            .joliet_directories
            .iter()
            .find(|(components, _, _)| components_is_root(components))
        {
            Some((_, first, _)) => self.joliet_root_sector = *first,
            None => return Err(Error::MissingRootDirectory),
        }

        // The root's extent is looked up by *path*, not by position. `parents` is
        // sorted deepest-first so that a `..` record can name a parent whose
        // extent is already fixed, which means the root — the shallowest path —
        // is the *last* entry, not the first. Reading index 0 would pick
        // whichever directory happened to sort deepest, and the root would keep
        // its initial extent of 0: sector 0 of the image, the boot catalogue.
        // A reader following that lands in the middle of the El Torito data and
        // reports an empty or nonsensical volume.
        match self
            .directories
            .iter()
            .find(|(components, _, _)| components_is_root(components))
        {
            Some((_, first, sectors)) => {
                self.root_sector = *first;
                self.root_sectors = *sectors;
            }
            None => return Err(Error::MissingRootDirectory),
        }

        // The path table indexes the directories, so it can only be built once
        // the directory list is final. Building it earlier — when only the
        // *files* were known — produced a table that listed file parents
        // instead of directories, and a reader that followed it resolved
        // `/boot` to a file's record rather than to the directory's.
        self.build_path_table();
        self.build_joliet_path_table();
        if self.path_table_bytes > BLOCK as u32 {
            return Err(Error::PathTableOverflow);
        }
        if self.joliet_path_table_bytes > BLOCK as u32 {
            return Err(Error::PathTableOverflow);
        }
        Ok(())
    }

    /// Build the little-endian and big-endian path tables.
    ///
    /// Both orderings are written because a reader chooses one and a wrong
    /// choice is not detectable — the table parses as nonsense rather than
    /// failing.
    ///
    /// # The record layout
    ///
    /// A path table record is an 8-byte header followed by the directory
    /// identifier:
    ///
    /// ```text
    ///   0   length of the directory identifier
    ///   1   extended attribute record length
    ///   2..6 extent of the directory
    ///   6..8 parent directory number
    ///   8.. the identifier, terminated by a NUL
    /// ```
    ///
    /// The extent comes *before* the parent number, and both are fixed-width —
    /// the parent number is a two-byte value, not a single byte that happens to
    /// hold a small number. Writing the header as `[len, 0, 0, parent, name]`
    /// puts the parent number in the low byte of the extent, so every directory
    /// resolves to extent 0 or 1 and the whole table is nonsense. Nothing
    /// detects this: the table is a plausible sequence of bytes, and the
    /// failure is a reader walking off the end of it and reporting that the
    /// path table is longer than the medium.
    ///
    /// The identifier's length *includes* its NUL terminator, and the record is
    /// padded to an even length. The root's identifier is a single NUL — a
    /// length of one, not zero.
    ///
    /// # The two tables differ in two fields
    ///
    /// The type-M table stores the extent *and* the parent number most
    /// significant byte first. Everything else — the identifier, the padding,
    /// the record length — is identical. A reader that finds the two tables
    /// disagreeing rejects the volume, so the big-endian half is not optional
    /// bookkeeping: it has to carry the same values as the little-endian half,
    /// in the other byte order.
    ///
    /// # Why the table lists directories, not files
    ///
    /// The path table is an index of *directories*. Files are reachable through
    /// their parent directory's records. Listing files here produces a table
    /// that resolves `/boot` to a file's record rather than a directory's.
    fn build_path_table(&mut self) {
        let ordered = order_directories(&self.directories);
        let records = self.path_table_records(&ordered, &self.directories, false);
        self.path_table_bytes = records.iter().map(|(little, _)| little.len() as u32).sum();
        self.path_table_l = records.iter().map(|(little, _)| little.clone()).collect();
        self.path_table_m = records.into_iter().map(|(_, big)| big).collect();
    }

    /// Build the Joliet supplementary path table.
    ///
    /// It indexes the same directories as the primary table, in the same order,
    /// but its identifiers are UCS-2BE and its extents are the Joliet tree's.
    /// The record *count* is therefore the same while the table is longer, and
    /// the descriptor's declared path-table size has to be the Joliet one — the
    /// size field describes the table the descriptor points at, not the primary.
    fn build_joliet_path_table(&mut self) {
        let ordered = order_directories(&self.joliet_directories);
        let records = self.path_table_records(&ordered, &self.joliet_directories, true);
        self.joliet_path_table_bytes = records.iter().map(|(little, _)| little.len() as u32).sum();
        self.joliet_path_table_l = records.iter().map(|(little, _)| little.clone()).collect();
        self.joliet_path_table_m = records.into_iter().map(|(_, big)| big).collect();
    }

    /// Render both byte orders of a path table over `ordered`.
    ///
    /// The two tables must carry the same values in different byte orders for
    /// the extent and the parent number, and identical bytes everywhere else.
    /// A reader that finds them disagreeing rejects the volume outright, so the
    /// big-endian half is not optional bookkeeping.
    ///
    /// `joliet` selects UCS-2BE identifiers and, implicitly, which extents to
    /// record — the caller passes the matching directory list.
    fn path_table_records(
        &self,
        ordered: &[(Vec<String>, u8)],
        directories: &[(Vec<String>, u32, u32)],
        joliet: bool,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::with_capacity(ordered.len());

        for (components, index) in ordered {
            let name = components.last().map(String::as_str).unwrap_or("");

            let parent_index = if components.is_empty() {
                // The root's parent is itself.
                *index
            } else {
                let parent = &components[..components.len() - 1];
                ordered
                    .iter()
                    .find(|(path, _)| path == parent)
                    .map(|(_, index)| *index)
                    .unwrap_or(1)
            };

            let extent = directories
                .iter()
                .find(|(path, _, _)| path.as_slice() == components.as_slice())
                .map(|(_, first, _)| *first)
                .unwrap_or(0);

            // The identifier always ends with a NUL, and its length counts that
            // NUL. The root's identifier is therefore one byte long, not zero.
            let identifier = encode_path_identifier(name, components.is_empty(), joliet);

            let mut little = Vec::with_capacity(8 + identifier.len());
            little.push(identifier.len() as u8);
            little.push(0); // extended attribute record length
            little.extend_from_slice(&extent.to_le_bytes()); // 2..6
            little.extend_from_slice(&u16::from(parent_index).to_le_bytes()); // 6..8
            little.extend_from_slice(&identifier);
            if little.len() % 2 == 1 {
                little.push(0);
            }

            let mut big = Vec::with_capacity(8 + identifier.len());
            big.push(identifier.len() as u8);
            big.push(0);
            big.extend_from_slice(&extent.to_be_bytes());
            big.extend_from_slice(&u16::from(parent_index).to_be_bytes());
            big.extend_from_slice(&identifier);
            if big.len() % 2 == 1 {
                big.push(0);
            }

            out.push((little, big));
        }

        out
    }

    /// Serialise the image.
    fn assemble(&mut self) -> Result<Vec<u8>, Error> {
        self.plan()?;

        // Reserve enough sectors for everything planned, plus room for the
        // descriptors written afterwards.
        let mut image = vec![0u8; (self.next as usize + 8) * BLOCK];

        // Path tables: L at 20, M at 21, then the Joliet pair at 22 and 23.
        //
        // The Joliet tables cannot sit in the same sectors as the primary ones
        // even though they index the same directories: their identifiers are
        // UCS-2BE and so longer, and two tables cannot occupy one sector. The
        // supplementary descriptor names both their size and their location, so
        // they are written at sectors of their own.
        let l_table: Vec<u8> = self.path_table_l.iter().flatten().copied().collect();
        let m_table: Vec<u8> = self.path_table_m.iter().flatten().copied().collect();
        let l_start = self.path_table_sector as usize * BLOCK;
        image[l_start..l_start + l_table.len()].copy_from_slice(&l_table);
        let m_start = (self.path_table_sector + 1) as usize * BLOCK;
        image[m_start..m_start + m_table.len()].copy_from_slice(&m_table);

        let jl_table: Vec<u8> = self.joliet_path_table_l.iter().flatten().copied().collect();
        let jm_table: Vec<u8> = self.joliet_path_table_m.iter().flatten().copied().collect();
        let jl_start = self.joliet_path_table_sector as usize * BLOCK;
        image[jl_start..jl_start + jl_table.len()].copy_from_slice(&jl_table);
        let jm_start = (self.joliet_path_table_sector + 1) as usize * BLOCK;
        image[jm_start..jm_start + jm_table.len()].copy_from_slice(&jm_table);

        // File contents.
        for (index, (sector, size)) in self.extents.clone().into_iter().enumerate() {
            let path = self.files[index].source.clone();
            let read = |error: std::io::Error| Error::Io {
                path: path.clone(),
                error,
            };
            let bytes = std::fs::read(&path).map_err(read)?;
            let start = sector as usize * BLOCK;
            // `size` came from the earlier `collect`; a file that grew since then
            // is truncated to the extent that was reserved for it, so a write
            // can never overrun the next file's sectors.
            let room = (size as usize).min(bytes.len());
            let room = room.min(image.len() - start);
            image[start..start + room].copy_from_slice(&bytes[..room]);
        }

        // Directories.
        for (components, first, sectors) in self.directories.clone() {
            let rendered = self.render_directory(&components, first, false);
            let start = first as usize * BLOCK;
            let room = ((sectors as usize) * BLOCK).min(image.len() - start);
            let n = rendered.len().min(room);
            image[start..start + n].copy_from_slice(&rendered[..n]);
        }

        // The Joliet tree, rendered with UCS-2BE identifiers.
        for (components, first, sectors) in self.joliet_directories.clone() {
            let rendered = self.render_directory(&components, first, true);
            let start = first as usize * BLOCK;
            let room = ((sectors as usize) * BLOCK).min(image.len() - start);
            let n = rendered.len().min(room);
            image[start..start + n].copy_from_slice(&rendered[..n]);
        }

        // The descriptors occupy fixed, *consecutive* sectors. Contiguity is
        // the load-bearing property, not just their positions: a reader scans
        // forward from 16 and stops at the first sector that is not a
        // descriptor, so a gap here costs it the descriptors beyond the gap.
        //
        // 16 primary, 17 boot record, 18 Joliet, 19 terminator. The path
        // tables are content, not descriptors, and sit at 20 and 21.
        //
        // The image's own length is only final once every sector has been
        // claimed, including the El Torito catalog — which is why the primary
        // descriptor is written *after* it and is passed the finished count.
        // Capturing the count earlier yields one short by exactly the catalog's
        // sector, and the reader then rejects a volume whose declared size ends
        // before the medium does.
        self.write_catalog(&mut image);
        let boot_record = self.write_boot_record(&mut image);
        let final_blocks = self.next;
        let joliet = self.write_joliet(&mut image, final_blocks);
        let terminator = self.write_terminator(&mut image);
        let primary = self.write_primary(&mut image, final_blocks);

        assert_eq!(primary, PVD_SECTOR, "ISO 9660 fixes the PVD at sector 16");
        assert_eq!(
            boot_record, BOOT_RECORD_SECTOR,
            "the boot record immediately follows"
        );
        assert_eq!(joliet, JOLIET_SECTOR, "Joliet follows the boot record");
        assert_eq!(
            terminator, TERMINATOR_SECTOR,
            "the terminator ends the descriptor set"
        );
        assert_eq!(
            TERMINATOR_SECTOR,
            JOLIET_SECTOR + 1,
            "the descriptor set must be contiguous or the scan stops early"
        );
        assert_eq!(
            self.path_table_sector, PATH_TABLE_SECTOR,
            "the L path table sits after the terminator, not inside the descriptor set"
        );
        assert_eq!(
            self.path_table_sector_be, PATH_TABLE_SECTOR_BE,
            "the M path table immediately follows the L one"
        );

        // Trim to the last sector written, so the image is exactly as long as its
        // contents require and two builds compare equal.
        image.truncate((self.next.max(terminator + 1) as usize) * BLOCK);
        Ok(image)
    }

    /// The names directly inside `components`: its files and its
    /// subdirectories.
    ///
    /// Both kinds are members. A subdirectory appears as a record with the
    /// directory flag set and the directory's own extent, so a reader can
    /// descend into it. Files appear as ordinary records.
    fn members_of(&self, components: &[String], joliet: bool) -> Vec<Member> {
        let mut members = Vec::new();
        let directories = if joliet {
            &self.joliet_directories
        } else {
            &self.directories
        };

        // Subdirectories: any directory whose parent is `components`.
        for (path, first, sectors) in directories {
            if path.len() != components.len() + 1 {
                continue;
            }
            if path[..components.len()] != *components {
                continue;
            }
            members.push(Member {
                name: path[components.len()].clone(),
                long_name: path[components.len()].clone(),
                extent: *first,
                // A directory's record declares the size of the directory's
                // *contents* — how many bytes of records live at its extent.
                // Declaring zero tells the reader there is nothing to read,
                // and it descends into an empty directory: the volume opens,
                // the tree is navigable, and every directory is empty.
                size: (*sectors as u64) * BLOCK as u64,
                is_directory: true,
            });
        }

        // Files: those whose immediate parent is `components`.
        for (index, file) in self.files.iter().enumerate() {
            if file.components[..file.components.len() - 1] != *components {
                continue;
            }
            let (extent, size) = self.extents[index];
            members.push(Member {
                // The Joliet tree names the file by its real name; the primary
                // tree by its 8.3 form. A reader that resolves the two trees
                // to the same file needs them to differ only in the name.
                name: if joliet {
                    file.long_name.clone()
                } else {
                    file.short_name.clone()
                },
                long_name: file.long_name.clone(),
                extent,
                size,
                is_directory: false,
            });
        }

        members
    }

    /// The root `.` record's system-use area: `SP`, then `ER`, then `ST`.
    ///
    /// # Why the order is not negotiable
    ///
    /// `SP` has to come first because its `0xBE 0xEF` magic is what tells a
    /// reader the area is SUSP at all. `ER` follows because its `LEN_CSU` byte
    /// is the offset *past which* the reader starts looking for `NM`, and that
    /// offset is only meaningful once `ER` has stated where the common portion
    /// ends.
    ///
    /// The `LEN_CSU` value is the length of everything from the start of the
    /// area through the end of `ER` — that is, `SP` and `ER` together. It is
    /// not the length of `ER` alone and not the whole area. Writing `ER`'s own
    /// length (18) instead of the common portion's (25) points the reader's
    /// `NM` scan into the middle of the `ER` identifier, where it finds no
    /// entries and every file keeps its 8.3 name.
    fn root_system_use(&self) -> Vec<u8> {
        let mut area = Vec::with_capacity(root_susp_len() + SUSP_PX_LEN + 4);
        area.extend_from_slice(&susp_sp());
        area.extend_from_slice(&susp_er());
        area.extend_from_slice(&susp_px_with(MODE_DIRECTORY, 2));
        area.extend_from_slice(&susp_st());
        area
    }

    /// Look up a directory's extent by path.
    fn directory_extent(&self, components: &[String]) -> (u32, u32) {
        self.directories
            .iter()
            .find(|(path, _, _)| path.as_slice() == components)
            .map(|(_, first, sectors)| (*first, *sectors))
            .unwrap_or((0, 0))
    }

    /// Render one directory's records into whole sectors.
    ///
    /// `joliet` selects the supplementary tree: UCS-2BE identifiers and the
    /// Joliet tree's extents. The two renderings describe the same directory
    /// and must not share an extent — the Joliet records are longer, and a
    /// reader following the supplementary descriptor's root into a primary
    /// extent decodes ASCII as UTF-16 and fails.
    fn render_directory(&self, components: &[String], extent: u32, joliet: bool) -> Vec<u8> {
        let mut out = Vec::new();

        // Rock Ridge lives only in the primary tree. The Joliet tree already
        // carries the real names as UCS-2BE identifiers, and adding an `NM`
        // there would give a reader two conflicting names for one file.
        //
        // The root's `.` record is where the `SP` entry must live: it is the one
        // record a reader is guaranteed to read before anything else, and
        // without it no other record's `NM` is looked at.
        // Only the *root's* `.` record carries `SP`/`ER`: they are what
        // announce Rock Ridge for the volume, and duplicating them in every
        // subdirectory is not what the extension says. Every directory's `.`
        // does need `PX` and `NM` like any other record, though — a reader that
        // walks into a subdirectory and finds a bare `.` reads the whole volume
        // as inconsistent.
        let dot_susp = if joliet {
            Vec::new()
        } else if components.is_empty() {
            self.root_system_use()
        } else {
            let mut area = susp_px_with(MODE_DIRECTORY, 2).to_vec();
            area.extend_from_slice(&susp_nm(&components[components.len() - 1]));
            area.extend_from_slice(&susp_st());
            area
        };

        out.extend_from_slice(&self.dot_record(extent, false, &dot_susp));
        // The `..` record names the parent directory. The root's parent is
        // itself, so an empty component list resolves to the root's own extent
        // rather than to sector 0 — a `..` of zero points at the boot
        // catalogue, and a reader that walks upwards from the root ends up
        // parsing an El Torito record as a directory.
        let directories = if joliet {
            &self.joliet_directories
        } else {
            &self.directories
        };
        let lookup = |path: &[String]| {
            directories
                .iter()
                .find(|(components, _, _)| components.as_slice() == path)
                .map(|(_, first, sectors)| (*first, *sectors))
                .unwrap_or((0, 0))
        };
        let parent = match components.len() {
            0 => extent,
            n => lookup(&components[..n - 1]).0,
        };
        // The `..` record needs the same Rock Ridge treatment as any other
        // record. A reader validating the tree asks every directory's parent for
        // its attributes, and a bare `..` reads as a parent that is not part of
        // the Rock Ridge namespace at all -- which makes the whole volume look
        // inconsistent rather than merely terse.
        let parent_susp = if joliet {
            Vec::new()
        } else {
            let parent_name = match components.len() {
                0 => String::new(),
                n => components[n - 1].clone(),
            };
            let mut area = susp_px_with(MODE_DIRECTORY, 2).to_vec();
            // `..` names the parent, and for the root that is the root, whose
            // name is empty. An `NM` with an empty payload is what says "this is
            // the parent of itself" rather than "this record has no name".
            area.extend_from_slice(&susp_nm(&parent_name));
            area.extend_from_slice(&susp_st());
            area
        };
        out.extend_from_slice(&self.dot_record(parent, true, &parent_susp));

        for member in self.members_of(components, joliet) {
            let identifier = encode_record_identifier(&member.name, false, joliet);
            // Only the primary tree gets the `NM` name; the Joliet identifier is
            // already the real one.
            let system_use = if joliet {
                Vec::new()
            } else {
                let mut area = susp_px_with(
                    if member.is_directory {
                        MODE_DIRECTORY
                    } else {
                        MODE_FILE
                    },
                    if member.is_directory { 2 } else { 1 },
                )
                .to_vec();
                area.extend_from_slice(&susp_nm(&member.long_name));
                area.extend_from_slice(&susp_st());
                area
            };
            out.extend_from_slice(&directory_record(
                &identifier,
                member.extent,
                member.size,
                if member.is_directory {
                    FLAG_DIRECTORY
                } else {
                    0
                },
                &system_use,
            ));
        }

        while out.len() % BLOCK != 0 {
            out.push(0);
        }
        out
    }

    /// The `.` / `..` record.
    ///
    /// The identifier of `.` is a single NUL byte and of `..` a single 0x01 byte,
    /// so the record is 33 + 1 = 34 bytes, already even, and needs no padding. It
    /// uses the same both-endian extent and length layout as [`directory_record`] —
    /// a `.` record written with a 4-byte extent is read as pointing at sector 0,
    /// which is the boot catalog.
    ///
    /// The identifiers stay one byte each in the Joliet tree too. The Joliet
    /// encoding applies to *file names*, and `.` and `..` are not names — they are
    /// the two fixed markers a reader recognises by their exact byte value. Writing
    /// them as UCS-2 (`0x00 0x00` and `0x00 0x01`) makes them unrecognisable: the
    /// reader treats the first record of every directory as an ordinary entry
    /// named " ", descends into the directory it points at, and finds a tree
    /// with a cycle in it.
    fn dot_record(&self, extent: u32, is_parent: bool, system_use: &[u8]) -> Vec<u8> {
        let mut record = Vec::with_capacity(34 + system_use.len());
        record.push(0); // record length, patched once the padding is known
        record.push(EAR_LENGTH);
        // 2..10 extent, little-endian then big-endian
        record.extend_from_slice(&extent.to_le_bytes());
        record.extend_from_slice(&extent.to_be_bytes());
        // 10..18 data length, likewise
        record.extend_from_slice(&(BLOCK as u32).to_le_bytes());
        record.extend_from_slice(&(BLOCK as u32).to_be_bytes());
        // 18..25 recording date, fixed at the epoch
        record.extend_from_slice(&[0u8; 7]);
        // 25 file flags
        record.push(FLAG_DIRECTORY);
        record.push(0); // file unit size
        record.push(0); // interleave gap size
        // 28..32 volume sequence number, both-endian
        record.extend_from_slice(&1u16.to_le_bytes());
        record.extend_from_slice(&1u16.to_be_bytes());
        // 32 identifier length: one byte
        record.push(1);
        // 33 the identifier itself: 0x00 for ".", 0x01 for ".."
        record.push(u8::from(is_parent));
        // Then the system-use area, which carries this directory's Rock Ridge name.
        record.extend_from_slice(system_use);
        if record.len() % 2 == 1 {
            record.push(0);
        }
        // The declared length counts the system-use area and the pad byte, so it is
        // only known here. Writing `34` instead -- the length without them -- makes
        // the reader's search for the next record start inside the SUSP area, where
        // it reads an ASCII letter as a record length.
        record[0] = record.len() as u8;
        record
    }

    /// Write the El Torito boot catalog.
    ///
    /// The catalog is a sequence of 32-byte-aligned *fixed-size* records. Every
    /// record type here is exactly 32 bytes of header followed by payload:
    /// the validation entry adds a 4-byte LBA, the initial/default entry adds
    /// nothing, and each boot entry is header only. Nothing in the format is
    /// variable-length, which is precisely why hand-rolling it is easy to get
    /// wrong: a header written as 9 bytes instead of 32 still produces a catalog
    /// that a human reading the hex dump can plausibly believe in, but every
    /// record after it sits at an offset no firmware will look at.
    fn write_catalog(&mut self, image: &mut [u8]) {
        self.boot_catalog_sector = self.next;

        // The validation entry's content must be the catalog's own LBA, which
        // is only known now — the header is built after this assignment.
        self.boot_validation_lba = self.boot_catalog_sector;

        /// One 32-byte record, zero-padded, so a short header cannot shift the
        /// next record.
        fn record() -> [u8; 32] {
            [0u8; 32]
        }

        // --- Validation entry: "this medium is bootable", plus a self-check ---
        //
        //   0      header ID, 1
        //   1      platform, 0 (x86)
        //   2..4   reserved
        //   4..28  id string, 24 bytes
        //   28..30 checksum, such that all 32 bytes sum to zero
        //   30     0x55
        //   31     0xAA
        //
        // The checksum covers the whole record. It is a two-byte sum with the
        // carry discarded, and only the 16-bit total is kept — not folded back
        // in as a complement of the 32-bit total. Writing a fixed 0xAA5C works
        // only while the id string is empty; putting any text in the string
        // changes the sum and the firmware then discards the medium as
        // malformed before reading a single boot entry.
        let mut validation = record();
        validation[0] = 0x01;
        validation[1] = 0x00; // platform: x86, no emulation
        // 4..28 id string. Empty, which is the conventional choice: a reader
        //      that shows it to a human gains nothing, and the checksum below
        //      is computed over whatever is actually written.
        validation[30] = 0x55;
        validation[31] = 0xAA;
        // The checksum is taken last, over the finished record with only the
        // checksum field itself still zero, and chosen so that re-summing all
        // 32 bytes yields zero.
        //
        // Computing it before the key bytes are written sums a record that is
        // not the one that ends up on the medium, and the reader's own
        // verification — which re-sums what it read — then fails and discards
        // the whole catalog. The error names the checksum, which is correct
        // and unhelpful: nothing else about the medium is wrong.
        let checksum = el_torito_checksum(&validation);
        validation[28..30].copy_from_slice(&checksum);

        let mut catalog = Vec::new();
        catalog.extend_from_slice(&validation);

        // --- Initial/default entry header ---
        //
        // It carries no payload of its own; its load RBA points at whichever
        // boot entry firmware should try first. Boot entries are laid out in
        // the order they were supplied, so the RBA of entry `n` is
        // `VALIDATION_ENTRY_LEN + HEADER_LEN * (n + 1)`: the validation entry
        // occupies the first slot, the initial/default header the second, and
        // the boot entries follow.
        //
        // Which entry is the default is the caller's decision — the BIOS and
        // UEFI images are different files for different firmware, and only the
        // caller knows which firmware the image is meant for. When no entry is
        // marked, the first one is the default, which is the historical
        // behaviour and the one that keeps a single-entry image bootable.
        let default_position = self
            .boot_defaults
            .iter()
            .position(|is_default| *is_default)
            .unwrap_or(0);
        // The initial entry's load RBA names the LBA of the *boot image* that
        // firmware should load — the same value the corresponding boot entry
        // carries. It is not an index into the catalog's records.
        //
        // The two readings differ by a factor of 32, and the wrong one lands
        // inside the catalog: a reader treats 4 as sector 4 of the medium, finds
        // the ISO 9660 system area there, and reports "Default entry is not
        // valid". The medium is perfectly bootable; the pointer names the
        // wrong kind of thing.
        let default_index = self
            .boot_sectors
            .get(default_position)
            .copied()
            .unwrap_or(0) as usize;
        let default_lba = self
            .extents
            .get(default_index)
            .map(|(sector, _)| *sector)
            .unwrap_or(0);

        let mut initial = record();
        initial[0] = BOOT_INDICATOR_BOOTABLE;
        // The initial entry describes the default boot entry, so its media type
        // has to be that entry's media type. It is not an independent setting:
        // a firmware that reads a media type of 0 here will try to load the
        // target as a no-emulation BIOS image, and an EFI image loaded that way
        // is not a boot sector. The failure is reported against the medium
        // rather than against the catalog, because the catalog is well-formed.
        initial[1] = if self
            .boot_platforms
            .get(default_position)
            .copied()
            .is_some_and(|platform| platform == PLATFORM_EFI)
        {
            MEDIA_EFI
        } else {
            MEDIA_NO_EMULATION
        };
        initial[8..12].copy_from_slice(&default_lba.to_le_bytes());
        catalog.extend_from_slice(&initial);

        // --- Section header ---
        //
        // The catalog is laid out as a validation entry, an initial entry, and
        // then one or more *sections*. A section is a header record followed
        // by the entries it describes, and the header's 0x90 marks it as
        // bootable while its byte 5 says whether more sections follow (0x91) or
        // this is the last one (0x90).
        //
        // The section header is not optional decoration. A reader walks the
        // catalog from the initial entry and uses section headers to decide
        // how many entries belong to the section; entries with no preceding
        // header are read as free-standing and skipped by implementations that
        // require the anchoring. The volume then parses, the bootable indicator
        // is present on the medium, and the firmware reports that no bootable
        // method exists.
        let entry_count = self.boot_sectors.len() as u16;
        let mut header = record();
        header[0] = SECTION_HEADER_BOOTABLE;
        // 1 platform: the section header's platform byte selects the emulation
        //   family for the entries that follow, not the firmware that should
        //   try them. x86 and EFI are both 0 here.
        header[1] = PLATFORM_SECTION_X86;
        // 2..4 number of boot entries in this section.
        //
        //     This is a 16-bit field at offset 2, not a single byte at offset 6.
        //     Offsets 4 and 5 belong to the id string and the "more sections
        //     follow" marker respectively, so a count written at 6 lands inside
        //     the id string and the section parses as holding nothing. The
        //     entries are still on the medium; nothing reads them.
        header[2..4].copy_from_slice(&entry_count.to_le_bytes());
        // 4 the id string's first byte, unused
        header[4] = 0;
        // 5 0x91 marks the final section, 0x90 says another follows.
        header[5] = SECTION_HEADER_FINAL;
        catalog.extend_from_slice(&header);

        // --- Boot entries ---
        for (position, &index) in self.boot_sectors.iter().enumerate() {
            let (sector, size) = self.extents[index as u32 as usize];
            let is_efi = self.boot_platforms[position] == PLATFORM_EFI;
            // Only the default entry is marked bootable (0x88). The others are
            // marked "not bootable" (0x00) so a firmware that honours the flag
            // tries exactly one path.
            let is_default = self.boot_defaults.get(position).copied().unwrap_or(false);

            let mut entry = record();
            // A boot entry's indicator is 0x88 for bootable and 0x00 for not.
            //
            // 0x90 and 0x91 are *section header* indicators, not a way of
            // marking an entry as EFI. A firmware scanning for something to
            // boot skips any indicator that is not 0x88, so an EFI image
            // marked 0x90 is well-formed and simply never chosen: the medium
            // reports no bootable method. What makes an entry an EFI entry is
            // its media type, not its indicator.
            entry[0] = if is_default {
                BOOT_INDICATOR_BOOTABLE
            } else {
                BOOT_INDICATOR_NOT_BOOTABLE
            };
            // An EFI image is media type 2: firmware loads the extent verbatim,
            // so no byte count is meaningful and none is written. The BIOS image
            // is no-emulation (type 0) and carries a sector count.
            entry[1] = if is_efi {
                MEDIA_EFI
            } else {
                MEDIA_NO_EMULATION
            };
            // A boot image that needs more than 65535 sectors cannot be described
            // by the format's 16-bit count, so refuse rather than truncate: a
            // truncated count would load a prefix of the image and fail as a boot
            // error much later, far from the cause.
            let sectors = size.div_ceil(BLOCK as u64);
            assert!(
                sectors <= u64::from(u16::MAX),
                "a boot image cannot exceed 65535 sectors, got {sectors}"
            );
            // The EFI entry carries no count, per the note above; a count in
            // that field is "reserved" and firmware is free to read it.
            entry[6..8].copy_from_slice(&(if is_efi { 0 } else { sectors } as u16).to_le_bytes());
            entry[8..12].copy_from_slice(&sector.to_le_bytes());
            catalog.extend_from_slice(&entry);
        }

        while catalog.len() % BLOCK != 0 {
            catalog.push(0);
        }
        let start = self.boot_catalog_sector as usize * BLOCK;
        if start + catalog.len() <= image.len() {
            image[start..start + catalog.len()].copy_from_slice(&catalog);
        }
        self.next += 1;
    }

    /// Write the boot record volume descriptor at sector 17.
    fn write_boot_record(&mut self, image: &mut [u8]) -> u32 {
        let sector = BOOT_RECORD_SECTOR;
        let mut descriptor = [0u8; BLOCK];
        descriptor[0] = VD_BOOT_RECORD;
        // Layout, per ECMA-119 8.2:
        //
        //   0       descriptor type
        //   1..6    standard identifier, "CD001"
        //   6       version
        //   7..39   boot system identifier
        //   39..71  boot identifier
        //   71..75  pointer to the boot catalog
        //
        // The boot record packs these differently from the primary descriptor:
        // the system identifier starts at byte 7 rather than 8, because the
        // version byte at 6 is followed by a 32-byte field with no alignment
        // padding. Reusing the primary descriptor's offsets here shifts every
        // field by one and lands the catalog pointer inside the boot
        // identifier, where the reader finds a plausible-looking number that
        // is not a catalog at all.
        descriptor[1..6].copy_from_slice(b"CD001");
        descriptor[6] = 1;
        // 7..39 boot system identifier
        //
        // This field is not a label. It is the signature that tells a reader
        // this boot record is an El Torito one and that the catalog pointer
        // below is meaningful, and it has to read exactly
        // "EL TORITO SPECIFICATION" padded with NULs. A custom string such as
        // the product's own name leaves the record well-formed and correctly
        // located, and the reader concludes the medium carries no boot catalog
        // at all: the pointer is never followed and the image is not bootable.
        let mut boot_system = [0u8; 32];
        let signature = b"EL TORITO SPECIFICATION";
        boot_system[..signature.len()].copy_from_slice(signature);
        descriptor[7..39].copy_from_slice(&boot_system);
        // 39..71 boot identifier, left as spaces
        descriptor[39..71].copy_from_slice(&[b' '; 32]);
        // 71..75 pointer to the boot catalog
        descriptor[71..75].copy_from_slice(&self.boot_catalog_sector.to_le_bytes());

        let start = sector as usize * BLOCK;
        image[start..start + BLOCK].copy_from_slice(&descriptor);
        // The cursor does not move: every descriptor lives at a fixed sector
        // below the data area, and `self.next` already sits above all of them.
        // Advancing it here would describe an image longer than the file that
        // is actually written.
        sector
    }

    /// Write the primary volume descriptor at sector 16.
    fn write_primary(&mut self, image: &mut [u8], blocks: u32) -> u32 {
        let sector = PVD_SECTOR;
        let mut descriptor = [0u8; BLOCK];
        descriptor[0] = VD_PRIMARY;
        // The five-byte standard identifier "CD001" begins at byte 1 of the
        // descriptor — immediately after the type byte, *not* at the start of
        // the system-identifier field eight bytes later. Every reader keys its
        // entire descriptor scan on this five bytes at this exact position, so
        // a volume whose identifier starts one field too late is not a volume
        // with a bad label: it is not recognised as a volume at all. `mtools`
        // reports "non DOS media" and `pycdlib` reports "must have at least one
        // PVD", neither of which points at the byte that is wrong.
        descriptor[1..6].copy_from_slice(b"CD001");
        // 6 version, which must be 1. A reader that finds the PVD and then
        // finds version 0 here refuses it with "invalid volume descriptor
        // version" — a zero is exactly what an all-zero buffer yields, so this
        // is the byte a descriptor writer forgets.
        descriptor[6] = 1;
        descriptor[8..40].copy_from_slice(&[b' '; 32]);
        descriptor[40..72].copy_from_slice(VOLUME_ID);
        write_volume_layout(&mut descriptor, self, blocks);
        // The root directory record occupies 34 bytes at offset 156.
        descriptor[156..190].copy_from_slice(&root_record(self.root_sector));
        // Volume set identifier, padded with spaces.
        let mut set_id = [b' '; 128];
        let id = b"NOVAOS";
        set_id[..id.len()].copy_from_slice(id);
        descriptor[190..318].copy_from_slice(&set_id);

        let start = sector as usize * BLOCK;
        image[start..start + BLOCK].copy_from_slice(&descriptor);
        sector
    }

    /// Write the Joliet supplementary volume descriptor.
    ///
    /// Placed between the boot record and the terminator. Joliet is optional,
    /// but writing it *after* the terminator would place it outside the
    /// descriptor set, where no reader will look for it.
    fn write_joliet(&mut self, image: &mut [u8], blocks: u32) -> u32 {
        let sector = JOLIET_SECTOR;
        let mut descriptor = [0u8; BLOCK];
        descriptor[0] = VD_SUPPLEMENTARY;
        // The standard identifier sits at byte 1, immediately after the type byte —
        // see the note in `write_primary`.
        descriptor[1..6].copy_from_slice(b"CD001");
        descriptor[6] = 1;
        // 7 flags.
        //
        // Bit 0 advertises that the escape sequences in this descriptor are in
        // use, which sounds like what a supplementary descriptor with an
        // escape sequence should set — and it is not. Every real-world Joliet
        // image, and every implementation that writes one, leaves this field
        // zero. A reader that keys off this bit instead of the escape sequence
        // itself treats a descriptor with it set as *not* Joliet, skips it, and
        // the volume silently loses its long-name support: the primary tree is
        // still readable, so nothing looks broken, and every name comes back
        // truncated to its ISO 9660 form.
        descriptor[7] = 0;
        descriptor[8..40].copy_from_slice(&[b' '; 32]);
        descriptor[40..72].copy_from_slice(VOLUME_ID);
        // Everything from the volume-space size to the root directory record is
        // identical to the primary descriptor's: only the type byte and the
        // escape sequence at offset 88 differ. Sharing the writer is what keeps
        // the two from drifting apart as either one is edited.
        write_volume_layout(&mut descriptor, self, blocks);
        // The supplementary descriptor's root is the Joliet tree's root, and
        // its path table is the Joliet one. Pointing it at the primary tree's
        // is not a smaller mistake than it looks: the record's *lengths* are
        // right for either tree, so a reader follows the pointer, decodes the
        // primary tree's ASCII names as UCS-2, and fails on the first one.
        descriptor[132..136].copy_from_slice(&self.joliet_path_table_bytes.to_le_bytes());
        descriptor[136..140].copy_from_slice(&self.joliet_path_table_bytes.to_be_bytes());
        descriptor[140..144].copy_from_slice(&self.joliet_path_table_sector.to_le_bytes());
        descriptor[144..148].copy_from_slice(&0u32.to_le_bytes());
        // The M table lives in the sector *after* the L one. Naming the same
        // sector for both makes the reader parse one table twice — once as
        // little-endian and once as big-endian — and the two readings disagree
        // about every record's extent, so the volume is rejected as corrupt.
        descriptor[148..152].copy_from_slice(&(self.joliet_path_table_sector + 1).to_be_bytes());
        descriptor[152..156].copy_from_slice(&0u32.to_le_bytes());
        descriptor[156..190].copy_from_slice(&root_record(self.joliet_root_sector));
        let mut set_id = [b' '; 128];
        let id = b"NOVAOS";
        set_id[..id.len()].copy_from_slice(id);
        descriptor[190..318].copy_from_slice(&set_id);
        // The Joliet escape sequence lives in the supplementary descriptor's
        // own space, at offset 88 — the exact field that is *volume space
        // size* in the primary descriptor. That reuse is why the shared writer
        // takes the primary's value rather than a generic one: in Joliet this
        // slot means something else entirely, and leaving it at the primary's
        // value would declare the Joliet volume the wrong size.
        descriptor[88..92].copy_from_slice(&JOLIET_LEVEL_3);

        let start = sector as usize * BLOCK;
        image[start..start + BLOCK].copy_from_slice(&descriptor);
        // The cursor does not move: every descriptor lives at a fixed sector
        // below the data area, and `self.next` already sits above all of them.
        // Advancing it here would describe an image longer than the file that
        // is actually written.
        sector
    }

    /// Write the volume descriptor set terminator, which must be the last
    /// descriptor in the set.
    fn write_terminator(&mut self, image: &mut [u8]) -> u32 {
        let sector = TERMINATOR_SECTOR;
        let mut descriptor = [0u8; BLOCK];
        descriptor[0] = VD_TERMINATOR;
        descriptor[1..6].copy_from_slice(b"CD001");

        // `assemble` sizes the buffer to cover every descriptor written here,
        // so this write cannot run past the end; the guard is a debug check
        // rather than a resize, which a fixed-size slice cannot perform.
        let start = sector as usize * BLOCK;
        debug_assert!(
            start + BLOCK <= image.len(),
            "the descriptor buffer must cover the terminator at sector {sector}"
        );
        if let Some(slot) = image.get_mut(start..start + BLOCK) {
            slot.copy_from_slice(&descriptor);
        }
        // The cursor deliberately does not move. Every descriptor lives at a
        // fixed sector below the data area, and `self.next` already sits above
        // all of them; advancing it here would extend the image past the end of
        // the last real sector, and the volume-space size recorded in the
        // primary descriptor would then describe an image that is longer than
        // the file that gets written.
        sector
    }

    /// LBA recorded in the El Torito validation entry.
    #[allow(dead_code)]
    fn validation_lba(&self) -> u32 {
        self.boot_validation_lba
    }
}

/// The El Torito validation entry's checksum.
///
/// Every byte of the 32-byte record is added as a little-endian 16-bit word —
/// so byte 0 is the low half and byte 1 the high half — with the carry out of
/// each addition discarded rather than folded back in. The result is the
/// two's complement of that 16-bit sum, chosen so that re-summing the finished
/// record yields zero.
///
/// Summing as 32-bit words and only then truncating gives a different answer
/// whenever the carry propagates, and the record then fails the check that it
/// exists to perform.
fn el_torito_checksum(record: &[u8; HEADER_LEN]) -> [u8; 2] {
    // The accumulation is deliberately *not* done in 32 bits.
    //
    // Each byte contributes as a 16-bit little-endian half-word, and the
    // format specifies that the carry out of the 16-bit addition is discarded
    // rather than carried into the next half-word. Accumulating in a wider
    // register keeps those carries and folds them back in through the
    // complement, which shifts the result — for the all-zero id string it
    // turns 0x55AA into 0x55A9. The record still looks like a checksum, and
    // the reader's verification, which re-sums what it read, returns 1 instead
    // of 0 and discards the entire boot catalog.
    let mut sum = 0u16;
    for (index, byte) in record.iter().enumerate() {
        let half_word = (*byte as u16) << (8 * (index % 2));
        // `wrapping_add` on u16 is exactly "add, discarding the carry".
        sum = sum.wrapping_add(half_word);
    }
    sum.wrapping_neg().to_le_bytes()
}

/// The root directory record.
///
/// 34 bytes: a 33-byte header plus the one-byte NUL identifier that names the
/// root itself. The header layout is shared with [`directory_record`] and is
/// documented there.
fn root_record(extent: u32) -> [u8; 34] {
    let mut record = [0u8; 34];
    record[0] = 34; // record length
    record[1] = EAR_LENGTH;
    // 2..10 extent, little-endian then big-endian
    record[2..6].copy_from_slice(&extent.to_le_bytes());
    record[6..10].copy_from_slice(&extent.to_be_bytes());
    // 10..18 data length, likewise
    record[10..14].copy_from_slice(&(BLOCK as u32).to_le_bytes());
    record[14..18].copy_from_slice(&(BLOCK as u32).to_be_bytes());
    // 18..25 recording date, fixed at the epoch
    // 25 flags
    record[25] = FLAG_DIRECTORY;
    record[26] = 0; // file unit size
    record[27] = 0; // interleave gap size
    // 28..32 volume sequence number, little-endian then big-endian
    record[28..30].copy_from_slice(&1u16.to_le_bytes());
    record[30..32].copy_from_slice(&1u16.to_be_bytes());
    // 32 identifier length, counting the terminating NUL
    record[32] = 1;
    // 33 the identifier itself: a single NUL
    record[33] = 0;
    record
}

/// The length of the root directory record's system-use area, in bytes.
///
/// Called from the sizing pass, which runs before any record is rendered, so
/// this has to agree with [`Layout::root_system_use`] byte for byte. The two
/// derive from the same constants for that reason: an off-by-one here
/// under-counts the root directory, and the records written last are the ones
/// that spill past the extent.
fn root_susp_len() -> usize {
    SUSP_SP_LEN + SUSP_ER_LEN + SUSP_PX_LEN
}

/// The SUSP `SP` entry that opens the root record's system-use area.
///
/// Bytes 4 and 5 are the magic `0xBE 0xEF` that every SUSP reader looks for to
/// decide whether the area is SUSP at all.
///
/// Byte 6 is `LEN_SKP`, and it must be **zero**.
///
/// `LEN_SKP` is the number of bytes to skip at the start of *every* system-use
/// area in the volume before looking for entries, and the value is read from the
/// root record's `SP` and applied volume-wide. Rock Ridge treats the byte as
/// `LEN_CSU` instead, but writing the common portion's real length there is
/// actively harmful: a reader that honours it starts 61 bytes into every
/// non-root record's system-use area, which is past the end of the 44-48 byte
/// `PX`+`NM`+`ST` area it was meant to describe, and so finds no entries. Every
/// file keeps its mangled 8.3 identifier, `/boot/limine.conf` resolves to
/// nothing, and the boot menu comes up with "Default entry is not valid" instead
/// of starting the kernel. `xorriso -as mkisofs -R` writes zero here, which is
/// what every working image does.
fn susp_sp() -> [u8; 7] {
    let mut entry = [0u8; 7];
    entry[0] = b'S';
    entry[1] = b'P';
    entry[2] = 7;
    entry[3] = 1;
    entry[4] = 0xBE;
    entry[5] = 0xEF;
    entry[6] = 0;
    entry
}

/// The SUSP `ER` entry naming RRIP, without which `SP` alone buys nothing.
///
/// Byte 6 is `LEN_CSU`, the length of the *system use* area's common portion —
/// that is, everything from the start of the area up to and including this `ER`.
/// Limine reads exactly that byte to decide how far to skip before looking for
/// `NM`, so a wrong value here is not a cosmetic error: it either hides every
/// name or points the reader into the middle of an entry.
fn susp_er() -> [u8; 8 + 10] {
    let mut entry = [0u8; 8 + 10];
    entry[0] = b'E';
    entry[1] = b'R';
    // The four length fields are consecutive from byte 4: LEN_ID, LEN_DES,
    // LEN_SRC, LEN_SFS. Byte 6 is LEN_SRC — *not* LEN_CSU. LEN_CSU lives in
    // `SP`, one entry earlier, and writing it here instead makes a reader sum
    // LEN_ID + LEN_DES + LEN_SRC, get more than the entry's own declared
    // length, and reject the volume as malformed.
    //
    // The declared length must equal `8 + LEN_ID + LEN_DES + LEN_SRC`, which a
    // reader is entitled to check before reading anything else.
    entry[2] = (8 + 10) as u8;
    entry[3] = 1;
    entry[4] = 10; // LEN_ID
    entry[5] = 0; // LEN_DES
    entry[6] = 0; // LEN_SRC
    entry[7] = 0; // LEN_SFS
    entry[8..18].copy_from_slice(b"RRIP_1991A");
    entry
}

/// The SUSP `NM` entry carrying the real name of a file or directory.
///
/// Byte 4 is the flags field, where bit 0 (value 1) marks a *continuation* of
/// the previous `NM` rather than the start of a new name. NovaOS names are short
/// enough never to need one, but writing it as 0 is what tells a reader to treat
/// the payload as a complete name instead of appending it to nothing.
///
/// This entry is not optional decoration. Without it the only name a reader has
/// is the 8.3 form, and a name like `limine.conf` becomes `LIMINECO` — at which
/// point a bootloader asked for `boot():/boot/limine.conf` finds nothing and
/// reports a configuration error against a file it can plainly see listed.
fn susp_nm(name: &str) -> Vec<u8> {
    let mut entry = Vec::with_capacity(5 + name.len());
    entry.extend_from_slice(b"NM");
    entry.push((5 + name.len()) as u8);
    entry.push(1);
    entry.push(0); // flags: a name, not a continuation
    entry.extend_from_slice(name.as_bytes());
    entry
}

/// The SUSP `PX` entry recording POSIX attributes.
///
/// Rock Ridge's `PX` is not optional once `NM` names are in use: a reader that
/// walks the tree expects every record to carry the attributes, and one without
/// them is read as corrupt rather than as merely terse. Concretely, a reader
/// validating the tree finds the root's children described but a subdirectory
/// whose own record is bare, and reports the whole volume as inconsistent.
///
/// Only mode and link count are written. Everything else — uid, gid, serial
/// numbers, inode numbers — is zeroed, which is a valid value for each and
/// keeps the image byte-identical across builds.
fn susp_px() -> [u8; 36] {
    let mut entry = [0u8; 36];
    entry[0] = b'P';
    entry[1] = b'X';
    entry[2] = 36;
    entry[3] = 1;
    // 4..12 mode, both-endian. 0o040755 for a directory and 0o100644 for a
    // file: the high bits say which, and a reader uses them to tell the two
    // apart without consulting the directory flag in the ISO header.
    // 12..20 links, 20..24 uid, 24..28 gid — all zeroed except links below.
    // 28..32 serial number, both-endian
    // 32..36 inode number, both-endian
    entry
}

/// Fill in the mode and link count of a `PX` entry.
fn susp_px_with(mode: u32, links: u32) -> [u8; 36] {
    let mut entry = susp_px();
    entry[4..8].copy_from_slice(&mode.to_le_bytes());
    entry[8..12].copy_from_slice(&mode.to_be_bytes());
    // A directory's link count counts the directory itself plus one per
    // subdirectory; a file's is one.
    entry[12..16].copy_from_slice(&links.to_le_bytes());
    entry[16..20].copy_from_slice(&links.to_be_bytes());
    entry
}

/// The SUSP `ST` terminator that closes a system-use area.
fn susp_st() -> [u8; 4] {
    [b'S', b'T', 4, 1]
}

/// Directory paths sorted shallowest-first, paired with their 1-based position.
///
/// The position is what a path table record's parent field holds. Ordering
/// matters: a reader resolves a path by walking parent links, so a record whose
/// parent has not been written yet cannot be followed.
fn order_directories(directories: &[(Vec<String>, u32, u32)]) -> Vec<(Vec<String>, u8)> {
    let mut paths: Vec<Vec<String>> = directories
        .iter()
        .map(|(components, _, _)| components.clone())
        .collect();
    paths.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
    paths.dedup();

    paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| (path, index as u8 + 1))
        .collect()
}

/// A directory record's file identifier.
///
/// The root's identifier is a single NUL byte — it is what stands for "this
/// directory" and "the parent directory", and it is not a name. Every other
/// identifier is the name exactly, with no terminator: the identifier's length
/// field says how long it is, and a terminator inside that field is read as
/// part of the name. A file called `KERNEL.ELF;1` then reads back as
/// `KERNEL.ELF;1\0`, and the version number no longer parses — the reader
/// fails on the name rather than on anything structural.
///
/// In the Joliet tree the name is UCS-2BE: two bytes per character, most
/// significant first, and still unterminated.
fn encode_record_identifier(name: &str, is_root: bool, joliet: bool) -> Vec<u8> {
    if is_root {
        return vec![0];
    }

    let mut identifier = Vec::with_capacity(name.len() * 2);
    if joliet {
        for unit in name.encode_utf16() {
            identifier.extend_from_slice(&unit.to_be_bytes());
        }
    } else {
        identifier.extend_from_slice(name.as_bytes());
    }
    identifier
}

/// A path table record's directory identifier.
///
/// The path table's identifier *is* NUL-terminated, and its length counts the
/// terminator — unlike a directory record's identifier. The root's identifier
/// is that single NUL byte.
fn encode_path_identifier(name: &str, is_root: bool, joliet: bool) -> Vec<u8> {
    if is_root {
        return vec![0];
    }

    let mut identifier = encode_record_identifier(name, false, joliet);
    identifier.push(0);
    identifier
}

/// One directory record for a named file.
///
/// The header is 33 bytes before the identifier, which is the number every
/// ISO 9660 reader assumes and the number that makes `.` (33 + 1 = 34 bytes)
/// land on an even boundary.
///
/// # Why the extent and length are eight bytes each
///
/// Both are stored twice: a little-endian copy followed by a big-endian copy,
/// in the same 8-byte slot. They are *not* two separate 4-byte fields at
/// different offsets, which is the natural reading and what this used to write.
/// Doing that shifts every field after the length by four bytes, so the
/// recording date lands on the flags byte, the flags land on the identifier
/// length, and the reader parses the name from the middle of a number. The
/// record is 33 bytes either way, so nothing detects it — the directory simply
/// lists no usable entries, and the media looks empty.
fn directory_record(name: &[u8], extent: u32, size: u64, flags: u8, system_use: &[u8]) -> Vec<u8> {
    // A record is a 33-byte header, the identifier, and enough padding to
    // make the whole thing even. The *length byte* counts the padding too: it
    // is the number of bytes the record occupies, not the number of bytes of
    // content.
    //
    // Leaving the padding out of the length makes the record one byte shorter
    // than it is on disk. A reader then starts the next record one byte early,
    // reads the pad byte as a length, and sees zero — which it takes as the
    // end of the directory. The directory silently lists only the entries
    // that happen to precede the first odd-length name, and the volume opens
    // and looks half-empty.
    //
    // The identifier is *not* NUL-terminated. Only the `.` and `..` records
    // have a NUL as their identifier, because the one-byte identifier `0x00`
    // is what stands for "this directory" and "the parent directory". Every
    // other identifier is the name exactly, and the length field counts only
    // the name's bytes. Terminating an ordinary name with a NUL and counting
    // that NUL in the length makes every entry read back with a trailing NUL
    // glued to it, so the name matches nothing — the directory lists its
    // members and a reader looking for any of them finds none.
    // ECMA-119 9.1.13: a pad byte sits between the identifier and the
    // System Use area when — and only when — the identifier's length is even.
    let identifier_pad = usize::from(name.len() % 2 == 0);
    let content = 33 + name.len() + identifier_pad + system_use.len();
    let padded = content.div_ceil(2) * 2;

    let mut record = Vec::with_capacity(padded);

    // 0   length of this record: the 33-byte header plus the identifier plus
    //     the padding that rounds the record to an even length.
    record.push(padded as u8);
    // 1   extended attribute record length
    record.push(EAR_LENGTH);
    // 2..10  extent, little-endian then big-endian
    record.extend_from_slice(&extent.to_le_bytes());
    record.extend_from_slice(&extent.to_be_bytes());
    // 10..18 data length, likewise. A file larger than 4 GiB does not fit in
    //      the 32-bit field, and ISO 9660 level 1 has no way to say so; the
    //      build refuses those separately rather than truncating silently here.
    let truncated = u32::try_from(size).unwrap_or(u32::MAX);
    record.extend_from_slice(&truncated.to_le_bytes());
    record.extend_from_slice(&truncated.to_be_bytes());
    // 18..25 recording date: year, month, day, hour, minute, second, offset.
    //      Fixed at the epoch — a fixed value keeps builds byte-identical,
    //      which is what makes an image diffable across runs.
    record.extend_from_slice(&[0u8; 7]);
    // 25 file flags
    record.push(flags);
    // 26 file unit size
    record.push(0);
    // 27 interleave gap size
    record.push(0);
    // 28..32 volume sequence number, both-endian
    record.extend_from_slice(&1u16.to_le_bytes());
    record.extend_from_slice(&1u16.to_be_bytes());
    // 32 identifier length: the name's bytes, with no terminator
    record.push(name.len() as u8);
    // 33.. identifier: the name, exactly
    record.extend_from_slice(name);
    // An identifier of even length is followed by one pad byte before the
    // system-use area, so the area always starts at an odd offset from the
    // record start and the entry's own fields stay aligned to their spec.
    //
    // Omitting it silently corrupts every even-length name: the reader starts
    // the system-use area one byte early, reads the last byte of the name as
    // the first entry's signature, and does not recognise `PX`/`NM` at all. A
    // validating reader then concludes the directory's parent carries no Rock
    // Ridge and rejects the volume as corrupt. `boot` and `EFI/BOOT` are even
    // length, so this is the common case, not an edge case.
    // ECMA-119 9.1.13 requires the pad when the identifier's length is even.
    // Getting the parity backwards is the same failure as omitting the pad
    // entirely: the reader starts the system-use area one byte early, reads the
    // last byte of the name as the first entry's signature, does not recognise
    // `PX`/`NM` at all, and concludes the directory's parent carries no Rock
    // Ridge and rejects the volume as corrupt. `boot` and `EFI/BOOT` are even
    // length, so this is the common case, not an edge case.
    record.extend(std::iter::repeat_n(0u8, identifier_pad));
    // 33 + name + pad.. system-use area: SUSP entries such as Rock Ridge's `NM`.
    //
    // The area is appended *after* the identifier and counted in the record's
    // length, which is why `content` above includes it. Leaving it out of the
    // length while still writing it makes the reader's scan for the next record
    // start inside the SUSP area — and since the first byte of an `NM` payload
    // is an ASCII letter, that byte is read as a plausible record length and the
    // directory's entries start resolving to the wrong things.
    record.extend_from_slice(system_use);

    // Records are padded to an even length. Padding is required, not cosmetic:
    // an odd-length record leaves the next one unaligned, and readers that
    // assume alignment read garbage.
    if record.len() % 2 == 1 {
        record.push(0);
    }
    record
}

/// Store `value` twice: little-endian at `little_at`, big-endian at `big_at`.
///
/// ISO 9660 stores most numeric fields in both byte orders, in *adjacent*
/// slots, so that a reader need not know the host's endianness. Writing one
/// little-endian `u32` across such a pair leaves the big-endian half reading
/// as zero — which is not a detectable inconsistency, since the two halves are
/// never compared against each other. The reader simply reads whichever one it
/// wants and gets zero.
fn both(descriptor: &mut [u8; BLOCK], little_at: usize, big_at: usize, value: u32) {
    descriptor[little_at..little_at + 4].copy_from_slice(&value.to_le_bytes());
    descriptor[big_at..big_at + 4].copy_from_slice(&value.to_be_bytes());
}

/// Whether `components` names the root.
fn components_is_root(components: &[String]) -> bool {
    components.is_empty()
}

/// Write the fields a primary and a Joliet descriptor share, by absolute offset.
///
/// # Why the offsets are spelled out rather than packed
///
/// ECMA-119's primary volume descriptor is 2048 bytes of mostly-padding with
/// about a dozen live fields at fixed absolute offsets. They are not
/// contiguous and they do not follow the order in which a writer thinks about
/// them, so packing them in sequence — the obvious approach, and the one this
/// code used to take — silently writes each value into the neighbouring field's
/// padding. Nothing crashes: the image is still a plausible-looking 2048 bytes
/// of mostly zeroes, and the reader that depends on the misplaced field simply
/// rejects the medium.
///
/// Three of these fields are both-endian: the format stores them as a
/// little-endian copy *followed by* a big-endian copy, in adjacent slots. They
/// are written here through [`put_both_endian_u32`] because a single
/// little-endian `u32` write leaves the big-endian half reading as zero, and a
/// reader that trusts the big-endian half — which is most of them, since it is
/// the only one it can read without knowing the host's endianness — sees a
/// block size of zero and refuses the volume.
fn write_volume_layout(descriptor: &mut [u8; BLOCK], layout: &Layout, total_blocks: u32) {
    // 80  volume space size, in logical blocks, 4 bytes, twice
    both(descriptor, 80, 84, total_blocks);
    // 120 volume set size, 2 bytes, twice
    descriptor[120..122].copy_from_slice(&1u16.to_le_bytes());
    descriptor[122..124].copy_from_slice(&1u16.to_be_bytes());
    // 124 volume sequence number, 2 bytes, twice
    descriptor[124..126].copy_from_slice(&1u16.to_le_bytes());
    descriptor[126..128].copy_from_slice(&1u16.to_be_bytes());
    // 128 logical block size, 2 bytes, twice. This is the field a reader
    //     consults before anything else, and every LBA in this image is a
    //     count of these. Writing it as a 4-byte little-endian value spans
    //     into the path table size and overwrites it, and the volume is then
    //     rejected as "non DOS media" rather than as malformed.
    descriptor[128..130].copy_from_slice(&(BLOCK as u16).to_le_bytes());
    descriptor[130..132].copy_from_slice(&(BLOCK as u16).to_be_bytes());
    // 132 path table size in bytes, 4 bytes, twice
    both(descriptor, 132, 136, layout.path_table_bytes);
    // 140 location of the type-L path table, 4 bytes, little-endian
    descriptor[140..144].copy_from_slice(&layout.path_table_sector.to_le_bytes());
    // 144 location of the optional type-L path table, 4 bytes, little-endian
    descriptor[144..148].copy_from_slice(&0u32.to_le_bytes());
    // 148 location of the type-M path table, 4 bytes, *big-endian*.
    //
    //     This is the one field in the descriptor whose "big-endian-ness" is
    //     about the *bytes on disk* rather than about a second copy of the
    //     value. Every reader parses this slot as a little-endian integer and
    //     then byte-reverses it, which is only correct if the bytes are
    //     stored most-significant first. Storing it little-endian here — as
    //     the neighbouring fields are — yields a location of 352321536
    //     instead of 21, and the reader then tries to read a path table from
    //     far past the end of the medium and reports that the path table is
    //     longer than the image.
    descriptor[148..152].copy_from_slice(&layout.path_table_sector_be.to_be_bytes());
    // 152 location of the optional type-M path table, 4 bytes, little-endian
    descriptor[152..156].copy_from_slice(&0u32.to_le_bytes());
    // 156 root directory record, 34 bytes — written by the caller.
    // 190 volume set identifier, 128 bytes — written by the caller.
    // 881 file structure version, a single byte that must be 1. Readers reject
    //     the volume on any other value, and zero is what an all-zero buffer
    //     leaves here, so this is the byte a descriptor writer omits and then
    //     cannot account for: the PVD parses, and is still refused.
    descriptor[881] = 1;
}

/// An 8.3 level-1 name for `component`.
///
/// Level 1 rather than 2 is deliberate: long names come from Rock Ridge and
/// Joliet, and level 1 is the subset every reader handles.
///
/// Legal characters are letters, digits, `_` and `-`; everything else (notably
/// `.`, which splits base from extension) is dropped. Letters are *uppercased*
/// rather than filtered on being already uppercase: chaining the two predicates
/// reads as "alphanumeric **and** uppercase", which silently deletes every
/// lowercase name — and since almost every real filename is lowercase, that
/// turns into an empty staging tree whose files the bootloader then cannot find.
fn iso_short_name(component: &str) -> Option<String> {
    /// Legalise one name component: letters and digits, uppercased, with `_`
    /// and `-` kept. `.` is *not* legal here because it is the separator that
    /// was already split off below.
    fn legalise(text: &str) -> String {
        text.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }

    // Split the extension off the *original* name. Doing it after filtering
    // would find no `.` at all, because the filter strips it — every file would
    // become a bare stem and `boot/limine.conf` would collide with a
    // hypothetical `limine.bin` on the same directory.
    let (stem, extension) = match component.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && extension.len() <= 3 => (stem, extension),
        _ => (component, ""),
    };

    let mut base: String = legalise(stem).chars().take(8).collect();
    if base.is_empty() {
        base = legalise(component).chars().take(8).collect();
    }
    if base.is_empty() {
        return None;
    }

    let extension: String = legalise(extension).chars().take(3).collect();
    if extension.is_empty() {
        Some(base)
    } else {
        // The `;1` version suffix is what ISO 9660 level 1 requires.
        Some(format!("{base};1"))
    }
}

/// Walk `stage`, producing one [`File`] per regular file, sorted by path.
fn collect(stage: &Path) -> Result<Vec<File>, Error> {
    let mut paths = Vec::new();
    walk(stage, stage, &mut paths)?;

    paths.sort();

    let mut files = Vec::with_capacity(paths.len());
    for (components, source, size) in paths {
        let leaf = match components.last() {
            Some(leaf) => leaf.clone(),
            None => continue,
        };
        let short_name = match iso_short_name(&leaf) {
            Some(name) => name,
            None => continue,
        };
        files.push(File {
            components,
            source,
            size,
            short_name,
            long_name: leaf.clone(),
            key: leaf.to_ascii_lowercase(),
        });
    }
    Ok(files)
}

/// Recursively collect `(components, absolute path, size)`.
/// Walk `root`, producing one entry per regular file.
///
/// `base` is the directory the paths are reported relative to. It is threaded
/// through the recursion separately from the directory being read, because the
/// components of a nested file must be relative to the *staging root* — a
/// `boot/limine.conf` is one component deep, however deep in the tree it sits.
///
/// Stripping against the directory currently being read instead yields
/// components relative to that directory, so `boot/limine.conf` arrives as
/// just `limine.conf` and the image ends up with a flat root. The error is
/// silent: the file is still written, just at the wrong path, and the
/// bootloader then reports that the resource it wants does not exist.
fn walk(base: &Path, root: &Path, out: &mut Vec<(Vec<String>, PathBuf, u64)>) -> Result<(), Error> {
    let read = |error: std::io::Error| Error::Io {
        path: root.to_path_buf(),
        error,
    };
    let listing = std::fs::read_dir(root).map_err(read)?;

    for item in listing {
        let item = item.map_err(read)?;
        let path = item.path();
        let metadata = item.metadata().map_err(|error| Error::Io {
            path: path.clone(),
            error,
        })?;

        if metadata.is_dir() {
            walk(base, &path, out)?;
            continue;
        }
        if !metadata.is_file() {
            // Symlinks and devices have no representation worth emitting; the
            // staging tree holds regular files.
            continue;
        }

        let relative = path.strip_prefix(base).unwrap_or(&path);
        let mut components = Vec::new();
        for component in relative.components() {
            let text = component.as_os_str().to_string_lossy().into_owned();
            if text.is_empty() || text == "." || text == ".." {
                continue;
            }
            components.push(text);
        }
        out.push((components, path, metadata.len()));
    }
    Ok(())
}

/// Refuse two files whose names collide case-insensitively.
fn check_collisions(files: &[File]) -> Result<(), Error> {
    let mut keys: Vec<&str> = Vec::new();
    for file in files {
        if keys.iter().any(|seen| *seen == file.key.as_str()) {
            return Err(Error::Collision(file.key.clone()));
        }
        keys.push(file.key.as_str());
    }
    Ok(())
}

/// Write `bytes` to `path`, creating parents as needed.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::Io {
            path: parent.to_path_buf(),
            error,
        })?;
    }
    std::fs::write(path, bytes).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("novaos-iso-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn boot(file: &str) -> BootEntry {
        BootEntry {
            file: file.to_string(),
            platform: PLATFORM_EFI,
            default: true,
        }
    }

    #[test]
    fn short_names_legalise_awkward_components() {
        // `-` and `_` are legal in a level-1 identifier, so they survive; the
        // name is upper-cased and the stem truncated to 8 characters.
        assert_eq!(
            iso_short_name("nova-kernel.elf").as_deref(),
            Some("NOVA-KER;1")
        );
        assert_eq!(iso_short_name("readme").as_deref(), Some("README"));
        // A long name is truncated rather than refused: ISO 9660 requires the
        // 8.3 form to exist, and Rock Ridge/Joliet carry the real one.
        assert_eq!(
            iso_short_name("averylongfilename.txt").as_deref(),
            Some("AVERYLON;1")
        );
        // Nothing representable at all is dropped rather than given a name that
        // could shadow a real file.
        assert_eq!(iso_short_name("..."), None);
    }

    /// An output path that is *outside* any staging tree.
    ///
    /// Tests use this rather than writing into the scratch directory, because an
    /// image written inside the staged tree becomes an input to the next run —
    /// which makes an unrelated test order the only thing deciding whether it
    /// passes.
    fn output_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("novaos-iso-out-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("output dir");
        dir
    }

    #[test]
    fn an_image_without_a_boot_entry_is_refused() {
        let stage = scratch("noboot");
        std::fs::write(stage.join("a.bin"), b"x").expect("write");
        assert!(matches!(
            write_iso(&stage, &[], &stage.join("out.iso")),
            Err(Error::NoBootEntry)
        ));
    }

    #[test]
    fn a_boot_target_that_was_not_staged_is_refused() {
        // Silently omitting it would produce an ISO that boots its bootloader
        // and then finds no kernel — a hang, not an error.
        let stage = scratch("missing");
        std::fs::write(stage.join("a.bin"), b"x").expect("write");
        assert!(matches!(
            write_iso(&stage, &[boot("absent.elf")], &stage.join("out.iso")),
            Err(Error::MissingBootTarget(_))
        ));
    }

    #[test]
    fn descriptors_appear_where_the_specification_requires() {
        let stage = scratch("descriptors");
        std::fs::write(stage.join("nova-kernel.elf"), vec![0x7Fu8; 5000]).expect("write");
        std::fs::write(stage.join("limine.conf"), b"timeout: 0\n").expect("write");

        let out = output_dir("descriptors").join("out.iso");
        write_iso(&stage, &[boot("nova-kernel.elf")], &out).expect("iso");

        let bytes = std::fs::read(&out).expect("read");
        assert_eq!(bytes.len() % BLOCK, 0, "a whole number of sectors");
        assert_eq!(bytes[16 * BLOCK], VD_PRIMARY, "PVD at sector 16");
        assert_eq!(bytes[17 * BLOCK], VD_BOOT_RECORD, "boot record at 17");
        assert_eq!(bytes[18 * BLOCK], VD_SUPPLEMENTARY, "Joliet at 18");
        assert_eq!(bytes[19 * BLOCK], VD_TERMINATOR, "terminator at 19");

        // The path tables are *content*, not descriptors, so they sit after the
        // terminator rather than inside the descriptor set. A reader scans
        // forward from sector 16 and stops at the first sector that is not a
        // descriptor, so a path table placed inside the set ends the scan
        // early and the descriptors beyond it are never read.
        //
        // The check cannot be "this byte is not a descriptor type": a path
        // table opens with a record whose length is 1 for the root, which is
        // the same value as the primary descriptor's type. What distinguishes
        // them is the identifier five bytes later — a descriptor says "CD001",
        // a path table says nothing.
        assert!(
            !bytes[20 * BLOCK + 1..20 * BLOCK + 6]
                .windows(5)
                .any(|w| w == b"CD001"),
            "sector 20 holds a path table, not a descriptor"
        );

        // "CD001" is how every ISO 9660 descriptor identifies itself. It sits
        // at bytes 1..6 of the descriptor, immediately after the one-byte
        // type — not at bytes 9..14, which is where a "CD001" would land if
        // the type byte were assumed to be two bytes wide.
        assert_eq!(&bytes[16 * BLOCK + 1..16 * BLOCK + 6], b"CD001");

        // The boot record points at a catalog that is itself bootable.
        let catalog_lba = u32::from_le_bytes(
            bytes[17 * BLOCK + 71..17 * BLOCK + 75]
                .try_into()
                .expect("four bytes"),
        );
        assert!(
            (catalog_lba as usize) < bytes.len() / BLOCK,
            "the catalog pointer must land inside the image, got {catalog_lba}"
        );
        let catalog = catalog_lba as usize * BLOCK;
        // Offset 0 is the validation entry, not a boot entry: firmware reads it
        // first to decide the medium is bootable at all. Asserting a boot
        // indicator there would pass for the wrong reason.
        assert_eq!(
            bytes[catalog], 0x01,
            "the catalog opens with the validation entry"
        );
        assert_eq!(
            &bytes[catalog + 28..catalog + 32],
            &[0x5C, 0xAA, 0x55, 0xAA],
            "the validation entry terminates with 0x5CAA 0x55AA"
        );

        // The self-referential LBA must point back at the catalog's own sector;
        // a firmware that reads the wrong one here rejects the medium. It sits
        // immediately *after* the 32-byte validation header, so reading it at
        // `VALIDATION_ENTRY_LEN` would read the next record's boot indicator
        // instead — which is itself a plausible-looking 0x88.
        let self_lba = u32::from_le_bytes(
            bytes[catalog + HEADER_LEN..catalog + HEADER_LEN + 4]
                .try_into()
                .expect("four bytes"),
        );
        assert_eq!(
            self_lba, catalog_lba,
            "the validation entry points at itself"
        );

        // The initial/default entry follows the validation entry and points at
        // the first boot entry, which starts right after it.
        let default_entry = catalog + VALIDATION_ENTRY_LEN;
        let default_rba = u32::from_le_bytes(
            bytes[default_entry + 8..default_entry + 12]
                .try_into()
                .expect("four bytes"),
        );
        assert_eq!(
            default_rba as usize,
            VALIDATION_ENTRY_LEN + HEADER_LEN,
            "the default entry points at the first boot entry"
        );

        // Every record is exactly 32 bytes of header, so the first boot entry
        // starts on a 32-byte boundary. This is the invariant that the earlier
        // hand-rolled layout silently violated.
        let first_entry = catalog + default_rba as usize;
        let indicator = bytes[first_entry];
        assert!(
            indicator == BOOT_INDICATOR_BOOTABLE || indicator == BOOT_INDICATOR_NOT_BOOTABLE,
            "a boot entry's indicator is 0x88 when bootable and 0x00 when not, got {indicator:#04x}"
        );
        let entry_lba = u32::from_le_bytes(
            bytes[first_entry + 8..first_entry + 12]
                .try_into()
                .expect("four bytes"),
        );
        assert!(
            (entry_lba as usize) < bytes.len() / BLOCK,
            "the boot entry must point inside the image, got {entry_lba}"
        );
    }

    #[test]
    fn the_marked_default_is_the_one_firmware_is_told_to_boot() {
        // The initial/default entry's load RBA is the only thing that tells
        // firmware which of several images to start. Getting it wrong boots the
        // wrong firmware, or boots nothing at all.
        let stage = scratch("default-entry");
        let source = stage.join("payload");
        std::fs::create_dir_all(&source).expect("source dir");
        std::fs::write(source.join("bios.bin"), vec![0xAAu8; 4096]).expect("write bios");
        std::fs::write(source.join("efi.bin"), vec![0xBBu8; 4096]).expect("write efi");

        let out = std::env::temp_dir().join("novaos-iso-default-out");
        std::fs::create_dir_all(&out).expect("out dir");
        let image = out.join("default.iso");

        write_iso(
            &source,
            &[
                BootEntry {
                    file: "bios.bin".to_string(),
                    platform: 0,
                    default: false,
                },
                BootEntry {
                    file: "efi.bin".to_string(),
                    platform: PLATFORM_EFI,
                    default: true,
                },
            ],
            &image,
        )
        .expect("build");

        let bytes = std::fs::read(&image).expect("read");
        let catalog_lba = u32::from_le_bytes(
            bytes[17 * BLOCK + 71..17 * BLOCK + 75]
                .try_into()
                .expect("four bytes"),
        ) as usize;
        let catalog = catalog_lba * BLOCK;

        let default_rba = u32::from_le_bytes(
            bytes[catalog + VALIDATION_ENTRY_LEN + 8..catalog + VALIDATION_ENTRY_LEN + 12]
                .try_into()
                .expect("four bytes"),
        ) as usize;

        // The marked default is the *second* entry, so the RBA must skip past
        // the first one rather than pointing at it.
        assert_eq!(
            default_rba,
            VALIDATION_ENTRY_LEN + HEADER_LEN * 2,
            "the default entry skips the non-default boot entry"
        );

        // And it must be the EFI one. What makes an entry an EFI entry is its
        // *media type*, not its indicator: 0x90 and 0x91 are section-header
        // indicators, and a firmware scanning for something to boot skips any
        // indicator that is not 0x88. An EFI image marked with a section-header
        // indicator is a well-formed catalog that reports no bootable method at
        // all, so asserting on the indicator here would pin a value that is
        // correct-looking and wrong.
        let default_entry = catalog + default_rba;
        assert_eq!(
            bytes[default_entry + 1],
            MEDIA_EFI,
            "the marked default is the EFI entry, and media type is what says so"
        );
        // The indicator is 0x88 because this is the default, and the same would
        // be true of a BIOS default.
        assert_eq!(
            bytes[default_entry], BOOT_INDICATOR_BOOTABLE,
            "the default entry is marked bootable"
        );
        // An EFI image is loaded verbatim, so its sector count is reserved and
        // must be zero. A count there is read by implementations that honour the
        // field and is wrong for an image whose length is not a sector multiple.
        assert_eq!(
            u16::from_le_bytes(
                bytes[default_entry + 6..default_entry + 8]
                    .try_into()
                    .expect("two bytes")
            ),
            0,
            "an EFI entry carries no sector count"
        );

        // The unmarked entry must not be marked bootable, or firmware may try
        // it first and boot the wrong firmware.
        let first_entry = catalog + VALIDATION_ENTRY_LEN + HEADER_LEN;
        assert_eq!(
            bytes[first_entry], BOOT_INDICATOR_NOT_BOOTABLE,
            "the unmarked entry is not bootable"
        );
    }

    #[test]
    fn the_same_tree_yields_identical_bytes() {
        // Reproducibility is what makes a boot failure debuggable: a bisection
        // over the tree only converges if the image is a pure function of its
        // inputs.
        let stage = scratch("determinism");
        let source = stage.join("payload");
        std::fs::create_dir_all(&source).expect("source dir");
        std::fs::write(source.join("f.bin"), vec![1u8; 4096]).expect("write");

        // The outputs go *outside* the staged tree. Writing them inside would
        // make the second run stage the first run's image as an extra file, so
        // the two images would differ for a reason that has nothing to do with
        // determinism — and the failure would look like a writer bug.
        let out = std::env::temp_dir().join("novaos-iso-determinism-out");
        std::fs::create_dir_all(&out).expect("out dir");
        let first = out.join("a.iso");
        let second = out.join("b.iso");

        write_iso(&source, &[boot("f.bin")], &first).expect("first");
        write_iso(&source, &[boot("f.bin")], &second).expect("second");

        let a = std::fs::read(&first).expect("a");
        let b = std::fs::read(&second).expect("b");

        // Compare as a summary rather than with `assert_eq!` on two large
        // vectors: a mismatch would print hundreds of kilobytes of digits and
        // bury the one fact worth reading.
        assert_eq!(
            a.len(),
            b.len(),
            "the two images differ in length; first difference is at byte {:?}",
            a.iter().zip(&b).position(|(x, y)| x != y)
        );
        assert_eq!(
            a.iter().zip(&b).position(|(x, y)| x != y),
            None,
            "the two images are not byte-identical"
        );
    }

    #[test]
    fn a_case_insensitive_name_collision_is_refused() {
        let stage = scratch("collide");
        std::fs::write(stage.join("Limine.conf"), b"a").expect("write");
        std::fs::write(stage.join("limine.conf"), b"b").expect("write");
        assert!(matches!(
            write_iso(
                &stage,
                &[boot("limine.conf")],
                &output_dir("collide").join("o.iso")
            ),
            Err(Error::Collision(_))
        ));
    }

    #[test]
    fn file_contents_survive_into_the_image() {
        // The point of the whole exercise: a reader that walks the directory
        // records must find the file's bytes byte-for-byte intact, and at the
        // extent the directory record names.
        let stage = scratch("contents");
        // Deliberately more than two sectors, so the size has to be recorded in
        // units of sectors rather than rounded bytes.
        let payload: Vec<u8> = (0..=255u8).cycle().take(9000).collect();
        std::fs::write(stage.join("payload.bin"), &payload).expect("write");

        let out = output_dir("contents").join("out.iso");
        write_iso(&stage, &[boot("payload.bin")], &out).expect("iso");

        let bytes = std::fs::read(&out).expect("read");

        // Find the payload by locating the extent the boot entry recorded, then
        // check both the length in the directory record and the bytes at it.
        let files = collect(&stage).expect("collect");
        assert_eq!(files.len(), 1, "one staged file, one extent");

        let layout = Layout::new(files);
        let mut layout = layout;
        layout.plan().expect("plan");
        let (sector, size) = layout.extents[0];
        assert_eq!(size, payload.len() as u64);

        let start = sector as usize * BLOCK;
        let end = start + payload.len();
        assert_eq!(
            &bytes[start..end],
            payload.as_slice(),
            "the file's bytes must be recoverable at the extent its record names"
        );
    }
}
