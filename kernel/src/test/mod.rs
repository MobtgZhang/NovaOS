//! In-kernel test harness for boot verification.
//!
//! Each suite prints a machine-greppable line so `scripts/qemu.sh` can assert on
//! the serial output without parsing free-form prose:
//!
//! ```text
//! [NOVA-TEST] SUITE boot_smoke
//! [NOVA-TEST]   case <name> ... ok|fail
//! [NOVA-TEST] RESULT suites=1 cases=12 passed=12 failed=0
//! ```
//!
//! A failing case is reported but does **not** halt the kernel: the remaining
//! suites still run so one CI log shows every regression at once. Only after
//! all suites finish does the harness exit with [`crate::EXIT_TEST_FAILURE`].
//!
//! # Suite names are the milestone contract
//!
//! Each suite is named after an acceptance criterion from the milestone plan
//! (`boot_smoke`, `memory_map_sanity`, `divide_by_zero_fault`,
//! `invalid_opcode_fault`, `serial_log_format`), and `scripts/qemu.sh` asserts on
//! exactly those strings. A renamed suite silently drops a gate, so the names
//! live here as a list the shell script and the kernel both check.
//!
//! # Milestones live in separate modules
//!
//! Each milestone's cases are in their own file -- [`m2`] for paging and user
//! mode, [`m34`] for capability, IPC, and services -- and this module only
//! lists them. The harness runs the groups in milestone order. The split
//! exists so milestones can be developed at the same time without two of them
//! editing the same lines of the same file, which is how a suite gets silently
//! dropped.

pub mod harness;
pub mod heap;
pub mod m2;
pub mod m34;
pub mod m5;
pub mod m6;
pub mod m7;
pub mod smp;
pub mod wallclock;

/// Every suite name the kernel runs, in order.
///
/// Duplicated deliberately rather than derived: this is the list the milestone's
/// exit gate is written against, so it has to be checkable as a literal. A host
/// test below asserts this matches the harness's registration list.
pub const SUITE_NAMES: &[&str] = &[
    "boot_smoke",
    "memory_map_sanity",
    "kernel_heap",
    "divide_by_zero_fault",
    "invalid_opcode_fault",
    "serial_log_format",
    "page_tables",
    "console",
    // M2: paging, user mode, and scheduling.
    "user_hello",
    "round_robin_threads",
    "preempt_busy_loop",
    "user_cannot_write_kernel",
    "task_a_cannot_read_task_b",
    "invalid_elf_rejected",
    "interrupt_vectors",
    // M3: capability and IPC.
    "cap_rights_enforced",
    "cap_send_only_denies_recv",
    "cap_transfer_via_message",
    "ipc_echo_64",
    "server_exit_wakes_client",
    "cap_revoke_takes_effect",
    "no_cap_no_priv",
    // M4: services, filesystem, and console.
    "file_read_without_cap_denied",
    "file_cap_scope_allows_descendant",
    "file_cap_scope_denies_sibling",
    "ramfs_write_read",
    // The file server's reclamation of dead clients' open files.
    "file_server_reclaims_dead_clients",
    "protocol_version_mismatch",
    "service_restart_after_crash",
    "initramfs_boot",
    // M5: PCI, VirtIO block, block cache, and VFS mounts.
    "pci_enumeration",
    "virtio_block_read",
    "virtio_block_write_and_verify",
    "block_timeout",
    "driver_crash_mid_io",
    "read_only_mount",
    // PCI configuration through the ECAM window, with the ports as fallback.
    "pci_ecam",
    // The disk format's delete and rename, and the generation that makes a
    // stale disk handle refuse. Runs after the M5 suites, which share the
    // disk image with it.
    "disk_delete_rename",
    // M6: display, compositor surfaces, focus, input, and GUI lifecycle.
    "display_two_windows",
    "move_window",
    "focus_follows_click",
    "key_only_focused_surface",
    "invalid_surface_buffer_denied",
    "gui_app_crash_isolated",
    "window_open_close_100",
    "idle_gui_cpu_no_busy_spin",
    // M7: VirtIO net, link state, DNS, malformed input, TCP, ICMP, DHCP,
    // and the socket API.
    "virtio_net_device",
    "net_link_query",
    "net_dns_resolves_example_com",
    "net_malformed_input_denied",
    "net_tcp_handshake",
    "net_icmp_echo",
    "net_dhcp_lease",
    "net_socket_api",
    // The TCP server side: listen marks a socket, accept completes one
    // inbound handshake, and ISNs are drawn fresh per connection.
    "net_listen_accept",
    // The wall clock: the CMOS RTC behind the filesystem's timestamps.
    "wall_clock",
    // M8: SMP bring-up.
    "smp_cpu_state",
];

/// Whether `address` falls inside the kernel image's virtual range.
///
/// Used by the fault tests to check that a reported instruction pointer is real
/// kernel text. A handler that read its frame from the wrong place produces an
/// address that is not in this range, which is precisely the bug this catches,
/// so the test does not need a symbol table to notice it.
///
/// The bounds come from the linker script's own symbols, so a change to the
/// link-time VMA needs no change here.
pub fn kernel_image_contains(address: u64) -> bool {
    unsafe extern "C" {
        static __kernel_vma_base: u8;
        static __image_end_vma: u8;
    }

    let start = core::ptr::addr_of!(__kernel_vma_base) as u64;
    let end = core::ptr::addr_of!(__image_end_vma) as u64;

    address >= start && address < end
}
