//! NovaOS UEFI 启动菜单应用（NovaBoot 统一启动协议的菜单层）。
//!
//! 这是一个 UEFI 应用，用于在固件阶段展示操作系统选择菜单：
//! 从 ESP 读取菜单配置、在控制台渲染菜单、响应键盘导航、选中条目后
//! 加载内核 ELF 头部并打印 NovaBoot 协议快照的关键字段。
//!
//! # 设计边界（重要）
//!
//! 本应用只负责“菜单 + 协议快照生成”。它目前**不**尝试把控制权交给
//! 内核入口点：NovaOS 内核目前通过 Limine 引导（`make run` 路径保持不变），
//! 而通过 NovaBoot 统一协议把控制权交给内核，需要内核端读取并校验
//! `BootProtocol` 快照，属于下一阶段。此处的跳转被明确标记为 TODO，
//! 不会声称已实现内核通过 UEFI 菜单启动。
//!
//! 四个架构（x86_64、aarch64、riscv64、loongarch64）在菜单配置中都有
//! 代表，但只有 x86_64 内核后端是完整的；其它架构条目是协议层的占位。

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::string::{String, ToString};

use core::fmt::Write as _;

use nova_boot::menu_config::parse;
use nova_boot::{BootMenu, BootMenuEntry, MAX_MENU_ENTRIES, MAX_MENU_NAME, MAX_MENU_PATH};
use nova_elf::Elf;
use uefi::entry;
use uefi::prelude::*;
use uefi::proto::console::text::{Key, ScanCode};
use uefi::proto::media::file::{File, FileAttribute, FileMode};
use uefi::table::runtime::ResetType;
use uefi::{CString16, Status};

/// 菜单配置文件在 ESP 上的相对路径（相于本 UEFI 镜像启动所在的卷）。
const MENU_CONFIG_PATH: &str = "\\EFI\\NOVA\\menu.conf";

/// 默认菜单：当配置文件缺失时使用。四个架构各一条，只有 x86_64 有对应
/// 完整的内核后端，其它架构目前是协议层占位。
const DEFAULT_MENU: &str = "\
title NovaOS x86_64
path /boot/nova-kernel.elf
arch x86_64

title NovaOS AArch64
path /boot/nova-kernel-aarch64.elf
arch aarch64

title NovaOS RISC-V 64
path /boot/nova-kernel-riscv64.elf
arch riscv64

title NovaOS LoongArch 64
path /boot/nova-kernel-loongarch64.elf
arch loongarch64
";

#[entry]
/// UEFI 入口点。
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // 必须在任何分配或 `println!` 之前初始化 UEFI helper：`global_allocator`
    // 所需的系统表句柄在这里才会被 `allocator` 缓存。
    uefi::helpers::init(&mut system_table).expect("NovaOS UEFI helpers 初始化失败");

    uefi::println!("\r\nNovaOS Boot Menu");
    uefi::println!("================\r\n");

    // 先读取菜单配置（需要启动服务，此借用在读取完成后结束）。
    let config_text = {
        let bs = system_table.boot_services();
        match read_menu_config(bs) {
            Ok(text) => {
                uefi::println!("menu config loaded from {}", MENU_CONFIG_PATH);
                text
            }
            Err(error) => {
                uefi::println!(
                    "menu config unavailable ({:?}); using embedded defaults",
                    error
                );
                DEFAULT_MENU.to_string()
            }
        }
    };

    let configs = match parse(&config_text) {
        Ok(configs) if !configs.is_empty() => configs,
        _ => {
            uefi::println!("failed to parse menu config; using embedded defaults");
            parse(DEFAULT_MENU).expect("embedded default menu must parse")
        }
    };

    // 将解析出的条目装入栈上固定缓冲，再构造 BorrowMenuEntry 切片。
    let mut names = [[0u8; MAX_MENU_NAME]; MAX_MENU_ENTRIES];
    let mut paths = [[0u8; MAX_MENU_PATH]; MAX_MENU_ENTRIES];
    let mut count = 0usize;
    // 把名称与路径拷进栈上固定缓冲（`parse` 已保证各自不超过上限，所以索引连续）。
    for (index, cfg) in configs.iter().enumerate().take(MAX_MENU_ENTRIES) {
        let name_bytes = cfg.name.as_bytes();
        let path_bytes = cfg.path.as_bytes();
        names[index][..name_bytes.len()].copy_from_slice(name_bytes);
        paths[index][..path_bytes.len()].copy_from_slice(path_bytes);
        count += 1;
    }
    if count == 0 {
        uefi::println!("no valid menu entries; halting");
        return Status::ABORTED;
    }

    // 把引用存进 MaybeUninit 槽位（只做不可变借用）。
    let mut slots = [core::mem::MaybeUninit::<BootMenuEntry>::uninit(); MAX_MENU_ENTRIES];
    for (index, cfg) in configs.iter().enumerate().take(count) {
        slots[index] = core::mem::MaybeUninit::new(BootMenuEntry {
            id: cfg.id,
            name: &names[index][..cfg.name.as_bytes().len()],
            architecture: cfg.architecture,
            kernel_path: &paths[index][..cfg.path.as_bytes().len()],
            flags: cfg.flags,
        });
    }

    let entries_slice =
        unsafe { core::slice::from_raw_parts(slots.as_ptr() as *const BootMenuEntry, count) };
    let menu = BootMenu::new(entries_slice);

    // 交互式菜单循环。
    let mut selected: usize = 0;
    'menu: loop {
        draw_menu(&mut system_table, &configs, selected);

        let key = wait_for_key(&mut system_table);
        match key {
            Key::Special(ScanCode::UP) => {
                if selected > 0 {
                    selected -= 1;
                }
            }
            Key::Special(ScanCode::DOWN) => {
                if selected + 1 < count {
                    selected += 1;
                }
            }
            Key::Printable(ch) => {
                let c = char::from(ch);
                if c == '\r' || c == '\n' {
                    // 回车：确认选中。
                    break 'menu;
                } else if c >= '1' && c <= '9' {
                    let digit = (c as u16 - b'1' as u16) as usize;
                    if digit < count {
                        selected = digit;
                        break 'menu;
                    }
                }
            }
            Key::Special(ScanCode::ESCAPE) => {
                uefi::println!("\r\nboot aborted by user");
                return Status::ABORTED;
            }
            _ => {}
        }
    }

    let entry = menu
        .select(configs[selected].id)
        .expect("selected entry exists");
    uefi::println!();
    let name_str = core::str::from_utf8(entry.name).unwrap_or("<invalid>");
    uefi::println!("selected: {}", name_str);
    uefi::println!("architecture: {}", entry.architecture.name());
    let path_str = core::str::from_utf8(entry.kernel_path).unwrap_or("<invalid>");
    uefi::println!("kernel path: {}", path_str);

    // 尽力加载内核 ELF 头部并打印入口点（不加载全部段，只解析头部）。
    load_kernel_header(system_table.boot_services(), entry.kernel_path);

    // TODO: 此处应写入 NovaBoot BootProtocol 快照到已约定的物理地址、
    // 调用 ExitBootServices，并跳转到内核入口点。需要内核端支持读取
    // NovaBoot 协议（目前内核仍依赖 Limine 符号），故暂不执行跳转，
    // 避免声称未验证的功能。`make run`（Limine 路径）不受影响。
    uefi::println!();
    uefi::println!("NOTE: kernel hand-off via NovaBoot is not yet wired.");
    uefi::println!("press any key to reboot");
    let _ = wait_for_key(&mut system_table);
    // `reset` 不返回（固件重启机器），后面不需要返回值。
    system_table
        .runtime_services()
        .reset(ResetType::WARM, Status::SUCCESS, None)
}

/// 把菜单绘制到 UEFI 文本输出控制台。
fn draw_menu(
    system_table: &mut SystemTable<Boot>,
    configs: &[nova_boot::menu_config::MenuEntryConfig],
    selected: usize,
) {
    let stdout = system_table.stdout();
    let _ = writeln!(stdout, "\r\nNovaOS Boot Menu");
    let _ = writeln!(stdout, "=================");
    let _ = writeln!(
        stdout,
        "arrow keys: navigate   enter: boot   digits: direct select   esc: abort"
    );
    let _ = writeln!(stdout);
    for (index, cfg) in configs.iter().enumerate() {
        let prefix = if index == selected { "> " } else { "  " };
        let _ = writeln!(stdout, "{}{}", prefix, cfg.name);
    }
    let _ = writeln!(stdout);
}

/// 等待并读取下一次按键（阻塞，直到有按键）。
///
/// `stdin()` 与 `boot_services()` 分别对 `SystemTable` 取可变与共享借用，
/// 因此不能同时持有；这里把每次借用都限制在一个小作用域里。
fn wait_for_key(system_table: &mut SystemTable<Boot>) -> Key {
    loop {
        let event = {
            let stdin = system_table.stdin();
            stdin
                .wait_for_key_event()
                .expect("UEFI input device has no wait-for-key event")
        };
        {
            let bs = system_table.boot_services();
            let _ = bs.wait_for_event(&mut [event]);
        }
        let stdin = system_table.stdin();
        match stdin.read_key() {
            Ok(Some(key)) => return key,
            Ok(None) => {}
            Err(error) => uefi::println!("keyboard read error: {:?}", error.status()),
        }
    }
}

/// 从本 UEFI 镜像启动所在的卷读取菜单配置文件。
fn read_menu_config(bs: &BootServices) -> Result<String, Status> {
    let mut fs = bs
        .get_image_file_system(bs.image_handle())
        .map_err(|e| e.status())?;
    let mut root = fs.open_volume().map_err(|e| e.status())?;
    let path = CString16::try_from(MENU_CONFIG_PATH).map_err(|_| Status::INVALID_PARAMETER)?;
    let handle = root
        .open(&path, FileMode::Read, FileAttribute::empty())
        .map_err(|e| e.status())?;
    let mut file = handle
        .into_regular_file()
        .ok_or(Status::INVALID_PARAMETER)?;
    let mut buffer = [0u8; 4096];
    let mut offset = 0usize;
    while offset < buffer.len() {
        let n = file.read(&mut buffer[offset..]).map_err(|e| e.status())?;
        if n == 0 {
            break;
        }
        offset += n;
    }
    core::str::from_utf8(&buffer[..offset])
        .map(|s| s.to_string())
        .map_err(|_| Status::INVALID_PARAMETER)
}

/// 尽力打开内核 ELF 文件并打印其入口点与可加载段数。
fn load_kernel_header(bs: &BootServices, path: &[u8]) {
    let path16: String = path.iter().copied().map(|c| c as char).collect();
    let uefi_path = path16.replace('/', "\\");
    let cpath = match CString16::try_from(uefi_path.as_str()) {
        Ok(c) => c,
        Err(_) => {
            uefi::println!("kernel path invalid: {}", path16);
            return;
        }
    };

    let mut fs = match bs.get_image_file_system(bs.image_handle()) {
        Ok(fs) => fs,
        Err(error) => {
            uefi::println!("cannot open image file system: {:?}", error.status());
            return;
        }
    };
    let mut root = match fs.open_volume() {
        Ok(root) => root,
        Err(error) => {
            uefi::println!("cannot open volume: {:?}", error.status());
            return;
        }
    };
    let handle = match root.open(&cpath, FileMode::Read, FileAttribute::empty()) {
        Ok(handle) => handle,
        Err(error) => {
            uefi::println!("cannot open kernel {}: {:?}", path16, error.status());
            return;
        }
    };
    let mut file = match handle.into_regular_file() {
        Some(file) => file,
        None => {
            uefi::println!("kernel path is not a regular file: {}", path16);
            return;
        }
    };

    let mut buffer = [0u8; 4096];
    let n = match file.read(&mut buffer) {
        Ok(n) => n,
        Err(error) => {
            uefi::println!("cannot read kernel: {:?}", error.status());
            return;
        }
    };

    match Elf::parse(&buffer[..n]) {
        Ok(elf) => {
            uefi::println!("kernel ELF parsed OK");
            uefi::println!("entry point: 0x{:016X}", elf.entry_point());
            let loadable = elf.loadable_segments().count();
            uefi::println!("loadable segments: {}", loadable);
        }
        Err(error) => {
            uefi::println!("kernel ELF parse error: {:?}", error.tag());
        }
    }
}
