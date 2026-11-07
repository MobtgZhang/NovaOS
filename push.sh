#!/usr/bin/env bash
#
# push.sh -- split the combined M6 Increment C commit into per-feature commits
# and push them to origin/main.
#
# Usage:
#   bash push.sh            # split into 8 commits, then push origin main
#   bash push.sh --no-push  # split into 8 commits but do not push
#
# Safety rules honoured by this script:
#   - It stages EXPLICIT PATHS ONLY. This script itself, everything under
#     reports/, and any generated or unrelated files are never committed.
#   - It rewrites only the single combined commit on top of BASE. The three
#     commits below it keep their existing messages untouched.
#   - It refuses to run unless HEAD is exactly the combined commit and the
#     working tree is clean.

set -euo pipefail
cd "$(dirname "$0")"

BASE="589fb5d"
EXPECT_SUBJECT="feat(m6): nova-ui toolkit, built-in font, file client and working desktop apps"

PUSH=yes
for arg in "$@"; do
  case "$arg" in
    --no-push) PUSH=no ;;
    *) echo "push.sh: unknown argument: $arg" >&2; exit 1 ;;
  esac
done

# ---------- guards ------------------------------------------------------

SUBJECT="$(git log -1 --format=%s)"
if [ "$SUBJECT" != "$EXPECT_SUBJECT" ]; then
  echo "push.sh: refusing to run: HEAD is not the combined M6 commit." >&2
  echo "  HEAD subject: $SUBJECT" >&2
  exit 1
fi

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "push.sh: refusing to run: the working tree is not clean." >&2
  exit 1
fi

# ---------- split -------------------------------------------------------

git reset --soft HEAD~1
git reset   # unstage everything; only explicit paths are staged below

commit_msg() {
  git commit -F - >/dev/null
}

# ---- 1. nova-ui toolkit, font, keyboard decoder ------------------------

git add crates/nova-ui/Cargo.toml \
        crates/nova-ui/FONT-LICENSE.txt \
        crates/nova-ui/src/font.rs \
        crates/nova-ui/src/keyboard.rs \
        crates/nova-ui/src/lib.rs \
        tools/gen_font.py \
        Cargo.toml \
        Cargo.lock
commit_msg <<'MSG'
feat(ui): add the nova-ui widget toolkit with a built-in font and keyboard decoder

crates/nova-ui/src/lib.rs:
- New host-testable no_std crate with Rect, palette and Canvas drawing
  helpers, a Button state machine, and a newline-aware TextBox whose
  caret and glyph layout share one `position` function so they can
  never disagree; `set_len` lets the TextBox adopt bytes already in the
  caller's buffer, and `decimal` renders unsigned integers as ASCII.
  It is a crate of its own because nova-apps is excluded from every
  host sweep, so a widget state machine living there would never be
  tested.

crates/nova-ui/src/font.rs:
- 8x16 raster font covering ASCII 32..126 as a const glyph table.

crates/nova-ui/FONT-LICENSE.txt:
- The Bitstream Vera permission notice under which the embedded
  DejaVu Sans Mono outlines are rasterised.

crates/nova-ui/src/keyboard.rs:
- Keyboard Decoder for PS/2 scancode set 1, including the E0 extended
  prefix. `Decoder::on_input` feeds both presses and releases:
  filtering releases used to leave Left Shift latched and made the E0
  prefix unreachable, so no arrow key ever worked. Pointer events are
  filtered by kind, and unknown key states are refused rather than
  read as presses.

crates/nova-ui/Cargo.toml:
- Declares the crate as no_std with its own Cargo metadata.

tools/gen_font.py:
- The generator that rasterises the font glyphs into font.rs.

Cargo.toml / Cargo.lock:
- Add nova-ui to the workspace.
MSG

# ---- 2. wire contracts: paged listing, pull-based input, scoped exec ---

git add crates/nova-abi/src/syscall.rs \
        crates/nova-protocols/src/file.rs \
        crates/nova-protocols/src/input.rs \
        crates/nova-protocols/src/lib.rs
commit_msg <<'MSG'
feat(protocols): paged file listing and pull-based input wire contracts

crates/nova-abi/src/syscall.rs:
- PROCESS_EXEC grows optional scope arguments (scope_ptr, scope_len,
  scope_rights, reserved) so a parent can pass a path grant down to
  its child. A zero scope_len means "no grant", which is what every
  pre-existing caller passes, so the ABI grows by addition and never
  by reinterpretation.

crates/nova-protocols/src/file.rs:
- New FILE_OP_LIST_AT opcode with a 16-byte FileListAtRequest that
  carries a page offset, plus offset/size layout assertions and a test
  proving a paged listing resumes where the previous page stopped.

crates/nova-protocols/src/input.rs:
- New INPUT_OP_GET pull request (8 bytes) and a 32-byte InputGetReply
  carrying kind, state, scancode, x, y and buttons, with the
  INPUT_EVENT_NONE/KEY/POINTER kind constants. Clients now drain their
  own input instead of reading one global last-key.

crates/nova-protocols/src/lib.rs:
- Re-exports the new file and input wire types, opcodes and size
  constants.
MSG

# ---- 3. RamFs truncate + Unsupported ----------------------------------

git add crates/nova-fs/src/ramfs.rs
commit_msg <<'MSG'
feat(fs): RamFs truncate and a distinct Unsupported error

crates/nova-fs/src/ramfs.rs:
- New FsError::Unsupported, distinct from ReadOnly: "no filesystem
  here can do that, whoever you are" rather than "you are not
  allowed". It maps to ErrorCode::Unsupported and the "E_UNSUPPORTED"
  name.
- New RamFs::truncate drops the old tail of a file and frees its
  quota, so a CREATE|TRUNCATE save can never leave stale bytes behind.
  Covered by the truncating_drops_the_old_tail_and_frees_the_quota
  test.
MSG

# ---- 4. kernel file service paging + queued input service -------------

git add kernel/src/services/fs.rs \
        kernel/src/services/input.rs \
        kernel/src/services/dispatch.rs \
        kernel/src/main.rs
commit_msg <<'MSG'
feat(kernel): paged file service and a queued, pull-based input service

kernel/src/services/fs.rs:
- seed_default_layout assembles the runtime directory tree (/bin,
  /home, /media, ...) before anything can open a path: RamFs starts
  with only a root inode and a file cannot be created under a
  directory that does not exist. It creates directories, it grants
  nothing, every task still needs a path capability.
- handle_list_at, list_from and list_mounted serve paged directory
  listings within LISTING_BUDGET (64-byte inline payload minus the
  reply header), so an over-large record simply yields count == 0 and
  the walk stops instead of truncating a name.
- truncate_scoped applies FILE_FLAG_TRUNCATE under the caller's scope;
  READ_BUDGET bounds read replies the same way; the new release()
  helper closes handles when an origin task exits.

kernel/src/services/input.rs:
- MAX_QUEUED_EVENTS (32) ring buffer of queued key and pointer events
  with push/pop per surface. The new handle_client_message answers
  INPUT_OP_GET for ring 3 clients, while the trusted handle_message
  path stays driver-only: ring 3 may read the queue but never inject
  into it.

kernel/src/services/dispatch.rs:
- Routes SERVICE_FILE with the caller's own scope_set_for grants,
  because the file service executes on the caller's behalf and the
  client's grants, never the service's, decide what a request may
  reach. Routes SERVICE_INPUT to the new client path.

kernel/src/main.rs:
- Calls seed_default_layout before drivers::init, because that is
  where a disk is mounted at /media/... and a mount needs its parent
  directory to exist.
MSG

# ---- 5. scoped PROCESS_EXEC -------------------------------------------

git add kernel/src/services/process.rs \
        kernel/src/syscall/mod.rs \
        user/apps/src/process.rs \
        user/apps/src/bin/init.rs
commit_msg <<'MSG'
feat(process): parent-driven path grants on PROCESS_EXEC

kernel/src/services/process.rs:
- exec_image now takes an optional (path, Rights) grant. The new
  check_grant_is_delegable refuses the call unless the parent itself
  holds the path with those rights AND carries GRANT, so a program can
  only pass on what it was itself given. exec_image_in_state_with_grant
  seeds the child's scope table from the grant.

kernel/src/syscall/mod.rs:
- The PROCESS_EXEC handler copies an optional scope path of up to
  MAX_SCOPE (128) bytes from user memory and validates the rights
  bits. Zero length means no grant, so the original three-argument
  call keeps working unchanged.

user/apps/src/process.rs:
- New exec_with_scope wraps the six-argument syscall so init can hand
  a path slice to its children.

user/apps/src/bin/init.rs:
- Starts every boot service with exactly the authority it needs and no
  more (per section 16.1): the editor gets READ|WRITE on /home, the
  file manager gets READ on /, and every other service starts with no
  file authority at all. init holds / with GRANT from boot, so it is
  the only place in the system that can cut these slices.
MSG

# ---- 6. graphics input client + fsclient library ----------------------

git add user/apps/src/graphics.rs \
        user/apps/src/fsclient.rs \
        user/apps/src/lib.rs \
        user/apps/Cargo.toml
commit_msg <<'MSG'
feat(apps): graphics input client and the user-space file client library

user/apps/src/graphics.rs:
- New Window type (open/present) over double-buffered display
  surfaces, and input_get/input_drain which pull per-surface key and
  pointer events through the new INPUT_OP_GET contract instead of one
  global last-key.

user/apps/src/fsclient.rs:
- Complete user-space file-service client: open, read, write, close,
  stat, list_at, read_all and save, all over bounded inline payloads.
  Every reply lands in a 64-byte buffer (error replies are 24 bytes),
  reads and writes are chunked to the wire budget, list_at pages are
  walked by a paged Records iterator, and save uses CREATE|TRUNCATE so
  a shorter save cannot leave the old tail behind.

user/apps/src/lib.rs:
- Publishes the fsclient module and re-exports nova_ui as ui plus its
  keyboard module for the desktop apps.

user/apps/Cargo.toml:
- Adds the nova-ui dependency.
MSG

# ---- 7. desktop applications ------------------------------------------

git add user/apps/src/bin/terminal.rs \
        user/apps/src/bin/editor.rs \
        user/apps/src/bin/file-manager.rs
commit_msg <<'MSG'
feat(apps): desktop terminal, editor and file manager on the new toolkit

user/apps/src/bin/terminal.rs:
- Rebuilt as a 440x320 window on the nova-ui Canvas and Decoder: a
  2048-byte scrolling log that drops its first line when full, line
  editing and history, pointer click-through, and commands that run
  through the shell.

user/apps/src/bin/editor.rs:
- 440x340 editor over /home/nova/notes.txt: loads and saves through
  the new fsclient (so saving truncates correctly), caret editing via
  the shared TextBox layout, Save and Load buttons, and status
  feedback after every operation.

user/apps/src/bin/file-manager.rs:
- 440x380 file browser over paged list_at pages: at most 16 entries
  per page and 16 pages, stopping cleanly when a page comes back with
  count == 0. Directory and file rows with sizes, a path bar, Up and
  Refresh buttons, click-to-descend (paths above 48 bytes are refused
  rather than truncated), keyboard navigation with Up/Down/Enter/
  Backspace/Escape, and a status line showing the entry counts.
MSG

# ---- 8. initramfs capacity ---------------------------------------------

git add kernel/src/boot/limine.rs
commit_msg <<'MSG'
feat(boot): raise the initramfs capacity to 512 KiB

kernel/src/boot/limine.rs:
- INITRAMFS_CAPACITY grows from 256 KiB to 512 KiB. The archive now
  carries eight packaged programs, three of which link the UI toolkit
  and the built-in font, and the total ran to about 270 KiB, past the
  old bound and into a boot failure. The length is still checked
  before the copy, so an oversized archive is refused rather than
  truncated into a corrupt one, and the storage is .bss, so raising
  the bound costs RAM the machine already has and no image bytes.
MSG

# ---------- report and push ---------------------------------------------

echo "push.sh: split complete. Commits to be pushed:"
git log --oneline "${BASE}..HEAD"

if [ "$PUSH" = yes ]; then
  git push origin main
  echo "push.sh: pushed to origin/main."
else
  echo "push.sh: --no-push given, nothing was pushed."
fi
