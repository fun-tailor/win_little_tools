
# SPEC: pid_io_monitor

## 1. 目标（What）

一个 **Windows 10 命令行程序**，给定 PID，**实时**监控该进程的磁盘 I/O 行为，周期性向 stdout 打印：

1. 该进程当前**读写最频繁的目录（folder 级，不细分到文件）**，按累计字节排序
2. 该进程**卡住超过阈值（默认 5s）的 I/O 操作**，含目录、操作类型、偏移

用于排查「进程在狂读磁盘但行为不应该」「进程卡死时卡在哪里的 I/O」两类问题。
**只定位到 folder，不要求具体文件 / 偏移精度**（但 offset 作为诊断信息保留）。

## 2. 运行形态（How to use）

```
pid_io_monitor.exe <PID> [--dump] [--no-elevate]
                             [--interval <秒>] [--stuck <秒>] [--duration <秒>]
```

- 启动后立即开始监控，**每 2 秒打印一次**聚合结果到 stdout，持续运行直到 Ctrl-C / 进程退出。
- **只在内容变化时打印**：(便于观察，但也会 silent 刷新发生)累计统计和卡死列表都与上一屏相同则跳过本次输出，避免目标进程停止 I/O 后反复刷同样的内容。
- 无 GUI、无交互输入（PID 从命令行参数取）。
- `--dump`：调试模式，把目标进程收到的每条原始事件（id / FileObject / Irp / FileName）打到 stderr，用于校准事件 ID 和字段名。
- `--interval` / `--stuck` 支持小数秒（最小 0.05），便于用较小的阈值验证卡死检测。
- `--duration`：运行指定秒数后自动退出，便于脚本化测试。

### 输出格式（stdout，每次刷新）

```
========== HH:MM:SS ==========
累计字节        操作数  目录
------------------------------------------------------------------------
<human_bytes>   <ops>   <folder>
...
(最多 15 行，按累计字节降序)

--- 卡住的 I/O (>5s) ---     # 仅当存在时打印
[STUCK   <elapsed>s] <READ|WRITE> <folder>  offset=<n>
```

## 3. 环境与版本矩阵（MUST）

| 项 | 要求 |
|---|---|
| OS | Windows 10 x64 |
| Rust | stable，edition 2021 |
| `ferrisetw` | **= 1.2.0**（锁定，勿升级） |
| `windows` | **^0.57.0**（必须与 ferrisetw 1.2.0 传递依赖一致；用 `cargo tree -i windows` 确认只有一份） |
| 权限 | **管理员**（Kernel-File provider 强制要求） |
| ETW Provider | `Microsoft-Windows-Kernel-File` `{EDD08927-9CC4-4E65-B970-C2560FB5C289}` |

## 4. 架构与数据流

```
ETW Kernel-File 事件流
        │  (回调线程；仅处理 TargetPID 的事件)
        ▼
  ┌─────────────────────────────────────────────┐
  │ 启动时：句柄表扫描 → file_map 初始映射       │
  │         （目标已打开的文件不会再有 Create）   │
  │ 事件分发                                     │
  │  NameCreate → key_map: FileKey -> Win路径    │
  │  Create  → file_map: FileObject -> Win路径   │
  │  Read/Write → stats: folder -> (ops, bytes)  │
  │              → pending: Irp -> PendingIo     │
  │  Close/Cleanup → file_map / key_map.remove   │
  │  OperationEnd → pending.remove(Irp)          │
  └─────────────────────────────────────────────┘
        │
        ▼  (打印线程，每 2s；内容不变则跳过)
  stdout: 热点目录 top15 + 超时 Irp 列表
```

### 关键数据结构

| 名称 | 类型 | 说明 |
|---|---|---|
| `TARGET_PID` | `AtomicU32` | 过滤用；不匹配直接 return |
| `FILE_MAP` | `Mutex<HashMap<u64 /*FileObject*/, String>>` | Create 建立，Close 移除；启动时由句柄表补齐 |
| `KEY_MAP` | `Mutex<HashMap<u64 /*FileKey*/, String>>` | NameCreate 建立，Close 移除；Read/Write 的第二路反查 |
| `STATS` | `Mutex<HashMap<String /*folder*/, (u64 ops, u64 bytes)>>` | 累计聚合 |
| `PENDING` | `Mutex<HashMap<u64 /*Irp*/, PendingIo>>` | 卡死检测 |
| `REPORTED_STUCK` | `Mutex<HashSet<u64 /*Irp*/>>` | 去重，避免重复报 |
| `DEVICE_MAP` | `HashMap<String /*\Device\HarddiskVolumeN*/, String /*D:*/>` | NT → Win32 路径转换 |

`PendingIo { folder: String, offset: u64, start: Instant, op: &'static str }`

## 5. 功能需求（验收点）

- [x] **R1 PID 过滤**：只处理 `record.process_id() == TARGET_PID` 的事件。
- [x] **R2 Folder 聚合**：收到 Read/Write 后，通过 FileObject 反查路径，取 `parent()` 作为 folder，累加 `ops` 和 `bytes`。
- [x] **R3 路径转换**：Create 事件 FileName 形如 `\Device\HarddiskVolume3\...`，须转为 `D:\...`；无匹配则原样输出。
- [x] **R4 周期打印**：每 2s 打印，按累计字节降序，最多 15 行。
- [x] **R5 卡死检测**：Read/Write 发起时以 `Irp` 为键记入 `pending`；`OperationEnd` 到达时移除；周期性扫描 `now - start >= 5s` 且未报过的，打印并记入 `reported_stuck`。
- [x] **R6 --dump**：打印每条目标进程事件的 id 及 `FileObject` / `Irp` / `FileName`，便于校准。
- [x] **R7 无数据提示**：若聚合为空，打印 `(还没有捕获到 I/O 事件)`。
- [x] **R8 优雅退出**：Ctrl-C 可退出（无需清理 ETW session 之外的资源；退出后系统 session 残留可接受但不推荐）。
- [x] **R9 启动补齐映射**：目标进程在监控启动前已打开的文件不会有 Create 事件。启动时枚举目标进程句柄表（`NtQuerySystemInformation(SystemExtendedHandleInformation)` + `DuplicateHandle` + `GetFinalPathNameByHandleW`），把 `FILE_OBJECT 指针 -> 路径` 填入 `FILE_MAP`，保证重启监控后仍能解析已有 I/O。
- [x] **R10 会话自愈**：ETW 会话使用固定名 `PidIoMonitor`，启动前先 `stop_trace_by_name`，清除上次非正常退出（关窗口/强杀）残留的会话，避免新会话收不到任何事件。
- [x] **R11 变化才打印**：累计统计与卡死列表都未变化时不重复打印上一屏。

## 6. 编译/运行阻塞项（已修复）

> 以下为最初实现时的阻塞项，已按 ferrisetw 1.2.0 + windows 0.57 实际 API 修正，保留作为记录：

- **E1 — `windows` 版本冲突**：已改为 `windows = "0.57"`，与 ferrisetw 1.2.0 的传递依赖一致（`cargo tree -i windows` 只有一份）。
- **E2 — `QueryDosDeviceW` 签名**：windows 0.57 返回 `u32`，第二参数 `&mut [u16]`，已按此实现。
- **E3 — 属性解析 API**：1.2.0 使用 `ferrisetw::parser::Parser::create(record, &schema)` + `parser.try_parse::<T>(name)`；`schema.try_get` 的两段式取值不可用。
- **E4 — `Provider::by_guid`**：返回 `Result`，已处理。
- **E5 — `UserTrace` 生命周期**：`start_and_process()` 返回持有 session 的 `UserTrace`，`Drop` 时 stop。
- **E6 — 事件 ID**：已用 `--dump` 校准，见 §8。

## 7. 校准流程（Calibration，MUST PASS）

agent 修复编译后，**必须**按以下顺序在真实机器上验证：

1. 管理员 cmd 运行 `pid_io_monitor.exe <PID> --dump`，对目标进程做一次明显文件读取。
2. 从 dump 中确认：
   - 带 `FileName` + `FileObject` 的事件 id → `EV_CREATE`
   - 带 `Irp` + `Offset` + `IoSize` + `FileObject` 的事件 id → `EV_READ` / `EV_WRITE`（两者字段相同，靠 id 区分）
   - 带 `FileObject` 的事件 id（无 FileName）→ `EV_CLOSE` / `EV_CLEANUP`
   - 带 `Irp` 的完成事件 id → `EV_OP_END`
3. 若字段名不同（如 `FileName` 实际为 `OpenPath` / `FilePath`），以 dump 输出为准修正 `get_*` 的字段名常量。
4. 将校准结果写回 §8 的常量表。

> 若 `--dump` 一个事件都收不到：确认管理员权限、PID 确实在做 I/O、ferrisetw 的默认 level/keyword 是否需要显式开启 `KERNEL_FILE_KEYWORD_FILEIO`。

## 8. 常量与字段（Calibrated）

> 已在 Windows 10 19041 上用 `--dump` 实测校准。

```rust
const KERNEL_FILE_GUID: u128 = 0xEDD08927_9CC4_4E65_B970_C2560FB5C289;

// 事件 ID（u16）
const EV_NAME_CREATE: u16 = 10; // FileKey + FileName
const EV_CREATE:      u16 = 12; // FileObject + FileName
const EV_CLEANUP:     u16 = 13;
const EV_CLOSE:       u16 = 14;
const EV_READ:        u16 = 15;
const EV_WRITE:       u16 = 16;
const EV_OP_END:      u16 = 24; // 注意：22 是 QueryInformation，不是 OperationEnd

// Keywords：0x3F0 = FILENAME | FILEIO | OP_END | CREATE | READ | WRITE
const KERNEL_FILE_KEYWORDS: u64 = 0x3F0;

// 字段名（实测；manifest 里是 IOSize / ByteOffset）
const F_FILE_NAME: &str = "FileName";
const F_FILE_OBJECT: &str = "FileObject";
const F_FILE_KEY: &str = "FileKey";
const F_IRP: &str = "Irp";
const F_IO_SIZE: &str = "IOSize";
const F_OFFSET: &str = "ByteOffset";
```

> 解析时对 `IoSize` / `Offset` 做兜底，兼容不同 provider 版本。

## 9. 非目标（Out of Scope）

- 不精确到「当前正在读的文件名 / 字节偏移」（folder 级即可；offset 仅作诊断字段附带）。
- 不做持久化、日志文件、GUI、交互式 PID 输入。
- 不支持 Protected Process（反作弊/杀软）—— 无解，接受。
- 不做 kernel 侧 PID 过滤（用户态过滤足够；若丢事件再优化，见 §11）。
- 不做网络路径 `\Device\Mup\...` 到 `\\server\share` 的映射（原样显示）。

## 10. 限制与风险

| 限制 | 影响 | 缓解 |
|---|---|---|
| 必须管理员 | 无权限时收不到事件 | 启动时检测并自动弹 UAC（`--no-elevate` 可关闭） |
| 用户态 PID 过滤 | 高频 I/O 进程可能丢事件 | 打印提示；必要时改用 `EnableTraceEx2` + `EVENT_FILTER_DESCRIPTOR(PID)` |
| 监控晚于目标启动 | 已有文件没有 Create 事件，Read/Write 无法反查目录 | 启动时扫描目标句柄表补齐 `FILE_MAP`（R9） |
| 非正常退出残留 ETW 会话 | 下次启动可能收不到事件 | 固定会话名，启动前 `stop_trace_by_name`（R10） |
| 卡死检测依赖 `OperationEnd` 的 `Irp` | 只有“IRP 已发起、完成事件超过阈值仍未到达”才算卡死；大量小 IRP 的慢速复制不构成单次卡死 | 已用单次大 IRP 验证（见 §12）；阈值可调 |
| ETW 回调内做重活会阻塞 session | 丢事件 | 回调内只做 map 更新；打印在独立线程 |

## 11. 未来扩展（可选，非本期）

- kernel 侧 PID 过滤（`EnableTraceEx2` filter descriptor）
- 滑动窗口速率（如 MB/s）而非累计字节
- folder 白名单/黑名单（忽略 `C:\Windows\` 噪音）
- 卡死时附带线程栈采样（`GetThreadTimes` 轮询 CPU，区分 I/O 卡死 vs CPU 卡死）

## 12. 验收清单（Definition of Done）

- [x] `cargo build --release` 零错误、零警告（允许 `unsafe` 但须注释）。
- [x] 管理员运行 `pid_io_monitor.exe <PID> --dump` 能收到目标进程的 Create 事件且 `FileName` 非空。
- [x] 去掉 `--dump`，对目标进程读写文件，2s 内 stdout 出现对应 folder 且字节数 > 0。
- [x] **卡死检测**：目标进程对 `test.mp4`(big size 测试文件)发起一次 768MB 的 `FILE_FLAG_NO_BUFFERING` 读（单个 IRP），
      监控以 `--interval 0.05 --stuck 0.1` 运行，stdout 出现
      `[STUCK  0.2s] READ  I:\DSH\pid_io  offset=0`。
      > 说明：本机磁盘上普通慢速复制由大量短 IRP 组成，不会触发“单个 IRP 超过 5s”；
      > 验收用降低阈值的方式验证同一条代码路径，真实卡死场景（网络盘/故障设备）同样适用。
- [x] **重启**：正常退出后重启、以及强杀监控后重启，都能继续收到事件（R10）。
- [x] **监控晚于目标启动**：目标先打开 `test.mp4` 再启动监控，仍能按目录聚合（R9）。
- [x] **空闲去重**：目标停止 I/O 后不再重复打印相同快照（R11）。
- [x] Ctrl-C 可退出。
- [x] §8 常量表已按实测校准并提交。

## 13. 参考

- ferrisetw 1.2.0 docs: https://docs.rs/ferrisetw/1.2.0
- windows 0.57 docs: https://docs.rs/windows/0.57.0
- Microsoft-Windows-Kernel-File 事件参考: Windows SDK `messages` / WPA 内置
- 校准辅助工具：`wpr -start FileIO -filemode` + WPA，可在写代码前确认字段
