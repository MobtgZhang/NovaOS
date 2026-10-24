//! Booting the image and reading the result.
//!
//! # How a kernel run is judged
//!
//! Three signals, in increasing order of trust:
//!
//! 1. **The serial log** contains the kernel's own `[NOVA-TEST]` lines. This is
//!    what actually says whether the tests passed — the exit status only says the
//!    machine stopped.
//! 2. **The exit status** says the kernel reached `isa-debug-exit` at all. A
//!    kernel that triple-faulted never gets there, and QEMU reports a non-zero
//!    code from `-no-reboot`.
//! 3. **The wall clock** says neither of the above happened. A timeout is
//!    reported *separately* from a failure, because "the kernel hung" and "the
//!    kernel's tests failed" call for completely different debugging.
//!
//! Separating those three is the whole reason this is not just
//! `Command::new("qemu").status()`. A harness that reports both as "test failed"
//! sends the reader to look at the wrong thing.

use std::path::PathBuf;

/// Build the image, then boot it and judge the result.
///
/// Building here rather than requiring `cargo xtask image` first is what makes
/// `run` a single-command workflow: the ISO is derived, and the stale-ISO
/// failure mode ("I edited the kernel and the old kernel booted") is removed.
pub fn run(args: &crate::Args) -> Result<(), String> {
    let iso = crate::image(args.cargo_profile())?;

    crate::limine::require()?;

    let code = crate::qemu::ovmf_code()?;
    let vars_template = crate::qemu::ovmf_vars()?;
    let vars = crate::qemu::fresh_vars(&vars_template)?;

    crate::log(&format!("firmware: {}", code.display()));

    let serial_log = serial_log_path();
    if let Some(parent) = serial_log.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }

    let command = qemu_command(&iso, &code, &vars, &serial_log);

    crate::log(&format!(
        "booting (timeout {}s); serial log -> {}",
        args.timeout(),
        serial_log.display()
    ));

    // Not `?`: the ISO cleanup below has to happen whether the boot passed or
    // failed, and `?` would return before reaching it.
    let outcome = invoke(&command, args.timeout(), &serial_log);

    if !args.keep_image() {
        // The ISO is reproducible from `cargo xtask image` and is often tens of
        // megabytes; keeping it by default would fill a developer's disk without
        // them asking.
        let _ = std::fs::remove_file(&iso);
    }

    outcome
}

/// The full QEMU argument vector.
fn qemu_command(
    iso: &std::path::Path,
    code: &std::path::Path,
    vars: &std::path::Path,
    serial_log: &std::path::Path,
) -> Vec<String> {
    let device = |flag: &str, value: String| -> Vec<String> { vec![flag.to_string(), value] };

    let mut command: Vec<String> = Vec::new();

    command.push("qemu-system-x86_64".to_string());
    command.extend(device(
        "-machine",
        // `q35` is the machine NovaOS targets and the one OVMF's 4 MiB split
        // build expects. TCG rather than KVM: the reference environment has no
        // `/dev/kvm`, and a full boot takes ~7 s, which is fine for a suite that
        // runs once per build.
        "q35,accel=tcg".to_string(),
    ));
    command.extend(device("-cpu", "qemu64".to_string()));
    command.extend(device("-smp", "1".to_string()));
    command.extend(device("-m", format!("{}M", crate::qemu::MEMORY_MIB)));

    // Two pflash drives: a read-only firmware and a writable variable store.
    // See `qemu`'s module docs for why `-bios` does not work here.
    command.extend(device(
        "-drive",
        format!(
            "if=pflash,format=raw,unit=0,readonly=on,file={}",
            code.display()
        ),
    ));
    command.extend(device(
        "-drive",
        format!("if=pflash,format=raw,unit=1,file={}", vars.display()),
    ));

    command.extend(device(
        "-drive",
        format!("file={},format=raw,media=cdrom", iso.display()),
    ));

    // The exit device. `iosize=0x04` matches the kernel's 32-bit write.
    command.extend(device(
        "-device",
        format!(
            "isa-debug-exit,iobase=0x{:x},iosize=0x04",
            crate::qemu::DEBUG_EXIT_IOBASE
        ),
    ));

    // The serial log goes to a file rather than to stdio so that the output is
    // preserved after QEMU exits and can be attached to a CI run.
    command.extend(device("-serial", format!("file:{}", serial_log.display())));

    command.extend(device("-display", "none".to_string()));
    // Without this, a triple fault reboots the machine and the harness waits out
    // the full timeout on what is already a dead kernel.
    command.push("-no-reboot".to_string());

    command
}

/// Run QEMU under a wall-clock timeout and classify the outcome.
fn invoke(
    command: &[String],
    timeout_seconds: u64,
    serial_log: &std::path::Path,
) -> Result<(), String> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        // QEMU's own output is captured rather than inherited so that a
        // firmware error goes into the same place a kernel panic does. The two
        // are the same class of problem here: "the machine produced no output".
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start QEMU: {error}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut qemu_output = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut qemu_output);
                }
                return classify(status, &qemu_output, serial_log);
            }
            // `kill()` clears the error, so the next `try_wait` returns `Err`
            // with `ErrorKind::InvalidInput` and the loop exits through the arm
            // below.
            Ok(None) => {}
            Err(_) => {
                return Err(format!(
                    "QEMU did not exit within {timeout_seconds}s and was killed.\n\
                     This is a *timeout*, not a test failure: the kernel most \
                     likely never reached isa-debug-exit.\n\
                     Last 40 lines of {}:\n{}",
                    serial_log.display(),
                    tail(serial_log, 40)
                ));
            }
        }

        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "QEMU did not exit within {timeout_seconds}s and was killed.\n\
                 This is a *timeout*, not a test failure: the kernel most likely \
                 never reached isa-debug-exit.\n\
                 Last 40 lines of {}:\n{}",
                serial_log.display(),
                tail(serial_log, 40)
            ));
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Decide pass or fail from the exit status and the serial log.
fn classify(
    status: std::process::ExitStatus,
    qemu_output: &str,
    serial_log: &std::path::Path,
) -> Result<(), String> {
    let log = strip_ansi(&std::fs::read_to_string(serial_log).unwrap_or_default());

    // The kernel's own verdict comes first: the exit status only proves the
    // machine stopped.
    if log.contains("[NOVA-TEST] SUMMARY failures detected") {
        return Err(format!(
            "kernel test suite reported failures\n\n{}",
            extract_failures(&log, 20)
        ));
    }

    let passed = log.contains("[NOVA-TEST] SUMMARY all suites passed");
    let reached_exit =
        status.success() || status.code() == Some(crate::qemu::OBSERVED_SUCCESS_STATUS as i32);

    match (passed, reached_exit) {
        (true, true) => {
            crate::log("kernel boot suite passed");
            Ok(())
        }
        (true, false) => Err(format!(
            "the kernel reported success but QEMU exited with {status}.\n\
             isa-debug-exit reported {:?}, expected {}.\n\
             The kernel and xtask disagree about the exit port; check \
             qemu::DEBUG_EXIT_IOBASE against kernel/src/arch/x86_64/exit.rs.",
            status.code(),
            crate::qemu::OBSERVED_SUCCESS_STATUS,
        )),
        (false, _) => Err(format!(
            "the kernel never printed a passing summary.\n\
             QEMU exited with {status}.\n\
             Serial log ({}) was {} bytes.\n\
             Firmware output:\n{}\n\
             Serial log:\n{}",
            serial_log.display(),
            log.len(),
            qemu_output.trim(),
            tail(serial_log, 40),
        )),
    }
}

/// Pull the failing test lines out of a serial log.
fn extract_failures(log: &str, limit: usize) -> String {
    let mut failures: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("... FAIL") || line.contains("[NOVA] ERR"))
        .collect();

    if failures.len() > limit {
        failures.truncate(limit);
    }

    if failures.is_empty() {
        return "no per-case failure line found; see the full log".to_string();
    }

    failures
        .iter()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last `count` lines of a file, for a failure message.
///
/// ANSI escape sequences are stripped first. Limine's menu and the framebuffer
/// console both emit cursor-positioning escapes, and a raw tail of those is
/// unreadable to a human — the failure message is read by exactly the person
/// least able to parse escape codes at a glance.
fn tail(path: &std::path::Path, count: usize) -> String {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) => return format!("  (cannot read {}: {error})", path.display()),
    };

    let stripped = strip_ansi(&contents);

    let lines: Vec<&str> = stripped.lines().collect();
    if lines.len() <= count {
        return lines
            .iter()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut rendered = format!("  ... {} earlier lines omitted ...", lines.len() - count);
    for line in &lines[lines.len() - count..] {
        rendered.push_str(&format!("\n  {line}"));
    }
    rendered
}

/// Remove ANSI CSI sequences from `text`.
///
/// Covers the two forms that appear in a QEMU serial log: CSI (`ESC [ … final`
/// byte, e.g. cursor moves and colour changes) and the keypad-mode `ESC =` /
/// `ESC >` sequences the UART driver emits at start-up.
fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != 0x1b {
            // Push the whole UTF-8 sequence, not one byte, so multi-byte
            // characters in kernel output survive intact.
            let width = utf8_width(bytes[index]);
            let end = (index + width).min(bytes.len());
            out.push_str(&text[index..end]);
            index = end;
            continue;
        }

        match bytes.get(index + 1) {
            Some(b'[') => {
                index += 2;
                // A CSI sequence ends at the first byte in 0x40..=0x7e.
                while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                    index += 1;
                }
                index += 1;
            }
            Some(b'=') | Some(b'>') | Some(b'(') | Some(b')') => index += 2,
            _ => index += 1,
        }
    }

    out
}

/// The length of the UTF-8 sequence starting with `first`.
///
/// Malformed input yields 1 rather than panicking: this runs over a serial log,
/// which is exactly the kind of data that can be truncated mid-character.
fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// Where the serial log is written for the most recent run.
///
/// Used by `scripts/qemu.sh`, which runs QEMU itself rather than going through
/// `xtask` so that it can impose its own timeout and archive the log itself.
pub fn serial_log_path() -> PathBuf {
    crate::boot::run_dir().join("serial.log")
}

#[cfg(test)]
mod tests {
    use super::{extract_failures, qemu_command, tail};
    use std::path::Path;

    fn temp_args() -> Vec<String> {
        qemu_command(
            Path::new("/tmp/nova.iso"),
            Path::new("/tmp/OVMF_CODE.fd"),
            Path::new("/tmp/OVMF_VARS.fd"),
            Path::new("/tmp/serial.log"),
        )
    }

    #[test]
    fn the_command_carries_every_load_bearing_flag() {
        let command = temp_args();
        let joined = command.join(" ");

        // Each of these was found by removing it and getting a failure that
        // looked like something else entirely.
        assert!(joined.contains("q35"), "machine type");
        assert!(
            joined.contains("accel=tcg"),
            "no KVM in the reference environment"
        );
        assert!(joined.contains("-smp 1"), "single CPU");
        assert!(joined.contains("-m 256M"), "memory size");
        assert!(joined.contains("if=pflash"), "OVMF needs pflash, not -bios");
        assert!(
            joined.contains("readonly=on"),
            "the code drive must be read-only"
        );
        assert!(
            joined.contains("unit=1"),
            "the variable store is a second drive"
        );
        assert!(joined.contains("media=cdrom"), "the ISO is a CD");
        assert!(joined.contains("isa-debug-exit"), "the exit device");
        assert!(joined.contains("iobase=0xf4"), "the port the kernel writes");
        assert!(
            joined.contains("-serial file:"),
            "the serial log goes to a file"
        );
        assert!(joined.contains("-display none"), "no window in CI");
        assert!(
            joined.contains("-no-reboot"),
            "a triple fault must not loop"
        );
    }

    #[test]
    fn the_exit_device_port_matches_the_kernel() {
        let joined = temp_args().join(" ");
        assert!(joined.contains(&format!("iobase=0x{:x}", crate::qemu::DEBUG_EXIT_IOBASE)));
    }

    #[test]
    fn the_iso_is_attached_exactly_once_as_a_cd() {
        // Two `-drive file=<iso>` entries would make QEMU pick one
        // nondeterministically, and an ISO attached as a *disk* rather than a CD
        // is not what the hybrid image's El Torito layout expects.
        let command = temp_args();
        let iso_drives = command
            .iter()
            .filter(|argument| argument.starts_with("file=/tmp/nova.iso"))
            .count();

        assert_eq!(iso_drives, 1, "the ISO must be attached exactly once");

        let cdrom = command
            .iter()
            .find(|argument| argument.contains("/tmp/nova.iso"))
            .expect("the ISO drive argument exists");
        assert!(
            cdrom.contains("media=cdrom"),
            "the ISO must be a cdrom, got {cdrom:?}",
        );
    }

    #[test]
    fn the_ovmf_drives_are_two_and_distinct() {
        // One pflash drive means no writable variable store; two pointing at the
        // same file means one run corrupts the next one's NVRAM.
        let command = temp_args();
        let pflash: Vec<&String> = command
            .iter()
            .filter(|argument| argument.contains("if=pflash"))
            .collect();

        assert_eq!(pflash.len(), 2, "OVMF needs a code drive and a vars drive");
        assert!(pflash[0].contains("readonly=on"), "code is read-only");
        assert!(!pflash[1].contains("readonly=on"), "vars must be writable");
        assert_ne!(pflash[0], pflash[1], "the two drives are distinct files");
    }

    #[test]
    fn failures_are_extracted_for_the_report() {
        let log = "\
[NOVA-TEST]   case a ... ok
[NOVA-TEST]   case b ... FAIL: something broke
[NOVA] ERR   + divide-by-zero: divide by zero (vector 0x0)
[NOVA-TEST]   case c ... ok
";
        let extracted = extract_failures(log, 20);
        assert!(extracted.contains("case b"));
        assert!(extracted.contains("divide-by-zero"));
        assert!(!extracted.contains("case a"));
    }

    #[test]
    fn extraction_is_capped() {
        let log: String = (0..100)
            .map(|index| format!("[NOVA-TEST]   case c{index} ... FAIL\n"))
            .collect();
        let extracted = extract_failures(&log, 5);
        assert_eq!(extracted.lines().count(), 5);
    }

    #[test]
    fn a_log_with_no_failures_says_so() {
        let extracted = extract_failures("[NOVA-TEST] SUMMARY all suites passed\n", 20);
        assert!(extracted.contains("no per-case failure"));
    }

    #[test]
    fn tail_reports_a_short_file_in_full() {
        let path = std::env::temp_dir().join("novaos-tail-test.log");
        std::fs::write(&path, "one\ntwo\n").expect("temp file is writable");

        let rendered = tail(&path, 40);
        assert!(rendered.contains("one"));
        assert!(rendered.contains("two"));
        assert!(!rendered.contains("omitted"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tail_says_how_much_it_dropped() {
        let path = std::env::temp_dir().join("novaos-tail-long-test.log");
        let body: String = (0..100).map(|index| format!("line{index}\n")).collect();
        std::fs::write(&path, body).expect("temp file is writable");

        let rendered = tail(&path, 5);
        assert!(rendered.contains("95 earlier lines omitted"));
        assert!(rendered.contains("line99"));
        assert!(!rendered.contains("line0\n"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_serial_log_is_reported_not_panicked() {
        let rendered = tail(Path::new("/nonexistent/serial.log"), 10);
        assert!(rendered.contains("cannot read"));
    }

    // --- Verdict classification ------------------------------------------------
    //
    // `cargo xtask test` is only useful if it tells a *failing* kernel apart from
    // one that never ran. The two look identical from the outside — QEMU stops
    // either way — so the distinction has to come from the serial log, which
    // means these cases cannot be covered by booting a working kernel.

    /// Write a serial log and return its path.
    ///
    /// The suffix disambiguates concurrent calls within one test binary. It is
    /// a plain counter rather than a formatted pointer: `{:p}` on a `&str`
    /// prints the pointer *and its length*, which produces names like
    /// `novaos-classify-193370-Pointer { addr: 0x.., metadata: 38 }.log`.
    /// That contains spaces and braces, so it is a legal filename only by
    /// accident, and the file it names is not the one the test then reads.
    fn log_with(contents: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);

        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("novaos-classify-{}-{seq}.log", std::process::id(),));
        std::fs::write(&path, contents).expect("temp file is writable");
        path
    }

    /// `classify` needs a real `ExitStatus`, which only a reaped child provides.
    fn status_for(code: i32) -> std::process::ExitStatus {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .status()
            .expect("sh is available")
    }

    #[test]
    fn a_passing_suite_with_the_expected_status_passes() {
        let path = log_with("[NOVA-TEST] SUMMARY all suites passed\n");
        let status = status_for(crate::qemu::OBSERVED_SUCCESS_STATUS as i32);

        let verdict = super::classify(status, "", &path);
        assert!(verdict.is_ok(), "expected Ok, got {verdict:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_reported_failure_is_a_failure_not_a_pass() {
        // The kernel's own verdict outranks the exit status: it reached
        // isa-debug-exit cleanly and still failed, so a passing status must not
        // be read as success.
        let path = log_with(
            "[NOVA-TEST]   case boot_smoke ... FAIL: no direct map\n\
             [NOVA-TEST] SUMMARY failures detected\n",
        );
        let status = status_for(crate::qemu::OBSERVED_SUCCESS_STATUS as i32);

        let verdict = super::classify(status, "", &path).expect_err("must fail");
        assert!(
            verdict.contains("boot_smoke"),
            "the failing case should be named in the report: {verdict}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_kernel_that_never_reported_is_a_pass_failure_not_a_timeout() {
        // No summary line at all: the kernel halted without reaching the
        // harness. Reported as a failure so it is distinguishable from the
        // timeout path, which `invoke` raises when QEMU has to be killed.
        let path = log_with("BdsDxe: starting Boot0001\n");
        let status = status_for(0);

        let verdict = super::classify(status, "", &path).expect_err("must fail");
        assert!(
            verdict.contains("never printed a passing summary"),
            "expected the no-summary verdict: {verdict}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_triple_fault_reset_is_not_reported_as_success() {
        // `-no-reboot` turns a triple fault's reset into a *successful* QEMU
        // exit, so status 0 with no summary is the signature of a kernel that
        // reset before running. It must not be mistaken for a pass — which is
        // exactly what happened before the TSS selector was fixed.
        let path = log_with("BdsDxe: starting Boot0001\n");
        let status = status_for(0);

        assert!(
            super::classify(status, "", &path).is_err(),
            "a reset kernel reported as passing would hide the bug entirely"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn disagreement_about_the_exit_port_is_called_out() {
        // A passing log with a status QEMU would not produce means the kernel
        // and xtask disagree about `iobase`, which would otherwise read as an
        // unexplained failure.
        let path = log_with("[NOVA-TEST] SUMMARY all suites passed\n");
        let status = status_for(7);

        let verdict = super::classify(status, "", &path).expect_err("must fail");
        assert!(
            verdict.contains("disagree about the exit port"),
            "expected the exit-port diagnosis: {verdict}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
