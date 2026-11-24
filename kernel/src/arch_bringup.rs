//! QEMU entry shared by the AArch64, RISC-V 64 and LoongArch 64 early ports.

use core::ops::Range;
use core::ptr::addr_of;

use nova_boot::fdt::{self, MemoryMap};

use crate::arch;
use crate::arch::capabilities::Console;
use crate::early_init::{self, PAGE_SIZE, PROCESSES};
use crate::initramfs::Archive;

/// The entry point the early architectures call from their `_start` stub.
///
/// # Safety
///
/// Reached only from the assembly stub with interrupts masked, on the
/// kernel's own stack, with no allocator and no services running.
#[unsafe(no_mangle)]
pub extern "C" fn bringup_entry(boot_arg0: u64, boot_arg1: u64, boot_arg2: u64) -> ! {
    // The console goes first: everything after this prints, and a console
    // that is not initialised silently drops bytes.
    crate::arch::console_init();

    let mut sink = crate::arch::capabilities::ByteSink::<256>::new();
    use core::fmt::Write as _;
    let _ = write!(sink, "[NOVA] INFO  | NovaOS {} boot\r\n", arch::arch_name());
    let _ = write!(
        sink,
        "[NOVA] INFO  | arch: {} page {} B\r\n",
        arch::arch_name(),
        arch::ADDRESS_SPACE.page_size
    );
    let _ = write!(
        sink,
        "[NOVA] INFO  | abi: {}.{} ({} syscalls)\r\n",
        nova_abi::ABI_VERSION.major,
        nova_abi::ABI_VERSION.minor,
        nova_abi::nr::ALL.len()
    );
    arch::console().send_bytes(sink.as_bytes());

    // SAFETY: the entry contract hands execution over exactly once, with
    // interrupts masked; `arch_init` may assume that.
    unsafe { arch::init() };

    let mut result = crate::arch::capabilities::ByteSink::<256>::new();
    let check = boot_users(boot_arg0, boot_arg1, boot_arg2);
    let status = match check {
        Ok(()) => {
            let _ = write!(
                result,
                "[NOVA-ARCH] PASS {} user-trap\r\n",
                arch::arch_name()
            );
            crate::EXIT_SUCCESS
        }
        Err(reason) => {
            let _ = write!(
                result,
                "[NOVA-ARCH] FAIL {} {reason}\r\n",
                arch::arch_name()
            );
            crate::EXIT_TEST_FAILURE
        }
    };
    arch::console().send_bytes(result.as_bytes());
    unsafe { arch::exit(status) }
}

unsafe extern "C" {
    static __text_start: u8;
    static __image_end_vma: u8;
}

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub(crate) fn report_vector_bits(bits: usize) {
    if !arch::virt_smp::is_boot_cpu() {
        return;
    }
    use core::fmt::Write as _;
    let mut sink = arch::capabilities::ByteSink::<64>::new();
    let _ = write!(sink, "[NOVA-ARCH] vector-bits {bits}\r\n");
    arch::console().send_bytes(sink.as_bytes());
}

pub(crate) struct EarlyFrames {
    start: usize,
    used: u128,
    #[cfg(target_arch = "riscv64")]
    timebase: Option<core::num::NonZeroU32>,
}

impl EarlyFrames {
    fn new(map: &mut MemoryMap, kernel: Range<u64>) -> Result<Self, &'static str> {
        let size = (u128::BITS as usize * PAGE_SIZE) as u64;
        let start = map.allocate_after_image(kernel, size, PAGE_SIZE as u64)
            .ok_or("no firmware RAM for an early CPU frame pool")?;
        if arch::ADDRESS_SPACE.physical_bits.is_some_and(|bits| (start + size - 1) >> bits != 0) {
            return Err("early frame pool exceeds the physical address width");
        }
        Ok(Self {
            start: start as usize,
            used: 0,
            #[cfg(target_arch = "riscv64")]
            timebase: map.timebase_frequency(),
        })
    }

    #[cfg(target_arch = "riscv64")]
    pub(crate) fn timebase_frequency(&self) -> Option<core::num::NonZeroU32> {
        self.timebase
    }

    #[cfg(target_arch = "aarch64")]
    pub(crate) fn identity_range(&self) -> Range<usize> {
        self.start..self.start + u128::BITS as usize * PAGE_SIZE
    }

    fn reserve(&mut self) -> Result<usize, &'static str> {
        let index = self.used.trailing_ones();
        if index == u128::BITS {
            return Err("early CPU frame pool exhausted");
        }
        self.used |= 1 << index;
        Ok(self.start + index as usize * PAGE_SIZE)
    }

    /// # Safety
    /// Available physical frames must be writable at their identity addresses.
    pub(crate) unsafe fn allocate<const COUNT: usize>(&mut self) -> Result<[usize; COUNT], &'static str> {
        let mut frames = [0; COUNT];
        unsafe { self.allocate_into(&mut frames)? };
        Ok(frames)
    }

    /// # Safety
    /// Available physical frames must be writable at their identity addresses.
    pub(crate) unsafe fn allocate_into(&mut self, frames: &mut [usize]) -> Result<(), &'static str> {
        if frames.len() > self.used.count_zeros() as usize {
            return Err("early CPU frame pool exhausted");
        }
        for frame in frames {
            let address = self.reserve()?;
            unsafe { core::ptr::write_bytes(address as *mut u8, 0, PAGE_SIZE) };
            *frame = address;
        }
        Ok(())
    }

    pub(crate) fn release(&mut self, frames: impl IntoIterator<Item = usize>) -> Result<(), &'static str> {
        let mut remaining = self.used;
        for frame in frames {
            let offset = frame.checked_sub(self.start).ok_or("frame outside early CPU pool")?;
            let index = offset / PAGE_SIZE;
            if index >= u128::BITS as usize || offset % PAGE_SIZE != 0 || remaining & (1 << index) == 0 {
                return Err("releasing an unowned early frame");
            }
            remaining &= !(1 << index);
        }
        self.used = remaining;
        Ok(())
    }
}

fn boot_users(boot_arg0: u64, boot_arg1: u64, boot_arg2: u64) -> Result<(), &'static str> {
    let (mut map, kernel, archive) = firmware_memory(boot_arg0, boot_arg1, boot_arg2)?;
    let mut ids = [0; arch::virt_smp::MAX_CPUS];
    let count = map.cpu_ids().len();
    ids.get_mut(..count).ok_or("too many early CPUs")?.copy_from_slice(map.cpu_ids());
    let ids = &ids[..count];
    arch::virt_smp::verify(ids)?;
    let mut frames = EarlyFrames::new(&mut map, kernel.clone())?;
    let init = archive.get(early_init::INIT_NAME);
    let check = archive.get(early_init::CHECK_NAME);
    let server = archive.get(early_init::ECHO_NAME);
    if let Some(check) = check {
        let peers = ids.iter().copied().filter(|id| *id as usize != arch::current_cpu());
        let shared = if count > 1 { Some(unsafe { frames.allocate::<1>()?[0] }) } else { None };
        for (index, id) in peers.enumerate() {
            let pool = EarlyFrames::new(&mut map, kernel.clone())?;
            let remote = shared.filter(|_| index == 0).map(|frame| RemoteMutex { frame, second_cpu: true });
            arch::virt_smp::start_check(id as usize, pool, check, remote)?;
        }
        run_check(&mut frames, check, shared.map(|frame| RemoteMutex { frame, second_cpu: false }))?;
        arch::virt_smp::finish_checks(ids)?;
        if let Some(shared) = shared {
            if !early_init::verified_remote_wake() {
                return Err("no cross-CPU futex wake IPI was received");
            }
            frames.release([shared])?;
            arch::console().send_bytes(b"[NOVA-ARCH] SMP cross-CPU futex wake IPI verified\r\n");
        }
    }
    if let Some(server) = server {
        let init = init.ok_or("initramfs has no init")?;
        let bsp = arch::current_cpu();
        if let Some(ap) = ids.iter().map(|&id| id as usize).find(|&id| id != bsp) {
            let pool = EarlyFrames::new(&mut map, kernel)?;
            early_init::install_remote_ipc([arch::virt_smp::current_slot(), arch::virt_smp::slot_for(ap)])?;
            arch::virt_smp::start_resident(ap, pool, server)?;
            return run_resident(&mut frames, init);
        }
        let images = [early_init::load(init, &mut frames)?, early_init::load(server, &mut frames)?];
        return arch::verify_privilege_transition(&mut frames, None, &images);
    }
    if check.is_some() {
        Ok(())
    } else {
        run_check(&mut frames, init.ok_or("initramfs has no init")?, None)
    }
}

pub(crate) fn run_resident(frames: &mut EarlyFrames, image: &[u8]) -> Result<(), &'static str> {
    let image = early_init::load(image, frames)?;
    arch::verify_privilege_transition(frames, None, &[image])
}

#[derive(Clone, Copy)]
pub(crate) struct RemoteMutex {
    frame: usize,
    second_cpu: bool,
}

pub(crate) fn run_check(frames: &mut EarlyFrames, check: &[u8], remote: Option<RemoteMutex>) -> Result<(), &'static str> {
    let previous = frames.used;
    let stacks = unsafe { frames.allocate::<2>()? };
    let shared = match remote {
        Some(remote) => remote.frame,
        None => unsafe { frames.allocate::<1>()?[0] },
    };
    let mut init = [None; PROCESSES];
    for slot in &mut init {
        *slot = Some(early_init::load(check, frames)?);
    }
    let mut init = init.map(|image| image.expect("init slot loaded"));
    for image in &mut init[..early_init::SERVICES] {
        image.shared = Some(shared);
    }
    if let Some(remote) = remote {
        let slot = usize::from(remote.second_cpu);
        if remote.second_cpu {
            for image in &mut init[..early_init::SERVICES] {
                image.probe_flags = early_init::PROBE_SECOND_CPU;
            }
        }
        init[slot].probe_flags |= early_init::PROBE_REMOTE_MUTEX;
    }
    arch::verify_privilege_transition(frames, Some(stacks), &init)?;
    frames.release(stacks)?;
    if remote.is_none() {
        frames.release([shared])?;
    }
    if frames.used != previous {
        return Err("early user or probe frames leaked");
    }
    Ok(())
}

fn firmware_memory(boot_arg0: u64, boot_arg1: u64, boot_arg2: u64)
    -> Result<(MemoryMap, Range<u64>, Archive), &'static str>
{
    let address = match arch::arch_name() {
        "aarch64" if boot_arg0 != 0 && boot_arg0.is_multiple_of(8) => boot_arg0,
        "loongarch64" => 0x10_0000,
        "riscv64" if boot_arg1 != 0 && boot_arg1.is_multiple_of(8) => boot_arg1,
        _ => return Err("firmware did not supply a device tree"),
    };
    let header = unsafe { core::slice::from_raw_parts(address as *const u8, fdt::HEADER_SIZE) };
    let size = fdt::total_size(header).ok_or("invalid firmware device tree header")?;
    address
        .checked_add(size as u64)
        .ok_or("invalid firmware device tree range")?;
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, size) };
    let mut map = MemoryMap::from_dtb(bytes, address).ok_or("invalid firmware RAM map")?;
    let image = addr_of!(__text_start) as u64..addr_of!(__image_end_vma) as u64;
    if arch::arch_name() == "loongarch64" {
        let initrd = loongarch_initrd(boot_arg2).ok_or("missing LoongArch EFI initrd table")?;
        map.reserve_initrd(initrd).ok_or("invalid LoongArch initrd range")?;
    }
    let archive = map.initrd().ok_or("firmware did not supply an initramfs in RAM")?;
    let size = archive.end - archive.start;
    if size > 2 * 1024 * 1024 || (archive.start < image.end && image.start < archive.end) {
        return Err("invalid initramfs size or overlap with kernel");
    }
    let bytes = unsafe { core::slice::from_raw_parts(archive.start as *const u8, size as usize) };
    Ok((map, image, Archive::parse(bytes)?))
}

fn loongarch_initrd(system_table: u64) -> Option<core::ops::Range<u64>> {
    const EFI_SIGNATURE: u64 = 0x5453_5953_2049_4249;
    const INITRD_GUID: (u64, u64) = (0x4f3d_68fc_5568_e427, 0x68cc_3152_55ca_74ac);
    const TABLE_LIMIT: u64 = 0x10_0000;

    let read = |address: u64| {
        if address == 0 || address >= TABLE_LIMIT || address % 8 != 0 {
            return None;
        }
        Some(unsafe { (address as *const u64).read_volatile() })
    };
    if read(system_table)? != EFI_SIGNATURE || system_table > TABLE_LIMIT - 120 {
        return None;
    }
    let count = read(system_table + 104)?;
    let entries = read(system_table + 112)?;
    if count > 64 || entries >= TABLE_LIMIT {
        return None;
    }
    for index in 0..count {
        let entry = entries + index * 24;
        if (read(entry)?, read(entry + 8)?) == INITRD_GUID {
            let data = read(entry + 16)?;
            let start = read(data)?;
            let size = read(data + 8)?;
            return Some(start..start.checked_add(size)?);
        }
    }
    None
}
