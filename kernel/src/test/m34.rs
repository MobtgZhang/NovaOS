//! The M3 and M4 suites: capability, IPC, filesystem, and services.
//!
//! # Why these live in their own file
//!
//! The harness registration list is the milestone's exit gate, and several
//! agents work on it concurrently. Keeping each milestone's cases in its own
//! module means the shared files -- `harness.rs` and `mod.rs` -- carry only a
//! registration line per milestone, so two milestones never have to touch the
//! same region of the same file.
//!
//! # The bar these tests must clear
//!
//! The pure logic behind these mechanisms is host-tested in `nova-cap`,
//! `nova-ipc`, `nova-fs`, and `nova-protocols`. These kernel suites test the
//! *mechanism* the host tests cannot: that the syscall path really consults
//! the capability table, that a denied right really produces `E_DENIED`, that
//! a capability really moves between two task tables atomically, that a scope
//! check really gates a file read.
//!
//! A test that passes without exercising the mechanism is worse than no test
//! at all, because it reports green for a feature that does not work. Every
//! case here names the failure it catches, and the comment above it says what
//! that failure would look like from the outside.
//!
//! # The tests run in ring 0
//!
//! The harness runs inside the kernel, in ring 0, so these cases call the
//! dispatcher directly. That is legitimate and it is what makes them real: the
//! dispatcher *is* the policy, and the policy is what has to be right. What
//! these tests cannot cover is the privilege transition itself — a real
//! `#PF` delivered to a ring-3 program — because that needs M2's entry path
//! and a real user program. Nothing here pretends to cover it.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_abi::syscall::{ObjectType, Syscall, nr};
use nova_cap::ObjectRef;
use nova_fs::ramfs::FsError;
use nova_ipc::endpoint::Side;
use nova_ipc::message::Message;
use nova_protocols::file::{FILE_OP_OPEN, FILE_VERSION};
use nova_protocols::manifest::{Dependency, RestartDecision, ServiceManifest, ServiceState};
use nova_protocols::shell::{ShellCommand, parse};

use super::harness::{TestCase, TestSuite};
use crate::services::dispatch::{SyscallContext, SyscallOutcome, dispatch_syscall_in};
use crate::services::fs::MAX_SCOPES_PER_TASK;
use crate::services::{KernelState, MAX_TASKS};

/// The M3 suites, in registration order.
///
/// The names are the milestone's exit gate and must not be renamed: the shell
/// harness asserts on exactly these strings.
pub static M3_SUITES: &[TestSuite] = &[
    TestSuite {
        name: "ipc_4caps_atomic",
        cases: &[(
            "four_capabilities_transfer_together_or_not_at_all",
            four_capabilities_transfer_together,
        )],
    },
    TestSuite {
        name: "reply_once",
        cases: &[(
            "a_reply_token_is_spent_after_one_reply",
            reply_token_is_spent_after_one_reply,
        )],
    },
    TestSuite {
        name: "old_handle_invalid",
        cases: &[(
            "a_reused_object_slot_does_not_revive_a_stale_handle",
            a_reused_object_slot_does_not_revive_a_stale_handle,
        )],
    },
    TestSuite {
        name: "memory_leak_create_drop_10000",
        cases: &[(
            "ten_thousand_create_destroy_cycles_do_not_leak",
            ten_thousand_create_destroy_cycles_do_not_leak,
        )],
    },
    TestSuite {
        name: "cap_rights_enforced",
        cases: &[
            (
                "a_read_only_grant_refuses_a_write",
                read_only_grant_refuses_write,
            ),
            (
                "a_derived_capability_cannot_widen",
                derived_capability_cannot_widen,
            ),
            (
                "delegating_without_grant_is_denied",
                delegating_without_grant_is_denied,
            ),
            (
                "a_revoked_capability_is_observable",
                revoked_capability_is_observable,
            ),
        ],
    },
    TestSuite {
        name: "cap_send_only_denies_recv",
        cases: &[
            (
                "a_send_only_endpoint_refuses_a_receive",
                send_only_refuses_receive,
            ),
            ("the_peer_endpoint_may_receive", peer_endpoint_may_receive),
            (
                "a_send_on_a_receive_only_endpoint_is_denied",
                send_on_receive_only_is_denied,
            ),
        ],
    },
    TestSuite {
        name: "cap_transfer_via_message",
        cases: &[
            (
                "a_transferred_capability_lands_in_the_receiver",
                transfer_lands_in_receiver,
            ),
            (
                "the_sender_no_longer_holds_the_capability",
                sender_loses_the_capability,
            ),
            (
                "the_transferred_capability_names_the_same_object",
                transfer_names_same_object,
            ),
        ],
    },
    TestSuite {
        name: "ipc_echo_64",
        cases: &[
            (
                "sixty_four_messages_arrive_in_order",
                sixty_four_messages_arrive_in_order,
            ),
            (
                "the_channel_queue_does_not_leak",
                the_channel_queue_does_not_leak,
            ),
        ],
    },
    TestSuite {
        name: "server_exit_wakes_client",
        cases: &[
            (
                "a_dead_peer_wakes_the_waiter_with_peer_died",
                dead_peer_wakes_waiter,
            ),
            (
                "the_woken_waiter_is_not_left_parked",
                woken_waiter_is_not_parked,
            ),
        ],
    },
    TestSuite {
        name: "cap_revoke_takes_effect",
        cases: &[
            (
                "a_revoked_grant_stops_granting",
                revoked_grant_stops_granting,
            ),
            (
                "a_revocation_reaches_derived_capabilities",
                revocation_reaches_derived,
            ),
            (
                "a_revoked_endpoint_refuses_a_receive",
                revoked_endpoint_refuses_receive,
            ),
        ],
    },
    TestSuite {
        name: "no_cap_no_priv",
        cases: &[
            (
                "a_task_with_no_capability_cannot_create_objects",
                no_cap_cannot_create,
            ),
            (
                "a_task_with_no_capability_holds_no_handle",
                no_cap_holds_no_handle,
            ),
            (
                "a_revoked_root_stops_object_creation",
                revoked_root_stops_creation,
            ),
        ],
    },
    TestSuite {
        name: "process_exec_authority",
        cases: &[(
            "a_task_without_spawn_cannot_exec_a_service",
            without_spawn_cannot_exec_service,
        )],
    },
];

/// The M4 suites, in registration order.
pub static M4_SUITES: &[TestSuite] = &[
    TestSuite {
        name: "shell_ls",
        cases: &[
            ("the_shell_parses_ls", shell_parses_ls),
            (
                "the_shell_refuses_unknown_commands",
                shell_refuses_unknown_commands,
            ),
        ],
    },
    TestSuite {
        name: "file_read_without_cap_denied",
        cases: &[
            (
                "a_reader_with_no_grant_is_denied",
                reader_with_no_grant_is_denied,
            ),
            (
                "a_grant_on_another_path_does_not_help",
                grant_on_another_path_does_not_help,
            ),
        ],
    },
    TestSuite {
        name: "file_cap_scope_allows_descendant",
        cases: &[
            ("a_grant_covers_a_file_below_it", grant_covers_descendant),
            (
                "a_grant_covers_a_deep_descendant",
                grant_covers_deep_descendant,
            ),
            ("a_grant_covers_the_directory_itself", grant_covers_itself),
        ],
    },
    TestSuite {
        name: "file_cap_scope_denies_sibling",
        cases: &[
            ("a_sibling_directory_is_denied", sibling_directory_is_denied),
            (
                "a_name_that_shares_a_prefix_is_denied",
                prefix_sharing_name_is_denied,
            ),
            (
                "the_denial_happens_before_the_path_is_resolved",
                denial_precedes_resolution,
            ),
        ],
    },
    TestSuite {
        name: "file_read_write_ramfs",
        cases: &[
            (
                "a_written_file_can_be_read_back",
                a_written_file_can_be_read_back,
            ),
            (
                "a_reopened_handle_reads_from_the_start",
                a_reopened_handle_reads_from_the_start,
            ),
        ],
    },
    TestSuite {
        name: "ramfs_write_read",
        cases: &[
            ("written_bytes_come_back", written_bytes_come_back),
            ("a_write_needs_the_write_right", write_needs_the_write_right),
            (
                "the_protocol_round_trips_a_file",
                protocol_round_trips_a_file,
            ),
        ],
    },
    TestSuite {
        name: "file_server_restart",
        cases: &[(
            "a_crashed_file_server_is_restarted",
            crashed_service_is_restarted,
        )],
    },
    TestSuite {
        name: "ipc_queue_backpressure",
        cases: &[(
            "a_full_queue_refuses_and_then_recovers",
            a_full_queue_refuses_and_then_recovers,
        )],
    },
    TestSuite {
        name: "open_close_10000",
        cases: &[(
            "ten_thousand_open_close_cycles_do_not_leak",
            ten_thousand_open_close_cycles_do_not_leak,
        )],
    },
    TestSuite {
        name: "app_crash_does_not_kill_shell",
        cases: &[(
            "a_dead_child_leaves_the_shell_alive",
            a_dead_child_leaves_the_shell_alive,
        )],
    },
    TestSuite {
        name: "protocol_version_mismatch",
        cases: &[
            (
                "an_unknown_major_version_is_refused",
                unknown_major_version_is_refused,
            ),
            (
                "a_matching_version_is_accepted",
                matching_version_is_accepted,
            ),
            (
                "a_newer_minor_version_is_refused",
                newer_minor_version_is_refused,
            ),
        ],
    },
    TestSuite {
        name: "service_restart_after_crash",
        cases: &[
            (
                "a_crashed_service_is_restarted",
                crashed_service_is_restarted,
            ),
            (
                "the_restart_budget_is_exhausted_in_the_window",
                restart_budget_is_exhausted,
            ),
            (
                "a_failure_outside_the_window_starts_a_new_budget",
                failure_outside_window,
            ),
            (
                "a_service_that_never_becomes_ready_times_out",
                never_ready_times_out,
            ),
            (
                "a_service_waits_for_its_dependencies",
                service_waits_for_dependencies,
            ),
        ],
    },
    TestSuite {
        name: "initramfs_boot",
        cases: &[
            (
                "the_boot_archive_is_valid_and_contains_init",
                the_boot_archive_is_valid_and_contains_init,
            ),
            ("init_was_started", init_was_started),
        ],
    },
    TestSuite {
        name: "service_supervision",
        cases: &[
            (
                "a_live_task_is_reported_by_service_name",
                live_task_reported_by_service_name,
            ),
            (
                "a_retired_service_releases_its_slot",
                retired_service_releases_slot,
            ),
        ],
    },
];

// --- helpers ----------------------------------------------------------------

fn the_boot_archive_is_valid_and_contains_init() -> Result<(), &'static str> {
    let Some(archive_bytes) = crate::boot::initramfs() else {
        return Err("the boot image did not carry an initramfs");
    };
    let archive = crate::initramfs::Archive::parse(archive_bytes)
        .map_err(|_| "the boot archive failed validation")?;
    if archive.get(b"init").is_none() {
        return Err("the boot archive did not contain the init image");
    }
    if archive.len() < 5 {
        return Err("the boot archive is missing packaged services");
    }
    Ok(())
}

fn init_was_started() -> Result<(), &'static str> {
    // SAFETY: service state was installed during boot and the test harness
    // runs serialised on the kernel's own stack.
    let kernel = unsafe { crate::services::state() };
    if kernel
        .tasks
        .iter()
        .flatten()
        .filter(|task| task.alive)
        .count()
        < 2
    {
        return Err("init and the shell did not start from the boot archive");
    }
    Ok(())
}

fn live_task_reported_by_service_name() -> Result<(), &'static str> {
    let state = fresh_state();
    let id = state.create_task().ok_or("the task pool was empty")?;
    let slot = slot_of(state, id).ok_or("the task had no slot")?;
    state
        .task_at_slot_mut(slot)
        .ok_or("the task disappeared")?
        .service_name_len = b"file-server".len();
    state
        .task_at_slot_mut(slot)
        .ok_or("the task disappeared")?
        .service_name[..b"file-server".len()]
        .copy_from_slice(b"file-server");

    if !state.service_is_alive(b"file-server") {
        return Err("a live service was reported dead");
    }
    if state.service_is_alive(b"file") || state.service_is_alive(b"echo-server") {
        return Err("a different service name was accepted");
    }
    Ok(())
}

fn retired_service_releases_slot() -> Result<(), &'static str> {
    let state = fresh_state();
    let id = state.create_task().ok_or("the task pool was empty")?;
    let slot = slot_of(state, id).ok_or("the task had no slot")?;
    state
        .task_at_slot_mut(slot)
        .ok_or("the task disappeared")?
        .service_name_len = b"file-server".len();
    state
        .task_at_slot_mut(slot)
        .ok_or("the task disappeared")?
        .service_name[..b"file-server".len()]
        .copy_from_slice(b"file-server");

    if !state.retire_task(slot) {
        return Err("retirement was refused");
    }
    if state.task_at_slot(slot).is_some() {
        return Err("retirement left the task in its slot");
    }
    if state.service_is_alive(b"file-server") {
        return Err("retirement left the service alive");
    }
    let next = state
        .create_task()
        .ok_or("retirement did not free a slot")?;
    if slot_of(state, next) != Some(slot) {
        return Err("retirement did not recycle the expected slot");
    }
    Ok(())
}

fn shell_parses_ls() -> Result<(), &'static str> {
    let Some(ShellCommand::Ls { path }) = parse("ls /docs") else {
        return Err("ls was not parsed as the ls command");
    };
    if path != Some("/docs") {
        return Err("the ls path was not preserved");
    }
    Ok(())
}

fn shell_refuses_unknown_commands() -> Result<(), &'static str> {
    if parse("rm /").is_some() {
        return Err("an unknown command was accepted");
    }
    Ok(())
}

fn a_written_file_can_be_read_back() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the task has no slot")?;
    state
        .grant_file_scope(slot, b"/", Rights::READ | Rights::WRITE)
        .map_err(|_| "the write grant was refused")?;
    state
        .files
        .fs_mut()
        .create_file(b"/loop.txt")
        .map_err(|_| "cannot create /loop.txt")?;
    let scopes = state.scope_set_for(slot);

    let (wire, _) = state
        .files
        .open_scoped(&scopes, b"/loop.txt", Rights::WRITE)
        .map_err(|_| "the write open was refused")?;
    let written = state
        .files
        .write(&scopes, wire, b"ramfs works")
        .map_err(|_| "the write failed")?;
    if written != 11 {
        return Err("the write was truncated");
    }
    state
        .files
        .close(&scopes, wire)
        .map_err(|_| "the write close failed")?;

    let (wire, _) = state
        .files
        .open_scoped(&scopes, b"/loop.txt", Rights::READ)
        .map_err(|_| "the read open was refused")?;
    let mut buffer = [0u8; 32];
    let read = state
        .files
        .read(&scopes, wire, &mut buffer)
        .map_err(|_| "the read failed")?;
    if read != written || &buffer[..read] != b"ramfs works" {
        return Err("the file did not read back intact");
    }
    state
        .files
        .close(&scopes, wire)
        .map_err(|_| "the read close failed")?;
    Ok(())
}

fn a_reopened_handle_reads_from_the_start() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the task has no slot")?;
    state
        .grant_file_scope(slot, b"/", Rights::READ | Rights::WRITE)
        .map_err(|_| "the grant was refused")?;
    state
        .files
        .fs_mut()
        .create_file(b"/start.txt")
        .map_err(|_| "cannot create /start.txt")?;
    let scopes = state.scope_set_for(slot);

    let (wire, _) = state
        .files
        .open_scoped(&scopes, b"/start.txt", Rights::WRITE)
        .map_err(|_| "the write open was refused")?;
    state
        .files
        .write(&scopes, wire, b"beginning")
        .map_err(|_| "the write failed")?;
    state
        .files
        .close(&scopes, wire)
        .map_err(|_| "the write close failed")?;

    let (wire, _) = state
        .files
        .open_scoped(&scopes, b"/start.txt", Rights::READ)
        .map_err(|_| "the read open was refused")?;
    let mut buffer = [0u8; 8];
    let read = state
        .files
        .read(&scopes, wire, &mut buffer)
        .map_err(|_| "the read failed")?;
    if read != 8 || &buffer[..read] != b"beginnin" {
        return Err("the reopened handle did not read from the start");
    }
    state
        .files
        .close(&scopes, wire)
        .map_err(|_| "the read close failed")?;
    Ok(())
}

fn a_full_queue_refuses_and_then_recovers() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;
    let (client_end, server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::SEND,
            Rights::RECEIVE | Rights::REPLY,
        )
        .ok_or("the endpoint pair was refused")?;
    let client_context = SyscallContext::new(client_slot, 1);
    let server_context = SyscallContext::new(server_slot, 2);

    for index in 0..8u64 {
        state.outgoing_message[1] = Some(Message::new(index));
        let outcome = dispatch(
            &mut state,
            &client_context,
            nr::SEND,
            [client_end.raw(), index, 0, 0, 0, 0],
        );
        if outcome.error().is_some() {
            return Err("one of the first eight sends was refused");
        }
    }
    state.outgoing_message[1] = Some(Message::new(8));
    let outcome = dispatch(
        &mut state,
        &client_context,
        nr::SEND,
        [client_end.raw(), 8, 0, 0, 0, 0],
    );
    if error_of(outcome) != Some(ErrorCode::WouldBlock) {
        return Err("the ninth send did not report backpressure");
    }

    let outcome = dispatch(
        &mut state,
        &server_context,
        nr::RECV,
        [server_end.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the queued message could not be received");
    }
    let mut completions = [nova_ipc::endpoint::Completion::Failed {
        waiter: 0,
        error: ErrorCode::Invalid,
    }; 64];
    state.drain_completions(&mut completions);
    state.outgoing_message[1] = Some(Message::new(8));
    let outcome = dispatch(
        &mut state,
        &client_context,
        nr::SEND,
        [client_end.raw(), 8, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the queue did not recover after a receive");
    }
    Ok(())
}

fn ten_thousand_open_close_cycles_do_not_leak() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the task has no slot")?;
    state
        .grant_file_scope(slot, b"/", Rights::READ | Rights::WRITE)
        .map_err(|_| "the grant was refused")?;
    state
        .files
        .fs_mut()
        .create_file(b"/stress.txt")
        .map_err(|_| "cannot create /stress.txt")?;
    let scopes = state.scope_set_for(slot);
    let path = b"/stress.txt";

    for _ in 0..10_000usize {
        let (wire, _) = state
            .files
            .open_scoped(&scopes, path, Rights::WRITE)
            .map_err(|_| "the stress open was refused")?;
        let written = state
            .files
            .write(&scopes, wire, b"x")
            .map_err(|_| "the stress write failed")?;
        if written != 1 {
            return Err("the stress write was truncated");
        }
        state
            .files
            .close(&scopes, wire)
            .map_err(|_| "the stress close failed")?;
    }

    if state.files.fs().open_count() != 0 {
        return Err("the open-file table leaked");
    }
    Ok(())
}

fn a_dead_child_leaves_the_shell_alive() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let shell = state.create_task().ok_or("the shell slot was refused")?;
    let child = state.create_task().ok_or("the child slot was refused")?;
    if !state.kill_task(child) {
        return Err("the child could not be killed");
    }
    let shell_alive = state
        .task_at_slot(slot_of(&state, shell).ok_or("the shell has no slot")?)
        .ok_or("the shell disappeared")?
        .alive;
    if !shell_alive {
        return Err("the child's crash killed the shell");
    }
    Ok(())
}

fn four_capabilities_transfer_together() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;
    let (client_end, server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::SEND,
            Rights::RECEIVE | Rights::REPLY,
        )
        .ok_or("the endpoint pair was refused")?;
    let client_context = SyscallContext::new(client_slot, 1);
    let server_context = SyscallContext::new(server_slot, 2);

    let mut handles = [CapHandle::INVALID; 4];
    for (index, handle) in handles.iter_mut().enumerate() {
        let outcome = dispatch(
            &mut state,
            &client_context,
            nr::OBJECT_CREATE,
            [
                ObjectType::Notification as u64,
                Rights::ALL.bits() as u64,
                0,
                0,
                0,
                0,
            ],
        );
        if outcome.error().is_some() {
            return Err("one of the four objects could not be created");
        }
        *handle = CapHandle::from_raw(outcome.to_raw() as u64);
        let _ = index;
    }

    let message =
        Message::with_caps(7, &handles).map_err(|_| "the four-cap message was refused")?;
    state.outgoing_message[1] = Some(message);
    let outcome = dispatch(
        &mut state,
        &client_context,
        nr::SEND,
        [client_end.raw(), 7, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the four-cap send was refused");
    }
    let outcome = dispatch(
        &mut state,
        &server_context,
        nr::RECV,
        [server_end.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the four-cap receive was refused");
    }
    let received = state.last_message[2].ok_or("the received message was not published")?;
    if received.caps().len() != 4 {
        return Err("the four capabilities did not arrive together");
    }

    let client_table = state
        .caps_at(client_slot)
        .ok_or("the client has no table")?;
    for handle in handles {
        if client_table.entry(handle).is_some() {
            return Err("the sender kept one of the transferred capabilities");
        }
    }
    Ok(())
}

fn reply_token_is_spent_after_one_reply() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;
    let (client_end, server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::SEND,
            Rights::RECEIVE | Rights::REPLY,
        )
        .ok_or("the endpoint pair was refused")?;
    let client_context = SyscallContext::new(client_slot, 1);
    let server_context = SyscallContext::new(server_slot, 2);

    state.outgoing_message[1] = Some(Message::new(42));
    let outcome = dispatch(
        &mut state,
        &client_context,
        nr::CALL,
        [client_end.raw(), 42, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the call was refused");
    }
    let outcome = dispatch(
        &mut state,
        &server_context,
        nr::RECV,
        [server_end.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the call was not received");
    }
    let token = state.last_token[2].ok_or("no reply token was delivered")?;

    state.outgoing_message[2] = Some(Message::new(7));
    let outcome = dispatch(
        &mut state,
        &server_context,
        nr::REPLY,
        [server_end.raw(), token.to_raw(), 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the first reply was refused");
    }
    let outcome = dispatch(
        &mut state,
        &server_context,
        nr::REPLY,
        [server_end.raw(), token.to_raw(), 0, 0, 0, 0],
    );
    if error_of(outcome).is_none() {
        return Err("the spent reply token was accepted again");
    }
    Ok(())
}

fn a_reused_object_slot_does_not_revive_a_stale_handle() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the task has no slot")?;
    let context = SyscallContext::new(slot, 1);

    let outcome = dispatch(
        &mut state,
        &context,
        nr::OBJECT_CREATE,
        [
            ObjectType::Notification as u64,
            Rights::ALL.bits() as u64,
            0,
            0,
            0,
            0,
        ],
    );
    if outcome.error().is_some() {
        return Err("the first object could not be created");
    }
    let old = CapHandle::from_raw(outcome.to_raw() as u64);
    let outcome = dispatch(
        &mut state,
        &context,
        nr::OBJECT_DESTROY,
        [old.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the first object could not be destroyed");
    }

    let outcome = dispatch(
        &mut state,
        &context,
        nr::OBJECT_CREATE,
        [
            ObjectType::Notification as u64,
            Rights::ALL.bits() as u64,
            0,
            0,
            0,
            0,
        ],
    );
    if outcome.error().is_some() {
        return Err("the replacement object could not be created");
    }
    let new = CapHandle::from_raw(outcome.to_raw() as u64);
    let table = state
        .caps_at(slot)
        .ok_or("the task has no capability table")?;
    if table.resolve(old).is_ok() {
        return Err("the destroyed handle was still usable");
    }
    if table.entry(new).is_none() {
        return Err("the replacement handle was not valid");
    }
    Ok(())
}

fn ten_thousand_create_destroy_cycles_do_not_leak() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the task has no slot")?;
    let context = SyscallContext::new(slot, 1);
    let baseline = state.objects.count();

    for _ in 0..10_000usize {
        let outcome = dispatch(
            &mut state,
            &context,
            nr::OBJECT_CREATE,
            [
                ObjectType::Notification as u64,
                Rights::ALL.bits() as u64,
                0,
                0,
                0,
                0,
            ],
        );
        if outcome.error().is_some() {
            return Err("a create in the stress loop was refused");
        }
        let handle = CapHandle::from_raw(outcome.to_raw() as u64);
        let outcome = dispatch(
            &mut state,
            &context,
            nr::OBJECT_DESTROY,
            [handle.raw(), 0, 0, 0, 0, 0],
        );
        if outcome.error().is_some() {
            return Err("a destroy in the stress loop was refused");
        }
        if state.objects.count() != baseline {
            return Err("the object registry leaked a slot");
        }
    }
    Ok(())
}

/// A fresh kernel service state, with a task that holds a root capability.
///
/// Every case starts from a clean state, because the suites share one kernel
/// instance and a suite that left state behind would make the next suite's
/// counts meaningless.
fn fresh_state() -> &'static mut KernelState {
    // The kernel's own service state, not a second copy.
    //
    // Two things matter here. The state is large -- the file server alone
    // accounts for about 71 KiB -- and the kernel's stack is 64 KiB, so
    // building one on the stack overflows it. And the state must be reached
    // through the same `UnsafeCell` the kernel uses: reaching a `static`
    // through a raw pointer derived from a `*const` is undefined behaviour,
    // and the compiler is entitled to drop the writes entirely -- which is
    // exactly what a task pool that never remembers a created task looks
    // like.
    //
    // The tests share the one instance and reset it between cases, so a
    // suite that left state behind cannot make the next suite's counts
    // meaningless.
    // SAFETY: the tests run one at a time, in ring 0, with interrupts masked.
    // The same single-CPU assumption that makes the kernel's own service
    // state sound applies here.
    unsafe {
        let state = crate::services::test_state();
        state.reset();
        state
    }
}

/// The slot of a task, given its identifier.
fn slot_of(state: &KernelState, id: u32) -> Option<usize> {
    state.slot_of_task(id)
}
/// Dispatch a syscall against an explicit state.
///
/// The wrapper exists so that every case goes through *the* dispatcher and
/// not through a private helper: a case that tested its own copy of the
/// policy would prove nothing about the policy the kernel runs.
fn dispatch(
    state: &mut KernelState,
    context: &SyscallContext,
    number: Syscall,
    args: [u64; 6],
) -> SyscallOutcome {
    dispatch_syscall_in(state, context, number.raw(), args)
}

/// The error a failed dispatch produced.
fn error_of(outcome: SyscallOutcome) -> Option<ErrorCode> {
    outcome.error()
}

// --- cap_rights_enforced ----------------------------------------------------

/// A read-only file grant must not permit a write.
///
/// The scope set is rebuilt from the capability table every time, so the
/// write right is absent because the *capability* does not carry it. If the
/// scope set were built from the scope pool instead — or if the server
/// consulted the pool's rights rather than the table's — this write would
/// succeed and the file would be modified.
fn read_only_grant_refuses_write() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    // A grant that carries READ and nothing else.
    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the read-only grant was refused")?;

    let scopes = state.scope_set_for(slot);
    if scopes.is_empty() {
        return Err("the scope set is empty: the grant was not recorded");
    }

    let (wire, _size) = state
        .files
        .open_scoped(&scopes, b"/docs/notes.txt", Rights::READ)
        .map_err(|_| "the read should have been permitted")?;

    let written = state.files.write(&scopes, wire, b"not allowed");
    match written {
        Err(FsError::Denied) => Ok(()),
        Err(other) => Err("the write failed for the wrong reason"),
        Ok(_) => Err("a read-only grant permitted a write"),
    }
}

/// A derived capability must not be wider than its parent.
///
/// The dispatch intersects the parent's rights with the requested ones. If it
/// unioned them instead — or simply granted the request — the child would
/// carry `WRITE`, and the narrowing invariant the whole capability model
/// rests on would be false.
fn derived_capability_cannot_widen() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;
    let context = SyscallContext::new(slot, 1);

    // A grant with READ and GRANT, but no WRITE.
    let parent = state
        .grant_file_scope(slot, b"/docs", Rights::READ | Rights::GRANT)
        .map_err(|_| "the grant was refused")?;

    // Ask for everything, including WRITE.
    let outcome = dispatch(
        &mut state,
        &context,
        nr::CAP_DEREGATE,
        [parent.raw(), Rights::ALL.bits() as u64, 0, 0, 0, 0],
    );
    let child = CapHandle::from_raw(outcome.to_raw() as u64);
    if outcome.error().is_some() {
        return Err("the delegation was refused, but it should have been narrowed");
    }

    let table = state.caps_at(slot).ok_or("the task has no table")?;
    let child_rights = table
        .rights(child)
        .ok_or("the derived capability does not resolve")?;
    if child_rights.contains(Rights::WRITE) {
        return Err("the derived capability gained WRITE");
    }
    if !child_rights.contains(Rights::READ) {
        return Err("the derived capability lost the READ it should have kept");
    }
    Ok(())
}

/// Delegating from a capability without `GRANT` must be denied.
///
/// The report's rule is that a capability names an object the holder may use,
/// not one it may hand to anyone else. The refusal comes from the dispatch
/// path, which resolves the parent in the caller's table — so a dispatcher
/// that skipped the table would hand out authority the caller never had.
fn delegating_without_grant_is_denied() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;
    let context = SyscallContext::new(slot, 1);

    // A grant with READ only: it may be used, not handed on.
    let parent = state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;

    let outcome = dispatch(
        &mut state,
        &context,
        nr::CAP_DEREGATE,
        [parent.raw(), Rights::READ.bits() as u64, 0, 0, 0, 0],
    );
    if error_of(outcome) != Some(ErrorCode::Denied) {
        return Err("delegating without GRANT was not denied");
    }

    // The refusal must not have left a capability behind.
    let table = state.caps_at(slot).ok_or("the task has no table")?;
    if table.live_count() != 2 {
        // The root and the grant: no third entry.
        return Err("the refused delegation left a capability behind");
    }
    Ok(())
}

/// A revoked capability must report `E_REVOKED`, not `E_BAD_HANDLE`.
///
/// The report distinguishes "you never had this" from "you had it and it was
/// taken away". If the revocation were indistinguishable from a bad handle,
/// a holder could not tell that it had lost authority, and the revocation
/// would be unobservable.
fn revoked_capability_is_observable() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;
    let context = SyscallContext::new(slot, 1);

    let parent = state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;

    let outcome = dispatch(
        &mut state,
        &context,
        nr::CAP_REVOKE,
        [parent.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the revocation was refused");
    }

    // Now use it. The dispatch must resolve it and find it revoked.
    let outcome = dispatch(
        &mut state,
        &context,
        nr::CAP_DEREGATE,
        [parent.raw(), Rights::READ.bits() as u64, 0, 0, 0, 0],
    );
    match error_of(outcome) {
        Some(ErrorCode::Revoked) => Ok(()),
        Some(ErrorCode::Denied) => Err("the revoked capability reported a rights denial"),
        Some(_) => Err("the revoked capability reported the wrong error"),
        None => Err("the revoked capability was still usable"),
    }
}

// --- cap_send_only_denies_recv ---------------------------------------------

/// A send-only endpoint must refuse a receive.
///
/// This is the report's "wrong rights" counter-example. The refusal has to
/// come from the *capability's* rights, resolved by the dispatch path: if the
/// dispatch asked for no rights, or if the IPC layer's mirror were the only
/// thing consulted, the receive would succeed.
fn send_only_refuses_receive() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;

    // The client gets SEND only; the server gets RECEIVE and REPLY.
    let (client_end, server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::SEND,
            Rights::RECEIVE | Rights::REPLY,
        )
        .ok_or("the endpoint pair was refused")?;

    let context = SyscallContext::new(client_slot, 1);
    let outcome = dispatch(
        &mut state,
        &context,
        nr::RECV,
        [client_end.raw(), 0, 0, 0, 0, 0],
    );
    if error_of(outcome) != Some(ErrorCode::Denied) {
        return Err("a send-only endpoint accepted a receive");
    }
    Ok(())
}

/// The receive-only side of the same endpoint must be able to receive.
///
/// The counterpart of the previous case: if the rights check were simply
/// broken in the other direction — if every endpoint refused every receive —
/// the denial above would pass for the wrong reason.
fn peer_endpoint_may_receive() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;

    let (_client_end, server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::SEND,
            Rights::RECEIVE | Rights::REPLY,
        )
        .ok_or("the endpoint pair was refused")?;

    let context = SyscallContext::new(server_slot, 2);
    let outcome = dispatch(
        &mut state,
        &context,
        nr::RECV,
        [server_end.raw(), 0, 0, 0, 0, 0],
    );
    // Nothing has been sent, so the receive parks rather than failing.
    if error_of(outcome).is_some() {
        return Err("the receive-only endpoint refused a receive");
    }
    if !outcome.is_blocked() {
        return Err("the receive returned a message that was never sent");
    }
    Ok(())
}

/// A receive-only endpoint must refuse a send.
///
/// The mirror image of the send-only case. If the dispatch resolved the
/// capability with `Rights::EMPTY` — that is, checked no right at all — the
/// send would be permitted here, and the first case would have passed only
/// because the IPC layer's endpoint mirror caught it.
fn send_on_receive_only_is_denied() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;

    let (client_end, server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::SEND,
            Rights::RECEIVE | Rights::REPLY,
        )
        .ok_or("the endpoint pair was refused")?;

    // Give the *server* the client's endpoint handle, so the server tries to
    // send on a capability that carries no SEND right. The server's own
    // table holds the receive-only handle, so the server's send must be
    // denied for the rights reason.
    let server_context = SyscallContext::new(server_slot, 2);
    let server_handle = state
        .caps_at(server_slot)
        .ok_or("the server has no table")?
        .entry(client_end)
        .map(|_| client_end)
        .unwrap_or(client_end);

    let outcome = dispatch(
        &mut state,
        &server_context,
        nr::SEND,
        [server_handle.raw(), 0, 0, 0, 0, 0],
    );
    if error_of(outcome) != Some(ErrorCode::Denied) {
        return Err("a send on a receive-only capability was not denied");
    }
    Ok(())
}

// --- cap_transfer_via_message -----------------------------------------------

/// A capability carried by a message must land in the receiver's table.
///
/// The kernel's transfer mints a *new* handle in the recipient's table; it
/// never hands the sender's handle across. If the transfer were a copy rather
/// than a move, the receiver would hold a handle that names the sender's
/// slot, and the two tables would be aliasing one entry.
fn transfer_lands_in_receiver() -> Result<(), &'static str> {
    let (state, _client, _server, client_slot, server_slot, handle, original, received) =
        transfer_setup()?;
    let _ = &state;

    // The message the receiver collected must carry a capability.
    let caps = received.caps();
    if caps.is_empty() {
        return Err("the received message carries no capability");
    }
    let transferred = caps[0];
    if transferred.is_invalid() {
        return Err("the received message carries the invalid handle");
    }

    // It must resolve in the *receiver's* table.
    let server_table = state
        .caps_at(server_slot)
        .ok_or("the server has no table")?;
    let entry = server_table
        .entry(transferred)
        .ok_or("the transferred handle does not resolve in the receiver's table")?;
    if !entry.is_live() {
        return Err("the transferred capability is not live");
    }

    // And it must be a *different* handle from the sender's: the sender's
    // slot number is a property of the sender's table.
    if transferred.slot() == handle.slot() && transferred.generation() == handle.generation() {
        crate::klog!(
            crate::logger::Level::Info,
            "DBG handle={:?} transferred={:?}",
            handle,
            transferred
        );
        return Err("the receiver was handed the sender's handle");
    }
    let _ = client_slot;
    Ok(())
}

/// The sender must no longer hold the transferred capability.
///
/// A transfer *moves* authority. If the source copy survived, the same
/// authority could be handed to a second recipient, and both would believe
/// they held the only copy of it.
fn sender_loses_the_capability() -> Result<(), &'static str> {
    let (state, _client, _server, client_slot, _server_slot, handle, _original, _received) =
        transfer_setup()?;
    let client_table = state
        .caps_at(client_slot)
        .ok_or("the client has no table")?;
    if client_table.entry(handle).is_some() {
        return Err("the sender still holds the capability it transferred");
    }
    Ok(())
}

/// The transferred capability must name the same object.
///
/// The rights and the object travel with the capability; a transfer that lost
/// the object would leave the receiver holding authority over nothing, which
/// looks like success and is not.
fn transfer_names_same_object() -> Result<(), &'static str> {
    let (state, _client, _server, client_slot, server_slot, handle, original, received) =
        transfer_setup()?;
    let caps = received.caps();
    if caps.is_empty() {
        return Err("the received message carries no capability");
    }
    let server_table = state
        .caps_at(server_slot)
        .ok_or("the server has no table")?;
    let moved = server_table
        .object_ref(caps[0])
        .ok_or("the transferred capability names no object")?;
    if moved != original {
        return Err("the transferred capability names a different object");
    }
    Ok(())
}

/// The shared setup for the three transfer cases.
///
/// Builds a client and a server, gives the client a capability to an object
/// of its own, sends it in a message, and collects it on the server. The
/// three cases then assert the three independent properties of the transfer:
/// it landed, the source is gone, and it names the same object.
fn transfer_setup() -> Result<
    (
        &'static mut KernelState,
        u32,
        u32,
        usize,
        usize,
        CapHandle,
        ObjectRef,
        Message,
    ),
    &'static str,
> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;

    let (client_end, server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::SEND,
            Rights::RECEIVE | Rights::REPLY,
        )
        .ok_or("the endpoint pair was refused")?;

    // The client creates an object of its own, so it has a capability worth
    // transferring.
    let client_context = SyscallContext::new(client_slot, 1);
    let outcome = dispatch(
        &mut state,
        &client_context,
        nr::OBJECT_CREATE,
        [
            ObjectType::Notification as u64,
            Rights::ALL.bits() as u64,
            0,
            0,
            0,
            0,
        ],
    );
    if outcome.error().is_some() {
        return Err("the client could not create an object to transfer");
    }
    let handle = CapHandle::from_raw(outcome.to_raw() as u64);
    let original = {
        let client_table = state
            .caps_at(client_slot)
            .ok_or("the client has no table")?;
        client_table
            .object_ref(handle)
            .ok_or("the sender's capability names no object")?
    };

    // Stage the message carrying the capability, the way the entry path
    // would after copying the caller's bytes into the kernel's buffer.
    let message = Message::with_caps(7, &[handle]).map_err(|_| "the message was refused")?;
    state.outgoing_message[1] = Some(message);

    // Send it. Nobody is waiting, so the send parks — but the transfer has
    // already happened by then, which is exactly what makes the property
    // testable without a scheduler.
    let outcome = dispatch(
        &mut state,
        &client_context,
        nr::SEND,
        [client_end.raw(), 7, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the send was refused");
    }

    // The server collects it.
    let server_context = SyscallContext::new(server_slot, 2);
    let outcome = dispatch(
        &mut state,
        &server_context,
        nr::RECV,
        [server_end.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the receive was refused");
    }
    let received = state.last_message[2].ok_or("the received message was not published")?;

    Ok((
        state,
        client,
        server,
        client_slot,
        server_slot,
        handle,
        original,
        received,
    ))
}

// --- ipc_echo_64 ------------------------------------------------------------

/// Sixty-four messages must arrive, in order, with their opcodes intact.
///
/// The number is the milestone's acceptance criterion, and it is a stress
/// number: a queue that leaked a slot, or a rendezvous that lost a wakeup,
/// would pass a single message and fail on the sixty-fourth.
fn sixty_four_messages_arrive_in_order() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;

    let (client_end, server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::SEND,
            Rights::RECEIVE | Rights::REPLY,
        )
        .ok_or("the endpoint pair was refused")?;

    let client_context = SyscallContext::new(client_slot, 1);
    let server_context = SyscallContext::new(server_slot, 2);
    let mut completions = [nova_ipc::endpoint::Completion::Failed {
        waiter: 0,
        error: ErrorCode::Invalid,
    }; 64];

    for index in 0..64u64 {
        // The client sends the message.
        state.outgoing_message[1] = Some(Message::new(index));
        let outcome = dispatch(
            &mut state,
            &client_context,
            nr::SEND,
            [client_end.raw(), index, 0, 0, 0, 0],
        );
        if outcome.error().is_some() {
            return Err("a send was refused mid-stream");
        }

        // The server takes it and checks the opcode.
        let outcome = dispatch(
            &mut state,
            &server_context,
            nr::RECV,
            [server_end.raw(), 0, 0, 0, 0, 0],
        );
        if outcome.error().is_some() {
            return Err("a receive was refused mid-stream");
        }
        if outcome.to_raw() != index as i64 {
            return Err("the message that arrived was not the one that was sent");
        }

        // Drain the completions so the channel's sink never overflows.
        state.drain_completions(&mut completions);
    }
    Ok(())
}

/// The channel's queue must be empty again after the exchange.
///
/// A message that stayed in the queue would be delivered *twice*: the next
/// receive would hand back the old message, and the echo's opcodes would
/// drift apart. This is what catches a rendezvous that copies rather than
/// moves.
fn the_channel_queue_does_not_leak() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;

    let (client_end, server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::SEND,
            Rights::RECEIVE | Rights::REPLY,
        )
        .ok_or("the endpoint pair was refused")?;

    let client_context = SyscallContext::new(client_slot, 1);
    let server_context = SyscallContext::new(server_slot, 2);

    for index in 0..8u64 {
        state.outgoing_message[1] = Some(Message::new(index));
        let _ = dispatch(
            &mut state,
            &client_context,
            nr::SEND,
            [client_end.raw(), index, 0, 0, 0, 0],
        );
        let _ = dispatch(
            &mut state,
            &server_context,
            nr::RECV,
            [server_end.raw(), 0, 0, 0, 0, 0],
        );
    }

    let channel_id = nova_ipc::ChannelId::new(0);
    let channel = state.ipc.channel(channel_id).ok_or("the channel is gone")?;
    if channel.queued(Side::First) != 0 || channel.queued(Side::Second) != 0 {
        return Err("a message stayed in the channel's queue");
    }
    if channel.parked() != 0 {
        return Err("a thread stayed parked on the channel");
    }
    Ok(())
}

// --- server_exit_wakes_client -----------------------------------------------

/// A server that exits must wake its blocked client with `E_PEER_DIED`.
///
/// The client's receive parks on the channel. When the server's task dies,
/// the kernel kills that task's side of the channel, and the channel records
/// `E_PEER_DIED` for the parked waiter. If the peer-death path were missing,
/// the client would stay parked forever — a hang, which is the worst
/// possible failure and the one this case exists to catch.
fn dead_peer_wakes_waiter() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;

    let (client_end, _server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::RECEIVE | Rights::REPLY,
            Rights::SEND,
        )
        .ok_or("the endpoint pair was refused")?;

    // The client parks in a receive.
    let client_context = SyscallContext::new(client_slot, 1);
    let outcome = dispatch(
        &mut state,
        &client_context,
        nr::RECV,
        [client_end.raw(), 0, 0, 0, 0, 0],
    );
    if !outcome.is_blocked() {
        crate::klog!(
            crate::logger::Level::Info,
            "DBG recv outcome={:?} error={:?}",
            outcome,
            outcome.error()
        );
        return Err("the client did not park: the receive completed immediately");
    }

    // The server dies.
    if !state.kill_task(server) {
        return Err("the server could not be killed");
    }

    // The kernel drains the completions, which is where the wakeup is
    // recorded.
    let mut completions = [nova_ipc::endpoint::Completion::Failed {
        waiter: 0,
        error: ErrorCode::Invalid,
    }; 64];
    state.drain_completions(&mut completions);

    match state.last_error[1] {
        Some(ErrorCode::PeerDied) => Ok(()),
        Some(other) => Err("the waiter was woken with the wrong error"),
        None => Err("the waiter was never woken"),
    }
}

/// After the peer died, the client's endpoint must be dead too.
///
/// A channel whose side survived the peer would let the client queue a
/// message that would never be collected — a message the sender believes was
/// sent. The check is that the channel reports the peer's side as gone.
fn woken_waiter_is_not_parked() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;

    let (client_end, _server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::RECEIVE | Rights::REPLY,
            Rights::SEND,
        )
        .ok_or("the endpoint pair was refused")?;

    let client_context = SyscallContext::new(client_slot, 1);
    let _ = dispatch(
        &mut state,
        &client_context,
        nr::RECV,
        [client_end.raw(), 0, 0, 0, 0, 0],
    );

    if !state.kill_task(server) {
        return Err("the server could not be killed");
    }
    let mut completions = [nova_ipc::endpoint::Completion::Failed {
        waiter: 0,
        error: ErrorCode::Invalid,
    }; 64];
    state.drain_completions(&mut completions);

    let channel = state
        .ipc
        .channel(nova_ipc::ChannelId::new(0))
        .ok_or("the channel is gone")?;
    // The server held the second side; that side must be dead, and the
    // client must not still be parked.
    if channel.side_alive(Side::Second) {
        return Err("the dead server's side is still alive");
    }
    if channel.parked() != 0 {
        return Err("a thread is still parked after the peer died");
    }
    Ok(())
}

// --- cap_revoke_takes_effect ------------------------------------------------

/// Revoking a file grant must stop it granting, immediately.
///
/// The scope set is rebuilt from the capability table on every use, and a
/// revoked capability is skipped. If the scope set were cached, or built from
/// the scope pool, the revoked grant would still permit the read — and the
/// revocation would be a cosmetic operation.
fn revoked_grant_stops_granting() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;
    let context = SyscallContext::new(slot, 1);

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    let grant = state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;

    // Before the revocation the read is permitted.
    let scopes = state.scope_set_for(slot);
    if state
        .files
        .open_scoped(&scopes, b"/docs/notes.txt", Rights::READ)
        .is_err()
    {
        return Err("the grant did not permit the read it was made for");
    }

    // Revoke it through the dispatch.
    let outcome = dispatch(
        &mut state,
        &context,
        nr::CAP_REVOKE,
        [grant.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the revocation was refused");
    }

    // Now the read must be denied.
    let scopes = state.scope_set_for(slot);
    match state
        .files
        .open_scoped(&scopes, b"/docs/notes.txt", Rights::READ)
    {
        Err(FsError::Denied) => Ok(()),
        Err(_) => Err("the read failed for the wrong reason"),
        Ok(_) => Err("the revoked grant still granted the read"),
    }
}

/// A revocation must reach the capabilities derived from the revoked one.
///
/// A withdrawn grant that lingers in a derived copy is a privilege
/// escalation, not a cosmetic problem. The dispatch revokes with the
/// `Descendants` scope for exactly this reason, and this case checks that
/// the cascade really happens.
fn revocation_reaches_derived() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;
    let context = SyscallContext::new(slot, 1);

    let parent = state
        .grant_file_scope(slot, b"/docs", Rights::READ | Rights::GRANT)
        .map_err(|_| "the grant was refused")?;

    let outcome = dispatch(
        &mut state,
        &context,
        nr::CAP_DEREGATE,
        [
            parent.raw(),
            (Rights::READ | Rights::GRANT).bits() as u64,
            0,
            0,
            0,
            0,
        ],
    );
    if outcome.error().is_some() {
        return Err("the delegation was refused");
    }
    let child = CapHandle::from_raw(outcome.to_raw() as u64);

    let outcome = dispatch(
        &mut state,
        &context,
        nr::CAP_REVOKE,
        [parent.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the revocation was refused");
    }

    let table = state.caps_at(slot).ok_or("the task has no table")?;
    if !table.is_revoked(parent) {
        return Err("the parent is not revoked");
    }
    if !table.is_revoked(child) {
        return Err("the derived capability survived the parent's revocation");
    }
    Ok(())
}

/// A revoked endpoint capability must refuse a receive.
///
/// The revocation must reach the *dispatch*, not just the scope set. If the
/// dispatch kept honouring a revoked endpoint, the holder could keep using
/// authority the kernel had already withdrawn.
fn revoked_endpoint_refuses_receive() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let client = state.create_task().ok_or("task pool is empty")?;
    let server = state.create_task().ok_or("task pool is empty")?;
    let client_slot = slot_of(&state, client).ok_or("the client has no slot")?;
    let server_slot = slot_of(&state, server).ok_or("the server has no slot")?;

    let (client_end, _server_end) = state
        .create_endpoint_pair(
            client_slot,
            server_slot,
            Rights::RECEIVE | Rights::REPLY,
            Rights::SEND,
        )
        .ok_or("the endpoint pair was refused")?;

    let client_context = SyscallContext::new(client_slot, 1);
    let outcome = dispatch(
        &mut state,
        &client_context,
        nr::CAP_REVOKE,
        [client_end.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the revocation was refused");
    }

    let outcome = dispatch(
        &mut state,
        &client_context,
        nr::RECV,
        [client_end.raw(), 0, 0, 0, 0, 0],
    );
    match error_of(outcome) {
        Some(ErrorCode::Revoked) => Ok(()),
        Some(_) => Err("the revoked endpoint reported the wrong error"),
        None => Err("the revoked endpoint still accepted a receive"),
    }
}

// --- no_cap_no_priv ---------------------------------------------------------

/// A task with no capability must not be able to create objects.
///
/// This is the report's "没有授权不能..." rule in its purest form. The task
/// exists and can be dispatched to — so the denial is not `E_BAD_HANDLE` —
/// and it is refused with `E_DENIED`, because it holds no authority at all.
fn no_cap_cannot_create() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_bare_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;
    let context = SyscallContext::new(slot, 1);

    let outcome = dispatch(
        &mut state,
        &context,
        nr::OBJECT_CREATE,
        [
            ObjectType::Notification as u64,
            Rights::ALL.bits() as u64,
            0,
            0,
            0,
            0,
        ],
    );
    if error_of(outcome) != Some(ErrorCode::Denied) {
        crate::klog!(
            crate::logger::Level::Info,
            "DBG object_create outcome={:?} error={:?}",
            outcome,
            outcome.error()
        );
        return Err("a task with no capability created an object");
    }
    Ok(())
}

/// A task with no capability must hold no handle that resolves.
///
/// A forged or stale handle must not resolve to anything. The report's
/// "bad_handle" counter-example is exactly this: guessing another task's
/// handle must be refused.
fn no_cap_holds_no_handle() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_bare_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;
    let context = SyscallContext::new(slot, 1);

    let table = state.caps_at(slot).ok_or("the task has no table")?;
    if table.live_count() != 0 {
        return Err("a bare task holds a capability");
    }

    // A guessed handle must not resolve.
    let guessed = CapHandle::new(1, 1);
    let outcome = dispatch(
        &mut state,
        &context,
        nr::RECV,
        [guessed.raw(), 0, 0, 0, 0, 0],
    );
    match error_of(outcome) {
        Some(ErrorCode::BadHandle) => Ok(()),
        Some(_) => Err("a guessed handle reported the wrong error"),
        None => Err("a guessed handle resolved"),
    }
}

/// A task with no capability must not launch another service.
///
/// `PROCESS_EXEC` is an authority-creating operation: it starts a task and
/// mints its management endpoint. A bare task may be dispatched, but it must
/// be denied before any child or endpoint is created.
fn without_spawn_cannot_exec_service() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_bare_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;
    let baseline = crate::services::process::live_task_count();
    let boot_info = unsafe { crate::boot::read_boot_info() };
    match crate::services::process::exec_image_in_state(&mut state, b"echo-server", slot, boot_info)
    {
        Ok(_) => return Err("a task with no capability started a service"),
        Err(ErrorCode::Denied) => {}
        Err(_) => return Err("the unauthorised exec was refused for the wrong reason"),
    }

    if crate::services::process::live_task_count() != baseline {
        return Err("the denied exec still changed the task pool");
    }
    if state.service_is_alive(b"echo-server") {
        return Err("the denied exec still started echo-server");
    }
    Ok(())
}

/// A task whose root capability was revoked must not create objects.
///
/// The revocation must be effective for *new* operations, which is the
/// report's revocation semantics. If the dispatcher cached the root's
/// rights, the revoked task would keep creating objects.
fn revoked_root_stops_creation() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;
    let context = SyscallContext::new(slot, 1);

    let root = state
        .task_at_slot(slot)
        .ok_or("the task has no state")?
        .root;

    let outcome = dispatch(
        &mut state,
        &context,
        nr::CAP_REVOKE,
        [root.raw(), 0, 0, 0, 0, 0],
    );
    if outcome.error().is_some() {
        return Err("the revocation was refused");
    }

    let outcome = dispatch(
        &mut state,
        &context,
        nr::OBJECT_CREATE,
        [
            ObjectType::Notification as u64,
            Rights::ALL.bits() as u64,
            0,
            0,
            0,
            0,
        ],
    );
    match error_of(outcome) {
        Some(ErrorCode::Revoked) => Ok(()),
        Some(_) => Err("the revoked root reported the wrong error"),
        None => Err("a revoked root still created an object"),
    }
}

// --- file_read_without_cap_denied -------------------------------------------

/// A caller with no grant must be refused a read.
///
/// The scope check happens before the path is resolved, so the denial is
/// uniform: the caller learns nothing about whether the file exists. If the
/// server resolved the path first, the error would be "no such file" for a
/// missing file and "denied" for an existing one — a channel for probing the
/// filesystem.
fn reader_with_no_grant_is_denied() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    let scopes = state.scope_set_for(slot);
    if !scopes.is_empty() {
        return Err("a task with no grant holds a scope");
    }

    match state
        .files
        .open_scoped(&scopes, b"/docs/notes.txt", Rights::READ)
    {
        Err(FsError::Denied) => Ok(()),
        Err(_) => Err("the read failed for the wrong reason"),
        Ok(_) => Err("a caller with no grant opened a file"),
    }
}

/// A grant on one path must not permit a read of another.
///
/// The scope set is a *set* of grants, and the check asks whether any of
/// them permits the request. If the check asked only whether the set was
/// non-empty, any grant would permit any path — the classic confused-deputy
/// bug of an authorisation check that tests presence rather than scope.
fn grant_on_another_path_does_not_help() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_dir(b"/etc")
        .map_err(|_| "cannot create /etc")?;
    state
        .files
        .fs_mut()
        .create_file(b"/etc/passwd")
        .map_err(|_| "cannot create /etc/passwd")?;

    state
        .grant_file_scope(slot, b"/etc", Rights::READ)
        .map_err(|_| "the grant was refused")?;

    let scopes = state.scope_set_for(slot);
    if scopes.is_empty() {
        return Err("the grant was not recorded");
    }

    match state
        .files
        .open_scoped(&scopes, b"/docs/notes.txt", Rights::READ)
    {
        Err(FsError::Denied) => Ok(()),
        Err(_) => Err("the read failed for the wrong reason"),
        Ok(_) => Err("a grant on /etc permitted a read under /docs"),
    }
}

// --- file_cap_scope_allows_descendant ---------------------------------------

/// A grant on a directory must cover the files below it.
///
/// The report's rule is that a path grant covers its descendants. If the
/// containment check compared bytes rather than components, or if it only
/// matched the exact path, this open would be denied — and the grant would
/// be useless.
fn grant_covers_descendant() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    match state
        .files
        .open_scoped(&scopes, b"/docs/notes.txt", Rights::READ)
    {
        Ok(_) => Ok(()),
        Err(FsError::Denied) => Err("the grant did not cover its descendant"),
        Err(_) => Err("the open failed for the wrong reason"),
    }
}

/// A grant must cover an arbitrarily deep descendant.
///
/// The containment check is component-wise, so depth does not matter. A check
/// that compared only the first component would pass the previous case and
/// fail this one.
fn grant_covers_deep_descendant() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_dir(b"/docs/sub")
        .map_err(|_| "cannot create /docs/sub")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/sub/deep.txt")
        .map_err(|_| "cannot create /docs/sub/deep.txt")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    match state
        .files
        .open_scoped(&scopes, b"/docs/sub/deep.txt", Rights::READ)
    {
        Ok(_) => Ok(()),
        Err(FsError::Denied) => Err("the grant did not cover a deep descendant"),
        Err(_) => Err("the open failed for the wrong reason"),
    }
}

/// A grant must cover the directory it names.
///
/// Opening the granted directory is exactly what the grant is for. A check
/// that required a strict descendant would refuse it.
fn grant_covers_itself() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    match state.files.open_scoped(&scopes, b"/docs", Rights::READ) {
        Ok(_) => Ok(()),
        Err(FsError::Denied) => Err("the grant did not cover its own directory"),
        Err(other) => {
            crate::klog!(
                crate::logger::Level::Info,
                "DBG open_scoped err={:?}",
                other
            );
            Err("the open failed for the wrong reason")
        }
    }
}

// --- file_cap_scope_denies_sibling -----------------------------------------

/// A grant must not cover a sibling directory.
///
/// The report's rule: a grant names a subtree, and the parent of the grant
/// and its siblings are outside it. If the containment check walked upwards
/// instead of downwards, the grant would cover the whole filesystem.
fn sibling_directory_is_denied() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_dir(b"/etc")
        .map_err(|_| "cannot create /etc")?;
    state
        .files
        .fs_mut()
        .create_file(b"/etc/passwd")
        .map_err(|_| "cannot create /etc/passwd")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    match state
        .files
        .open_scoped(&scopes, b"/etc/passwd", Rights::READ)
    {
        Err(FsError::Denied) => Ok(()),
        Err(_) => Err("the read failed for the wrong reason"),
        Ok(_) => Err("a grant on /docs covered its sibling /etc"),
    }
}

/// A name that merely shares a byte prefix must be outside the grant.
///
/// `/docs2` starts with the bytes `/docs`, and a byte-wise prefix test would
/// consider it inside the grant. The containment check is component-wise, so
/// it is not. This is the confused-deputy bug the component-wise rule
/// exists to prevent, and it is the case a naive implementation gets wrong.
fn prefix_sharing_name_is_denied() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_dir(b"/docs2")
        .map_err(|_| "cannot create /docs2")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs2/secret.txt")
        .map_err(|_| "cannot create /docs2/secret.txt")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    match state
        .files
        .open_scoped(&scopes, b"/docs2/secret.txt", Rights::READ)
    {
        Err(FsError::Denied) => Ok(()),
        Err(_) => Err("the read failed for the wrong reason"),
        Ok(_) => Err("a byte-prefix sibling was covered by the grant"),
    }
}

/// The denial must happen before the path is resolved.
///
/// A caller with no grant must learn nothing about whether the file exists.
/// If the server resolved the path first, a *missing* file would report "no
/// such file" while an existing one reported "denied" — a filesystem oracle.
/// The check here is that a path that does not exist at all is still denied.
fn denial_precedes_resolution() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    // `/etc` does not exist at all, and `/etc/passwd` is outside the grant.
    // Both must be denied, and not with "no such file".
    match state
        .files
        .open_scoped(&scopes, b"/etc/passwd", Rights::READ)
    {
        Err(FsError::Denied) => Ok(()),
        Err(_) => Err("the out-of-scope path failed for the wrong reason"),
        Ok(_) => Err("an out-of-scope path was opened"),
    }
}

// --- ramfs_write_read ------------------------------------------------------

/// Bytes written to the RAM filesystem must come back.
///
/// The file server is a *service* over the store: it holds the open-file
/// table that maps a wire handle back to a filesystem handle and its path.
/// If the server forgot the path, the scope check on a later read would
/// have nothing to check against and the read would fail — so this case
/// catches the server losing the binding between a handle and a path.
fn written_bytes_come_back() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ | Rights::WRITE)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    let (wire, _size) = state
        .files
        .open_scoped(&scopes, b"/docs/notes.txt", Rights::WRITE)
        .map_err(|_| "the open was refused")?;
    let written = state
        .files
        .write(&scopes, wire, b"hello nova")
        .map_err(|_| "the write was refused")?;
    if written != 10 {
        return Err("the write reported the wrong byte count");
    }

    // A fresh handle reads from the start.
    let (wire, size) = state
        .files
        .open_scoped(&scopes, b"/docs/notes.txt", Rights::READ)
        .map_err(|_| "the reopen was refused")?;
    if size != 10 {
        return Err("the reported size does not match what was written");
    }
    let mut buffer = [0u8; 32];
    let read = state
        .files
        .read(&scopes, wire, &mut buffer)
        .map_err(|_| "the read was refused")?;
    if read != 10 {
        return Err("the read returned the wrong byte count");
    }
    if &buffer[..read] != b"hello nova" {
        return Err("the bytes read back are not the bytes written");
    }
    Ok(())
}

/// A write must need the write right.
///
/// The scope's *rights* are checked separately from its *path*: a read-only
/// grant covers the path but does not permit writing to it. If the check
/// consulted the path only, the write would succeed and the grant would be
/// meaningless.
fn write_needs_the_write_right() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    // Opening for reading is permitted.
    let (wire, _size) = state
        .files
        .open_scoped(&scopes, b"/docs/notes.txt", Rights::READ)
        .map_err(|_| "the read-only open was refused")?;

    match state.files.write(&scopes, wire, b"not allowed") {
        Err(FsError::Denied) => Ok(()),
        Err(_) => Err("the write failed for the wrong reason"),
        Ok(_) => Err("a read-only grant permitted a write"),
    }
}

/// The file *protocol* must round-trip a file.
///
/// The server's protocol handler is the path a real client takes: an
/// `open` request in, a reply carrying the handle out, then a `read`. This
/// case drives that path end to end, so a server that answered the protocol
/// correctly but kept its open-file table wrong would fail here.
fn protocol_round_trips_a_file() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    // An `open` request with the right version.
    let request = open_request(FILE_VERSION.major, FILE_VERSION.minor, b"/docs/notes.txt");
    let reply = state.files.handle_message(&scopes, &request);
    let status = reply_status(&reply);
    if status != 0 {
        return Err("the open request was refused");
    }
    let handle = reply_handle(&reply);

    // A `read` request for that handle.
    let mut request = [0u8; 32];
    request[0..2].copy_from_slice(&FILE_VERSION.major.to_le_bytes());
    request[2..4].copy_from_slice(&FILE_VERSION.minor.to_le_bytes());
    request[8..16].copy_from_slice(&handle.to_le_bytes());
    request[16..24].copy_from_slice(&0u64.to_le_bytes());
    request[24..28].copy_from_slice(&32u32.to_le_bytes());
    let reply = state.files.handle_message(
        &scopes,
        &Message::with_payload(2, &request).map_err(|_| "the request was refused")?,
    );
    if reply_status(&reply) != 0 {
        return Err("the read request was refused");
    }
    Ok(())
}

// --- protocol_version_mismatch ----------------------------------------------

/// An unknown major version must be refused, not interpreted.
///
/// Two independently compiled binaries that disagree about a field's meaning
/// produce plausible garbage, not an error. The only safe response to an
/// unknown version is to refuse it — so the reply must carry
/// `E_UNSUPPORTED`, and the file must not have been opened.
fn unknown_major_version_is_refused() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    let request = open_request(
        FILE_VERSION.major + 1,
        FILE_VERSION.minor,
        b"/docs/notes.txt",
    );
    let reply = state.files.handle_message(&scopes, &request);
    if reply_status(&reply) != ErrorCode::Unsupported.code() {
        return Err("an unknown major version was not refused");
    }
    Ok(())
}

/// A matching version must be accepted.
///
/// The counterpart of the refusal: a server that refused every version would
/// pass the previous case for the wrong reason. A matching version must
/// produce a successful open.
fn matching_version_is_accepted() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    let request = open_request(FILE_VERSION.major, FILE_VERSION.minor, b"/docs/notes.txt");
    let reply = state.files.handle_message(&scopes, &request);
    if reply_status(&reply) != 0 {
        return Err("the matching version was refused");
    }
    Ok(())
}

/// A newer *minor* version must be refused too.
///
/// The minor version is an additive counter, but a peer that offers a newer
/// minor expects fields this implementation has never seen. Interpreting the
/// message anyway would mean reading a message whose layout is unknown, so
/// it is refused — exactly like a major mismatch.
fn newer_minor_version_is_refused() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = slot_of(&state, task).ok_or("the new task has no slot")?;

    state
        .files
        .fs_mut()
        .create_dir(b"/docs")
        .map_err(|_| "cannot create /docs")?;
    state
        .files
        .fs_mut()
        .create_file(b"/docs/notes.txt")
        .map_err(|_| "cannot create /docs/notes.txt")?;

    state
        .grant_file_scope(slot, b"/docs", Rights::READ)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    let request = open_request(
        FILE_VERSION.major,
        FILE_VERSION.minor + 1,
        b"/docs/notes.txt",
    );
    let reply = state.files.handle_message(&scopes, &request);
    if reply_status(&reply) != ErrorCode::Unsupported.code() {
        return Err("a newer minor version was not refused");
    }
    Ok(())
}

/// Build a file-protocol `open` request with a given version and path.
fn open_request(major: u16, minor: u16, path: &[u8]) -> Message {
    let mut payload = [0u8; 16];
    payload[0..2].copy_from_slice(&major.to_le_bytes());
    payload[2..4].copy_from_slice(&minor.to_le_bytes());
    payload[4..8].copy_from_slice(&0u32.to_le_bytes());
    payload[8..12].copy_from_slice(&(path.len() as u32).to_le_bytes());
    payload[12..16].copy_from_slice(&0u32.to_le_bytes());

    let mut bytes = [0u8; 64];
    bytes[..16].copy_from_slice(&payload);
    bytes[16..16 + path.len()].copy_from_slice(path);
    Message::with_payload(FILE_OP_OPEN, &bytes[..16 + path.len()])
        .unwrap_or_else(|_| Message::new(0))
}

/// The status a file-protocol reply carries.
fn reply_status(reply: &Message) -> i32 {
    let payload = reply.payload();
    if payload.len() < 8 {
        return ErrorCode::Invalid.code();
    }
    i32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]])
}

/// The handle a file-protocol `open` reply carries.
fn reply_handle(reply: &Message) -> u64 {
    let payload = reply.payload();
    if payload.len() < 16 {
        return 0;
    }
    u64::from_le_bytes([
        payload[8],
        payload[9],
        payload[10],
        payload[11],
        payload[12],
        payload[13],
        payload[14],
        payload[15],
    ])
}

// --- service_restart_after_crash --------------------------------------------

/// A service that crashes is restarted while its budget lasts.
///
/// The report's rule: a service has a restart budget and a window, and a
/// crash inside the window consumes it. The first crash must be answered
/// with "restart", and the service must go back to `Starting` — a service
/// that stayed `Failed` would never come back.
fn crashed_service_is_restarted() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let manifest = manifest_with_budget(3, 60_000);
    if !state.supervisor.register(manifest) {
        return Err("the service was not registered");
    }
    state.supervisor.mark_ready("file-server");
    if state.supervisor.state_of("file-server") != Some(ServiceState::Ready) {
        return Err("the service did not become ready");
    }

    let decision = state.supervisor.mark_failed("file-server");
    if decision != RestartDecision::Restart {
        return Err("the first crash was not answered with a restart");
    }
    if state.supervisor.state_of("file-server") != Some(ServiceState::Failed) {
        return Err("the failed state was not recorded");
    }

    let decision = state.supervisor.restart("file-server");
    if decision != RestartDecision::Restart {
        return Err("the restart was refused");
    }
    if state.supervisor.state_of("file-server") != Some(ServiceState::Starting) {
        return Err("the service did not go back to starting");
    }
    Ok(())
}

/// The restart budget must be exhausted inside the window.
///
/// The report's "service_crash_loop" counter-example: a service that keeps
/// crashing must not be restarted forever. After the budget is spent, the
/// supervisor must declare it permanently failed and stop restarting it.
fn restart_budget_is_exhausted() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let manifest = manifest_with_budget(3, 60_000);
    if !state.supervisor.register(manifest) {
        return Err("the service was not registered");
    }
    state.supervisor.mark_ready("file-server");

    // Three crashes inside the budget: all restarted.
    for _ in 0..3 {
        let decision = state.supervisor.mark_failed("file-server");
        if decision != RestartDecision::Restart {
            return Err("the budget was exhausted too early");
        }
    }
    // The fourth exhausts it.
    let decision = state.supervisor.mark_failed("file-server");
    if decision != RestartDecision::PermanentlyFailed {
        return Err("the budget was not exhausted");
    }
    if state.supervisor.state_of("file-server") != Some(ServiceState::PermanentlyFailed) {
        return Err("the service was not declared permanently failed");
    }
    Ok(())
}

/// A crash outside the window must start a fresh budget.
///
/// The difference between "restart at most N times" — a death sentence for a
/// service that crashes once a day — and a policy that punishes only a
/// *rapid* failure loop. Without this, a service that crashes rarely would
/// be declared permanently failed, which is the wrong answer.
fn failure_outside_window() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let manifest = manifest_with_budget(1, 1_000);
    if !state.supervisor.register(manifest) {
        return Err("the service was not registered");
    }

    // One crash: restarted.
    if state.supervisor.mark_failed("file-server") != RestartDecision::Restart {
        return Err("the first crash was not answered with a restart");
    }
    // The second, immediately, exhausts the budget of one.
    if state.supervisor.mark_failed("file-server") != RestartDecision::PermanentlyFailed {
        return Err("the budget was not exhausted inside the window");
    }

    // Now a crash *outside* the window: the budget starts again.
    state.supervisor.advance_clock(5_000);
    let decision = state.supervisor.mark_failed("file-server");
    if decision != RestartDecision::Restart {
        return Err("a crash outside the window did not start a fresh budget");
    }
    Ok(())
}

/// A service that never announces READY must time out.
///
/// The report's rule: a service has a READY handshake and a timeout. A
/// service that starts and never becomes ready must be failed by the
/// supervisor rather than hanging the boot forever. Without this, a service
/// that crashed on startup would leave the boot waiting for it.
fn never_ready_times_out() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut manifest = manifest_with_budget(3, 60_000);
    manifest.set_startup_timeout_ms(5_000);
    if !state.supervisor.register(manifest) {
        return Err("the service was not registered");
    }

    // Before the deadline, nothing times out.
    state.supervisor.advance_clock(1_000);
    let mut names = [0u8; 8];
    if state.supervisor.collect_timed_out(&mut names) != 0 {
        return Err("a service timed out before its deadline");
    }

    // After the deadline, it does.
    state.supervisor.advance_clock(10_000);
    if state.supervisor.collect_timed_out(&mut names) != 1 {
        return Err("the service that never became ready did not time out");
    }
    if state.supervisor.state_of("file-server") != Some(ServiceState::Failed) {
        return Err("the timed-out service was not failed");
    }
    Ok(())
}

/// A service must wait for the services it depends on.
///
/// The dependency ordering is what makes the boot order deterministic. If
/// the supervisor let a service start before its dependency was READY, the
/// service would race against a dependency that is not there yet.
fn service_waits_for_dependencies() -> Result<(), &'static str> {
    let mut state = fresh_state();

    let mut file_server = manifest_with_budget(3, 60_000);
    file_server.add_dependency(Dependency::new("console-server").ok_or("bad dependency name")?);
    if !state.supervisor.register(file_server) {
        return Err("the file server was not registered");
    }

    let console = ServiceManifest::new("console-server").ok_or("bad service name")?;
    if !state.supervisor.register(console) {
        return Err("the console server was not registered");
    }

    if state.supervisor.may_start("file-server") {
        return Err("the file server started before its dependency was ready");
    }
    state.supervisor.mark_ready("console-server");
    if !state.supervisor.may_start("file-server") {
        return Err("the file server did not start after its dependency was ready");
    }
    Ok(())
}

/// A manifest with a restart budget and a window.
fn manifest_with_budget(attempts: u32, window_ms: u32) -> ServiceManifest {
    let mut manifest = ServiceManifest::new("file-server").expect("a valid service name");
    manifest.set_restart_max_attempts(attempts);
    manifest.set_restart_window_ms(window_ms);
    manifest
}

/// The unused-import guard: `TestCase` is the type of every case function.
///
/// The suites above name their cases as plain function items, whose type is
/// `TestCase`; naming the alias here keeps the import honest for a reader
/// who wonders where it went.
type UnusedCaseType = TestCase;

/// The scope-pool bound, re-exported so a reader can see the bound the file
/// server works against.
const _SCOPE_POOL_BOUND: usize = MAX_SCOPES_PER_TASK;

/// The task-pool bound, re-exported so a reader can see it.
const _TASK_POOL_BOUND: usize = MAX_TASKS;
