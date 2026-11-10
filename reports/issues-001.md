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
| 附带发现 | **已修复**（2026-11-08） | 中断屏蔽采样 bug：`Interrupts::mask` 先 `cli` 再读 RFLAGS，永远返回"先前未屏蔽"，每个 `MaskGuard` Drop 都无条件 `sti`——AP bring-up（全程应 IF=0）首次堆分配结束后中断被强行打开，武装好的 APIC timer 随即在调度器恢复序列中途触发 tick，把半途上下文毒化进线程记录，随后每次恢复都落到 noreturn 调用点的 int3 填充（#BP，ip 随构建漂移）。修复为先采样后 `cli`；§8.2 栈迁堆正是踩中此雷的催化剂 |
| §8.2 线程栈迁堆（第一项） | **已修复**（2026-11-08） | `StackBuffer`/`StackPool`/`STACKS` 静态池整体删除；`Thread.stack_slot` → `stack_base: u64`，创建时 `alloc(Layout(16 KiB,16))`（先占表槽再分配，防泄漏），回收时 `dealloc`；`stack_bounds`/`kernel_stack_top` 由 base 推算，栈数量上限随线程表存在。smp 测试 `created_threads_dispatch_across_online_cpus` 同步加固：入口由占位地址 0 改为 `ap_idle_entry` 安全 park 循环（原入口被 tick 派发后会跳 0 执行低内存垃圾），清理改为"循环重标 Exited + 各 CPU 自行 reap"。boot 套件 65 suites / 228 cases 全绿 ×2 次，`scripts/gates.sh` 五道门禁全过 |

**仍然未解**：§8.2 剩余静态池（能力表/IPC 通道/socket/mount 等迁堆）；§4.3 时间源；§5.1 ECAM；§6/§7 FS 与 TCP 项；驱动现代化（§5.3/§5.4）。〔§2.5 堆本体、§2.5 范围注中的 StackPool、§4.1、§4.2（virtio-net 路径）、§1.2（引导侧 + 内核侧）、§4.4 均已修复并验证（2026-11-08）〕

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

### 1.2 早期页表只支持 4K 页〔未实现〕**Medium** → **已修复并验证（2026-11-08，引导侧 + 内核侧）**

> **更新**：NovaBoot 的恒等映射已改为 PML4[0]→PDPT→512 个 PD、每 PD 2 MiB 叶子，盖满低 4 GiB（此前只盖 1 GiB，引导镜像在 ~2 GiB 时首次取指即三重故障）。
>
> **内核侧也已修复（2026-11-08）**：`walk_to_leaf`/`translate` 的层级-尺寸配对原本错位一级——PDPT 级 `PS` 位（1 GiB 巨叶）被当 2 MiB、PD 级 `PS` 位（2 MiB 巨叶）被当 4 KiB，与 `Leaf` 自身的文档矛盾。后果（潜在，因现有 MMIO 地址都恰好在 RAM 巨叶体之外）：`map()` 会把巨叶当 4K 条目**静默覆写**，废掉同叶其余 511（或 5 亿）页；`translate()` 对叶内偏移 ≥4K 的地址丢失高位偏移位。直接映射恰由这些巨叶构成，第一次对 RAM 覆盖范围内的别名调 `map_kernel_mmio` 就会损坏映射。顺带修了 `translate_user`（用户映射现全为 4K，但损坏/未来的巨叶不应被当页表走进去）。新增 `memory_map_sanity` 两用例（巨叶翻译带全偏移、重映射巨叶页幂等且叶仍盖邻页），**曾以旧配对运行确认失败**（测试真能咬住 bug）。门禁全绿，boot 套件 64 suites / 221 cases 全过。

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

### 2.5 没有内核堆分配器〔未实现〕**High** → **已修复并验证（2026-11-08）**

> **已修复**：新增 `kernel/src/mm/heap.rs`——首个 `GlobalAlloc` 实现。帧分配器供给整帧，堆按**地址序单向空闲链 + 首适配 + 相邻合并**做子分配：块头一个 word（size | USED 位，双 free 靠 USED 位拒绝，不信任调用方元数据）；free 块第二字存 next；`MIN_BLOCK=32`、块对齐 16；align>16 时过分配 + back-offset word 存于 payload 前 8 字节，`release` 读回。增长路径从帧分配器领帧直到某个连续 run 足够宽，**每个较短的 run 都入空闲链**（堆永不还帧，中途放弃的 run 就是泄漏；位图洞结束一个 run、开启下一个，`insert_free` 自带合并会把后来相邻的 run 接上）；碎片病态时以 `2×need` 帧为预算防止一次增长吃光帧图。中断纪律：堆操作先 `mask_preserving()` 再拿递归锁（递归 SpinLock 会把中断里同 CPU 加锁当合法重入）。配套 `kernel_heap` 套件 7 用例（roundtrip 模式保持、跨帧宽分配、2000 小对象、align=64、双 free 拒绝、相邻合并、stats 守恒）。**验证**：boot 65 suites / 228 cases 全绿，`scripts/gates.sh` 五道门禁全过（提交 `91081e1` + `cad264f`；期间发现并修复了 §4.4 中断恢复寄存器互换——堆自身簿记 trace 自洽，故障随布局漂移的根因在中断路径）。
>
> **范围与未竟**：`alloc` 已可用（测试 harness、`Box`/`Vec` 走堆）；§8.2 的第一项——线程栈——**已迁堆**（`StackPool` 删除，`Thread.stack_base` 指向堆块，随创建/回收进出；2026-11-08，见更新记录表）。能力表、IPC 通道等仍是编译期定长数组，硬上限（§9.4）对它们原样。下一步是逐项迁余下的静态池。

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

### 4.1 vector 0x20 双重占用 + 错误的 EOI 组合〔缺陷〕**High** → **已修复并验证（2026-11-08）**

> **已修复**：APIC timer 换独立 vector 0x30（`apic::TIMER_VECTOR`，pub）；`pit::init` 不再 unmask IRQ0，PIC 保持全 mask 纪律——按源分发 EOI 的问题因"每 vector 单一中断源"而在构造上消失。PIC 重映射范围 0x20..0x30 全部安装共享 `pic_irq_stub`/`nova_kernel_pic_irq_handler`：双 PIC EOI + `intr::SPURIOUS_IRQS` 计数 + `task::resume_from_block` 原样续行（被 mask 的 8259 仍可能产生 spurious 中断，旧行为是 panic stub，第一次就死机）。新增 `idt::gate_target()` 供测试读取门目标地址；新增 `interrupt_vectors` suite（3 用例：timer 门在 PIC 范围外且不与其重合、PIC 范围共享一个 handler、spurious 计数初始为零）。门禁全绿，boot 套件 64 suites / 218 cases 全过，`-smp 4` 三 AP 正常 tick。

- 8259 PIC 被重映射到 `0x20/0x28`（`arch/x86_64/intr.rs:26-28`），PIT 的 IRQ0 因此走 vector 0x20（`pit.rs:139` 还 `unmask_irq(0)`）；
- 同时本地 APIC 的 LVT timer 也被配置为 **同一个 vector 0x20**、periodic 模式、count 0x1000（`arch/x86_64/apic.rs:41,74-89`），且 BSP 一并初始化（`apic.rs:116-119`）；
- 定时器 handler `nova_kernel_timer_handler`（`arch/x86_64/idt.rs:1316-1349`）**无条件只发 APIC EOI**（idt.rs:1326 `super::apic::eoi()`），从不调用 `intr::send_eoi()`（PIC EOI）。

后果：中断源是 APIC timer 时一切正常；但 PIT 的第一次 IRQ0 一旦经 PIC 送进同一个 vector，PIC 的 In-Service 寄存器永远不会被清——该 IRQ 及更低优先级线从此"卡住"，而代码没有任何地方意识到这一点。当前碰巧靠 APIC timer 供 tick 才没出事，**这是靠巧合而不是靠设计正确的双中断源共用一 vector**。正确做法：PIC 完全 mask 掉（或直接不 unmask IRQ0），APIC timer 换一个独立 vector（如 0x32+），并在 handler 里按中断源分发 EOI。

### 4.2 除 tick 外没有任何中断处理能力〔未实现〕**High** → **已修复并验证（2026-11-08，virtio-net 路径）**

> **已修复**：virtio-net 经 **MSI-X** 走上中断驱动路径（免 IOAPIC/MADT——MSI 消息直写本地 APIC）。实现要点：
>
> - **向量分配**：设备占用 vector 0x31（`intr::DEVICE_VECTOR_BASE`）。PIC 重映射范围已全 mask、APIC timer 独占 0x30，MSI 消息自带向量号，因此"门上的 handler 即中断源"，EOI 在构造上无歧义。IDT 装 `virtio_irq_stub`（完整寄存器文件入栈）→ handler：APIC EOI + `net::note_device_interrupt()`（共享原子计数）+ `task::resume_from_block` 原样续行——调度器完全不感知设备中断，设备自身的状态锁留在本已持有它的代码路径里。
> - **MSI-X 启用**（`pci::msix_capability` + `Transport::enable_msix`）：从 config space 读能力头 → 探测表所在 BAR → **逐页 `map_kernel_mmio`**（表在 MMIO BAR，直接映射不覆盖——virtio-net 表在 bar1 @ 0x81081000，PCI hole）→ 写全部表项（addr=`0xFEE00000|dest<<12`、data=向量号、ctrl=0）→ 最后才置 enable 位，设备不可能经陈旧表项投递。
> - **两个寄存器级事实**（QEMU 源码核实）：modern virtio-pci 每队列初始 vector 为 `NO_VECTOR`，**必须**在队列选中期间经 common config 的 `queue_msix_vector`（offset 0x1A）分配向量，否则队列永不中断；notify 步长与 MSI-X 无关（恒为 4），现有 multiplier 逻辑不变。
> - **等待路径**：`await_completion` 改为等待计数——`POLL_BUDGET` 只剩"设备停止中断"的兜底上限；计数变化后收缩为 `IRQ_DRAIN_BUDGET=1_000_000` 的短排水窗，覆盖 used-ring 与标志的一次读竞争。无 MSI-X 能力的设备降级为原轮询等待（记 Warn，不吞错）。
> - **顺带修真 bug**：`gate_target()` 重组 handler 地址时高 dword 位移用了 48 而非 32（内核地址高 32 位全 1，必然截断）——新测试用例抓住。
>
> 新增 `interrupt_vectors` 第 4 用例：设备向量门已装、与 timer 门及 PIC 范围共享 handler 均不重合。门禁全绿，boot 套件 64 suites / 219 cases 全过，net_tcp_handshake 等网络套件全过。**范围说明**：本项的"修复"覆盖 virtio-net 一台设备的中断驱动路径（第一台 claimant）；virtio-blk 仍走轮询、tick 里的 PS/2 轮询仍在，I/O APIC/IRQ 路由表/下半部基础设施仍未建——这些留给后续接 NVMe/USB 时一并做。

- `idt.rs:652-658`：vector > 31 且无专用 handler 的中断一律 panic（"hardware interrupt with no handler"）。
- 没有 I/O APIC、没有 MSI/MSI-X、没有 IRQ 路由表；所有设备（virtio-blk、virtio-net、PS/2）都靠**在时钟 tick 里轮询**（idt.rs:1332-1343 甚至在 tick handler 里直接轮询 PS/2 控制器）。
- 影响：a) I/O 吞吐被 100 Hz tick 限死（virtio-net 的 `POLL_BUDGET=100_000_000` 次自旋，`drivers/virtio/net.rs:118`，一次收包可能自旋数秒）；b) 中断驱动的全部基础设施（下半部、软中断、tasklet、wakeup）为零，将来接 NVMe/USB 必须从零建。

### 4.3 时间子系统单薄〔部分实现：墙上时钟已就绪（2026-11-08）；HPET/TSC 仍未做〕**Medium**

- 只有 PIT（`arch/x86_64/pit.rs`）+ 被意外依赖的 APIC timer；无 HPET、无 TSC 校准（`msr.rs` 存在但没用它做时间源）。~~系统没有任何"绝对时间"概念~~ → **CMOS RTC 墙上时钟已实现**（提交本节所述 Phase E），文件系统时间戳（§6.2）的地基已具备。

> **已实现（墙上时钟，2026-11-08）**：`kernel/src/arch/x86_64/rtc.rs`（CMOS RTC 驱动）+ `kernel/src/time.rs`（`wall_clock_seconds() -> Option<u64>` 门面）+ 新 boot 套件 `wall_clock`（8 用例，boot **66 suites / 236 cases 全绿**）。要点：
> - **每次调用实读，不缓存快照**：读一次只做两打端口 I/O 周期，比带时间戳的文件操作便宜两个数量级；缓存的"开机时刻"会随 uptime 越来越陈旧，故不采用。`None`（读不稳/日期不合理）时调用方省略时间戳而非记录垃圾值。
> - **硬件细节全覆盖**：BCD/二进制（StatusB bit2）、12h/24h（StatusB bit1，PM 位在小时寄存器 raw bit7，12AM→0、12PM→12）、UIP 更新周期（StatusA bit7 等待 + 连续两次读一致才算成功，`UIP_BUDGET`/`READ_BUDGET` 有界，病态时钟返回 `None` 而非挂死）。
> - **世纪寄存器经 boot 协议传入**：FADT（"FACP"）offset 55 的 `RTC century` 由 nova-boot-menu 的 XSDT 遍历读出（与 MADT 双表并行扫描、签名+长度≥56+校验和三重验证），随 `BootProtocol` 末尾追加的 `acpi_century: u8` 字段携带；`PROTOCOL_MINOR` 0→1（字段逐字段读写 + 快照页清零，旧 menu 读到 0 = "未知世纪"，安全；版本门使旧内核干净拒绝新 menu）。无 FADT 时内核按 2000+year 推断。协议侧新增 `BadAcpiCentury` 校验（寄存器索引 > 0x7F 拒绝——port 0x70 的 bit7 是 NMI 控制位，不是地址位）。
> - **并发**：CMOS 端口对是全机状态，`sync::SpinLock` 串行化（irq-safe、按 CPU 槽位重入）；NMI 位在会话内保存、逐次索引写恢复。
> - **日历算术**：`days_from_civil` 用 March-based 的 Hinnant 公式（`153*m+2)/5`，闰日落在二月末不前移），已知值锚定 1970-01-01=0、2000-01-01=946684800、2024-02-29/03-01、2026-11-08=1794096000。
> - 仍未做：HPET、TSC 校准（高精度时间源）——tick 仍是 100 Hz PIT，I/O 吞吐瓶颈（§4.2）不变。


### 4.4 中断恢复路径 RSI/RDI 互换〔缺陷〕**High** → **已修复并验证（2026-11-08，随 §2.5 调试发现）**

> **已修复**：`task::resume_from_block` 的 pop 序把 RSI/RDI 弹反（`write_block` 也配套写反）。这对自洽，所以调度器切换路径无恙；但 timer/PIC/virtio 三个中断 stub 把 **stub 自己保存的寄存器区**（`UserRegisters` 布局，RDI 在低位，编译期断言钉死）直接交给 `resume_from_block` 恢复——**每次中断恢复都悄悄互换被打断代码的 RSI 和 RDI**。异常路径（`resume_from_frame`）序是对的，所以只有中断路径坏。修复：两侧统一到 `UserRegisters` 约定（`resume_from_block` 先弹 RDI、`write_block` 把 RDI 写在低位槽），中断 stub 不动。验证：内核堆调试中 verify 循环的 `#GP`（timer tick 落在两条指令之间，恢复后指针寄存器变成立即数 → 非规范地址）随修复消失，boot 228 cases 全绿（提交 `91081e1`）。

- 症状极具迷惑性：故障落在"不可能 fault"的指令上（合法 HHDM 地址上的 `cmp` 报 #GP、ip 落进 ASCII 字符串、dealloc 收到 ptr=8/align=-128 的垃圾参数），且随每次重建漂移；插桩改变时序会让故障"消失"。根因不在故障点，而在几百个周期前的一次中断恢复。
- 教训与既有文档呼应：`idt.rs` 对"pop 序差一个 word → 寄存器文件整体错位"的静默失败模式已有大段注释（`resume_from_frame` 的 120 vs 128），这次是同一类错误的另一变种——**成对寄存器的相对顺序**，两个消费者（字段访问 vs 弹栈恢复）对同一保存区各持一套约定，必须有一方统一到另一方的钉死布局。

---

## 5. 驱动管理

### 5.1 PCI 仅限传统端口枚举〔部分修复（2026-11-08）：ECAM/MMCONFIG 已实现；深层 PCIe 拓扑仍开放〕**Medium**

- ~~只有 `0xCF8/0xCFC` 端口式配置访问；无 PCIe ECAM/MMCONFIG~~ → **已实现并验证（2026-11-08）**：boot 协议 minor 1→2 新增 `ecam_base: u64` + `ecam_bus_end: u8`（零 = 无窗口，旧 menu 的零页快照天然兼容）；nova-boot-menu 在 XSDT 三表并行遍历（MADT+FADT+MCFG）中解析 MCFG——注意 MCFG 的条目不在 offset 36，PCI Firmware Spec §4.1.2 在公共头后放 8 字节保留区，条目在 44（base u64）/52（segment u16）/54-55（起止 bus），单条目表长 60，QEMU 的 `build_mcfg` 正是此布局；kernel `pci.rs` 用 atomics 持有窗口，`read_u32`/`write_u32` 覆盖到的总线走 ECAM（`bus<<20|slot<<15|func<<12|offset`），否则走端口；HHDM 只映射 RAM，窗口按总线懒映射进内核半区（每总线 1 MiB=256 页，`map_kernel_mmio`，与 virtio BAR 同一模式），映射失败回落端口路径；端口路径保留为 `read_u32_ports` 供双机制一致性测试。内核测试新套件 `pci_ecam`（3 用例：窗口存在性 + 同一寄存器双机制读一致 + 经窗口枚举找到块/网设备），boot **70 suites / 251 cases 全绿**。
- `MAX_BUSES=8`、`MAX_DEVICES=32`、多层级 PCIe 拓扑（bridge 后的二级总线在真实机器上如何跟随）**仍未做**——ECAM 地址编码本身支持到 bus 255，但枚举走仍是单根单层的跟随逻辑。
- 设备识别表只有 virtio legacy/modern 几个 ID（pci.rs:90-96）。

### 5.2 virtio 块设备：全轮询、单请求在飞〔缺陷〕**Medium**

- `drivers/virtio/blk.rs`：一次只允许一个 in-flight 请求，`POLL_BUDGET`（约 800 万次迭代）内自旋等待完成；block cache 只有 8 行。顺序读一块 4 KiB 没问题，任何稍大的 I/O 都会退化成不可接受的延迟。
- 无 virtio 事件索引/中断驱动路径；DMA 约束问题见 §2.6。

### 5.3 输入：PS/2 轮询，无 USB〔未实现〕**High**

- `drivers/input.rs:1-20`：PS/2 控制器 `0x60/0x64`，在 tick handler 里被轮询。真实现代电脑几乎全部是 USB 键鼠（很多主板 BIOS 里 PS/2 仿真默认关闭）——**在现代硬件上这个内核大概率没有键盘**。需要 xHCI + HID 报告解析，工作量大但绕不开。
- **评估（2026-11-08，未实现）**：q35 虚拟机里 PS/2 有效（QEMU 提供 i8042 仿真），所以内核测试拓扑不受影响；但真实硬件路线的前置依赖是 §5.1 的 PCI 基础设施（xHCI 是 PCIe 设备，BAR 映射/能力解析已就绪）加上 xHCI 调度器+环（ring）+ TRB 与 HID 报告描述符解析，是一整块独立工程，本轮不做。

### 5.4 存储：无 NVMe/AHCI/SATA/SD〔未实现〕**High**

- 唯一存储后端是 virtio-blk。真实机器的磁盘 99% 是 NVMe（PCIe）或 AHCI SATA。没有这些，"运行在现代硬件"只剩 U 盘 FDD 模式这类边缘情形。
- **评估（2026-11-08，未实现）**：NVMe 与 xHCI 同理——控制器发现/枚举的 PCI 部分（ECAM 配置读、BAR 探测、MSI-X 能力解析）Phase H 已铺好，剩余为 NVMe 队列模型（submission/completion pair + 64 字节命令）与 PRP 页表；AHCI 是另一条 MMIO+DMA 路径。均为独立大块，本轮仅评估不动工。

### 5.5 显示：仅 GOP framebuffer〔Low〕

- `drivers/display.rs` 依赖 boot protocol 的 framebuffer 信息；无 EDID/模式切换、无 GPU 加速。可接受，但合成器（`services/compositor.rs`，1222 行）在 100 Hz tick 驱动下的重绘节奏没有 vsync/脏矩形优化。

### 5.6 排版：唯一的"字体"是 8×16 位图字体〔已建立矢量排版子系统（2026-11-08）：`nova-type`〕**High**

- **原始状态**：`nova-ui` 内嵌 8×16 像素字体，UEFI 菜单 5×7 位图；无任何矢量字体、无可缩放渲染、无抗锯齿，更没有中文。像素级字体正是用户明令禁止的方向。
- **已实现（`crates/nova-type`，no_std、无 alloc、无 float、`forbid(unsafe)`）**：
  - `parser.rs`：sfnt/TrueType 表解析（head/maxp/cmap4+cmap12/hhea/hmtx/loca/glyf），全部经边界检查——截断与垃圾输入返回错误而非 panic；简单字形 + 复合字形（F2Dot14 缩放、点匹配参数置零、深度帽 4）；`cmap` 走 Unicode (3,1)/(3,10)/(0,*)。
  - `raster.rs`：26.6 定点扫描线光栅化器——轮廓缩放至 q6 子像素、二次贝塞尔偏差有界细分（de Casteljau 整数平分）、每像素 4×4 采样 + 非零环绕规则的排序交点游走，覆盖率 0..255 即抗锯齿；字形上限 64×64 px。
  - `cache.rs`：32 槽固定环形字形缓存，键 =（glyph, size）。
  - 宿主测试 15 项（`tests/render.rs`）针对提交进仓库的固定测试资产 `assets/fonts/iosevka-test.ttf`（2.6 KB）渲染验证：字形正确性（ASCII art 可视化比对）、'A' 左右对称性、基线语义、等宽 advance、确定性、截断安全、缓存语义。
- **字体资产**（`tools/fonts/build.py` 从上游 release 可复现，`assets/fonts/` 提交子集）：
  - `iosevka-console.ttf` 17.8 KB（be5invis/Iosevka：ASCII + 制表符 + 箭头）→ 内核控制台 + UEFI 菜单；
  - `sarasa-terminal.ttf` 1.03 MB（be5invis/Sarasa-Gothic Mono SC：ASCII + GB2312 一级汉字 3755 + CJK 标点）→ 终端字体（用户指定）；
  - `han-sans-gui.ttf` 1.03 MB（adobe-fonts/source-han-sans SC：经 cu2qu CFF→glyf 转换后子集化）→ GUI 应用（editor/file-manager）。
- **集成进度**：
  - **内核屏显控制台（2026-11-09）**：`kernel/src/drivers/screen.rs`——Iosevka 16px 矢量渲染、α 混合逐像素合成、`SpinLock` 重入安全、GlyphCache 32 槽；display 映射前日志进 8 KiB pending 缓冲、`on_display_ready()` 排水重放（启动日志开头而非尾部可见）；滚屏 + 制表符 + ASCII-only。`logger::log`/`panic_report` 与 WRITE syscall 双发（串口+屏显），用户态输出同样上屏。main.rs：`display::init` 前移到 banner 之前；新增 `BootMode::Tests=3` 分支（wait_for_aps + `run_all()`），desktop/safe 走 `switch_to_next_for_cpu(0)` 交权给 init——测试套件不再无条件随每次启动运行。QEMU 截图验证：矢量字、抗锯齿、回放、滚屏全部正确。
  - **UEFI 菜单矢量字 + 第三入口（2026-11-09）**：`crates/nova-boot-menu/src/font.rs` 重写为矢量 `Typeface`（嵌入 iosevka-console.ttf，经 nova-type）；`Screen::draw_text` 改为矢量混色（fg*α+bg*(1-α) 在 RGB 空间合成，bg 显式传入）；菜单加第三项 "Kernel Test Suite"（键 '3'）；读 ESP `\EFI\NovaOS\boot-mode` 标记文件（"desktop"/"safe"/"test"）直选并跳过交互；`boot_kernel` 尾部清屏画居中 "loading kernel..."。xtask 侧 `run::run_with_mode` 在镜像构建后 `mcopy` 注入 marker；`cargo xtask test` 走 Tests 模式（同一镜像、测试为目的地），`cargo xtask run` 保持 desktop。QEMU 验证：菜单三项矢量渲染正确、marker 直选 → 70 套件 251 用例全过、desktop 启动到 init 交权且不再跑测试套件。
  - **内核–用户态启动链路与多核故障路径修复（2026-11-09）**：
  - `kernel/src/sched/mod.rs` `fault_current_user_and_switch`：不再硬编码 CPU 0，改用 `crate::task::current_cpu()`，使非 BSP CPU 上的用户线程 fault 时被正确杀死（不再错杀 BSP 上的 init）；验证子进程在 cpu 1 上首跑 fault 被正确杀死（`kill cpu=1 slot=1`、`faultcpu=1`）。
  - `kernel/src/arch/x86_64/gdt.rs`：TSS 布局修复，删除 `reserved1`，RSP0@4 / IST1@36；多 AP 不再错位/共享内核栈。
  - `kernel/src/syscall/mod.rs`：删除 `return_to_user` 与过早的 `sti()`；Continue 路径统一 `save_current_syscall_context(registers)` + `resume_current_for_cpu(current_cpu())`，resume block 恢复全部 15 个 GPR。
  - `crates/nova-type/src/parser.rs`：**root cause，init 用户栈被写坏**。`Font::simple_outline` 在内核栈上放 ≈4.6 KiB scratch（`flags`/`xs`/`ys`），日志路径下 16 KiB 线程栈溢出，把相邻物理帧（init 用户栈顶页）写坏，导致 init `write_fmt` 的 `Arguments` 指针损坏、崩溃。scratch 迁移至 `Outline` 结构（宿主拥有），栈用量降至数百字节。
  - `kernel/src/arch/x86_64/smp.rs`：AP trampoline `or eax, 0x101` 仅置 SCE+LME，**未置 NXE(bit 10)**；AP 上用户 PTE 的 NX bit(bit 63) 被当保留位 → 每个用户数据页 RSVD page fault。改为 `or eax, 0x501`（SCE|LME|NXE）。
  - `kernel/src/mm/address_space.rs`：用户页暂不设 NX bit（QEMU TCG 页表 walk 对 AP 的 NX bit 当保留位处理的 workaround；真实硬件 EFER.NXE 已置，可恢复 `!protection.is_executable()`）。
  - 验证：QEMU 下 `init ready (desktop)`、`init: started file-server`、`file-server ready` 依次打出；用户态在非 BSP CPU 上可正常首跑；`./scripts/gates.sh` 全绿，`cargo xtask test --timeout 240` 70 suites / 251 cases 全 pass，`cargo fmt --all --check`、clippy `-D warnings`、bare-metal build 通过。
  - **SMP 用户态完整链路修复（2026-11-09）**：
    - **root cause**：每个 AP 带有独立的 GDT/TSS/IDT、独自的线程池与调度器，`create_thread` 第一轮即分发到 AP，但 AP 从未安装 SYSCALL MSRs（`IA32_STAR`/`IA32_LSTAR`/`IA32_FM_SFMASK` 在 AP 上重置值为 0）。`init`、`file-server` 等用户线程在 AP 上首跑时 `syscall` 的 RIP=0 → 立即 #PF（`ip=0x0 errcode=0x150`），线程静默消失，串口只见 AP 起了、没人跑。修复在 `arch::x86_64::init_cpu()` 内同步安装 per-CPU syscall MSRs（`install_msrs` 单点实现，`init_ap()` 供 AP 专用路径）。
    - **附带修复①（`IF=0` 下 `hlt` 永久睡眠）**：`switch_to_next_for_cpu()` 在 "no runnable thread" 的回退分支里直接 `hlt` 未 `sti`——x86-64 上 `IF=0` 时 CPU 不会再被 timer interrupt 唤醒，AP 上的 idle CPU 永远停住、自己的用户线程即使变可运行也没人投递。改为标准 `sti;hlt`（interrupt gate 自动清 IF、`iretq` 恢复，掩蔽契约不变），并保留原 `no_non_exited fallback` 作为防御性 requeue fallback（正常常规路径不会触发）。
    - **附带修复②（syscall context 读了错的 pool）**：`current_syscall_context()` 原来读 `thread_in_slot(slot)`（硬编码 CPU 0 pool），AP 上恢复的线程经 `syscall` 时 slot 名义是 AP 的，读出来的却是 BSP pool 里的（或 None）同名 slot；改成 `current_cpu()` + `thread_in_slot_for_cpu(cpu, slot)` 完成同一 CPU 选择。
    - **附带修复③（`create_thread` 轮转分发到在线 CPU，而不是一直 BSP）**：`task::create_thread` 中 fetch cursor + `is_online` 探测 + 回退 BSP 的循环。`tests::smp_cpu_state.rs` 的 `created_threads_dispatch_across_online_cpus` 事实上是 Round-Robin 分布断言。这是用户态能跨核跑真实负载的前提。
    - **result**：真正的 round-robin 分发。`create_thread` 不再钉在 BSP；`-smp 4` QEMU TCG 下 desktop 端到端：`init ready`、6 个服务全部启动完毕（`file-server ready`、`console-server ready`、`shell ready: help ls cat echo ps`、`nova:>` 提示符、`terminal ready`、`editor ready`、`file-manager ready`），桌面 UI 截图（QMP screendump）显示 terminal / file-manager / editor 窗口正常渲染、Save/Load 按钮存在。
- 用户态终端（Sarasa Gothic）与 GUI（han-sans）接入随后续阶段提交。
---

## 6. 文件系统与存储服务

（本节大量引用此前子代理的详尽发现，行号经复核。）

### 6.1 没有真正的 VFS〔未实现〕**High**

- `kernel/src/services/fs.rs`：FileServer 内部是**硬编码 mount 表**，`MAX_MOUNTS = 2`；路径→文件系统的分派不是可插拔接口。加第二个真实文件系统（FAT、ext2）必须改 FileServer 本体。

### 6.2 磁盘格式 NOVADSK 的结构性缺陷〔部分修复（2026-11-08）：句柄 generation + delete/rename 已实现；两步写不一致仍开放〕**High**

- ~~`crates/nova_fs/src/diskfs.rs:92`：`MAX_SLOTS = 8`~~ 槽位数仍为 8，固定 4 KiB 块不变；**delete / rename 已实现**（见下），目录层次 / 时间戳 / 权限 / 链接仍未做。
- 写路径（diskfs.rs:640-686）**两步不一致仍未修**：先写数据块、后改目录项长度，两步之间掉电仍可产生不一致；无日志、无校验和（delete/rename 的单扇区目录重写是其上唯一的原子性来源，见下）。
- ~~wire handle 无 generation~~ → **已修复并验证（2026-11-08）**：`FileServer` 现为每个打开文件表槽位维护 `generations: [u16; MAX_SERVER_FILES]`，磁盘文件的 wire handle 改为 `DISK_WIRE_BIT | (generation << 32) | slot`（RAM 句柄本就携带 RamFs 自己的 generation，无需改）。close/回收时槽位 generation 递增，陈旧句柄经 wire 等值查找失配 → `BadHandle`，而不是静默指向槽位的新租户（ABA）。测试：`disk_delete_rename.a_stale_disk_handle_cannot_address_a_reused_slot`。

> **已实现（delete / rename，2026-11-08）**：`DiskFs::delete`/`DiskFs::rename`（nova-fs 层，8 个宿主测试）+ 内核 `FileServer::delete_scoped`/`rename_scoped`（scope 检查：两个名字都要 WRITE 授权）+ 协议新操作码 `FILE_OP_DELETE=8`/`FILE_OP_RENAME=9`（`FILE_VERSION` 1.0→1.1，negotiate 接受旧 minor，旧客户端不受影响；新增 `FileDeleteRequest`/`FileRenameRequest` wire 布局）+ 客户端 `fsclient::delete`/`fsclient::rename`。原子性设计：delete 置零槽位（名字+长度）后**单扇区**目录重写——崩溃前旧目录仍自洽，删除要么整体发生要么不发生；rename 只改一个目录项的名字字段，目标名校验与冲突检查先于任何改动，槽位（因此数据区）不动——打开句柄跨 rename 依然有效（UNIX 语义），被删除文件句柄下一次使用即 `BadHandle`。内核测试 `disk_delete_rename`（3 用例，boot **68 suites / 241 cases 全绿**）注册在 M5 组**之后**运行：磁盘映像整个 boot 共享，delete/rename 是真实破坏，不能触及仍依赖种子文件的 M5 测试。局限：RAM 文件系统仍无 delete/rename（返回 `Unsupported` 而非半支持）；跨挂载点 rename 拒绝。

### 6.3 客户端死亡不回收打开文件〔缺陷〕**High** → **已修复并验证（2026-11-08）**

> **已修复**：`ServerFile` 增加 `owner: usize`（打开者的任务槽位）；`FileServer::handle_message` 增加 `task_slot` 参数（dispatch 与 net 服务同款模式传入 `context.task_slot`），协议 open 路径经新的 `open_for_task` 记录 owner；服务器自用的 open（truncate 回退路径）记为哨兵 `SERVER_OWNED`，任何真实槽位都不会匹配。`kill_task → files.close_owner(slot)` 在能力撤销级联之后执行：**先回收槽位、后释放底层文件**（flush 失败不会把死客户端的文件困在表里，因为客户端已不存在、无从被告知）。测试：`file_server_reclaims_dead_clients`（2 用例）——死亡即 `open_count()→0`；先灌满 32 个槽再 kill，继承任务能重新打开满额文件，证明回收的是槽位本身而非仅句柄不可用。

- 原缺陷：`services/fs.rs:296-305` 的 `ServerFile` 没有 owner/task 字段，内核 kill 一个任务时（`kill_task → revoke_all` 能力级联是完备的）无法回收它打开的文件；32 个文件槽（`MAX_OPEN_FILES`）可被恶意/失败客户端耗尽，构成拒绝服务——"能力撤销即资源回收"这条招牌特色的直接违背。现与 compositor 的 `remove_owner` 同构：能力撤销、文件回收、通道唤醒在同一个 `kill_task` 里完成。

### 6.4 RamFS 尺寸过小〔Low〕

- 总量 16 KiB、单文件 1 KiB；initramfs 容量 512 KiB（已用 ~270 KiB）。作为 `/tmp` 与 IPC 大 payload 的载体都嫌小。

### 6.5 用户态 file-server / console-server 是 stub〔设计张力〕**Medium**

- 内核内有**真实的** FileServer/Console/Net/Compositor 服务（`kernel/src/services/*.rs`），而 initramfs 里对应的用户态服务程序是壳。Symbian 风格宣称 client/server 微内核，实际服务在内核地址空间内运行——权限检查虽经能力系统，但"服务崩溃可隔离"这个微内核核心卖点目前不存在（服务 panic = 内核 panic）。这是架构宣言与实现的根本张力，需要要么把服务真正搬出 ring 0，要么明确改口"受保护模块"混合内核。

---

## 7. 网络栈

### 7.1 分层结构（正面记录）

- `crates/nova-net`（1280 行）是纯函数式包构造/解析库：Ethernet/ARP/IPv4/UDP/TCP/DNS/ICMP/DHCP，全部可宿主测试——这是整个网络栈最有价值的部分。
- `kernel/src/services/net.rs`（795 行）是服务层：`MAX_SOCKETS = 8`（net.rs:62）、同步 request/replay 语义、socket 按 `task_slot` 归属。

### 7.2 TCP 实现是"教科书第 1 章"〔部分修复（2026-11-08）：随机 ISN + listen/accept 已实现；重传/乱序/窗口仍开放〕**High**

- ~~`TCP_INITIAL_SEQUENCE = 0x1234_5678` 常量 ISN~~ → **已修复并验证（2026-11-08）**：`VirtioNet::initial_sequence()` 每连接现抽 ISN——CPUID leaf1 ECX bit30 支持 RDRAND 时直接取硬件随机数；否则 SplitMix64 finalizer 混合三个无硬件 RNG 机器也有的熵源（本机 MAC 两字节 + §4.3 墙钟秒数 + `ISN_COUNTER` AtomicU32 调用计数，保证同一秒内两次连接不撞号），结果恒非零（0 → 退回常量，常量保留为 fallback 并在文档中标注"已退役"）。测试 `net_listen_accept.initial_sequences_differ_and_are_never_zero` 对活设备验证非零 + 相邻两次不同。
- ~~只有主动 `connect`，没有 `listen`/`accept`~~ → **已实现（2026-11-08）**：端到端三层——驱动层 `VirtioNet::accept_tcp(port)` 被动握手（SYN→SYN-ACK→ACK 两阶段轮询，客户端 MAC/IP 直接取自入帧、不走 ARP），`drivers/net.rs` 包装；协议层 `NET_OP_SOCKET_LISTEN=12`/`NET_OP_SOCKET_ACCEPT=13`（`NET_VERSION` 1.0→1.1，negotiate 接受旧 minor；`NetSocketListenRequest/Reply`、`NetSocketAcceptRequest/Reply` wire 布局 + offset 断言）；服务层 `SocketSlot.listening` 标记 + `handle_socket_listen`/`handle_socket_accept`（UDP listen→`Unsupported`；未 listen 就 accept→`Denied`；connected socket 再 listen/accept→`Busy`；跨 task→`BadHandle`；accept 成功后 socket 停止 listening 变为连接——一 accept 一连接，与驱动栈的"第 1 章"预算一致）。内核测试新套件 `net_listen_accept`（7 用例，boot **69 suites / 248 cases 全绿**）。
- **局限（仍未做）**：accept 的完整三次握手无法在 QEMU 测试拓扑里端到端验证——测试网络只有 `guestfwd` 转发的出站对端（`TCP_TEST_SERVER`），没有向 guest 注入入站 SYN 的手段；测试覆盖为服务层校验路径 + ISN 属性，被动握手的真实往返留待有 hostfwd/hook 的拓扑。重传、乱序、重复 ACK、窗口缩放、拥塞控制、Nagle、keepalive 全部仍未做：丢一个包 = 连接卡死到 `POLL_BUDGET` 耗尽，这一条没有变。

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
| 内核栈 | 16 KiB × 8 | `task/mod.rs:88`（**已动态化（2026-11-08）**：栈改内核堆分配，随线程表存在；表中其余静态池未动） |
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

## 13. 用户系统 / Safe Terminal / 桌面 — 当前状态（2026-11-09）

### 13.1 Safe Terminal（boot mode 2）〔部分修复（2026-11-09）〕**Medium**

- **现状**：从 Desktop 菜单或 ESP marker（`desktop` / `safe` / `test`）选择 Safe Terminal 时，内核先把屏幕清成黑底，静默所有 kernel log（dispatch `drivers::screen::set_quiet(true)`），仅用户态输出走屏幕——显示的就是 NovaOS ASCII logo + `novaos login:` 提示符。ASCII 以矢量字体在 Iosevka 中 antialias 写黑屏上，**像素字体已完全移除**（§5.6）。
- **登录/账户**：shell 启动后要求输入用户名与密码（不回显）。当前签名了两个账户：
  - `root` / 密码 `root`（uid 0、gid 0）
  - `zhang` / 密码 `zhang`（uid 1000、gid 1000）
  密码使用 FNV-1a/64 哈希校验（不是加密强度哈希，但对 VNC 会话的威胁模型够用）。`crates/nova-user` 是新 crate，host 可测。
- **文件系统权限**：`nova-fs` 的每个 inode 增加了 `owner`/`group`/`mode`（9 位 `rwx`），owner 位默认 `0`，dir 0o755、file 0o644；`stat` 输出 `rwx` 串；`ls -l` 显示 owner/group/size + 权限字串。`chmod`/`chown` 命令通过 shell 路由：`chmod` 要 root 或文件 owner，`chown` 要 root。
- **交互**：cwd 追踪到 `user@novaos:<path>` 格式（2026-11-09 起含冒号，`root@novaos:/docs>`），`cd`、`pwd`、`ls`、`ls -l`、`cat`、`echo`、`ps`、`whoami`、`id`、`su` 全部实现并实测通过。
- **Safe Terminal 实测（QEMU + vncdotool 注入键盘事件）**：以 `root` 身份登录、运行 `cd /docs`、`pwd`、`cat readme.txt`、`cd ..`、`pwd`、`chmod 600 /hello.txt`、`chown 1000 /docs/readme.txt`、`ls -l /`、`ls -l /docs`、`su zhang`、切回密码 `zhang` 登录并执行命令，全部正常工作（密码不回显、权限检查正确、`id`、`whoami` 依次输出 root@uid=0 zhang@uid=1000）。
- **当前不足**：PS/2 键盘映射登记在 `kernel/src/drivers/input.rs`（framebuffer ASCII 渲染路径缺失），目前只在 kernel console 中为 `read()` 写入缓冲；Safe Terminal 的输入走键盘映射到 stdin ring `kernel/src/stdin.rs`，支持 US 键盘布局（shift 跟踪）、退格、回车、Tab。中文输入未做。
### 13.2 桌面、鼠标指针、nova-graphic（2026-11-09）〔进度中〕

- **PS/2 鼠标**：`drivers/input.rs` 同时识别 0x60 端口的 KB/MS 数据字节（状态端口 bit 5 区分），汇编 3 字节 mouse packet（flags/dx/dy），用 `i8 as i16` 符号扩展。绝对坐标保存在静态 `MOUSE_POS`，以 `(0,0)` 初始、每次 poll 更新基于鼠标增量（dy 在屏幕坐标系为反向）。
- **鼠标指针**：`services/compositor.rs::present()` 末端画一个简单箭头（黑色 + 白色填充）。每帧呈现后光标出现在桌面上。
- **nova-graphic crate**（`crates/nova-graphic`）：以 Qt6 为参照的 widget 库：
  - `Widget` trait：`rect()` / `set_rect()` / `draw(&Canvas)` / `on_event(&Event) -> bool`；rect 是公共 `Rect{x,y,w,h}`，所有 widget 是 value、可复制、不需要 alloc；
  - 当前实现：`QPushButton`、`QCheckBox`、`QRadioButton`、`QLineEdit`（含 Normal/Password echo）、`QSlider`、`QProgressBar`、`QListWidget`、`QLabel`、`VBox`（占位容器）、`Colour`、`Event`/`MouseButton`/`MouseState`。
  - 每个 widget 都是 host 可测的（`nova-graphic` 在 HOST_PACKAGES 中）。
- **当前不足**：`nova-graphic` 仅提供原语；没有把它接到 `init` 的桌面启动序列——也就是说用户不会看到 QPushButton/QCheckBox 渲染出来的窗口。widget 的键盘处理没接入，只处理鼠标点击；窗口的标题/关闭按钮没有做。VNC 注入验证了鼠标指针在 Desktop 模式确实出现。
### 13.3 Safe Terminal / 桌面交互修复与 widgets-demo 验收（2026-11-09）〔已修复〕**Medium**

- **桌面黑屏死锁（已 root-cause 修复）**：Qt/GUI 代码合入后桌面 boot 静默停在 `sched: cpu 0 idle, no runnable thread`，串口无任何错误。根因：initramfs 从 8 个 ELF 增至 9 个（新增 widgets-demo），而内核 `initramfs::Archive` 的 `MAX_FILES` 硬上限为 8 → `Archive::parse` 返回 `Err("initramfs-file-quota")`，而 `main.rs` 把该错误吞掉，init 根本未被启动 → 零可运行线程 → 永久 idle 黑屏。**修复**：`MAX_FILES` 提升至 16；`main.rs` 在 parse 失败时打印 `initramfs: parse failed: {reason}`（与 spawn 失败同级的 ERROR，不再静默）。
- **spawn 配额**：widgets-demo 作为第 8 个服务启动时撞上 `services::MAX_TASKS = 8`（capability table 64 KiB/个 + thread pool `MAX_THREADS = 8`）→ `process: no task slot`。**修复**：`MAX_TASKS` 8→12、`MAX_THREADS` 8→12（每 CPU 12 个线程槽）。
- **PS/2 鼠标从未上报**：init 只发了控制器级 `0xA8`（aux enable），**没有**给鼠标发 `0xD4 + 0xF4`（data reporting enable + ACK）→ 鼠标一辈子不发 packet（键盘正常）。另：鼠标 ACK 字节 `0xFA` 被当成 packet 第 0 字节 → 三字节 packet 框架永久错位。**修复**：init 序列补 `0xD4/0xF4`；poll 路径在 packet index=0 时丢弃裸 `0xFA`（本系统唯一发送的 aux 命令是 `0xF4`，0xFA 永远是它的 ACK）；packet 组装后**真正调用 `mouse_move()`**（之前重构时丢失 → MOUSE_POS 永远为初始值 (512,384)）。
- **指针不重绘**：纯鼠标移动不 dirty 任何 surface → `present()` 早退 → 箭头永远停在出厂坐标。**修复**：CompositorService 增加 `pointer_moved` 标记（`note_pointer_moved()`，input 路径在 packet 完成时置位），`frame_pending()` 纳入该标志 → 下一帧**强制 full repaint**（背景+全部 surface+箭头）→ 箭头位置实时更新、无残影。
- **VNC 鼠标绝对坐标不可达**：QEMU VNC 送绝对指针事件，PS/2 鼠标仅能收相对增量 → `vncdotool move` 对 guest 无效（光标原地不动，这是此前"鼠标可活动"结论的真实来源：从未真正移动，只是静态可见）。正确做法：QMP `input-send-event` 以 `rel` 轴发送相对增量；点击 = `btn left` 按下/释放。验证脚本据此重写，并对所有交互做了逐步截图比对。
- **nova-graphic 修复**：
  - `QSlider`：原式 `(rel * 100).min(w) / w` 先把 `rel*100` 钳到 `w` 再除 → 任何点击只能得到 `1%`。改为宽端 `u64` 乘法后除、钳到 100。
  - `VBox`：从占位静态数组 `[Option<&'static mut dyn Widget>; 8]` 重写为**真实垂直布局引擎**（`layout(rect, heights, out)`、`child_rect`、`natural_height`），host 可测 + 5 个单元测试（进入 HOST_PACKAGES）。
  - `QLineEdit`：新增 `draw_with_text()`（caller 持有 buffer，widget 不持有字符串）；Password echo 用 `*` 逐字符掩码（依赖 face 里有 `*`，无需 `•`）；caret 跟随可见文本宽度。
  - `QCheckBox`：原 `✓`（U+2713）不在嵌入字体子集内 → 勾选后无任何视觉反馈。改为**矢量折线打勾**（8 个 1-px cell 的折线）。
  - `QRadioButton` 组合互斥修复：原 `enforce_radio_exclusivity` 永远优先 light → 点击 Dark 后 dark 立刻被清零、永远切换不了主题。改为**上升沿检测**（哪个按钮刚被按下就清掉另一个），符合 Qt 互斥语义；键盘路径同步修复。
- **widgets-demo**（`user/apps/src/bin/widgets-demo.rs`）：桌面新增 Qt6 风格 showcase 窗口，**8 个控件全部实装且实测可用**（VNC/QMP 注入事件逐项验收）：名字 `QLineEdit`（字符输入+密码掩码/明文切换+caret）、`QCheckBox`（Show password as text）、`QRadioButton` 组（Light/Dark theme，互斥，整窗换肤）、`QSlider`（Volume，拖动/点击置位）、`QProgressBar`（跟随 slider 填充）、`QListWidget`（Apple/Banana/Cherry/Durian，点选高亮）、`QPushButton` OK/Cancel（OK 累计 submits 并刷新状态栏）、`QLabel` 标题、`VBox` 布局全部行（row）按布局引擎定位。状态栏实时显示 `volume=…% item=… light|dark submits=…`。焦点：点击任意控件即聚焦（Tab 循环焦点，LineEdit 中 Enter 提交、Backspace 删除）。
- **验收（1024×768 QEMU，-smp 4）**：窗口启动后位于整窗最前；`"nova"` 输入进 QLineEdit（密码掩码 `****`），点 checkbox 切换为明文可见；点 Dark theme 整窗换深色并互斥取消 Light；list 点 Cherry → `item=Cherry`；slider 点 86% 处 → `volume=86% ` + 进度条同步；OK → `submits=1`；全程鼠标箭头位置实时跟随 rel 事件且不留残影。
- **Safe Terminal 回归**：同一镜像 safe-terminal 模式正常（ASCII logo、login、root@novaos:/> 提示符、pwd）。
- **Gates**：`fmt`、`clippy --workspace --exclude nova-kernel nova-rt nova-apps nova-boot-menu --all-targets -D warnings`、host tests（70 suits / 251 cases）、`nova-graphic` host 单元测试、`cargo xtask test --timeout 240`、裸机构建 —— 全部绿。

---

---
### 13.4 Safe Terminal 输入与真实 VFS（2026-11-09）〔已修复〕**Medium**

- **退格不擦除**：`drivers::screen::feed` 原对 0x08 一律静默丢弃 → 光标退回、单元格残留。修复：kernel console 现在对 `0x08` 执行「退一格 + 该单元格用背景色整体重绘」（等价终端的 `BS` = 退格 + 清格），Iosevka 的 cell 宽度由 `cell_advance()` 现算。shell 的 `\x08 \x08` 序列现在干净擦除。**实测**：敲 abcde、退格 3 次，屏幕只剩 `ab`，光标在正确位置。
- **clear 后提示符在底部**：原 `clear` 打印 40 个换行，只滚屏不回顶。新增 syscall **`CONSOLE_CLEAR`（ABI 0.3，syscall 25）**：清整个 console frame 为背景色并把 pen 重置到第一个 cell（即真正的 `clear(1)`）。Shell 的 `clear` 命令改调 `nova_apps::syscall::console_clear()`。**实测**：`clear` 后 `root@novaos:/>` 出现在屏幕左上角。
- **文件系统是假的（私有 RamFs，`cd /` 只看到 hello.txt/docs）**：shell 此前用**自己进程内的私有 RamFs**（boot seed 了 4 个假文件）。修复：shell 的文件操作全部走 **`fsclient` → 内核 file service（SERVICE_FILE）→ 内核 RamFs/挂载磁盘**：
  - `ls`/`ls -l`/`cat`/`cd`/`pwd` 全部经 `fsclient::list_at/Records`、`stat`、`open/read`；
  - `ls -l` 显示 `rwx` 串、owner/gid/size（stat 回复由 24B 扩到 40B，新增 owner/gid/mode 三元组；FILE_VERSION 1.2；协议布局 pin 更新）；
  - 启动时内核往 RamFs 播种 **真实系统树**（`/etc/hostname`、`/system/version`、`/system/runtime/{cpu-count,memory,abi}`、`/EFI/NovaOS/{boot-mode,initramfs.list}`（启动时真实 initramfs 清单）、`/home/nova/notes.txt`）——内容全部来自 boot 时内核真实观测，无硬编码内容假文件；
  - `/media/test`、`/media/ro` 是真实 virtio-blk 挂载（DiskFs，`/media/test/hello.txt` 内容可达）；
  - **新增 FILE_OP_CHMOD(10) / FILE_OP_CHOWN(11)**：mode 9 位、owner/gid；服务端校验 scope WRITE，RamFs::chmod/chown 再按 owner-or-root / root-only 校验（shell 层再按会话 uid 预检：root 可改任何文件；zhang 只能改自己是 owner 的文件；chown 仅 root）。
  - 真实效果：`cd /` 看到 `system/ devices/ proc/ tmp/ var/ home/ media/ dev/ etc/ EFI/`；`cat /system/version` → `NovaOS 0.1.0 (x86_64)`；`cat /EFI/NovaOS/initramfs.list` → 启动镜像真实清单（init/file-server/…/widgets-demo）；`cat /etc/hostname` → `novaos`。
- **fsclient::read 永远失败（根因）**：`read()` 对 `out.len() > 48`（READ_CHUNK 协议上限）直接 `E_INVALID`，shell 传 1024B buffer 必挂——**文件服务的读路径从未真正被验证过**（M6 测试只走 write 路径和小文件）。修复：read 按 `READ_CHUNK` 裁剪，调用方循环读取。顺带修复 shell `cat` 的报错：现在打印真实 `error.name()`（`cat: E_INVALID` 风格）。
- **init 授权**：shell（desktop 与 safe-terminal 两种模式）、terminal 现在拿到 `/` 的 `READ|WRITE` grant（此前 shell 只有自己的假 RamFs）。
- **验收（QEMU, safe-terminal）**：`ls /`（真系统树）、`ls /system`、`ls -l /etc`（`-rw-r--r-- 0 0 7 hostname`）、`chmod 600 /etc/hostname` → `ls -l` 显示 `-rw-------`、`su zhang` → `chmod` → `operation not permitted`（zhang 不是 owner）、`ls -l /media/test`（`-rw-r--r-- 0 0 32 hello.txt`）、`cat /media/test/hello.txt` → `hello from the NovaOS test disk`、`whoami`/`id` 全部正确。
- **Gates**：fmt、clippy `-D warnings`、host 70/251、`cargo xtask test --timeout 240`、裸机构建——全绿。
### 13.5 nova-desktop-server + 差分合成（消除鼠标闪烁）（2026-11-09）〔已修复〕**Medium**

- **鼠标移动整屏闪烁**：上一轮的临时修复（鼠标包 → `pointer_moved → full repaint`）让每次 pass 全屏重绘，视觉上持续 flicker。根因：compositor 的 `present()` 只在有 damage 时工作，鼠标箭头没有 damage 通道，于是每帧整屏重画。
- **修复（Wayland 式 damage 模型）**：新增 crate **`nova-desktop-server`**（no_std，host 可测，20 个单元测试）——`DamageAccumulator` 按 surface 累积 damage 并合并重叠/相邻矩形、折叠超限矩形为边界盒、output 裁剪；`DesktopServer` 作为 policy 参考实现（z-order、hit_test、seat、click→focus+raise、frame 决策 `Frame::Nothing | Regions | Full`），`Seat` 做指针/键盘焦点路由；`protocol` 模块定义 Wayland 风格的 wire 消息（CreateSurface/Attach/Damage/Commit/Place/Raise/DestroySurface + FrameDone/Pointer*/KeyEvent 事件，opcode 稳定、布局 pin 测试）。
- **内核 compositor 改用差分路径**：`present()` 现在计算本帧 paint plan——任何 surface `full` damage → 全屏重绘（保留 attach/move/resize 的正确语义）；否则 **区域重绘**：把所有 damaged 输出区域 + 旧箭头 cell + 新箭头 cell 合成为一个 tight 区域 → `fill_rect` 背景 → z 序按区域裁剪 blit 各 surface（新增 `display::blit_window` 支持源偏移的 sub-rect 拷贝）→ 最后画箭头。指针移动**零整屏重绘**。
- **验收**：1024×768 QEMU 下鼠标做 8 步小幅扫动并前后截图，差异区域恰为 `(512,384)-(551,444)`——仅箭头的新旧位置，窗口内容像素级不变；`widgets-demo` 窗口不再闪烁、文件管理器列表完整可读。
- **额外修复**：`blit_window` 长度校验（`stride*(sy+h)`，曾多出一行导致底边相交时 `E_INVALID` 让多个 app 报 `present failed E_INVALID`——已消除，串口干净）。

### 13.7 NovaOS EDE Desktop 移植（2026-11-09）〔已完成〕**Medium**

- **crates 结构**（一个组件一个 crate，严格 Rust):`nova-edelib`(edelib 的 Rust 移植：`EdeLog` 统一日志、`EdeConfig` INI 配置、`DesktopFile` 解析免费桌面 `.desktop`、`EdeApplication` 应用基座）、`nova-ede-desktop`（桌面图标层模型：解析 .desktop → 图标、命中、选择/取消、`DESKTOP_DIR`)、`nova-ede-panel`（面板 + `start-menu`/`quick-launch`/`taskbar`/`systray` 四个 applet、命中测试、级联放置）、`nova-ede-launch`（启动器模型：过滤、选择环绕、大小写不敏感匹配）、`nova-ede-tip`（悬浮提示 + 延迟）、`nova-ede-notifyd`（通知 daemon：有界队列、按 tick 过期、`app|summary|body` 线格式）、`nova-ede-about`（About 对话框内容）。桌面渲染沿用 NovaOS 的矢量字体/无贴图管线，**零像素图**。
- **桌面 shell 重写**（`user/apps/src/bin/desktop-shell.rs`):`DesktopLayer`(.desktop 驱动图标列，含选择高亮）、`Panel`（NovaOS 彗星 emblem Start 按钮、快捷启动区、任务栏芯片、右侧 EDE 标记）、Start 菜单（comet 头部 + "NovaOS EDE" + `EDE, reborn in Rust` 标语 + 应用列表 + GPL 页脚）、通知气泡、About 对话框；窗口改为从图标列右侧开始（内核级联原点默认 48——m6.rs 测试钉住该值；桌面启动模式由 main.rs 以 `set_spawn_origin_x(176)` 右移，**不再覆盖图标**，测试模式与 safe terminal 保持 48)；桌面 shell 获得 `/` 的 `READ` scope(.desktop 文件读取修复）；根 filesystem 在启动时播种 4 个 `terminal/files/editor/widgets.desktop` 与 `.ede/panel.conf` / `.ede/desktop.conf`(`/home/nova/Desktop`)。
- **修复的真实 bug**：`.desktop` 文件解析缓冲用 48 字节页大小截断（`Exec=terminal` 被裁成 `E`)——图标列空白；改为 1024 字节缓冲后解析成功（`scanned 4 icons`)。桌面 shell 无 scope grant 导致 `list_at` 全失败 → 赋予 READ。窗口级联原点导致图标不可点/不可见 → 迁移到 x=176。
- **启动菜单**："Desktop (EDE)"（`nova-boot-menu`,Boot0001 标签）。
- **验收（QEMU,VNC/QMP)**:`/` 菜单 EDE、桌面图标列（Terminal/Files/Editor/Widgets)、Start 菜单打开/关闭（comet 头部+NovaOS EDE+应用项+About 入口）、面板（彗星 Start + Nova 字标 + 三栏快捷启动 + 四芯片任务栏 + 右侧 EDE 徽章）、图标单击选择/再次点击启动（→ 新 terminal 窗口）、图标列不被窗口覆盖、`-smp 1/4` 均启动、safe-terminal 回归正常。
- **Gates**:fmt、clippy `-D warnings`、host 测试（含新 crate 的 17 个单元测试）、`cargo xtask test --timeout 240`、裸机构建 —— 全绿。

### 13.8 NovaOS EDE 补全移植：24 crate、审计缺陷修复（2026-11-10）〔已完成〕**Medium**

- **背景**：§13.7 的首轮移植经审计判定不完整（组件缺项 + 若干运行期缺陷）。本轮按上游 EDE 2.2.0 的真实组件清单补齐并逐个强化，最终 **24 个 crate**（`nova-edelib` + 23 个 EDE 组件），**229 个 host 单元测试**，全部 `no_std`、`forbid(unsafe_code)`、host 可测。
- **强化既有 7 个 crate**：
  - `nova-edelib`：`DesktopFile` 补全 `GenericName/Path/URL/Categories/OnlyShowIn/NotShowIn/Terminal/StartupNotify/Hidden/NoDisplay`、组作用域解析（`[Desktop Action]` 不泄漏）、原始 `Exec` 与去 field-code 双轨（`exec_raw` vs `exec`）、`expand_exec` 完整 argv 展开（引号/转义/`%f %F %u %U %i %c %k %%`，`ExecArgs` 公开）；`EdeConfig` 补 `get_u32` 空值→None、溢出→None、`get_i32`（带符号）、`has_section`、`iter_keys`、`section_names`。
  - `nova-ede-desktop`：4 列网格、basename 去重 `add_named`（重扫保位保选中）、`refresh`（消失的图标移除）、shift 多选 `select_also`、框选 `select_band`、拖动 `move_to`、`auto_arrange`、`save_positions`/`load_position`（icons.conf 往返）。
  - `nova-ede-launch`：`PreferredKind` 四槽、`resolve()` mail-URL 路由（mail 先于 URL 判定）、`resolve_entry`（Path/Terminal/StartupNotify → `LaunchRequest`）、历史（去重、最新在前、上限 8）、`entry()`/`selected_index()` 供 Start 菜单行取用。
  - `nova-ede-notifyd`：按 spec 的 id、`replace()`（原地替换不增气泡）、urgency（Critical 永不过期）、timeout 0 = sticky（`u32::MAX`）、hover 暂停过期、`dismiss(id)`、堆叠布局、线格式 `app|summary|body|urgency|timeout`。
  - `nova-ede-tip`：保留 shell 在用的 `Tooltip`，新增真正的 TipsBook（fortune `%` 分隔解析、前后翻页环绕、覆盖末条的 LCG 随机、`autostart_desktop()` 写 `Hidden=true`）。
  - `nova-ede-panel`：`PanelSettings`（`panel.conf` 的 position/height/hider/applets）、动态 `TaskbarModel` 芯片（点激活/最小化按上游语义）、pinned 回退、hider 收纳（`Hit::Hider`/`Action::ShowPanel`）、`apply_settings` 消费高度与 applet 白名单（`height` 改为运行期字段，命中/绘制同步）。
  - `nova-ede-about`：credits 容量与构建信息修正。
- **新增 17 个组件 crate**（均带完整测试）：`nova-ede-session`（evoke：start_order 启动计划、Splash 步进、XSETTINGS 风格类型化 SettingsStore、logout/restart/shutdown 意图与能力检查）、`nova-ede-autostart`（XDG 扫描、basename 去重用户覆盖、Hidden/NoDisplay/OnlyShowIn 过滤、安全模式选择器、autorun 搜索序）、`nova-ede-bell`（0-100/100-1000/0-1000 边界、`Bell/*` 配置往返、`audible()` 如实报告静音）、`nova-ede-keyboard`（重复延时/速率范围、modifier 选项去重、越界回退默认）、`nova-ede-preferred`（四类首选应用、配置往返、`[Preferred]` 序列化）、`nova-ede-screensaver`（超时钳位、空闲状态机、锁屏联动）、`nova-ede-screen-lock`（密码状态机、失败计数、DELAY_TICKS 冻结、解锁后死亡）、`nova-ede-timedate`（civil↔Unix 时间戳往返、闰年、星期、时区/NTP 配置）、`nova-ede-calc`（四则+`C/CE`、tape、除零报 Error；**自带 no_std 数学**：参数归约的 sin/cos/tan 级数、exp 按 ln2 拆幂、ln 的 atanh 变换、Newton sqrt，全部以已知值钉住）、`nova-ede-image-view`（P1-P6 Netpbm 全解码含二进制 PBM 位展开、fit/zoom 离散步进、pan 边界）、`nova-ede-help`（标题目录、页面解析、`--titles`、浏览器候选）、`nova-ede-dialog`（kdialog 形状的 7 种对话框、退出码 0/1/2/254、按 kind 过滤答案）、`nova-ede-conf`（ede-conf.conf 项目表、坏项跳过、点击→Exec）、`nova-ede-desktop-conf`（Desktop/Icons 全键往返、越界忽略、wallpaper_mode 上游数值）、`nova-ede-tip-compiler`（StrFile v2 **大端**头+偏移表+字符串的编译与读取、ordered/rotated 标志、单调偏移校验、rot13 写读往返——上游 rot13 标志错位的 bug 在此修正）、`nova-ede-crash`（ede-crasher 命令行、标题/详情页组装、仅 EDE 应用带 Report 按钮）、`nova-ede-bug-report`（报告表单、校验、文本往返、FIFO 队列；**无网络、无内嵌凭据**——上游 Bugzilla 账号被有意丢弃）。
- **审计缺陷修复（desktop-shell 集成）**：
  - **指针按下/释放边沿**：shell 记录上一次 `buttons`，只在 up→down 沿上做选择/启动/关菜单/消气泡；hover 期间按住不触发任何启动。图标按压改为“潜在拖动”：落点在已选图标上按下→移动超 3px 变拖动，原地释放才启动。
  - **动态任务栏（原为假芯片）**：协议新增 4 个 opcode——`SURFACE_OP_LIST`（分页窗口清单：id/z/focused/minimized，64 字节载荷 4 条/页）、`SURFACE_OP_NAME`（读 owner task 的 initramfs 启动名作标题）、`SURFACE_OP_ACTIVATE`（抬升+聚焦+反最小化，EWMH `_NET_ACTIVE_WINDOW` 语义的任意客户端请求）、`SURFACE_OP_MINIMIZE`（最小化：不绘制、不命中、焦点移交 topmost）；内核 compositor 增加 `minimized` 状态并贯穿 present/draw_all/paint_region/pointer_at/topmost，新增 5 个单测；shell 每 32 tick 分页拉取并发布到 `Panel::set_windows`，面板按焦点高亮、点击发 `Action::Activate/Minimize`；自身 surface 不入列。
  - **通知去重**：删除失败的 `/var/run/notify/<file>` 以文件名（=应用身份）记住，下轮 drain 跳过投递只重试删除——消除“每 16 tick 重复同一条消息”的气泡刷屏。
  - **启动反馈**：`launch()` 不再吞掉 `process::exec` 的失败，空名拒绝、失败写串口 `no such service`。
  - **配置种子消费**：`panel.conf` → `PanelSettings::from_config` → `apply_settings`（height 16-64 钳位 + applet 白名单 gating start/quick/taskbar/tray 绘制与命中；空列表=全开）；`desktop.conf` → `DesktopConf`（`wallpaper_use`+`wallpaper` 选壁纸否则底色、`icons` 列表过滤桌面图标与 Start 菜单、`one_click_exec` 单击启动、`label_*` 等全键往返）；内核种子补 `wallpaper_use=true`。
  - **Start 菜单键盘**：`nova_ui::keyboard::Decoder` 接入——打字过滤（副标题实时显示过滤串）、Backspace、Up/Down 移动选择（高亮行）、Enter 启动、Escape 关闭；菜单行改为从 Launchpad 匹配集绘制（与过滤一致），行命中与绘制同几何。
  - **hider**：`panel.conf hider=true` 时指针离开面板 240 tick 收纳为 grip、回到条带即展开（默认关，不影响现有观感）。
- **验收**：`cargo xtask test --timeout 240` —— 内核 **70 套件 / 251 用例全过**；host 测试 **79 套件 / 612 用例全过**（其中 24 个 EDE crate 229 个）；`./scripts/gates.sh` 五道门禁（fmt、host clippy `-D warnings`、UEFI clippy、host test、裸机构建、QEMU 启动套件）**全绿**；另在 QEMU+VNC/QMP 下做了桌面端到端抽查：点击 Nova → Start 菜单开 → 键入 `ter` 实时过滤 → Enter 启动新终端 → 新窗口秒入动态任务栏（`editor,file-manager,widgets-demo,terminal,terminal`）→ 被抬升桌面正确遮盖的旧窗口保持隐藏 → 全程 0 fault / 0 kill。
- **集成中发现并修复的启动崩溃（desktop-shell 栈溢出）**：首次带配置消费的构建中 desktop-shell 启动即撞 guard page（sp 恰在 64KB 栈底之下 40 字节）。根因是 `EdeConfig` 作为**值类型**的旧容量（16 段 × 64 键 × 312B ≈ 328KB），被优化器连同 `load_configs` 内联进 `main`，帧膨胀到 367KB。双管齐下修复：`EdeConfig` 容量收缩为 8 段 × 16 键 × 128B（≈25KB，对全部 EDE 配置文件——panel.conf 4 键、desktop.conf 2 段 13 键、ede-conf.conf 7 段——都宽裕），同时 `USER_STACK_SIZE` 64KB→128KB 留出工程余量（loader 按同一常量映射 32 页，guard 派生自常量，m2 测试全部引用常量而非硬编码地址，故无破坏）。
- **诊断过程的两条硬结论**（三版交叉验证）：① **`present()` 热路径上打内核日志会诱发用户进程控制流损坏**（`ip=0x0` / 野跳转 `ip=0xff40…` 崩溃循环、服务重启耗尽任务槽）——present 内联日志 11 faults、改到 syscall 路径日志 2 faults、完全无日志 0 faults；机制未完全查明（疑似 UART 轮询延迟扰动 PS/2/定时器时序暴露既有竞态），但结论可靠：**帧呈现路径不得打日志**，诊断只能事后用静态手段或采样式钩子。② QMP `screendump` 会与 present 风暴竞争，抓到"升序绘制中途"的撕裂帧（窗口先画、桌面后画），曾被误判为 z 序错误——去掉日志后的最终验证表明 raise→遮盖→新窗口置顶的行为全部正确。
- **备注**：host clippy 若包含 `nova-apps` 会因 no_std 二进制的 unwinding 结构性失败——`gates.sh` 按其文档排除 `nova-apps`/`nova-kernel`/`nova-rt`/`nova-boot-menu`；本轮顺手清掉了 `graphics.rs` 中无调用者的 `fill_word`/`fill_rect` 死代码，并把 `Records::next` 改为真正的 `Iterator` 实现。

### 13.9 显示所有权分离：日志/桌面分屏、闪烁、Safe Terminal 无鼠标（2026-11-10）〔已修复〕**High**

- **背景**（用户验收反馈）：桌面运行时屏幕"一半桌面一半 Log"、鼠标一动就剧烈闪烁、Safe Terminal 阶段出现鼠标指针、质疑 ExitBootServices 后仍使用 UEFI GOP。逐条排查后定位四个根因并修复：
- **根因 1 —— 日志与桌面双写 framebuffer**：桌面模式下 `SCREEN_QUIET` 只在 SafeTerminal 启动时置位，内核 `klog` 每行都画上屏；更严重的是用户 WRITE syscall 走 `push_bytes_always`（无视 quiet），应用的 `ready`/`launch` 输出直接画在桌面上。**修复（显示器所有权模型）**：`screen.rs` 新增 `DESKTOP_ACTIVE` 标志——compositor 的 `present()` 每次先判定"是否有 surface 持有 buffer"：有 → 桌面拥有屏幕，控制台对内核日志与用户输出**一律静默**（仅串口输出）；无 → 控制台收回屏幕并 `clear_for_userland()` 拿到干净画布。panic 路径新增 `push_bytes_panic` 绕过一切静音（panic 必须可见）。`clear_for_userland` 在桌面活跃时变为 no-op，文本 shell 的 `clear` 命令不再能抹掉桌面。
- **根因 2 —— 鼠标移动触发整屏重绘**：`on_pointer` 原先对每个指针事件无条件返回 dirty → 每个鼠标包都整面 damage + 全屏 region 重绘（6 个 surface 拷贝），叠加日志双写即为"闪个不停"。**修复**：`on_pointer` 改为**只在视觉真的变化时置脏**——纯移动（无 hover 变化、无 tooltip 显隐变化）不重绘；光标本身由 compositor 的小区域（12×12 新旧 cell）重绘负责。通知投递/过期也补了脏标记。保留的功能性重绘：按下沿、拖动移动、选择变化、菜单开合、气泡消失。
- **根因 3 —— Safe Terminal 出现鼠标**：无任何窗口时 `present()` 仍走 cursor 绘制路径。**修复**：`present()` 开头的 `!gui` 分支直接消费帧触发并返回——不 clear、不画光标、不碰显示；文本控制台完整拥有屏幕。实测：Safe Terminal 下移动鼠标前后截屏**逐像素零差异**。
- **根因 4 —— GOP 审计**：逐行核查 `nova-boot-menu`：全部 GOP 调用（`locate_handle_buffer`/`set_mode`/`frame_buffer`/`Blt`）都发生在 `exit_boot_services` 之前的闭包内；`BootHandoff` 只携带 pml4/栈顶/快照/入口四个地址；内核侧 `novaboot`/`drivers/display` 对 GOP 零引用——只把启动协议快照里的物理 framebuffer 地址映射后用 CPU 直写。**结论：ExitBootServices 之后不存在任何 GOP 协议调用**，并在 `exit_boot_services` 调用点写入了不可违规的契约注释。
- **事件验证**（回应"没有实现桌面 events"）：QMP 注入实测全链路——按下沿开菜单、`t/e/r` 实时过滤、Enter 启动（串口 `ede-shell: launch terminal`，屏幕无日志残留）、Escape 关菜单、Nova 按钮开合、空白处点击关菜单、任务栏芯片 Activate/Minimize、右键（本轮新增：选中图标 / 关菜单清选择）。指针按下/释放边沿、按键（含 Shift 状态的 Decoder）、hover/tooltip 均在 `desktop-shell` 的 `on_pointer`/`on_key` 中实现。
- **验收**：`./scripts/gates.sh` 全绿（fmt、host clippy、UEFI clippy、host 79/612、裸机构建、QEMU 70/251）；QEMU+VNC 实测桌面模式全程屏幕无任何日志文本、Safe Terminal 无鼠标且移动鼠标零像素变化。遗留说明：多 surface 并发 present 与 shell 绘制之间存在微小撕裂窗口（另一 surface 的 commit 恰逢 shell 重绘中途时，截屏可捕获混合帧）——彻底解决需要双缓冲/提交同步协议，记为后续工作。

### 13.10 NovaOS EDE 2.x 外观与交互完整还原 + 动态分辨率 / bottom-pin / 光标钳制（2026-11-10）〔已完成〕**Medium**

- **背景**：§13.8/§13.9 完成 EDE 的部件级移植，但外观与交互层仍停留在工程基线：desktop-shell 硬编码 1024×768、Start 菜单是 comet 而非 EDE 菜单、标题栏全工程灰、无 Run 对话框、`TIME` 未接、panel applet 顺序与 EDE 2.x 顺序不一致。
- **本次修复**（全部在 `user/apps/src/bin/desktop-shell.rs` + `crates/nova-ui` + `crates/nova-ede-panel` + 内核种子）：
  - **EDE 外观**：`nova-ui` 引入 pekwm `default` 色板（focused title `#4B6983`、unfocused `#EAE8E3`、border `#C6C6C6`、buttons `#E6E6E6`、menu header `#7F9EBC`、menu body `#FBFBFB`）+ bevel 按钮（pressed 内缩）；新增通用 `draw_titlebar`/`titlebar_hit` EDE titlebar；panel 切换为 EDE applet 顺序（start_menu, quick_launch, clock, taskbar, systray）并保留 EDE Start 按钮（80px）；Start 菜单改为 EDE 菜单（header `EDE`，行：Programs / Window List / Run… / About EDE；Programs 页 → 类别 → 应用）。
  - **交互补全**：Run 对话框（Command: 单行 + OK/Cancel，居中、modal，Escape 取消/Enter 提交）；About EDE（版本/宣传语/credits，按任意键/点击关闭）；TIME syscall（`Syscall(26)`，RTC 墙钟）→ 面板时钟。
  - **动态分辨率**：shell 不再假设 1024×768 — `graphics::display_query()` → `Shell::new(width, height)`，menu/dialog/panel/taskbar 全部按运行时尺寸（失败回退 DEFAULT 1024×768）定位；`menu_top`、`run_dialog_x/y` 随 `self.height/width` 计算。
  - **bottom-pinned 桌面 surface**：`SURFACE_CREATE_FLAG_BOTTOM` → `CompositorService::create_bottom` → `Surface.bottom`；`raise_unchecked` 保持其在普通 surface 之下；桌面以该 pinned surface 持有（点击壁纸聚焦 desktop、不盖窗）。
  - **光标钳制**：`kernel/src/drivers/input.rs` `mouse_move` clamp 到 `display_size - CURSOR_SPAN(12)`，箭头整体不滚动出屏。
- **验收（QEMU `-smp 4` headless + VNC/QMP）**：EDE 菜单开合、Programs→类别→应用、Run… 打字/提交、About EDE、TIME 面板时钟、taskbar 芯片、bottom-pin、光标钳制 —— 实测链路全部通；`./scripts/gates.sh` 全绿。

### 13.11 virtio-gpu 显卡驱动：GPU scanout、flush 节流、virtio-vga、页表 offset bug（2026-11-10）〔已修复〕**High**

- **背景**：§13.5/§13.9 起的桌面输出走 CPU framebuffer + GOP（`exit_boot_services` 前）直写；验收要求"桌面显示必须实现显卡驱动、用显卡去绘制、不要用 CPU 绘制、刷新频率 60~144 FPS 甚至更高"。
- **修复**：
  - **显卡架构**：新增 virtio-gpu driver（`kernel/src/drivers/virtio/gpu.rs`：DmaWindow、SG 合并、rdtsc 硬截止、polled control queue、`POLL_CYCLES=50_000_000`/`ATTEMPTS=2` cycle-deadline polling）；QEMU 设备 `virtio-vga,xres=1024,yres=768`（`tools/xtask/src/run.rs`、`scripts/qemu.sh` —— VGA 前端供 OVMF GOP、virtio-gpu backend 供本 driver，单 console）；`kernel/src/drivers/pci.rs` `DEVICE_VIRTIO_GPU=0x1050` + `is_virtio_gpu()`；`display::init` GPU-first、失败回退 GOP。
  - **flush**：`display::flush()` = 整表面 `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`；rate limiter 由 TSC `FLUSH_INTERVAL_CYCLES=10_000_000`（≈10 ms @1 GHz，帧率硬下限 ≥100 FPS）守护；仅在 `screen::console_take_dirty()` 标脏时 flush（console-only 路径不再为全桌面 flush）。
  - **页表 offset bug**：`kernel/src/mm/page_table.rs` 的 `translate`/`translate_user` 漏接 page offset（低 12 位）→ user 虚拟地址按 4K 页对齐、高 12 位取错 → 桌面/用户进程地址空间错乱。已修复 + 回归测试 `an_offset_address_translates_exactly`（`kernel/src/test/m2.rs`）。
- **刷新率**：flush 走 TSC deadline 节流，`FLUSH_INTERVAL_CYCLES≈10 ms` → 单帧周期 ≥100 FPS，满足 60~144 FPS 需求的下限；present 热路径零日志（`§13.8` 的"present 不得打日志"结论继续有效）。
- **验收**：`./scripts/gates.sh` 全绿；串口 `display: virtio-gpu scanout 1024x768 pitch 4096 at virt … — the device owns the scanout`；VNC/QMP 截图实测桌面/窗口/菜单/光标/bottom-pin 全部正确。

### 13.6 nova-desktop + desktop-shell（2026-11-09）〔已完成〕**Medium**

- **crate `nova-desktop`**（no_std，host 可测，7 个测试）：壁纸（深海渐变 + 确定性 xorshift 星空 + 地平线光带 + NovaOS 信标星，纯数学绘制零贴图）、图标列（Terminal/Files/Editor/Widgets，矢量原语绘制 + 标签 + 按压内缩反馈）、底部面板（`NovaOS` 词标 + Terminal/Files/Editor/Widgets 启动芯片 + 右侧 `novaos://desktop`）。`DesktopShell` 状态机：按压武装、释放触发 `DesktopCommand::Launch(name)`（`process::exec`），release 离开图标格取消。
- **app `desktop-shell`**：全屏 1024×768 背景窗口（compositor `SURFACE_OP_MOVE` 请求 (0,0) 放置），启动时位于 z 序最底层；点击图标或面板芯片 → exec 对应应用（在桌面上拉起新窗口）。
- **layout**：`USER_HEAP_SIZE` 1 MiB→4 MiB（桌面全屏 buffer 3 MiB 之需，ABI 常量，文档同步 `0x4040_0000`）；内核 `INITRAMFS_CAPACITY` 6→8 MiB（desktop-shell 使镜像到 6.4 MB）。
- **验收（QEMU VNC/QMP）**：桌面以 desktop-shell 为最底层启动；壁纸+星空+图标+面板全部渲染；点击 "Widgets" 图标 → `desktop-shell: launch widgets-demo` → 新 widgets-demo 窗口出现在图标列旁；点击面板 "Terminal" 芯片 → 新终端窗口出现（`NovaOS terminal (Sarasa Gothic)`，`NovaOS terminal (Sarasa Gothic)` + 输入行可用）；`-smp 1`/512MB 下 desktop-shell 正常启动（file-manager 窗口在该最小配置报 `E_NO_MEMORY`，`-smp 4`/2048MB 无此问题；属于小内存配置的容量限制，非本次回归）。
- **Gates**：fmt、clippy `-D warnings`（workspace 含两新 crate）、host 70/251、`cargo xtask test --timeout 240`、裸机构建——全绿。
