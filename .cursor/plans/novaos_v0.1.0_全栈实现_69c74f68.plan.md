---
name: NovaOS v0.1.0 全栈实现
overview: 在 /mnt/mount/ssd1/NovaOS 从零实现 NovaOS v0.1.0：基于 Limine 12.9.4 + nightly Rust 的 x86-64 微内核，依次交付 M0 工程骨架、M1 启动与异常、M2 分页/用户态/调度、M3 Capability+IPC、M4 服务/RAMFS/Console/Shell，最终在 QEMU/OVMF 下可启动并通过全套自动化测试。
todos:
  - id: m0-skeleton
    content: M0：workspace + nightly rust-toolchain + xtask(build/image/run/test/fetch-limine) + scripts/qemu.sh + ADR + CI，达成 QEMU 串口冒烟闭环
    status: completed
  - id: m1-boot
    content: M1：端口 IO 串口 + limine BootInfo 校验 + GDT/IDT/TSS + 异常处理 + bitmap 物理页分配器（含 host 单测）
    status: in_progress
  - id: m2-paging-user
    content: M2：递归页表与 AddressSpace + 地址布局文档 + 线程上下文与 syscall 入口（iretq）+ nova-elf 加载器 + user/nova-rt
    status: pending
  - id: m2-sched
    content: M2：PIT 抢占 + Round-Robin 调度 + 线程退出回收，通过进程隔离与抢占验收测试
    status: pending
  - id: m3-cap
    content: M3：nova-abi 冻结 + CapabilityTable（权利收缩/handle generation 不变式 host 单测）+ 内核对象注册表
    status: pending
  - id: m3-ipc
    content: M3：Endpoint send/recv/call/reply + ReplyToken 一次性 + cap 原子转授 + 对端死亡唤醒与超时丢弃
    status: pending
  - id: m3-init
    content: M3：initramfs 打包 + 首个 init 用户进程 + initramfs 引导，启动 EchoServer 客户端/服务端 IPC 验收
    status: pending
  - id: m4-services
    content: M4：ServiceManager（依赖/READY/重启窗口）+ nova-protocols 版本化 + console/file 服务协议
    status: pending
  - id: m4-ramfs
    content: M4：RAMFS + FileServer + 文件能力作用域授权 + 路径规范化与 checked arithmetic
    status: pending
  - id: m4-shell
    content: M4：ConsoleServer + 用户态单线程 executor + Shell 内建 help/ls/cat/echo/ps，完成 M4 全部门槛测试
    status: pending
  - id: final-dod
    content: 收尾：第 19.3 节反例测试全通过 + fmt/clippy 零告警 + docs/abi 完善 + 干净环境可复现构建验证
    status: pending
isProject: false
---

# NovaOS v0.1.0 实施计划

## 已实测验证的环境基线（前置事实，非假设）

在 `/tmp/spike` 完成了全链路验证，本计划基于这些结论：

- 工具链 stable `1.99.1` + nightly 均可用；`x86_64-unknown-none` **仅 nightly 已安装**
- `limine` crate `0.6.5` 在 stable 下编译失败（`#![feature(ptr_metadata)]`），nightly 通过
- Limine boot protocol base revision 6；bootloader 用 **v12.9.4**（`limine-binary.tar.xz`）
- 系统无 `lld`，但 rustup 自带 `rust-lld`（`-C linker-flavor=ld.lld`）
- `/dev/kvm` 不存在 → QEMU 仅 TCG，全链路启动约 7 秒
- OVMF 必须用**双 pflash**，`-bios` + ISO 方式不工作
- `xorriso 1.5.6`、`mtools`、`grub-mkrescue` 均已就位

### 三个已复现的「内核无输出」陷阱

1. 串口必须用端口 IO（`in al,dx` / `out dx,al`）；直接 `write_volatile(0x3F8)` 触发 `#PF CR2=0x3f8` → 无 IDT → 三重故障
2. Limine 12.x 路径必须带资源前缀：`path: boot():/nova-kernel.elf`，否则 `PANIC: Failed to open executable`
3. `isa-debug-exit` 端口随 `iobase` 变化：`iobase=0xf4` 要写 **0xf4**（写 0x501 会导致 QEMU 永不退出）

### 追加：M0 收尾时新发现的五个「静默失败」陷阱

上面三个都属于「会报错」类。实际把内核跑通的过程中，另有一组失败**全部没有任何报错**，只能靠逐项二分 QEMU 行为定位，值得单列：

4. **Limine 12 配置语法整体换过**：条目以 `/Title` 开头（不是 11.x 的 `kernel_path:`），协议名是 `limine`（不是 `elf64`），`timeout: 0` 表示「不显示菜单直接启动」而 `no` 恰恰相反。任何一处写错，Limine 只会输出 `[config file contains no valid entries]` 然后停在条目菜单——**没有构建错误**，harness 只会报 timeout。另：`default_entry` 必须写裸名 `NovaOS`，`/NovaOS` 会被解析成空前缀而静默关闭 autoboot。
5. **`module_path` 指向 ISO 上不存在的文件会让整条目作废**，同样只报 `no valid entries`，不点名哪个文件。initramfs 未构建时必须整行省略。
6. **控制台必须在任何可能 panic 的代码之前初始化**：panic handler 经控制台输出，而未初始化的控制台会**丢弃**写入（`send_byte` 直接 return）。顺序错了的表象是「以 panic 状态退出 + 串口日志为空」。
7. **`ltr` 会 fault，且此时若 IDT 未装载就是三重故障**。而 `-no-reboot` 把 reset 变成 **QEMU 成功退出（状态 0）**，harness 因此对「内核根本没跑」报 green。本次即是 `KERNEL_TSS_SELECTOR = 0x18` 指向了留空的 slot 3，而 TSS 写在 slot 4。现在 selector 由 `TSS_SLOT` 推导，且 IDT 先于 GDT 装载。
8. **Limine 12 通过导出的 `LIMINE_*` 符号名发现请求，不看 section**。沿用 11.x 的 `link_section = ".boot.rodata"` 会导致所有请求不被填充，内核对着一个支持 revision 6 的 12.9.4 报 `bootloader too old`——**诊断信息指向了完全错误的方向**。

诊断手段记录：QEMU `-d int,cpu_reset -D <file>` 是定位三重故障唯一有效的手段；`readelf -sW <kernel> | grep LIMINE_` 可确认请求是否真的被导出。

已验证的完整命令（成功输出 `SPIKE_OK` + `[NOVA-TEST] SUMMARY` 并 7s 退出）：

```bash
xorriso -as mkisofs -R -r -J -b boot/limine-bios-cd.bin -no-emul-boot \
  -boot-load-size 4 -boot-info-table -hfsplus -apm-block-size 2048 \
  --efi-boot boot/limine-uefi-cd.bin -efi-boot-part --efi-boot-image \
  --protective-msdos-label . -o nova.iso

qemu-system-x86_64 -machine q35,accel=tcg -cpu qemu64 -smp 1 -m 256M \
  -drive if=pflash,format=raw,unit=0,readonly=on,file=OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,unit=1,file=OVMF_VARS_4M.fd \
  -drive file=nova.iso,format=raw,media=cdrom \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -serial file:serial.log -display none -no-reboot
```

---

## 阶段划分

```mermaid
flowchart LR
    M0["M0 工程+测试闭环"] --> M1["M1 启动/异常/物理内存"]
    M1 --> M2["M2 分页/用户态/调度"]
    M2 --> M3["M3 Capability+IPC"]
    M3 --> M4["M4 服务/RAMFS/Console/Shell"]
```

严格串行，未达退出门槛不进入下一阶段。

---

## M0：工程骨架与自动化测试闭环

- `Cargo.toml` workspace（`resolver = "3"`、`edition = "2024"`）
- `rust-toolchain.toml`：**channel = nightly**、target `x86_64-unknown-none`
- `.cargo/config.toml`：`xtask` alias；kernel 的 `linker-flavor=ld.lld`、`panic=abort`
- `tools/xtask/`：`build` / `image` / `run` / `test` / `fetch-limine`（下载 v12.9.4 并校验 SHA256）
- `scripts/qemu.sh`：封装 QEMU 命令，统一超时、串口日志归档、`[NOVA-TEST]` 解析
- `docs/adr/0001-boot.md`、`0002-stable-vs-nightly.md`、`0003-address-layout.md`
- CI：host `cargo test` + `clippy` + QEMU 冒烟

**Done**：`cargo xtask run` 启动 QEMU 并打印版本号；`cargo xtask test` 能区分失败与超时。

## M1：启动、异常处理与物理内存

`kernel/src/` 骨架：`main.rs`、`arch/x86_64/{serial,gdt,idt,tss,intr}.rs`、`boot/limine_adapter.rs`、`mm/frame.rs`、`log.rs`

- 串口按实测的端口 IO 写法；**禁止** MMIO 访问 UART
- `limine_adapter.rs` 必须校验 memmap 条目边界与溢出，之后不再引用 bootloader 页面
- IDT 必须在任何可能 fault 的操作之前装载；PF 处理打印 CR2/错误码/全寄存器并以非零码退出
- bitmap 物理页分配器；host 侧纯 Rust 镜像实现跑 `cargo test`

**验收**：`boot_smoke`、`memory_map_sanity`、`divide_by_zero_fault`、`invalid_opcode_fault`、`serial_log_format`

## M2：分页、用户态、线程与抢占

先冻结 `docs/abi/address-layout.md`：内核高半区 `0xFFFF_8000_0000_0000`+、HHDM 由 Limine 提供 offset、用户空间 `0x1000`–`0x7FFF_FFFF_F000`（W^X）、用户栈含 guard page。**不硬编码地址常量**，由 HHDM/页表动态推导。

顺序：递归页表 → CR3/AddressSpace → 线程上下文与 `iretq` → `syscall` 入口（初版不用 `sysretq`）→ ELF64 加载器 → PIT 抢占调度 → 线程退出回收。

新增 `user/nova-rt/`（`_start`、页分配器、格式化输出），绕开为新 OS 移植 Rust `std`。

**验收**：`user_hello`、`round_robin_threads`、`preempt_busy_loop`、`user_cannot_write_kernel`、`task_a_cannot_read_task_b`、`invalid_elf_rejected`

## M3：Capability 与 IPC

- `crates/nova-abi/`：`CapHandle(u64)`（高 32 位 generation + 低 32 位 slot）、`IpcHeader`（24 字节编译期断言）、错误码表、syscall 号；`max_inline_payload = 64`、`max_caps_per_msg = 4`；`reserved` 字段必须为 0
- CapabilityTable：内核可信 `{ object_ref, rights, generation, derivation_parent, revocation_state }`
- 不变式（host 单测）：`Rights(C) ⊆ Rights(P)`、slot 复用后旧 handle 失效、slot 0 无效、generation 回绕需退役
- IPC：`send`/`recv`/`call`/`reply`，ReplyToken 一次性；带 cap 的发送必须原子（全转授或原样失败）
- 同步 rendezvous + 阻塞发送者起步；对端死亡唤醒并返回 `E_PEER_DIED`；超时后迟到回复丢弃
- initramfs 由 xtask 打包；`init` 持引导期有限根能力，收缩暴露面

**验收**：`ipc_echo_64`、`cap_send_only_denies_recv`、`ipc_4caps_atomic`、`old_handle_invalid`、`reply_once`、`server_exit_wakes_client`、`memory_leak_create_drop_10000`

## M4：ServiceManager、RAMFS、Console、Shell

- 服务 manifest：依赖、READY 握手、超时、`restart_max_attempts` + 时间窗口；**manifest 只是期望声明，不自动获得权限**
- RAMFS + FileServer：路径规范化、checked arithmetic、文件句柄而非反复按路径解析；文件能力按路径作用域授予
- ConsoleServer + Shell 内建 `help`/`ls`/`cat`/`echo`/`ps`；用户态单线程 executor 先行

**验收**：`shell_ls`、`file_read_write_ramfs`、`file_read_without_cap_denied`、`file_server_restart`、`ipc_queue_backpressure`、`open_close_10000`、`app_crash_does_not_kill_shell`

---

## 目标目录结构

```text
NovaOS/
├── rust-toolchain.toml      # nightly + x86_64-unknown-none
├── kernel/                  # no_std/no_main 微内核
├── crates/{nova-abi,nova-elf,nova-protocols}/
├── user/
│   ├── nova-rt/              # _start / 分配器 / 输出
│   ├── nova-ipc/ nova-cap/ nova-async/ nova-fs/   # SDK
│   ├── init/                 # 首个用户进程 + ServiceManager
│   └── services/{file-server,console-server}/
├── tools/xtask/
├── boot/                     # limine.conf 模板 + initramfs manifest
└── docs/{adr,abi,protocols}/
```

PR 切分（每 PR 一个设计目标，PR#1 不做空壳模块生成器）：
M0 骨架 → M1 串口/异常 → M1 物理内存 → M2 分页 → M2 用户态+syscall → M2 调度 → M3 nova-abi+cap → M3 IPC → M3 init → M4 FileServer → M4 Shell。

## 质量门槛

- `cargo fmt` / `cargo clippy -D warnings` 零告警
- 每个新增 `unsafe` 紧邻 `// SAFETY:` 说明不变量
- 纯逻辑（cap 表、状态机、协议解析、ELF 校验）全部 host 侧单测
- 每个 QEMU 测试带超时并归档串口日志
- 堆分配器就绪前不用 `Vec`/`Box`，用静态有界池

## 风险止损

- 用户态异常频繁引发内核 panic → **暂停 M4**，回到内存隔离
- IPC 同步导致卡死 → 补队列上限、超时、优先级继承
- 范围失控 → 严格按 M0→M4 门槛，不提前做 GUI/网络/存储
- 性能 → 只做同环境回归对比，不承诺绝对数字

**v0.1.0 明确不做**：GUI、磁盘/VirtIO、网络、SMP、驱动、多用户、包管理。

## Definition of Done

1. `cargo xtask test` 从干净环境可复现且全绿
2. QEMU/OVMF 独立启动，串口输出 `[NOVA-TEST] SUMMARY passed=N failed=0`
3. 两个隔离用户进程通过受 Capability 约束的 Endpoint 通信
4. `init` 能启动、监管并重启用户态服务
5. Shell 可在 RAMFS 上 `ls`/`cat`/`echo`/`ps`
6. NovaOS.md 第 19.3 节全部反例测试通过
7. 所有新增 `unsafe` 有 Safety 注释与审查记录
8. 无未解释的内核 panic
9. `docs/abi/` 记录 syscall 调用约定、地址布局、IPC 语义