use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{Args, boot, build, log};

const TEST_CPUS: usize = 4;

#[derive(Clone, Copy)]
pub enum Port {
    Aarch64,
    Riscv64,
    Loongarch64,
}

impl Port {
    const ALL: [Self; 3] = [Self::Aarch64, Self::Riscv64, Self::Loongarch64];

    pub fn parse(name: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|port| port.name() == name)
            .ok_or_else(|| format!("unknown early port {name:?}; use aarch64, riscv64 or loongarch64"))
    }

    fn name(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::Riscv64 => "riscv64",
            Self::Loongarch64 => "loongarch64",
        }
    }

    fn target(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64-unknown-none",
            Self::Riscv64 => "riscv64gc-unknown-none-elf",
            Self::Loongarch64 => "loongarch64-unknown-none",
        }
    }

    fn machine_args(self) -> &'static [&'static str] {
        match self {
            Self::Aarch64 => &["-machine", "virt,gic-version=2", "-m", "128M"],
            Self::Riscv64 => &["-machine", "virt", "-bios", "default", "-m", "128M"],
            Self::Loongarch64 => &["-machine", "virt", "-m", "1G"],
        }
    }

    fn variants(self) -> &'static [CpuVariant] {
        match self {
            Self::Aarch64 => &[CpuVariant { log_dir: "", cpu: "cortex-a57", vector_bits: None }],
            Self::Riscv64 => &[
                CpuVariant { log_dir: "", cpu: "rv64,v=true,vlen=256", vector_bits: Some(256) },
                CpuVariant { log_dir: "scalar", cpu: "rv64,v=false", vector_bits: Some(0) },
            ],
            Self::Loongarch64 => &[
                CpuVariant { log_dir: "", cpu: "la464,lsx=on,lasx=on", vector_bits: Some(256) },
                CpuVariant { log_dir: "lsx", cpu: "la464,lsx=on,lasx=off", vector_bits: Some(128) },
                CpuVariant { log_dir: "scalar", cpu: "la464,lsx=off,lasx=off", vector_bits: Some(0) },
            ],
        }
    }
}

struct CpuVariant {
    log_dir: &'static str,
    cpu: &'static str,
    vector_bits: Option<usize>,
}

struct PortImages {
    directory: PathBuf,
    kernel: PathBuf,
    probe_initramfs: PathBuf,
    service_initramfs: PathBuf,
}

#[derive(Clone, Copy)]
enum BootCheck {
    Traps(Option<usize>),
    Services { cpus: usize },
}

impl BootCheck {
    fn cpus(self) -> usize {
        match self {
            Self::Traps(_) => TEST_CPUS,
            Self::Services { cpus } => cpus,
        }
    }
}

pub fn test(args: &Args) -> Result<(), String> {
    let profile = args.cargo_profile().unwrap_or(build::DEFAULT_PROFILE);
    let timeout = Duration::from_secs(args.timeout());
    let mut failures = Vec::new();
    for port in Port::ALL {
        if args.arch.is_some_and(|selected| selected.name() != port.name()) {
            continue;
        }
        match prepare_port(port, profile) {
            Ok(images) => {
                for variant in port.variants() {
                    if let Err(error) = boot_port(port, variant, &images, BootCheck::Traps(variant.vector_bits), timeout) {
                        failures.push(error);
                    }
                }
                for cpus in [1, TEST_CPUS] {
                    if let Err(error) = boot_port(port, &port.variants()[0], &images, BootCheck::Services { cpus }, timeout) {
                        failures.push(error);
                    }
                }
            }
            Err(error) => failures.push(error),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

pub fn run(args: &Args) -> Result<(), String> {
    let port = args.arch.ok_or("run-ports requires --arch")?;
    let profile = args.cargo_profile().unwrap_or(build::DEFAULT_PROFILE);
    let images = prepare_port(port, profile)?;
    let mut qemu = qemu_command(port, &port.variants()[0], &images.kernel, &images.service_initramfs, TEST_CPUS);
    let status = qemu.status().map_err(|error| format!("cannot run {:?}: {error}", qemu.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("QEMU exited with {status}"))
    }
}

fn prepare_port(port: Port, profile: &str) -> Result<PortImages, String> {
    let target = port.target();
    crate::run_command(
        &["cargo", "build", "-p", "nova-kernel", "-p", "nova-port-init", "--target", target, "--profile", profile],
        &format!("{} kernel and init build", port.name()),
    )?;
    let artifact_profile = if profile == "dev" { "debug" } else { profile };
    let artifacts = boot::target_dir().join(target).join(artifact_profile);
    let directory = boot::target_dir().join("novaos/ports").join(port.name());
    fs::create_dir_all(&directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    let kernel = artifacts.join("nova-kernel");
    let kernel = if matches!(port, Port::Aarch64) {
        let image = directory.join("Image");
        write_aarch64_image(&kernel, &image)?;
        image
    } else {
        kernel
    };
    let binary = artifacts.join("nova-port-init");
    let probe_initramfs = directory.join("initramfs");
    let service_initramfs = directory.join("initramfs-services");
    let mut archive = Vec::new();
    boot::append_initramfs_file(&mut archive, "init-check", &binary)?;
    fs::write(&probe_initramfs, &archive).map_err(|error| format!("{}: {error}", probe_initramfs.display()))?;
    archive.clear();
    boot::append_initramfs_file(&mut archive, "init", &binary)?;
    boot::append_initramfs_file(&mut archive, "echo-server", &binary)?;
    boot::append_initramfs_file(&mut archive, "init-check", &binary)?;
    fs::write(&service_initramfs, &archive).map_err(|error| format!("{}: {error}", service_initramfs.display()))?;
    Ok(PortImages { directory, kernel, probe_initramfs, service_initramfs })
}

fn boot_port(
    port: Port, variant: &CpuVariant, images: &PortImages, check: BootCheck, timeout: Duration,
) -> Result<(), String> {
    let (subdir, initramfs, label) = match check {
        BootCheck::Traps(_) => (variant.log_dir, &images.probe_initramfs, "user-trap"),
        BootCheck::Services { cpus: 1 } => ("services-up", &images.service_initramfs, "single-CPU resident services"),
        BootCheck::Services { .. } => ("services", &images.service_initramfs, "SMP resident services"),
    };
    let directory = images.directory.join(subdir);
    fs::create_dir_all(&directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    let serial = directory.join("serial.log");
    let diagnostics = directory.join("qemu.log");
    let serial_file = File::create(&serial).map_err(|error| format!("{}: {error}", serial.display()))?;
    let diagnostics_file = File::create(&diagnostics).map_err(|error| format!("{}: {error}", diagnostics.display()))?;
    log(&format!("booting {} ({}) {label}; serial log -> {}", port.name(), variant.cpu, serial.display()));
    let mut command = qemu_command(port, variant, &images.kernel, initramfs, check.cpus());
    let mut qemu = command
        .stdin(Stdio::null())
        .stdout(serial_file)
        .stderr(diagnostics_file)
        .spawn()
        .map_err(|error| format!("cannot run {:?}: {error}", command.get_program()))?;

    let result = wait_for_result(&mut qemu, &serial, port, check, timeout);
    let _ = qemu.kill();
    let _ = qemu.wait();
    result.map_err(|error| format!("{} ({}): {error}; see {} and {}", port.name(), variant.cpu, serial.display(), diagnostics.display()))?;
    log(&format!("{} ({}) {label} check passed", port.name(), variant.cpu));
    Ok(())
}

fn qemu_command(port: Port, variant: &CpuVariant, kernel: &Path, initramfs: &Path, cpus: usize) -> Command {
    let mut command = Command::new(format!("qemu-system-{}", port.name()));
    command
        .arg("-smp")
        .arg(cpus.to_string())
        .args(["-display", "none", "-monitor", "none", "-serial", "stdio", "-no-reboot"])
        .args(port.machine_args())
        .args(["-cpu", variant.cpu])
        .arg("-kernel")
        .arg(kernel)
        .arg("-initrd")
        .arg(initramfs);
    command
}

fn write_aarch64_image(elf: &Path, image: &Path) -> Result<(), String> {
    let libdir = Command::new("rustc")
        .args(["--print", "target-libdir"])
        .output()
        .map_err(|error| format!("cannot locate Rust LLVM tools: {error}"))?;
    if !libdir.status.success() {
        return Err(format!("rustc --print target-libdir failed: {}", libdir.status));
    }
    let libdir = String::from_utf8(libdir.stdout).map_err(|error| error.to_string())?;
    let objcopy = Path::new(libdir.trim()).join("../bin/llvm-objcopy");
    let status = Command::new(&objcopy)
        .args(["-O", "binary"])
        .arg(elf)
        .arg(image)
        .status()
        .map_err(|error| format!("cannot run {}: {error}", objcopy.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("llvm-objcopy failed: {status}"))
    }
}

fn wait_for_result(qemu: &mut Child, serial: &Path, port: Port, check: BootCheck, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        let exited = qemu.try_wait().map_err(|error| format!("QEMU wait: {error}"))?;
        let serial = fs::read(serial).map_err(|error| format!("read serial log: {error}"))?;
        if let Some(result) = serial_result(&String::from_utf8_lossy(&serial), port, check) {
            result?;
            if let Some(status) = exited.filter(|status| matches!(check, BootCheck::Services { .. }) || !status.success()) {
                return Err(format!("QEMU exited with {status}"));
            }
            return Ok(());
        }
        if let Some(status) = exited {
            return Err(format!("QEMU exited with {status} before a complete result"));
        }
        if start.elapsed() >= timeout {
            return Err(format!("no result within {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn serial_result(serial: &str, port: Port, check: BootCheck) -> Option<Result<(), String>> {
    let pass = format!("[NOVA-ARCH] PASS {} user-trap", port.name());
    let cpus = check.cpus();
    let online = format!("[NOVA-ARCH] SMP {cpus} CPUs online, IPI and lock verified");
    let schedulers = format!("[NOVA-ARCH] SMP {} secondary user schedulers verified", cpus - 1);
    let mut passed = false;
    let mut observed = None;
    let mut ready = false;
    let mut smp_online = false;
    let mut smp_users = false;
    let mut smp_futex = false;
    let mut smp_rpc = false;
    for line in serial.split_inclusive('\n').filter(|line| line.ends_with('\n')) {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.starts_with("[NOVA-ARCH] FAIL ") || line.starts_with("[NOVA] ERR   | panic: ") {
            return Some(Err(line.to_owned()));
        }
        if let Some(bits) = line.strip_prefix("[NOVA-ARCH] vector-bits ") {
            observed = bits.parse::<usize>().ok();
        }
        smp_online |= line == online;
        smp_users |= smp_online && line == schedulers;
        smp_futex |= smp_users && line == "[NOVA-ARCH] SMP cross-CPU futex wake IPI verified";
        smp_rpc |= smp_futex && ready && line == "[NOVA-ARCH] SMP resident cross-CPU RPC verified";
        match check {
            BootCheck::Traps(_) => passed |= line == pass,
            BootCheck::Services { .. } => {
                ready |= line == "[NOVA-INIT] echo service ready";
                passed |= ready && line == "[NOVA-INIT] 128 RPCs complete";
            }
        }
    }
    passed.then(|| {
        if !smp_online || cpus > 1 && !smp_futex {
            return Err(format!("expected {cpus} CPUs online and SMP verification when applicable"));
        }
        if matches!(check, BootCheck::Services { .. }) && cpus > 1 && !smp_rpc {
            return Err("expected 128 replies through the resident cross-CPU IPC channel".into());
        }
        if let BootCheck::Traps(Some(expected)) = check
            && observed != Some(expected)
        {
            return Err(format!("expected vector width {expected}, observed {observed:?}"));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{BootCheck, Port, TEST_CPUS, serial_result};

    const SMP: &str = "[NOVA-ARCH] SMP 4 CPUs online, IPI and lock verified\r\n[NOVA-ARCH] SMP 3 secondary user schedulers verified\r\n[NOVA-ARCH] SMP cross-CPU futex wake IPI verified\r\n";

    #[test]
    fn pass_requires_the_selected_port_and_a_complete_line() {
        let marker = "[NOVA-ARCH] PASS aarch64 user-trap";
        assert_eq!(serial_result(marker, Port::Aarch64, BootCheck::Traps(None)), None);
        let log = format!("boot\r\n{SMP}{marker}\r\n");
        assert_eq!(serial_result(&log, Port::Riscv64, BootCheck::Traps(Some(256))), None);
        assert_eq!(serial_result(&log, Port::Aarch64, BootCheck::Traps(None)), Some(Ok(())));
    }

    #[test]
    fn a_failure_marker_overrides_a_pass() {
        let log = "[NOVA-ARCH] PASS aarch64 user-trap\r\n[NOVA-ARCH] FAIL aarch64 timer\r\n";
        assert!(matches!(serial_result(log, Port::Aarch64, BootCheck::Traps(None)), Some(Err(_))));
    }

    #[test]
    fn a_pass_requires_the_configured_vector_width() {
        let pass = format!("{SMP}[NOVA-ARCH] PASS riscv64 user-trap\r\n");
        let scalar = format!("[NOVA-ARCH] vector-bits 0\r\n{pass}");
        assert_eq!(serial_result(&scalar, Port::Riscv64, BootCheck::Traps(Some(0))), Some(Ok(())));
        assert!(matches!(serial_result(&scalar, Port::Riscv64, BootCheck::Traps(Some(256))), Some(Err(_))));
        assert!(matches!(serial_result(&pass, Port::Riscv64, BootCheck::Traps(Some(0))), Some(Err(_))));
    }

    #[test]
    fn service_check_requires_rpc_completion_after_readiness() {
        let ready = "[NOVA-INIT] echo service ready\r\n";
        let done = "[NOVA-INIT] 128 RPCs complete\r\n";
        let remote = "[NOVA-ARCH] SMP resident cross-CPU RPC verified\r\n";
        let check = |log: &str| serial_result(log, Port::Aarch64, BootCheck::Services { cpus: TEST_CPUS });
        assert_eq!(check(ready), None);
        assert_eq!(check(&format!("{done}{ready}")), None);
        assert!(matches!(check(&format!("{ready}{done}")), Some(Err(_))));
        assert!(matches!(check(&format!("{SMP}{ready}{done}")), Some(Err(_))));
        assert_eq!(check(&format!("{SMP}{ready}{remote}{done}")), Some(Ok(())));
        let single = format!("[NOVA-ARCH] SMP 1 CPUs online, IPI and lock verified\r\n{ready}{done}");
        assert_eq!(serial_result(&single, Port::Aarch64, BootCheck::Services { cpus: 1 }), Some(Ok(())));
        assert!(matches!(check(&single), Some(Err(_))));
    }
}
