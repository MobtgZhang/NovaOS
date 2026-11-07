# NovaOS 代码问题与未实现功能分析报告（issues-001）

- 日期：2026-11-07（2026-11-08 更新：§1/§2/§3 的 P0 项已修复并验证，见文首"更新记录"）
- 对象：当前 `main` 分支代码（Limine 移除完成、205/205 测试通过之后的现状）
- 方法：逐文件读码（内核 `mm/`、`sched/`、`task/`、`arch/x86_64/`、`boot/`、`drivers/`、`services/`、`syscall/`，以及 `crates/nova-net`、`crates/nova_fs`），配合既有子代理对文件系统/驱动子系统的详尽发现。
- 严重程度：**High** = 阻塞"运行于现代硬件"或存在正确性风险；**Medium** = 功能缺陷或明显短板；**Low** = 设计债。
- 标注约定：〔未实现〕= 功能不存在；〔缺陷〕= 已写出的代码有 bug；〔设计张力〕= 与宣称的架构自相矛盾。

---

## 更新记录（2026-11-08）

以下条目已**修复并经 QEMU `-smp 4` headless 端到端验证**（内核测试套件 214/214 全绿，`scripts/gates.sh` 五道门禁全过；提交 `a835722`、`7f36054`）：

| 条目 | 状态 | 验证证据 |
|---|---|---|
| §1.1 ACPI 解析 | **已实现（最小集）** | `nova-boot-menu/src/acpi.rs`：RSDP→XSDT→MADT type-0 枚举 4 个处理器；boot log `acpi: 4 processors, bsp lapic id 0x0` |
| §1.2 早期页表 4K-only | **引导侧已修** | NovaBoot 恒等映射改为 PML4[0]→PDPT→512 个 PD×2 MiB 叶子，盖满低 4 GiB；内核侧 `walk_to_leaf` 仍 4K-only（见下文原状） |
| §2.1 256 MiB 硬上限 | **已修复** | 位图改运行期尺寸；2 GiB QEMU 实测 `18452 allocated + 499922 free = 518374 frames (63 KiB bitmap)`；DMA 4 GiB 边界经 `alloc_dma` + `DMA_FRAME_LIMIT` 显式约束（§2.6 的隐式保证随之显式化） |
| §2.3 HHDM 逐 4K 映射 | **已修复** | HHDM 用巨叶；测试 `the_hhdm_window_uses_huge_leaves` 通过 |
| §3.1 AP bring-up 不存在 | **已修复** | 真实 INIT-SIPI + 低内存 trampoline（16 位手写机器码全部 32 位立即数补 `66` 前缀）；`-smp 4` 实测 `cpu 1/2/3 online=true scheduler_threads=1 ticks=18794/10694/770` |
| 附带发现 | **已修复** | 帧位图两处正确性 bug（计数 u32 回绕、`find_first_free_from` 用掩码当移位量）——宿主复现后修复；顺序分配 hint 改单调前进 |
| §3.2 per-CPU 抽象 | **已修复**（2026-11-08） | 新增 `arch::x86_64::percpu`（GS base，IA32_GS_BASE，offset 0x00 用户栈指针 / 0x08 内核栈顶 / 0x10 CPU 槽位）；syscall stub 与 `return_to_user` 全部 GS 寻址，删除单全局 `NOVA_KERNEL_USER_STACK_POINTER` 与"BSP-only TSS"假设；`sched`/`task` 所有 cpu-0 包装改 `arch::current_cpu()`；`PENDING_EFFECT`/`PENDING_EXIT_STATUS` 改 per-CPU 数组（§3.4 随之关闭）；服务层、帧分配器、virtio net/block 单例全部套上新的递归 irq-safe `SpinLock`（`kernel/src/sync.rs`，按 CPU 槽位判重入）；`task::create_thread` 轮转分发到在线 CPU，新测试 `created_threads_dispatch_across_online_cpus` 断言线程落在多个 CPU。门禁全绿，boot 套件 63 suites / 215 cases 全过，`-smp 4`（xtask 硬编码）3 AP 在线 |
| §3.4 全局单槽 syscall 副作用 | **已修复**（2026-11-08） | 随 §3.2：改 per-CPU 数组，槽位经 GS 读取当前 CPU |

**仍然未解**：§4 中断模型全套（§4.1/4.2 P0）；内核堆（§2.5）；`walk_to_leaf` 巨叶（§1.2 内核侧）；§4.3 时间源；§5.1 ECAM；§6/§7 FS 与 TCP 项；驱动现代化（§5.3/§5.4）。

---

## 0. 总体判断

NovaOS 目前是一个**轮询驱动、全静态池**的 Symbian 风格微内核原型，其强项是能力系统 / IPC / 策略层可宿主测试（214 测试全绿），弱项是它实际上**只能在 QEMU 的一个较窄的配置窗口里运行**：slirp 网络、virtio 设备、PS/2 + GOP 固件仿真——但内存上限与单 CPU 这两条最大的硬阻塞已在 2026-11-08 移除（见文首更新记录）。下表是各子系统的"完成度 × 是否阻塞现代硬件"一览：

| 子系统 | 完成度 | 阻塞现代硬件？ |
|---|---|---|
| 内存管理 | 位图分配器（运行期尺寸，2 GiB 实测）+ 4 级页表 + HHDM 巨叶；无堆/COW | **不再**（>256 MiB panic 已解；剩余是质量债） |
| SMP | **真实多核唤醒已验证**（3 AP 在线、各自调度器线程与 tick）；用户线程路径仍绑死 CPU 0 | 部分（唤醒已通，per-CPU 抽象未做） |
| 中断 | PIC 重映射 + APIC timer，设备全轮询 | **是**（无法服务任何设备 IRQ） |
| 驱动 | virtio-blk/net 轮询、PS/2 轮询、GOP | **是**（无 NVMe/AHCI/USB/xHCI/ECAM） |
| 文件系统 | NOVADSK 自定义格式 + RamFS，无 VFS | 部分（可用但脆弱） |
| 网络 | 包构造/解析库 + 轮询 TCP 客户端 | 部分（绑死 QEMU slirp） |
| 进程/IPC/能力 | 微内核核心已成形，最有价值 | 否（质量较好） |
| 启动 | UEFI + NovaBoot + **ACPI MADT 处理器枚举** | 部分（MADT 已解；FADT/HPET/IOAPIC 未解） |

---

## 1. 启动与"现代硬件可运行性"

### 1.1 完全没有 ACPI 解析〔未实现〕**High** → **处理器枚举已实现（2026-11-08）**

> **更新**：RSDP→XSDT→MADT type-0 枚举已在 `crates/nova-boot-menu/src/acpi.rs` 实现，经 BootProtocol 快照传给内核；`-smp 4` 实测枚举出 4 个处理器并全部唤醒。**仍未解**：FADT（电源管理/ACPI 模式）、HPET、RTC 墙上时钟、I/O APIC 枚举（§4 的前提）。

- 全代码库中 `ACPI`/`MADT`/`RSDP`/`HPET` 仅出现在注释里（`kernel/src/boot/novaboot.rs:240`、`kernel/src/boot/smp.rs:198`、`kernel/src/test/harness.rs:54`）。
- 后果链：
  - 无法枚举 CPU → SMP 永远是单核（见 §2）；
  - 没有 `RSDP`/`MADT`，真实硬件上无法知道 I/O APIC、HPET、NUMA 拓扑；
  - 没有 `FADT`，无法做电源管理，也无法区分"ACPI 模式"下的 PIC 状态；
  - 没有 RTC/`FADT` 世纪寄存器，内核**没有任何墙上时钟**——这也解释了文件系统为什么没有时间戳（§6）。
- NovaBoot 的 `BootProtocol` 快照结构已为 `cpu_ids: [u32; MAX_CPUS]`（`kernel/src/boot/info.rs:233`）留好了位置，但填充端写死 `cpu_count=1, bsp_lapic_id=0`（`kernel/src/boot/novaboot.rs:236-243`）。

### 1.2 早期页表只支持 4K 页〔未实现〕**Medium** → **引导侧已修（2026-11-08）**

> **更新**：NovaBoot 的恒等映射已改为 PML4[0]→PDPT→512 个 PD、每 PD 2 MiB 叶子，盖满低 4 GiB（此前只盖 1 GiB，引导镜像在 ~2 GiB 时首次取指即三重故障）。内核侧 `walk_to_leaf` 仍不识别大页位——任何内核查询/修改路径遇到 2 MiB 叶子仍会走错，需与 §2.3 的收尾联动。

- `crates/nova-boot` 的引导期页表（PML4[0]/[256]/[511] 三条链）全部是 4K 叶子；内核侧 `mm/page_table.rs` 的 `walk_to_leaf` 同样不支持 2MiB/1GiB 叶子（§3）。
- 在 4K-only 下映射 256 MiB HHDM 需要 65536 个 PTE；真实机器 32 GiB 内存需要 800 万+ 个 PTE（约 16 GiB 的表空间都不够），**这是"现代硬件不可运行"的第一数学障碍**。

### 1.3 启动管理器本身质量良好（正面记录）

- GOP 菜单、ESP FAT 加载 ELF/initramfs、BootProtocol 快照、`ExitBootServices`、`cli/mov rsp/jmp` 手跳，均已验证可用（`crates/nova-boot-menu/`）。
- `normalise_framebuffer` 对 QEMU GOP 以像素报 stride 的固件怪癖做了归一化——这类"真实固件怪癖防御"是好苗子。

---

## 2. 内存管理

### 2.1 物理帧分配器 256 MiB 硬上限，超限直接 panic〔缺陷 + 设计债〕**High** → **已修复（2026-11-08）**

> **更新**：位图改为运行期尺寸——从固件内存图中量出帧数、在最大的可用区内划出位图存储（63 KiB @ 2 GiB），设计上限 ≥192 GiB（6 MiB 位图）；2 GiB QEMU 实测 518374 帧正常分配回收。4 GiB DMA 边界不再靠"上限 < 4 GiB"隐式成立，而是 `alloc_dma` + `DMA_FRAME_LIMIT` 显式约束。附带修掉两个正确性 bug：位图计数 u32 回绕、`find_first_free_from` 用位掩码当移位量（宿主复现后修复，commit `7f36054`）。§2.2 的单 CPU `UnsafeCell` 契约仍在，是 SMP per-CPU 化时的第一改动点。

- `kernel/src/mm/frame.rs:265-268`：`MAX_BITMAP_WORDS = 1024` → `MAX_FRAMES = 65536`（4K 帧）= 256 MiB。
- `kernel/src/mm/frame.rs:112-120`：机器可用内存超过上限时 `Bitmap::with_capacity` 失败即 `panic!`，并提示"raise MAX_BITMAP_WORDS"。**现代任何一台真实电脑都会在此崩溃**。这是移植性的头号阻塞项。
- 位图是编译期定长、落在 `.bss`；正确做法是启动期从可用内存里划出位图（或切换到多级空闲列表/buddy）。

### 2.2 `FrameAllocator` 是单 CPU 的 `UnsafeCell`〔设计张力〕**Medium** → **已修复（2026-11-08）**

> **已修复**：`bitmap: UnsafeCell<Bitmap>` 改为 `SpinLock<Bitmap>`（与 §3.2 同一把递归 irq-safe 锁）。锁覆盖 `alloc`/`alloc_dma` 的整个 hint-then-rescan 序列（否则两个 CPU 可同时看中同一个空闲位、把一页发给两个主人），seeding/`dealloc`/`stats`/`free_count` 同锁；安全的 `Bitmap` 逻辑未动。virtio net/block 单例同批迁移。

### 2.3 HHDM 逐 4K 映射、无巨页〔缺陷〕**High**（与 1.2 同根）→ **HHDM 已修（2026-11-08）**

> **更新**：`map_hhdm` 已用 2 MiB 巨叶（测试 `the_hhdm_window_uses_huge_leaves` 通过）。**未解**：`mm/page_table.rs` 的 `walk_to_leaf` 仍不识别大页位——内核查询/修改路径（页表遍历、权限修改、将来的 unmap）遇到巨叶仍会错判，需按下面清单收尾。

- `kernel/src/mm/address_space.rs` 的 `map_hhdm` 对 usable 区域逐帧循环 `map_kernel_page`，256 MiB 就是 65536 次页表写 + 中间表分配；`map_kernel_mmio` 同样 4K。
- `mm/page_table.rs` 的 `walk_to_leaf` 不识别大页位，意味着即使硬件支持，内核任何查询/修改路径都假定 4K 叶子。加巨页必须同时改：`walk_to_leaf`、`map_hhdm`、NovaBoot 早期页表、以及帧位图的"连续 512 帧对齐"假设。

### 2.4 每进程地址空间：共享中间表的设计取舍〔设计张力〕**Low/Medium**

- `mm/address_space.rs`：每进程 PML4 只复制上半区（内核区）条目，中间表共享——省内存，但意味着**内核映射的修改必须立即在所有 CPU/所有进程生效**（单 CPU 下天然成立；SMP + 多进程下，改内核页表需要 IPI 或约定只在启动期修改）。文档已意识到，但 SMP 落地时这是隐蔽正确性雷区。

### 2.5 没有内核堆分配器〔未实现〕**High**（下一步）

- 内核没有任何通用 `alloc`：线程栈（`task/mod.rs:336-389` StackPool）、能力表、IPC 通道、socket、mount 表……全部是编译期定长静态数组。这导致：
  - 所有资源都有硬上限（§9.4 汇总），任何超限都是拒绝服务而不是退化降速；
  - 新进程地址空间、`MemoryObject` 等只能从帧位图切页，无法按需分配任意尺寸对象；
  - 这是"无法运行真实工作负载"的结构性原因。没有堆，就没有动态进程数、动态 socket 数、动态消息缓冲。
- 附带：无 COW、无请求调页、无 swap、无 mmap 匿名区、无 `UNMAP` syscall（`services/dispatch.rs:234` 明说返回 `E_UNSUPPORTED`）——虚拟内存子系统实际上只实现了"建表 + 映射 + W^X"。

### 2.6 DMA 与 IOMMU〔未实现〕**Medium**

- virtio 驱动的 DMA 缓冲直接用物理帧，没有 IOMMU/iova 概念，靠"帧分配器上限 256 MiB < 4 GiB"隐式保证 DMA 地址安全。一旦修 2.1（内存超过 4 GiB），virtio 32-bit 设备会拿到 >4 GiB 的地址而静默损坏——修 2.1 时必须同时引入 bounce buffer 或 DMA 约束检查。
  - **更新（2026-11-08）**：2.1 修复时已同步引入 `alloc_dma` + `DMA_FRAME_LIMIT`（4 GiB/4 KiB）显式约束，DMA 路径不可能拿到边界之上的帧；IOMMU/iova 仍是未实现项。

---

## 3. SMP（多核多线程）

### 3.1 AP bring-up 完全不存在〔未实现〕**High** → **已实现并验证（2026-11-08）**

> **更新**：`kernel/src/arch/x86_64/smp.rs` 实现完整 INIT–SIPI 唤醒链：MADT 拿 LAPIC ID → INIT IPI → 两次 SIPI（含重试）→ 低内存 trampoline（<1 MiB 页，恒等映射进内核空间 PML4[0]）→ 16 位手写机器码开 PAE/LME/NXE/PG → far jump 进 64 位 stub → per-CPU 内核栈续行（`boot_after_ap_stack_switch`）。`-smp 4` headless 实测 **cpu 1/2/3 全部 online，各自调度器线程与真实 timer tick**。调试中定位的关键缺陷：手写 16 位机器码里的 32 位立即数没有 `66` 前缀——`or eax, 0x80000001` 实际解码成 `or ax, 0x0001`（PG 位从未置上），被吞的立即数尾部还变成 `add [bx+si], al` 顺带写物理内存。该经验已写进 trampoline 的文档注释。

- `kernel/src/boot/smp.rs:190-200` 注释直说："There is deliberately no AP entry point yet … the kernel boots the bootstrap processor only"。
- 已有的部分：`AP_ONLINE`/`AP_TICKS`/`AP_TSS_RSP0` 等**诊断原子**（smp.rs:27-126）、per-CPU GDT/TSS 安装路径（`arch/x86_64/mod.rs:167-173 init_cpu`）、AP 内核-only idle 线程裸函数 `ap_idle_entry`（smp.rs:177-188，`sti/hlt/jmp` 自环）。也就是说"AP 到达后要做什么"已备好，但**没有人能把 AP 叫醒**：没有 MADT 解析、没有 INIT-SIPI 发送、没有低内存 trampoline 页。
- SMP 测试套件因此在 `processor_count()==1` 时自我跳过（`test/harness.rs:54`）——205 个测试里 SMP 部分是 SKIPPED，"通过"具有误导性。

### 3.2 调度器与用户线程绑死 CPU 0〔缺陷〕**High** → **已修复并验证（2026-11-08）**

> **已修复**：GS-base per-CPU 抽象落地（`arch::x86_64::percpu`，含编译期偏移断言）；syscall stub、`return_to_user`、`save_current_syscall_context`、`current_thread_space`、调度器全部包装改经 `arch::current_cpu()`；线程创建轮转分发。原判断的四个硬编码点全部消除。共享可变状态（服务层、帧分配器、virtio）以递归 irq-safe SpinLock 收口。

- `kernel/src/sched/mod.rs:521-554` `save_current_syscall_context`：**硬编码 `state_for_cpu(0)` / `thread_in_slot_for_cpu(0, ...)`**。同样地 `task/mod.rs:600-609 current_thread_space` 也读 `thread_pool(0)`。
- `process.rs:615` `create_thread` 一律 `create_thread_for_cpu(0, ...)`。
- 结论：即未来把 AP 唤醒了，所有用户线程的上下文保存、地址空间读取、线程创建都仍然落在 BSP 的表上——AP 的定时器 tick 会去碰 `cpu!=0` 的状态，而用户态路径只认 CPU 0，会产生两个调度器各管一半的混乱。**多核化前必须先引入 GS-base/per-CPU 寄存器式的"当前 CPU"抽象**，替换掉遍布代码的 `cli` + "单 CPU 写"契约（`task/mod.rs:329,405,462` 三处 `unsafe impl Sync` 全部依赖它）。

### 3.3 调度器策略极简〔未实现〕**Medium**

- round-robin、`DEFAULT_TIMESLICE = 4` tick @ 100 Hz = 40 ms（`sched/mod.rs:65-72`、`pit.rs:72`）；无优先级、无实时类、无负载均衡、无亲和性、无 tickless；PIT tick 驱动而非 APIC timer 单票（见 §5 的实际 tick 源混乱）。
- 无 idle 线程：BSP 上无可运行线程时直接 `exit(EXIT_PANIC)`——桌面会话里"最后一个用户线程退出"等于内核自尽。AP 侧倒是有 idle 线程（smp.rs:172-188），两套语义不一致。

### 3.4 全局单槽 syscall 副作用〔设计张力〕**Low** → **已修复（2026-11-08）**

- `task/mod.rs:511-569`：`PENDING_EFFECT`/`PENDING_EXIT_STATUS` 是 `static mut` 全局单槽，契约是"syscall 路径串行 + 中断屏蔽"。多核化后每个 syscall 都要改成 per-CPU 槽。

---

## 4. 中断体系

### 4.1 vector 0x20 双重占用 + 错误的 EOI 组合〔缺陷〕**High**

这是本次读码新发现的一个真实正确性问题：

- 8259 PIC 被重映射到 `0x20/0x28`（`arch/x86_64/intr.rs:26-28`），PIT 的 IRQ0 因此走 vector 0x20（`pit.rs:139` 还 `unmask_irq(0)`）；
- 同时本地 APIC 的 LVT timer 也被配置为 **同一个 vector 0x20**、periodic 模式、count 0x1000（`arch/x86_64/apic.rs:41,74-89`），且 BSP 一并初始化（`apic.rs:116-119`）；
- 定时器 handler `nova_kernel_timer_handler`（`arch/x86_64/idt.rs:1316-1349`）**无条件只发 APIC EOI**（idt.rs:1326 `super::apic::eoi()`），从不调用 `intr::send_eoi()`（PIC EOI）。

后果：中断源是 APIC timer 时一切正常；但 PIT 的第一次 IRQ0 一旦经 PIC 送进同一个 vector，PIC 的 In-Service 寄存器永远不会被清——该 IRQ 及更低优先级线从此"卡住"，而代码没有任何地方意识到这一点。当前碰巧靠 APIC timer 供 tick 才没出事，**这是靠巧合而不是靠设计正确的双中断源共用一 vector**。正确做法：PIC 完全 mask 掉（或直接不 unmask IRQ0），APIC timer 换一个独立 vector（如 0x32+），并在 handler 里按中断源分发 EOI。

### 4.2 除 tick 外没有任何中断处理能力〔未实现〕**High**

- `idt.rs:652-658`：vector > 31 且无专用 handler 的中断一律 panic（"hardware interrupt with no handler"）。
- 没有 I/O APIC、没有 MSI/MSI-X、没有 IRQ 路由表；所有设备（virtio-blk、virtio-net、PS/2）都靠**在时钟 tick 里轮询**（idt.rs:1332-1343 甚至在 tick handler 里直接轮询 PS/2 控制器）。
- 影响：a) I/O 吞吐被 100 Hz tick 限死（virtio-net 的 `POLL_BUDGET=100_000_000` 次自旋，`drivers/virtio/net.rs:118`，一次收包可能自旋数秒）；b) 中断驱动的全部基础设施（下半部、软中断、tasklet、wakeup）为零，将来接 NVMe/USB 必须从零建。

### 4.3 时间子系统单薄〔未实现〕**Medium**

- 只有 PIT（`arch/x86_64/pit.rs`）+ 被意外依赖的 APIC timer；无 HPET、无 TSC 校准（`msr.rs` 存在但没用它做时间源）、无 RTC/墙上时钟。系统没有任何"绝对时间"概念，这直接导致文件系统无法实现时间戳（§6.3）。

---

## 5. 驱动管理

### 5.1 PCI 仅限传统端口枚举〔未实现〕**Medium**

- `kernel/src/drivers/pci.rs:34-40`：只有 `0xCF8/0xCFC` 端口式配置访问；无 PCIe ECAM/MMCONFIG（现代固件常把端口式访问保留在最小编置上，ECAM 才是正道）；`MAX_BUSES=8`、`MAX_DEVICES=32`（pci.rs:54,61），不支持多层级 PCIe 拓扑。
- 设备识别表只有 virtio legacy/modern 几个 ID（pci.rs:90-96）。

### 5.2 virtio 块设备：全轮询、单请求在飞〔缺陷〕**Medium**

- `drivers/virtio/blk.rs`：一次只允许一个 in-flight 请求，`POLL_BUDGET`（约 800 万次迭代）内自旋等待完成；block cache 只有 8 行。顺序读一块 4 KiB 没问题，任何稍大的 I/O 都会退化成不可接受的延迟。
- 无 virtio 事件索引/中断驱动路径；DMA 约束问题见 §2.6。

### 5.3 输入：PS/2 轮询，无 USB〔未实现〕**High**

- `drivers/input.rs:1-20`：PS/2 控制器 `0x60/0x64`，在 tick handler 里被轮询。真实现代电脑几乎全部是 USB 键鼠（很多主板 BIOS 里 PS/2 仿真默认关闭）——**在现代硬件上这个内核大概率没有键盘**。需要 xHCI + HID 报告解析，工作量大但绕不开。

### 5.4 存储：无 NVMe/AHCI/SATA/SD〔未实现〕**High**

- 唯一存储后端是 virtio-blk。真实机器的磁盘 99% 是 NVMe（PCIe）或 AHCI SATA。没有这些，"运行在现代硬件"只剩 U 盘 FDD 模式这类边缘情形。

### 5.5 显示：仅 GOP framebuffer〔Low〕

- `drivers/display.rs` 依赖 boot protocol 的 framebuffer 信息；无 EDID/模式切换、无 GPU 加速。可接受，但合成器（`services/compositor.rs`，1222 行）在 100 Hz tick 驱动下的重绘节奏没有 vsync/脏矩形优化。

---

## 6. 文件系统与存储服务

（本节大量引用此前子代理的详尽发现，行号经复核。）

### 6.1 没有真正的 VFS〔未实现〕**High**

- `kernel/src/services/fs.rs`：FileServer 内部是**硬编码 mount 表**，`MAX_MOUNTS = 2`；路径→文件系统的分派不是可插拔接口。加第二个真实文件系统（FAT、ext2）必须改 FileServer 本体。

### 6.2 磁盘格式 NOVADSK 的结构性缺陷〔缺陷〕**High**

- `crates/nova_fs/src/diskfs.rs:92`：`MAX_SLOTS = 8` ——整个磁盘最多 8 个文件；固定 4 KiB 块；**没有 delete / rename / truncate / 目录层次 / 时间戳 / 权限 / 链接**。
- 写路径（diskfs.rs:640-686）：**先写数据块、后改目录项长度**，两步之间掉电即产生"目录声称有长度、数据却是垃圾"的不一致；无日志、无校验和、无原子 rename。
- wire handle 无 generation（`services/fs.rs:94,1101`）：客户端持有的句柄在服务端槽复用后（ABA）会**静默指向另一个文件**——能力系统"句柄即权限"的承诺在文件句柄上被打破。

### 6.3 客户端死亡不回收打开文件〔缺陷〕**High**

- `services/fs.rs:296-305`：`ServerFile` 没有 owner/task 字段，内核 kill 一个任务时（`kill_task → revoke_all` 能力级联是完备的）**无法回收它打开的文件**；32 个文件槽（`MAX_OPEN_FILES`）可被恶意/失败客户端耗尽，构成拒绝服务。这是"能力撤销即资源回收"这条招牌特色的直接违背，建议优先修。

### 6.4 RamFS 尺寸过小〔Low〕

- 总量 16 KiB、单文件 1 KiB；initramfs 容量 512 KiB（已用 ~270 KiB）。作为 `/tmp` 与 IPC 大 payload 的载体都嫌小。

### 6.5 用户态 file-server / console-server 是 stub〔设计张力〕**Medium**

- 内核内有**真实的** FileServer/Console/Net/Compositor 服务（`kernel/src/services/*.rs`），而 initramfs 里对应的用户态服务程序是壳。Symbian 风格宣称 client/server 微内核，实际服务在内核地址空间内运行——权限检查虽经能力系统，但"服务崩溃可隔离"这个微内核核心卖点目前不存在（服务 panic = 内核 panic）。这是架构宣言与实现的根本张力，需要要么把服务真正搬出 ring 0，要么明确改口"受保护模块"混合内核。

---

## 7. 网络栈

### 7.1 分层结构（正面记录）

- `crates/nova-net`（1280 行）是纯函数式包构造/解析库：Ethernet/ARP/IPv4/UDP/TCP/DNS/ICMP/DHCP，全部可宿主测试——这是整个网络栈最有价值的部分。
- `kernel/src/services/net.rs`（795 行）是服务层：`MAX_SOCKETS = 8`（net.rs:62）、同步 request/replay 语义、socket 按 `task_slot` 归属。

### 7.2 TCP 实现是"教科书第 1 章"〔未实现〕**High**

- `drivers/virtio/net.rs:94`：`TCP_INITIAL_SEQUENCE = 0x1234_5678` **常量 ISN**——固定 ISN 在真实网络上既是安全问题（可预测序列号 → 会话劫持），也保证了两台 NovaOS 互连时的必然冲突。
- `connect_tcp`（net.rs:608-700）：内联轮询式三次握手；`send_tcp`/收包路径要求 `segment.sequence == remote_seq` **精确相等**（net.rs:752）——不支持乱序、丢包重传、重复 ACK、窗口缩放、拥塞控制、Nagle、keepalive。丢一个包 = 连接卡死到 `POLL_BUDGET` 耗尽。
- 只有主动 `connect`，没有 `listen`/`accept`——**无法作为服务器**。

### 7.3 绑死 QEMU slirp〔缺陷〕**Medium**

- `drivers/virtio/net.rs:76-82`：本机地址 `10.0.2.15`、网关 `10.0.2.2`、DNS `10.0.2.3` 是 slirp 默认值**写死在常量里**；DHCP 代码（nova-net 有完整 build/parse）存在但没被用来发现配置。
- ARP 无缓存（`LOOKUP_ATTEMPTS = 8`，net.rs:121）：每次发包都广播 ARP 并自旋等答复。
- 无 IPv6、无分片重组、无 ICMP 错误处理、无校验和卸载、单 vring 单队列、全轮询。

### 7.4 socket API 同步阻塞〔未实现〕**Medium**

- `services/net.rs` 全部经 `SERVICE_REQUEST` 同步往返：无 `select`/`poll`/非阻塞、无监听队列；`MAX_SOCKETS=8` 且句柄耗尽即失败。

---

## 8. 进程 / 线程 / IPC / 能力系统（当前最强子系统）

### 8.1 正面记录（值得保住的设计资产）

- **能力表 + 撤销级联**：`kill_task → revoke_all` 级联、`Rights::GRANT` 控制再委托（`services/process.rs:59-71`：委托检查重建自父进程自身 scope 集，撤销即时生效）、spawn 时子任务只拿窄根（`SPAWN | GRANT`，process.rs:132-141）、init 持 `/` 的 `READ|WRITE|GRANT` 而非旁路（process.rs:220-236）。
- **IPC 能力转移原子性**：`services/ipc.rs:275-321` `TableTransfer` 的 prepare/commit/abort 三段式，句柄在接收方表内铸造、避免源槽号泄漏；对端死亡经 `kill_side → E_PEER_DIED` 唤醒所有 park 中的线程（ipc.rs:1-50 文档完整）。
- **策略/机制分离**：策略在 `nova_core`/`nova-ipc` 宿主可测，机制（resume block、iretq trampoline）在 `task/mod.rs:896-926`，一个 trampoline 服务 ring0/ring3 两种恢复，设计干净。

### 8.2 严重的尺寸上限（全部互相叠加成"8 的世界"）〔未实现〕**High**

| 资源 | 上限 | 位置 |
|---|---|---|
| 任务（进程） | **8** | `services/mod.rs:100` |
| 每 CPU 线程 | **8** | `task/mod.rs:94`（注意 `services/mod.rs:107` 另有一个 `MAX_THREADS=64`，**两处常量名冲突、语义不一致**〔缺陷〕） |
| 内核栈 | 16 KiB × 8 | `task/mod.rs:88` |
| IPC 通道 | 16 | `services/mod.rs:113` |
| socket | 8 | `services/net.rs:62` |
| mount | 2 | `services/fs.rs` |
| 磁盘文件 | 8 | `diskfs.rs:92` |
| CPU | 8（快照）但用户线程路径实际只用 0 | `boot/info.rs:36` vs `sched/mod.rs:549` |
| 内存 | ~~256 MiB~~ → 运行期尺寸（2 GiB 实测，设计上限 ≥192 GiB） | `frame.rs` |
| initramfs | 512 KiB | `initramfs.rs` |
| 消息 payload | 64 B 内联 | `SERVICE_REQUEST` |

根源就是 §2.5 的"没有内核堆"：一切皆静态池，超限即 `E_NO_MEMORY`。桌面环境 + 文件服务 + 网络 + 一个应用就已经把 8 个任务槽用满。

### 8.3 进程模型缺失的部分〔未实现〕**Medium/High**

- **单线程进程**：`THREAD_SPAWN` 返回 `E_UNSUPPORTED`（`services/dispatch.rs:234`）——没有多线程进程，SMP"多线程"叙事实际不成立。
- `PROCESS_EXEC` 只认 initramfs 内的静态 ELF，无 argv/envp 传递、无动态链接、无 ELF 头部校验外的安全加载（`loader/mod.rs`）。
- 无信号、无 `waitpid`（只有退出状态单值回传）、无进程组/会话、无 `CANCEL_WAIT`（dispatch.rs:237 自认没有阻塞原语之外的取消）。
- 无 zero-copy IPC：64 B 内联 + `MemoryObject MAP` 是唯二的数据通道；Symbian 系统里成熟的"消息携带大缓冲"模式没有对应物。**建议**：把"帧捐赠"（把一个物理帧的所有权随消息转移，接收方 unmap、发送方 revoke）做成一等公民——它与现有能力转移的 prepare/commit/abort 结构天然同构，且能成为本内核的招牌特色（§11）。

---

## 9. 系统调用完成度

已实现（`syscall/mod.rs:417-484` + `services/dispatch.rs:244-274`）：
`GET_VERSION`、`GET_BOOT_MODE`、`WRITE`（console）、`PROCESS_EXEC`、`PROCESS_STATUS`、`OBJECT_CREATE/DESTROY`、`CAP_DEREGATE/CAP_REVOKE`、`SEND/RECV/CALL/REPLY`、`MAP`、`YIELD`、`THREAD_EXIT`、`ENDPOINT_CREATE/PEER_CREATE/SELF`、`PROCESS_COUNT`、`SERVICE_REQUEST`。

明确返回 `E_UNSUPPORTED`（dispatch.rs:234-237）：
- `UNMAP`（虚拟内存收回）
- `THREAD_SPAWN`（多线程）
- `CANCEL_WAIT`（阻塞取消）

完全没有的：`read`（console 只有 write？文件读经 SERVICE_REQUEST，终端输入经 INPUT_OP_GET）、`sleep`/nanosleep、`clock_*`、信号、`fork`、`exec` 参数、`mmap` 匿名区、`pipe`、`dup`、`set_errno` 语义。syscall 入口本身质量高（SFMASK 清 IF/TF/DF/AC，`syscall/mod.rs:74-88`，注释解释了每个位的原因）。

---

## 10. "运行于现代硬件"差距清单（按优先级）

**P0 —— 不做就在真机第一分钟内失败**

> 进度（2026-11-08）：**1 ✅、2 半 ✅（引导侧/HHDM 已巨叶，`walk_to_leaf` 未改）、3 ✅（处理器枚举）、4 半 ✅（AP 唤醒已通，per-CPU 未做）、5 未动**。§2.6 的 DMA 4 GiB 边界随 1 显式化。

1. ~~物理内存 > 256 MiB panic（frame.rs:112-120）~~ → **已修**：运行期尺寸位图；DMA 边界经 `alloc_dma` 显式约束（§2.6）。
2. 巨页支持（2MiB/1GiB）：~~`map_hhdm`、NovaBoot 早期页表~~ → **已修**；**剩余**：`walk_to_leaf` 识别大页位（内核查询/修改路径）。
3. ~~ACPI 最小集：RSDP → MADT（CPU 枚举）~~ → **已修**（`nova-boot-menu/src/acpi.rs`）；**剩余**：IOAPIC 枚举（第 5 项的前提）、FADT/HPET。
4. ~~AP bring-up：MADT LAPIC ID → INIT-SIPI → 低内存 trampoline → per-CPU init_cpu~~ → **唤醒链已通**（`-smp 4` 三 AP 在线）；**剩余**：per-CPU 抽象（GS base），拆除 sched/task 硬编码 cpu 0（sched/mod.rs:549、task/mod.rs:606、process.rs:615-617）与 `static mut PENDING_EFFECT`。
5. 中断模型：APIC timer 独立 vector、PIC 全 mask 或弃用；I/O APIC 路由 + 至少一种设备中断驱动模式（推荐 MSI-X 先行，xHCI 必需）。**未动**。

**P1 —— 真机上"可用"所需**
6. xHCI + USB HID（键鼠）；无它则无输入。
7. NVMe（或至少 AHCI）存储驱动。
8. PCIe ECAM 枚举（端口式 0xCF8 在部分现代平台受限）。
9. 命令行帧分配之上的内核堆分配器 → 逐个解除 §8.2 的静态上限（至少 task/thread/socket/mount 动态化）。
10. HPET/TSC 时间源 + RTC 墙钟（文件时间戳的前提）。

**P2 —— 质量与健壮性**
11. TCP：随机 ISN、重传/乱序/窗口管理、listen/accept；ARP 缓存；DHCP 真正用于取址。
12. 文件系统：句柄 generation、`ServerFile` owner 回收、磁盘格式加日志或写时复制 + 校验和；delete/rename 至少其一。
13. 服务真正出内核（或改架构宣言）。
14. virtio 中断驱动 + 多请求在飞 + 真实 block cache。

---

## 11. 自身根本特色的建议（Symbian 特色之外）

现有代码里已有三个可以**放大成本系统 identity**的种子：

1. **"撤销即回收"（Revocation as Reclamation）**：能力系统已经做到 kill→revoke→级联释放 IPC 端点。把它推广成全局不变式：*任何资源（文件句柄、内存对象、socket、DMA 缓冲）的生命周期都严格等于某个能力的生命周期，revoke 一个能力，内核同步回收底层资源*。当前违背它的两处（`ServerFile` 无 owner、wire handle 无 generation）恰好就是修复方向——修好后这将成为与 QNX/Fuchsia 都不同的可验证承诺，并且是确定性的（无 GC、无泄漏）。
2. **帧捐赠消息（Frame-Grant Messaging）**：把 §8.3 的大数据 IPC 设计为"物理帧所有权转移"——消息携带的不是指针而是帧能力，转移 = 发送方表内销毁 + 接收方表内铸造 + 双方页表原子翻转。零拷贝、无锁、与现有 prepare/commit/abort 同构，是微内核里少见的把"capability = memory"做到底的路线（seL4 有 frame caps，但"消息即帧转移"的语义化封装可以做成 NovaOS 的标签）。
3. **确定性边界（Deterministic Envelope）**：全静态池 + 轮询 + 100Hz 的现状虽然是短板，但反过来是"上界可证明"的底子。与其照抄 Linux 的动态一切，不如把"每个 syscall 的最坏执行时间上界"作为设计目标：tickless APIC timer、无堆分配路径的服务、有界自旋全部换成中断驱动——最终宣传点可以是"每条系统调用的延迟上界写在文档里"。这要求 §10 的 P0/P1 完成后仍守住静态上界纪律（上限变大、但仍然有界）。

---

## 12. 结语

按子系统计，**能力/IPC/策略层是真实资产**（宿主可测、原子转移、撤销级联），**内存/SMP/中断/驱动是四根必须重做的柱子**，文件系统和网络是"可用但脆弱"的原型。四根柱子的共同主题是同一件事：*这个内核的所有正确性论证都建立在"单 CPU + 中断屏蔽 + 一切有界"上，而现代硬件恰好逐条否定这三个前提*。修复顺序建议严格按 §10 P0 的 1→5 推进，因为它们互为前提（内存上限不解，堆无从谈起；ACPI 不解，SMP 无从谈起；per-CPU 不解，中断驱动无从谈起）。

**进度（2026-11-08）**：四根柱子中内存（P0-1）已完全重建，SMP（P0-3/4 的唤醒链）与巨页（P0-2 的引导侧/HHDM）已打通验证——三者互相咬合（ACPI 枚举喂给 SMP，巨页喂给引导，位图喂给一切），修复过程中发现"看似无关的 5 个测试失败"实为帧位图两处计数/扫描 bug。剩余关键路径：per-CPU 抽象（P0-4 收尾）→ 中断模型（P0-5）→ 驱动现代化（P1）。经验教训值得留下：**宿主可测的子系统和真机行为之间的鸿沟，只能靠"宿主复现→定位→内核修复"的循环来闭合**——本次帧位图 bug 在宿主上 20 分钟复现，在 QEMU 里消耗了整个调试会话。
