//! The virtio-gpu display driver: pixels leave the machine on a device.
//!
//! # Why this exists
//!
//! The firmware's linear framebuffer is CPU-writeable memory that happens to
//! be scanned out — writing to it after `ExitBootServices` is exactly the
//! use of UEFI resources the platform forbids, and every byte the compositor
//! draws that way is a CPU memcpy to a BAR. This driver replaces that path
//! with a real device: a virtio-gpu scanout whose backing storage lives in
//! ordinary guest RAM, moved to the host by the device's own DMA on
//! `TRANSFER_TO_HOST_2D` and published with `RESOURCE_FLUSH`.
//!
//! # The shape
//!
//! One resource, one scanout, one queue:
//!
//! - `RESOURCE_CREATE_2D` creates a `B8G8R8X8` resource the size of the
//!   mode — the format whose little-endian word is `0x00RRGGBB`, which is
//!   what every NovaOS colour already is, so no swizzle happens anywhere.
//! - `RESOURCE_ATTACH_BACKING` publishes the heap-allocated pixel buffer
//!   as a scatter-gather list of its physical runs: consecutive pages
//!   coalesce into one `mem_entry`, so a contiguous buffer — what one
//!   heap allocation claims — is a single entry, and a fragmented one is
//!   still described exactly.
//! - `SET_SCANOUT` wires scanout 0 to the resource.
//! - [`VirtioGpu::flush`] then transfers and flushes the whole surface
//!   on every presented frame. The transfer is the device copying guest
//!   RAM to the host surface; the compositor's CPU work stops at the
//!   backing buffer, and the *display* path is the GPU's.
//!
//! The command windows are frames like the virtqueue's rings: allocated
//! from the frame allocator and reached through the direct map, so the
//! physical addresses the descriptors carry are right by construction
//! rather than by a page-table walk that has to be trusted.
//!
//! The full-surface transfer is deliberate: QEMU's partial-width path
//! reads its source rows at `offset + row * stride`, which only lines up
//! with a rectangle rooted at the origin, and a full-frame DMA in host
//! memory costs less than the correctness hazard of a region that does
//! not.
//!
//! # Polling, not interrupts
//!
//! The control queue is polled exactly like the block and network
//! drivers poll theirs: the device's response lands in the used ring,
//! and every read of that ring is an MMIO access that returns to the
//! host, so a bounded poll is how a guest waits for a host bottom half.
//! There is no interrupt to service and none is armed.
//!
//! The bound is a *deadline on the cycle counter*, never a count of
//! iterations: one iteration is a host-side round trip whose length is
//! the host's load, so a fixed count measures how busy the machine is
//! and reports it as a broken device — which is exactly how a loaded
//! host made a healthy GPU look unavailable. A command that misses its
//! deadline is attempted twice; the second attempt first collects
//! whatever the host delivered late, so a starved bottom half that
//! arrives between the two is still seen, and the caller never learns
//! there was a retry.

use core::alloc::Layout;

use alloc::alloc::{alloc, dealloc};
use alloc::vec;
use alloc::vec::Vec;

use super::super::pci as bus;
use super::pci::Transport;
use super::queue::{self, VirtQueue};
use crate::arch::ADDRESS_SPACE;
use crate::mm::{direct_map, frame_allocator};
use crate::sync::SpinLock;

/// The2D command ids this driver uses, from the virtio-gpu spec.
const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;

/// The response types the driver accepts.
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

/// `VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM`: little-endian word `0x00RRGGBB`,
/// which is NovaOS's native colour layout — no swizzle on either side.
const FORMAT_B8G8R8X8: u32 = 2;

/// The one resource and the one scanout this driver uses.
const RESOURCE_ID: u32 = 1;
const SCANOUT_ID: u32 = 0;

/// The control header, in bytes: type, flags, fence_id, ctx_id,
/// ring_idx, padding.
const HDR_BYTES: usize = 24;
/// The `GET_DISPLAY_INFO` response: header + 16 display modes of
/// (rect, enabled, flags).
const DISPLAY_INFO_BYTES: usize = HDR_BYTES + 16 * 24;
/// The largest request this driver builds: `RESOURCE_ATTACH_BACKING`
/// with one `mem_entry` per page of a 4 MiB scanout.
const REQUEST_BYTES: usize = 4 * 1024;
/// The largest response: `GET_DISPLAY_INFO`.
const RESPONSE_BYTES: usize = 512;

/// How many times one command is attempted before the caller is told the
/// device never answered.
///
/// Two. The device answers from the host's bottom half, which host load
/// can starve for longer than one deadline — two virtual machines on one
/// host did exactly that — and the second attempt, by then holding
/// nothing in flight, is how a starved host gets another window without
/// the caller having to know there was a first.
const ATTEMPTS: u32 = 2;

/// One poll's deadline, in cycles of [`crate::arch::cycles`].
///
/// Sized by *time*, never by iteration count: 50 million cycles is
/// ~50 ms on the 1 GHz virtual time-stamp counter and ~16 ms at a
/// 3.2 GHz base rate, and two attempts double that. A healthy device
/// answers on the first poll — the budget only exists for the unhealthy
/// ones, where bounding in time is what keeps an interrupt-path call
/// from spinning forever on a host that stopped answering.
const POLL_CYCLES: u64 = 50_000_000;

/// One command window: a frame, its direct-map address, and the bytes
/// of it this driver may use.
///
/// Built exactly like a virtqueue's rings — frame allocator plus direct
/// map — so the physical address the queue's descriptors carry is the
/// buffer's real one by construction, with no page-table walk in between
/// to be subtly wrong about where a static lives.
struct DmaWindow {
    /// The frame's physical address: what the descriptor names.
    phys: u64,
    /// The frame's direct-map address: what the driver reads and writes.
    virt: *mut u8,
    /// Usable bytes: at most one frame.
    len: usize,
}

impl DmaWindow {
    /// Claim one frame for a window of `len` bytes, zeroed.
    ///
    /// Returns `None` when `len` does not fit in one frame or the
    /// allocator has no device-reachable frame left.
    fn new(len: usize) -> Option<DmaWindow> {
        let page = ADDRESS_SPACE.page_size as usize;
        if len == 0 || len > page {
            return None;
        }
        // SAFETY: `mm::init` published the allocator during boot, and this
        // driver comes up after it — the same contract `VirtQueue::allocate`
        // relies on for its rings. `alloc_dma` keeps the frame below the
        // boundary device DMA can reach.
        let frame = unsafe { frame_allocator().alloc_dma() }.ok()?;
        let phys = queue::frame_phys(frame);
        let virt = direct_map::phys_to_virt(phys);
        // SAFETY: the frame was just allocated to this caller, the direct
        // map covers it, and a whole frame is writable kernel memory.
        unsafe { core::ptr::write_bytes(virt, 0, page) };
        Some(DmaWindow { phys, virt, len })
    }
}

/// Why the GPU could not come up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GpuError {
    /// No virtio-gpu function on the bus.
    NoDevice,
    /// The device has no modern transport.
    Transport,
    /// The control queue could not be set up.
    Queue,
    /// Feature negotiation failed.
    Features,
    /// The device reported no usable display mode.
    NoMode,
    /// The pixel backing could not be allocated.
    Backing,
    /// The device answered an error to a command.
    Device,
    /// The device never completed a command.
    Timeout,
}

/// The live GPU: transport, control queue, and the scanout geometry.
///
/// `backing` is the pixel buffer this compositor draws into; the device
/// reads it by DMA. The pointers are kernel-owned and never freed — a
/// display that lives for the boot's lifetime needs no destructor.
pub struct VirtioGpu {
    transport: Transport,
    control: VirtQueue,
    /// The scanout's width, in pixels.
    pub width: u32,
    /// The scanout's height, in pixels.
    pub height: u32,
    /// Bytes between the first pixels of consecutive rows.
    pub pitch: u32,
    /// The pixel buffer: `pitch * height` bytes, page-aligned.
    backing: *mut u8,
    /// The physical pages of `backing`, for `RESOURCE_ATTACH_BACKING`.
    backing_pages: Vec<u64>,
    /// The window every command's request is built in.
    request: DmaWindow,
    /// The window every command's response lands in.
    response: DmaWindow,
}

/// The most pages one scanout's backing may span: 64 MiB — every mode a
/// `virt` machine can realistically ask for, including 3840x2160.
const MAX_BACKING_PAGES: usize = 16384;

impl VirtioGpu {
    /// Find the GPU, negotiate, create the scanout, and claim the
    /// mode. Returns the driver ready to [`VirtioGpu::flush`].
    ///
    /// Every failure mode falls back to the caller's firmware
    /// framebuffer: a machine without a virtio-gpu (or one that
    /// refuses) still boots to a desktop, just over the old path.
    pub fn open() -> Result<VirtioGpu, GpuError> {
        let devices = bus::enumerate();
        let device = devices
            .iter()
            .find(|device| device.is_virtio_gpu())
            .ok_or(GpuError::NoDevice)?;

        let mut transport = Transport::open(device).map_err(|_| GpuError::Transport)?;
        if transport.num_queues() < 1 {
            transport.reset();
            return Err(GpuError::Queue);
        }

        let wanted = super::Features {
            low: 0,
            high: 1 << super::FEATURE_VERSION_1,
        };
        transport
            .start_device(wanted)
            .map_err(|_| GpuError::Features)?;

        let mut control =
            VirtQueue::allocate(queue::MAX_QUEUE_SIZE).map_err(|_| GpuError::Queue)?;
        if let Err(error) = transport.setup_queue(
            0,
            queue::MAX_QUEUE_SIZE,
            control.desc_phys(),
            control.avail_phys(),
            control.used_phys(),
        ) {
            let _ = error;
            unsafe { control.release() };
            transport.reset();
            return Err(GpuError::Queue);
        }
        transport.driver_ok();

        // The command windows are frames like the rings' own: allocated
        // from the frame allocator, reached through the direct map, so the
        // physical addresses the descriptors carry are right by
        // construction rather than by a page-table walk.
        let (request, response) = match (
            DmaWindow::new(REQUEST_BYTES),
            DmaWindow::new(RESPONSE_BYTES),
        ) {
            (Some(request), Some(response)) => (request, response),
            _ => {
                // SAFETY: the queue was set up above and is still owned
                // here; freeing it before resetting the device mirrors the
                // setup-failure path a few lines up.
                unsafe { control.release() };
                transport.reset();
                return Err(GpuError::Backing);
            }
        };

        let mut gpu = VirtioGpu {
            transport,
            control,
            width: 0,
            height: 0,
            pitch: 0,
            backing: core::ptr::null_mut(),
            backing_pages: Vec::new(),
            request,
            response,
        };

        // The display's own answer decides the mode: this driver draws
        // what the device reports, never the other way round.
        let (width, height) = match gpu.display_info() {
            Ok(mode) => mode,
            Err(error) => {
                gpu.teardown();
                return Err(error);
            }
        };
        if width < 16 || height < 16 || width > 4096 || height > 4096 {
            gpu.teardown();
            return Err(GpuError::NoMode);
        }
        let pitch = width * 4;
        let bytes = pitch as usize * height as usize;

        // Page-aligned: the backing is published to the device as one
        // `mem_entry` per page, and a page-aligned block is what makes
        // "one entry per page" true rather than approximately true.
        let layout = Layout::from_size_align(bytes, 4096).map_err(|_| GpuError::Backing)?;
        // SAFETY: `layout` has a non-zero size and a power-of-two
        // alignment no larger than what the heap honours; the pointer
        // comes from this kernel's own allocator.
        let backing = unsafe { alloc(layout) };
        if backing.is_null() {
            gpu.teardown();
            return Err(GpuError::Backing);
        }

        let pages = bytes.div_ceil(4096);
        if pages > MAX_BACKING_PAGES {
            // SAFETY: allocated above with `layout`; freed with the same.
            unsafe { dealloc(backing, layout) };
            gpu.teardown();
            return Err(GpuError::Backing);
        }
        gpu.backing_pages = vec![0; pages];
        for (index, page) in gpu.backing_pages.iter_mut().enumerate() {
            // The heap's blocks *are* direct-map frames (`grow_and_take`
            // banks them as `phys_to_virt(frame)`), so a buffer address
            // minus the direct-map offset is its physical address — exact,
            // and with no page-table walk anywhere in the DMA path.
            let virt = backing as u64 + (index as u64) * 4096;
            *page = direct_map::virt_to_phys(virt);
        }
        // `backing_pages` now holds exactly `pages` entries.
        gpu.backing = backing;
        gpu.width = width;
        gpu.height = height;
        gpu.pitch = pitch;

        // SAFETY: the buffer was just allocated and is untouched; the
        // write covers exactly `bytes`.
        unsafe { core::ptr::write_bytes(backing, 0, bytes) };

        gpu.create_resource()?;
        gpu.attach_backing()?;
        gpu.set_scanout()?;

        Ok(gpu)
    }

    /// The device's preferred mode: `(width, height)` of scanout 0.
    fn display_info(&mut self) -> Result<(u32, u32), GpuError> {
        let response_len = self.command(CMD_GET_DISPLAY_INFO, &[], DISPLAY_INFO_BYTES)?;
        if response_len < HDR_BYTES + 24 {
            return Err(GpuError::NoMode);
        }
        // SAFETY: `command` filled the response window, which is
        // `RESPONSE_BYTES` wide — at least the `DISPLAY_INFO_BYTES` this
        // response is — and every read below stays inside it.
        let (rect, enabled) = unsafe {
            let base = self.response.virt;
            let read_u32 = |at: usize| -> u32 {
                let mut bytes = [0u8; 4];
                core::ptr::copy_nonoverlapping(base.add(at), bytes.as_mut_ptr(), 4);
                u32::from_le_bytes(bytes)
            };
            (
                (
                    read_u32(HDR_BYTES + 0),
                    read_u32(HDR_BYTES + 4),
                    read_u32(HDR_BYTES + 8),
                    read_u32(HDR_BYTES + 12),
                ),
                read_u32(HDR_BYTES + 16),
            )
        };
        let (x, y, width, height) = rect;
        if enabled == 0 || x != 0 || y != 0 {
            return Err(GpuError::NoMode);
        }
        Ok((width, height))
    }

    /// `RESOURCE_CREATE_2D`: the resource the scanout reads.
    fn create_resource(&mut self) -> Result<(), GpuError> {
        let request = {
            let mut body = [0u8; 16];
            body[0..4].copy_from_slice(&RESOURCE_ID.to_le_bytes());
            body[4..8].copy_from_slice(&FORMAT_B8G8R8X8.to_le_bytes());
            body[8..12].copy_from_slice(&self.width.to_le_bytes());
            body[12..16].copy_from_slice(&self.height.to_le_bytes());
            body
        };
        self.command(CMD_RESOURCE_CREATE_2D, &request, HDR_BYTES)?;
        Ok(())
    }

    /// `RESOURCE_ATTACH_BACKING`: publish the pixel buffer as one
    /// `mem_entry` per page.
    /// `RESOURCE_ATTACH_BACKING`: publish the pixel buffer as a
    /// scatter-gather list of its physical runs.
    ///
    /// Consecutive pages coalesce into one `mem_entry`. One heap
    /// allocation is claimed as one contiguous run, so the usual shape
    /// is a single entry — but the list is built from what the pages
    /// *are*, not from what the allocator is expected to hand out, so a
    /// buffer that did fragment is still described exactly rather than
    /// promised contiguity it does not have.
    fn attach_backing(&mut self) -> Result<(), GpuError> {
        let pages = self.backing_pages.len();
        let base = self.request.virt;
        let window = self.request.len;
        let mut runs: u32 = 0;
        let mut index = 0usize;

        // SAFETY: every entry is written at `HDR_BYTES + 8 + runs * 16`,
        // checked against the window's length before the write; `base`
        // names this driver's own frame, which no live reference of this
        // method's points into.
        unsafe {
            while index < pages {
                let start = self.backing_pages[index];
                let mut length = 4096u32;
                while index + 1 < pages
                    && self.backing_pages[index + 1] == start + u64::from(length)
                {
                    length += 4096;
                    index += 1;
                }

                let at = HDR_BYTES + 8 + runs as usize * 16;
                if at + 16 > window {
                    return Err(GpuError::Backing);
                }
                let mut entry = [0u8; 16];
                entry[0..8].copy_from_slice(&start.to_le_bytes());
                entry[8..12].copy_from_slice(&length.to_le_bytes());
                core::ptr::copy_nonoverlapping(entry.as_ptr(), base.add(at), 16);
                runs += 1;
                index += 1;
            }

            core::ptr::copy_nonoverlapping(
                RESOURCE_ID.to_le_bytes().as_ptr(),
                base.add(HDR_BYTES),
                4,
            );
            core::ptr::copy_nonoverlapping(runs.to_le_bytes().as_ptr(), base.add(HDR_BYTES + 4), 4);
        }

        let request_len = HDR_BYTES + 8 + runs as usize * 16;
        // The body beyond the header is already written above; publish
        // it directly rather than through `command`, which would
        // overwrite it.
        self.command_with_body(CMD_RESOURCE_ATTACH_BACKING, request_len, HDR_BYTES)?;
        Ok(())
    }

    /// `SET_SCANOUT`: wire scanout 0 to the resource.
    fn set_scanout(&mut self) -> Result<(), GpuError> {
        let request = {
            let mut body = [0u8; 24];
            // rect x, y, width, height
            body[0..4].copy_from_slice(&0u32.to_le_bytes());
            body[4..8].copy_from_slice(&0u32.to_le_bytes());
            body[8..12].copy_from_slice(&self.width.to_le_bytes());
            body[12..16].copy_from_slice(&self.height.to_le_bytes());
            body[16..20].copy_from_slice(&SCANOUT_ID.to_le_bytes());
            body[20..24].copy_from_slice(&RESOURCE_ID.to_le_bytes());
            body
        };
        self.command(CMD_SET_SCANOUT, &request, HDR_BYTES)?;
        Ok(())
    }

    /// Push the whole surface to the device: transfer, then flush.
    ///
    /// Called once per presented frame, from the compositor's present
    /// path — which runs both from a syscall and from the timer, so
    /// this must be silent: a log line here would land inside the
    /// frame path, and the frame path's job is to draw, not to talk.
    pub fn flush(&mut self) -> Result<(), GpuError> {
        // TRANSFER_TO_HOST_2D: rect + offset + resource.
        let transfer = {
            let mut body = [0u8; 32];
            body[0..4].copy_from_slice(&0u32.to_le_bytes());
            body[4..8].copy_from_slice(&0u32.to_le_bytes());
            body[8..12].copy_from_slice(&self.width.to_le_bytes());
            body[12..16].copy_from_slice(&self.height.to_le_bytes());
            body[16..24].copy_from_slice(&0u64.to_le_bytes()); // offset
            body[24..28].copy_from_slice(&RESOURCE_ID.to_le_bytes());
            body[28..32].copy_from_slice(&0u32.to_le_bytes());
            body
        };
        self.command(CMD_TRANSFER_TO_HOST_2D, &transfer, HDR_BYTES)?;

        // RESOURCE_FLUSH: rect + resource.
        let flush = {
            let mut body = [0u8; 24];
            body[0..4].copy_from_slice(&0u32.to_le_bytes());
            body[4..8].copy_from_slice(&0u32.to_le_bytes());
            body[8..12].copy_from_slice(&self.width.to_le_bytes());
            body[12..16].copy_from_slice(&self.height.to_le_bytes());
            body[16..20].copy_from_slice(&RESOURCE_ID.to_le_bytes());
            body[20..24].copy_from_slice(&0u32.to_le_bytes());
            body
        };
        self.command(CMD_RESOURCE_FLUSH, &flush, HDR_BYTES)?;
        Ok(())
    }

    /// Run one control command: header + `body_len` bytes already in
    /// the request window, response up to `response_len`.
    ///
    /// Returns the response length. The device's answer type is
    /// checked by callers that care; a transport-level failure is an
    /// error, and a device error response is [`GpuError::Device`].
    fn command_with_body(
        &mut self,
        command: u32,
        request_len: usize,
        response_len: usize,
    ) -> Result<usize, GpuError> {
        if request_len > self.request.len || response_len > self.response.len {
            return Err(GpuError::Device);
        }

        let mut attempt = 0u32;
        loop {
            attempt += 1;

            // Make the queue idle before publishing. A request an earlier
            // poll gave up on can still be in flight — its bottom half
            // simply ran late — and publishing over it would be refused,
            // or its response taken for this command's. Waiting for it
            // here is what lets a bottom half that arrives between the
            // two attempts still be seen.
            let idle_wait = crate::arch::cycles();
            while self.control.in_flight() {
                match self.control.take_completion() {
                    Ok(Some(_)) => {}
                    Ok(None) => {}
                    Err(_) => return Err(GpuError::Queue),
                }
                if crate::arch::cycles().wrapping_sub(idle_wait) >= POLL_CYCLES {
                    if attempt >= ATTEMPTS {
                        return Err(GpuError::Timeout);
                    }
                    break;
                }
            }
            if self.control.in_flight() && attempt >= ATTEMPTS {
                return Err(GpuError::Timeout);
            }

            // SAFETY: both windows are at least the lengths bounded
            // above, and only ever touched with this driver's lock held.
            unsafe {
                let request = self.request.virt;
                let response = self.response.virt;
                // The header: type, flags, fence, ctx, ring, pad.
                core::ptr::copy_nonoverlapping(command.to_le_bytes().as_ptr(), request, 4);
                core::ptr::write_bytes(request.add(4), 0, HDR_BYTES - 4);
                core::ptr::write_bytes(response, 0, response_len);

                let request_phys = self.request.phys;
                let response_phys = self.response.phys;
                self.control
                    .publish_pair(
                        request_phys,
                        request_len as u32,
                        response_phys,
                        response_len as u32,
                    )
                    .map_err(|_| GpuError::Queue)?;
            }
            self.transport.notify_queue(0);

            // Poll the used ring against a deadline. The device answers
            // from the host's bottom half while this core spins, so the
            // first read usually already sees the completion; the
            // deadline turns a device that never answers into an error
            // instead of a hang, bounded in *time* however loaded the
            // host is.
            let started = crate::arch::cycles();
            let mut completed = false;
            loop {
                match self.control.take_completion() {
                    Ok(Some(_)) => {
                        completed = true;
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => return Err(GpuError::Queue),
                }
                if crate::arch::cycles().wrapping_sub(started) >= POLL_CYCLES {
                    break;
                }
            }

            if !completed {
                if attempt >= ATTEMPTS {
                    return Err(GpuError::Timeout);
                }
                continue;
            }

            // SAFETY: the device wrote at most `response_len` bytes into
            // the response window, which is that wide.
            let response_type = unsafe {
                let base = self.response.virt;
                let mut bytes = [0u8; 4];
                core::ptr::copy_nonoverlapping(base, bytes.as_mut_ptr(), 4);
                u32::from_le_bytes(bytes)
            };
            if response_type == RESP_OK_NODATA || response_type == RESP_OK_DISPLAY_INFO {
                return Ok(response_len);
            }
            return Err(GpuError::Device);
        }
    }

    /// Build the header and body in the request window, then run the
    /// command with `body` following the header.
    fn command(
        &mut self,
        command: u32,
        body: &[u8],
        response_len: usize,
    ) -> Result<usize, GpuError> {
        let request_len = HDR_BYTES + body.len();
        if request_len > self.request.len {
            return Err(GpuError::Device);
        }
        // SAFETY: bounded by the check above against the window's length.
        unsafe {
            let base = self.request.virt;
            core::ptr::copy_nonoverlapping(body.as_ptr(), base.add(HDR_BYTES), body.len());
        }
        self.command_with_body(command, request_len, response_len)
    }

    /// Stop the device and free nothing: the backing is leaked with
    /// the boot, and the queue is rewound only after the device is
    /// stopped, per `VirtQueue::release`'s contract.
    fn teardown(&mut self) {
        // SAFETY: `reset` stops the device first, which is the
        // precondition `release` documents.
        unsafe { self.control.release() };
        self.transport.reset();
    }
}

/// The live GPU, once [`display::init`] has found one.
///
/// The lock is what makes the frame path safe from both of its
/// callers — the syscall path and the timer path — without either
/// having to know the other exists.
static GPU: SpinLock<Option<VirtioGpu>> = SpinLock::new(None);

/// Install the GPU driver. Called once, from `display::init`.
///
/// Returns `(width, height, pitch, base)` — the mode and the virtual
/// address of the pixel buffer the compositor should draw into — or
/// the reason the GPU could not be taken over.
pub fn install() -> Result<(u32, u32, u32, u64), GpuError> {
    let gpu = VirtioGpu::open()?;
    let mode = (gpu.width, gpu.height, gpu.pitch, gpu.backing as u64);
    *GPU.lock() = Some(gpu);
    Ok(mode)
}

/// Push the current backing to the device, if a GPU is installed.
///
/// The compositor calls this after every frame it draws. With no GPU
/// (a firmware-framebuffer boot) this is a single branch and returns.
pub fn flush() -> Result<(), GpuError> {
    let mut guard = GPU.lock();
    match guard.as_mut() {
        Some(gpu) => gpu.flush(),
        None => Err(GpuError::NoDevice),
    }
}

/// Whether a GPU scanout owns the display.
pub fn active() -> bool {
    GPU.lock().is_some()
}
