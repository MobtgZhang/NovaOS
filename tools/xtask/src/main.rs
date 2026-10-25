//! Build automation for NovaOS.
//!
//! # Why this exists
//!
//! `cargo build` produces an ELF and stops. It does not produce anything a machine
//! can boot: there is no bootloader, no filesystem, and no firmware. Every
//! NovaOS command that ends in "run this" therefore has to assemble three
//! artifacts — the kernel ELF, a Limine ISO, and an OVMF variable store — and
//! then invoke QEMU with a specific device configuration.
//!
//! # The three traps this tool exists to prevent
//!
//! Each of these was reproduced and fixed during the environment spike; all
//! three present as "the kernel produced no output at all", which is
//! indistinguishable from a hang:
//!
//! 1. **The UART is at an IO port, not a memory address.** Dereferencing `0x3f8`
//!    raises `#PF` before the IDT exists, which triple-faults and resets the
//!    machine. The kernel handles this in `arch/x86_64/console.rs`; nothing here
//!    needs to know, but it is why a "nothing printed" failure is never assumed
//!    to be a serial-configuration problem here.
//! 2. **Limine 12.x requires a resource prefix on every path.** A kernel at
//!    `/nova-kernel.elf` is not found; it must be `boot():/nova-kernel.elf`.
//!    This is baked into [`boot::LIMINE_CONFIG`].
//! 3. **`isa-debug-exit`'s port is whatever `iobase=` says.** The device does
//!    not listen on a fixed port. With `iobase=0xf4` the kernel must write
//!    `0xf4`; writing the more commonly-seen `0x501` makes QEMU run forever, so
//!    a passing test suite looks like a hang. [`qemu::DEBUG_EXIT_IOBASE`] and
//!    the kernel's `ExitImpl` are the same constant on purpose.
//!
//! See `docs/abi/boot-contract.md` for the full contract.

mod boot;
mod build;
mod iso;
mod limine;
mod qemu;
mod run;
mod test;

use std::process::ExitCode;

/// Everything `xtask` can do.
///
/// Kept as a flat list rather than nested subcommands-with-subcommands because
/// there are only six of them; a command that needs a subcommand is a sign the
/// command is doing two jobs.
const USAGE: &str = "\
NovaOS build automation

USAGE:
    cargo xtask <command> [options]

COMMANDS:
    build           Build the kernel and every host-side crate
    image           Package the kernel into a bootable Limine ISO
    run             Build, then boot the image in QEMU with a serial console
    test            Run xtask's tests, the host unit tests, then the QEMU boot suite
    fetch-limine    Download and verify the pinned Limine release
    clean           Remove build artifacts

OPTIONS:
    --release       Use the release profile
    --profile <p>   Use a named profile
    --timeout <s>   QEMU run timeout in seconds (default: 30)
    --headless      Do not open a graphical window (default)
    --keep-image    Do not delete the ISO after `run`
    -h, --help      Show this message
";

/// Parsed command line.
struct Args {
    /// The subcommand, if one was given.
    command: Option<String>,
    /// Whether `--release` was passed.
    release: bool,
    /// An explicit `--profile` name, which wins over `release`.
    profile: Option<String>,
    /// The QEMU timeout in seconds.
    timeout: u64,
    /// Whether to keep the ISO after a run.
    keep_image: bool,
    /// Whether help was requested.
    help: bool,
}

impl Args {
    /// Parse `argv[1..]`.
    ///
    /// Unknown *flags* are an error rather than ignored: a typo'd `--relase`
    /// that silently built a debug kernel would waste far more time than an
    /// error message costs.
    fn parse(raw: &[String]) -> Result<Args, String> {
        let mut args = Args {
            command: None,
            release: false,
            profile: None,
            timeout: 30,
            keep_image: false,
            help: false,
        };

        let mut iter = raw.iter().peekable();
        while let Some(argument) = iter.next() {
            match argument.as_str() {
                "-h" | "--help" => args.help = true,
                "--release" => args.release = true,
                "--keep-image" => args.keep_image = true,
                // Accepted and ignored: headless is the default, and having the
                // flag keeps scripts written against an earlier version working.
                "--headless" => {}
                "--timeout" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--timeout needs a value in seconds".to_string())?;
                    args.timeout = value
                        .parse()
                        .map_err(|_| format!("--timeout value {value:?} is not a number"))?;
                }
                "--profile" => {
                    args.profile = Some(
                        iter.next()
                            .ok_or_else(|| "--profile needs a name".to_string())?
                            .clone(),
                    );
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option {other:?}"));
                }
                other => {
                    if args.command.is_some() {
                        return Err(format!("unexpected argument {other:?}"));
                    }
                    args.command = Some(other.to_string());
                }
            }
        }

        Ok(args)
    }

    /// The Cargo profile to build with, if the caller asked for a specific one.
    fn cargo_profile(&self) -> Option<&str> {
        // An explicit `--profile` beats `--release`: naming a profile is a more
        // specific instruction than a shorthand for "the release profile".
        self.profile
            .as_deref()
            .or(if self.release { Some("release") } else { None })
    }

    /// The QEMU timeout in seconds.
    fn timeout(&self) -> u64 {
        self.timeout
    }

    /// Whether the ISO should survive a run.
    fn keep_image(&self) -> bool {
        self.keep_image
    }
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match Args::parse(&raw) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    if args.help || args.command.is_none() {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let result = match args.command.as_deref() {
        Some("build") => build::build_for_profile(args.cargo_profile()).map(|_| ()),
        Some("image") => image(args.cargo_profile()).map(|_| ()),
        Some("run") => run::run(&args),
        Some("test") => test_all(&args),
        Some("fetch-limine") => limine::fetch(),
        Some("clean") => clean(),
        // Unreachable in practice: `args.command.is_none()` returned above. Kept
        // as a catch-all rather than `unreachable!()` so that adding a command
        // without a match arm produces a usage error, not a panic.
        _ => Err(format!("unknown command {:?}\n\n{USAGE}", args.command)),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Remove build artifacts.
fn clean() -> Result<(), String> {
    run_command(&["cargo", "clean"], "clean")
}

/// Package the kernel into a bootable ISO, returning its path.
///
/// `run` calls this rather than requiring the caller to have run `image`
/// first: the ISO is derived from the kernel binary, and making the caller
/// sequence two commands is how a stale ISO gets booted.
///
/// Builds the kernel and the user-space runtime, not the host crates — see
/// [`build::build_kernel_only`].
pub fn image(profile: Option<&str>) -> Result<std::path::PathBuf, String> {
    let profile = build::build_kernel_only(profile)?;

    let kernel = boot::kernel_artifact(Some(&profile))?;
    log(&format!("kernel: {}", kernel.display()));

    let user = boot::user_artifact(Some(&profile))?;
    log(&format!("user image: {}", user.display()));

    limine::require()?;
    let apps = boot::application_directory(Some(&profile))?;
    let initramfs = boot::build_initramfs(&apps)?;
    let stage = boot::stage_image_with_initramfs(&kernel, Some(&user), Some(&initramfs))?;

    boot::build_iso(&stage)?;
    let iso = boot::iso_path();
    log(&format!("image: {}", iso.display()));
    Ok(iso)
}

/// Run the host tests, then the QEMU boot suite.
///
/// Three suites, in that order, stopping at the first failure. The cheap ones
/// come first because they fail in milliseconds with a precise message; a boot
/// suite that costs seconds of emulation should only run once the cheap checks
/// have passed.
///
/// 1. `xtask`'s own tests — the ISO layout, the QEMU arguments, the verdict
///    classification. These are the parts of the harness that decide whether
///    a boot counts as a pass.
/// 2. The host tests — pure logic compiled under `std`.
/// 3. The QEMU boot suite — the kernel, booted for real.
///
/// The order between the first two is not important; the order of both before
/// the third is.
fn test_all(args: &Args) -> Result<(), String> {
    test::driver()?;
    test::host()?;
    run::run(args)?;
    log("all tests passed");
    Ok(())
}

/// Print a status line to stderr.
///
/// stderr rather than stdout so a caller can pipe stdout and still see
/// progress; the test harness parses the serial log, not this stream.
pub fn log(message: &str) {
    eprintln!("[xtask] {message}");
}

/// Run a command, inheriting stdio so the user sees compiler output.
pub fn run_command(program: &[&str], what: &str) -> Result<(), String> {
    log(&format!("{}: {}", what, program.join(" ")));

    let status = std::process::Command::new(program[0])
        .args(&program[1..])
        .status()
        .map_err(|error| format!("failed to run `{}`: {error}", program[0]))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed with {status}"))
    }
}

/// [`run_command`] for an owned argument vector.
pub fn run_command_owned(command: &[String], what: &str) -> Result<(), String> {
    let borrowed: Vec<&str> = command.iter().map(String::as_str).collect();
    run_command(&borrowed, what)
}

#[cfg(test)]
mod tests {
    use super::{Args, USAGE};

    fn parse(raw: &[&str]) -> Result<Args, String> {
        let owned: Vec<String> = raw.iter().map(|s| (*s).to_string()).collect();
        Args::parse(&owned)
    }

    #[test]
    fn a_bare_invocation_has_no_command() {
        let args = parse(&[]).expect("empty argv is valid");
        assert!(args.command.is_none());
        assert!(!args.help);
    }

    #[test]
    fn each_subcommand_parses() {
        for command in ["build", "image", "run", "test", "fetch-limine", "clean"] {
            let args = parse(&[command]).expect("a known command parses");
            assert_eq!(args.command.as_deref(), Some(command));
        }
    }

    #[test]
    fn the_default_timeout_is_bounded() {
        // No timeout at all would make a hung kernel indistinguishable from a
        // slow one, which is exactly the failure mode the exit port exists to
        // avoid — but a timeout is still the backstop for when it does not fire.
        assert_eq!(parse(&["run"]).expect("parses").timeout(), 30);
        assert_eq!(
            parse(&["run", "--timeout", "5"]).expect("parses").timeout(),
            5
        );
    }

    #[test]
    fn release_and_profile_both_select_a_profile() {
        assert_eq!(parse(&["build"]).expect("parses").cargo_profile(), None);
        assert_eq!(
            parse(&["build", "--release"])
                .expect("parses")
                .cargo_profile(),
            Some("release")
        );
        assert_eq!(
            parse(&["build", "--profile", "dev"])
                .expect("parses")
                .cargo_profile(),
            Some("dev")
        );
    }

    #[test]
    fn an_explicit_profile_beats_the_release_shorthand() {
        // Naming a profile is a more specific instruction than `--release`.
        let args = parse(&["build", "--release", "--profile", "bench"]).expect("parses");
        assert_eq!(args.cargo_profile(), Some("bench"));
    }

    #[test]
    fn an_unknown_flag_is_an_error_rather_than_ignored() {
        // A typo'd `--relase` that silently built a debug kernel would waste far
        // more time than an error message costs.
        assert!(parse(&["build", "--relase"]).is_err());
        assert!(parse(&["build", "--nonsense"]).is_err());
    }

    #[test]
    fn a_flag_missing_its_value_is_an_error() {
        assert!(parse(&["run", "--timeout"]).is_err());
        assert!(parse(&["run", "--timeout", "soon"]).is_err());
        assert!(parse(&["build", "--profile"]).is_err());
    }

    #[test]
    fn two_commands_are_rejected() {
        assert!(parse(&["build", "image"]).is_err());
    }

    #[test]
    fn keep_image_is_opt_in() {
        assert!(!parse(&["run"]).expect("parses").keep_image());
        assert!(
            parse(&["run", "--keep-image"])
                .expect("parses")
                .keep_image()
        );
    }

    #[test]
    fn help_is_available_as_a_flag_or_by_absence() {
        assert!(parse(&["--help"]).expect("parses").help);
        assert!(parse(&["-h"]).expect("parses").help);
    }

    #[test]
    fn the_usage_text_documents_every_subcommand() {
        // A subcommand added without a usage line is a command nobody can
        // discover, so the two lists are cross-checked here.
        for command in ["build", "image", "run", "test", "fetch-limine", "clean"] {
            assert!(USAGE.contains(command), "{command} is missing from USAGE");
        }
    }
}
