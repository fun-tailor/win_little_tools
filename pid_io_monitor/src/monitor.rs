//! ETW 事件处理、目录聚合与卡死检测。

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema::Schema;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::{stop_trace_by_name, UserTrace};
use ferrisetw::EventRecord;
use windows::core::GUID;

use crate::paths::{folder_of, nt_to_win};
use crate::winutil::{local_time_string, shutdown_requested};

// ================= 常量（已按 Win10 19041 provider manifest 校准） =================

/// Microsoft-Windows-Kernel-File
const KERNEL_FILE_GUID: u128 = 0xEDD08927_9CC4_4E65_B970_C2560FB5C289;

// 事件 ID：wevtutil gp Microsoft-Windows-Kernel-File /ge:true 与官方 manifest 一致。
const EV_NAME_CREATE: u16 = 10; // FileKey + FileName
const EV_CREATE: u16 = 12; // FileObject + FileName
const EV_CLEANUP: u16 = 13;
const EV_CLOSE: u16 = 14;
const EV_READ: u16 = 15;
const EV_WRITE: u16 = 16;
const EV_OP_END: u16 = 24; // 注意：不是 22，22 是 QueryInformation

// Keywords（manifest 原值）
const KERNEL_FILE_KEYWORD_FILENAME: u64 = 0x10;
const KERNEL_FILE_KEYWORD_FILEIO: u64 = 0x20;
const KERNEL_FILE_KEYWORD_OP_END: u64 = 0x40;
const KERNEL_FILE_KEYWORD_CREATE: u64 = 0x80;
const KERNEL_FILE_KEYWORD_READ: u64 = 0x100;
const KERNEL_FILE_KEYWORD_WRITE: u64 = 0x200;

const KERNEL_FILE_KEYWORDS: u64 = KERNEL_FILE_KEYWORD_FILENAME
    | KERNEL_FILE_KEYWORD_FILEIO
    | KERNEL_FILE_KEYWORD_OP_END
    | KERNEL_FILE_KEYWORD_CREATE
    | KERNEL_FILE_KEYWORD_READ
    | KERNEL_FILE_KEYWORD_WRITE;

// 字段名（manifest 里是 IOSize / ByteOffset，不是 IoSize / Offset）
const F_FILE_NAME: &str = "FileName";
const F_FILE_PATH: &str = "FilePath";
const F_FILE_OBJECT: &str = "FileObject";
const F_FILE_KEY: &str = "FileKey";
const F_IRP: &str = "Irp";
const F_IO_SIZE: &str = "IOSize";
const F_IO_SIZE_ALT: &str = "IoSize";
const F_OFFSET: &str = "ByteOffset";
const F_OFFSET_ALT: &str = "Offset";

/// FileObject / FileKey 映射的安全上限，防止长时间运行内存无限增长。
const MAX_MAP_ENTRIES: usize = 500_000;

/// 每次最多打印的目录行数。
const MAX_PRINT_ROWS: usize = 15;

/// 固定 ETW 会话名。
///
/// 不用 PID 后缀，这样每次启动都能先清掉上次异常退出（例如直接关窗口/强杀）
/// 留下的同名会话；否则 Windows 可能保留旧会话，导致新会话收不到事件。
const SESSION_NAME: &str = "PidIoMonitor";

// ================= 配置 =================

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub target_pid: u32,
    pub dump: bool,
    pub print_interval: Duration,
    pub stuck_threshold: Duration,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn set_config(config: Config) {
    let _ = CONFIG.set(config);
}

fn config() -> &'static Config {
    CONFIG.get().expect("monitor::set_config 必须先调用")
}

// ================= 全局状态 =================

struct PendingIo {
    folder: String,
    offset: u64,
    start: Instant,
    op: &'static str,
}

static FILE_MAP: LazyLock<Mutex<HashMap<u64, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static KEY_MAP: LazyLock<Mutex<HashMap<u64, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// folder -> (ops, bytes)
static STATS: LazyLock<Mutex<HashMap<String, (u64, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PENDING: LazyLock<Mutex<HashMap<u64, PendingIo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static REPORTED_STUCK: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

static EVENTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static EVENTS_TARGET: AtomicU64 = AtomicU64::new(0);
static UNRESOLVED: AtomicU64 = AtomicU64::new(0);
static MAP_OVERFLOW: AtomicBool = AtomicBool::new(false);

// ================= ETW 会话 =================

pub fn start_trace() -> Result<UserTrace, String> {
    // 目标进程可能在监控启动前就已经打开了文件；这些文件不会有 Create 事件，
    // 先扫描句柄表补一份初始 FileObject -> 路径映射。
    // 放在启动会话之前，避免刚启动瞬间到达的事件因缺少映射而被丢弃。
    seed_file_map(config().target_pid);

    let start = || {
        let provider = Provider::by_guid(GUID::from_u128(KERNEL_FILE_GUID))
            .any(KERNEL_FILE_KEYWORDS)
            .level(5)
            .add_callback(on_event)
            .build();

        UserTrace::new()
            .named(SESSION_NAME.to_string())
            .enable(provider)
            .start_and_process()
    };

    // 启动前先清理残留会话。正常退出时 Drop 会 stop，但直接关窗口/强杀不会。
    let _ = stop_trace_by_name(SESSION_NAME);

    match start() {
        Ok(trace) => Ok(trace),
        Err(first) => {
            // 极少数情况下 stop 与 start 之间有延迟，清一次再重试。
            let _ = stop_trace_by_name(SESSION_NAME);
            start().map_err(|e| format!("启动 ETW 会话失败: {e:?}（首次尝试: {first:?}）"))
        }
    }
}

fn seed_file_map(pid: u32) {
    match crate::handles::open_file_objects(pid) {
        Ok(baseline) => {
            if baseline.is_empty() {
                return;
            }
            let mut map = FILE_MAP.lock().unwrap();
            for (object, path) in baseline {
                if map.len() >= MAX_MAP_ENTRIES {
                    break;
                }
                map.insert(object, path);
            }
        }
        Err(error) => {
            if config().dump {
                eprintln!("[dump] 初始句柄枚举失败: {error}");
            }
        }
    }
}

// ================= ETW 回调 =================

fn on_event(record: &EventRecord, schema_locator: &SchemaLocator) {
    EVENTS_TOTAL.fetch_add(1, Ordering::Relaxed);

    // R1：只处理目标 PID。用户态过滤，先做，避免无谓的 schema 解析。
    if record.process_id() != config().target_pid {
        return;
    }
    EVENTS_TARGET.fetch_add(1, Ordering::Relaxed);

    let schema = match schema_locator.event_schema(record) {
        Ok(schema) => schema,
        Err(_) => return,
    };
    let parser = Parser::create(record, &schema);
    let event_id = record.event_id();

    if config().dump {
        dump_event(record, &schema, &parser);
    }

    match event_id {
        // NameCreate 只有 FileKey + FileName，没有 FileObject。
        EV_NAME_CREATE => {
            let key = get_u64(&parser, F_FILE_KEY).filter(|value| *value != 0);
            let name = get_string(&parser, F_FILE_NAME);
            if let (Some(key), Some(name)) = (key, name) {
                insert_mapping(&KEY_MAP, key, nt_to_win(&name));
            }
        }
        // Create 有 FileObject + FileName，这是 Read/Write 反查路径的主映射。
        EV_CREATE => {
            let object = get_u64(&parser, F_FILE_OBJECT).filter(|value| *value != 0);
            let name = get_string(&parser, F_FILE_NAME);
            if let (Some(object), Some(name)) = (object, name) {
                insert_mapping(&FILE_MAP, object, nt_to_win(&name));
            }
        }
        EV_CLEANUP | EV_CLOSE => {
            let object = get_u64(&parser, F_FILE_OBJECT).filter(|value| *value != 0);
            let key = get_u64(&parser, F_FILE_KEY).filter(|value| *value != 0);

            let mut objects = FILE_MAP.lock().unwrap();
            let mut keys = KEY_MAP.lock().unwrap();
            if let Some(object) = object {
                objects.remove(&object);
            }
            if let Some(key) = key {
                keys.remove(&key);
            }
        }
        EV_READ => handle_io(&parser, true),
        EV_WRITE => handle_io(&parser, false),
        EV_OP_END => {
            if let Some(irp) = get_u64(&parser, F_IRP).filter(|value| *value != 0) {
                PENDING.lock().unwrap().remove(&irp);
                // IRP 可能被复用；完成时立刻清掉去重记录。
                REPORTED_STUCK.lock().unwrap().remove(&irp);
            }
        }
        _ => {}
    }
}

fn handle_io(parser: &Parser<'_, '_>, is_read: bool) {
    let object = get_u64(parser, F_FILE_OBJECT).filter(|value| *value != 0);
    let key = get_u64(parser, F_FILE_KEY).filter(|value| *value != 0);

    // 优先用 FileObject（Create 建立），退回 FileKey（NameCreate 建立）。
    let path = object
        .and_then(|object| FILE_MAP.lock().unwrap().get(&object).cloned())
        .or_else(|| key.and_then(|key| KEY_MAP.lock().unwrap().get(&key).cloned()));

    let path = match path {
        Some(path) => path,
        None => {
            // 文件在监控启动前就已打开，没看到 Create 事件，无法定位目录。
            UNRESOLVED.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let folder = folder_of(&path);
    let size = get_u64(parser, F_IO_SIZE)
        .or_else(|| get_u64(parser, F_IO_SIZE_ALT))
        .unwrap_or(0);
    let offset = get_u64(parser, F_OFFSET)
        .or_else(|| get_u64(parser, F_OFFSET_ALT))
        .unwrap_or(0);

    // R2：按 folder 聚合 ops / bytes。
    {
        let mut stats = STATS.lock().unwrap();
        let entry = stats.entry(folder.clone()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(size);
    }

    // R5：以 Irp 为键记录未完成的 I/O。
    if let Some(irp) = get_u64(parser, F_IRP).filter(|value| *value != 0) {
        let op = if is_read { "READ" } else { "WRITE" };
        PENDING.lock().unwrap().insert(
            irp,
            PendingIo {
                folder,
                offset,
                start: Instant::now(),
                op,
            },
        );
        // IRP 复用：新操作不应被旧的“已报过”记录挡住。
        REPORTED_STUCK.lock().unwrap().remove(&irp);
    }
}

fn insert_mapping(map: &Mutex<HashMap<u64, String>>, key: u64, path: String) {
    let mut map = map.lock().unwrap();
    if map.len() >= MAX_MAP_ENTRIES {
        map.clear();
        MAP_OVERFLOW.store(true, Ordering::Relaxed);
    }
    map.insert(key, path);
}

// ================= 属性提取 =================

fn get_string(parser: &Parser<'_, '_>, name: &str) -> Option<String> {
    parser.try_parse::<String>(name).ok()
}

fn get_u64(parser: &Parser<'_, '_>, name: &str) -> Option<u64> {
    parser
        .try_parse::<u64>(name)
        .ok()
        .or_else(|| parser.try_parse::<u32>(name).ok().map(u64::from))
        .or_else(|| parser.try_parse::<u16>(name).ok().map(u64::from))
        .or_else(|| parser.try_parse::<u8>(name).ok().map(u64::from))
}

// ================= --dump =================

const DUMP_STRING_FIELDS: &[&str] = &[F_FILE_NAME, F_FILE_PATH];
const DUMP_HEX_FIELDS: &[&str] = &[F_FILE_OBJECT, F_FILE_KEY, F_IRP, "ExtraInformation"];
const DUMP_DEC_FIELDS: &[&str] = &[
    F_IO_SIZE,
    F_IO_SIZE_ALT,
    F_OFFSET,
    F_OFFSET_ALT,
    "Status",
    "CreateOptions",
    "CreateAttributes",
    "ShareAccess",
    "InfoClass",
    "Length",
    "FileIndex",
    "IssuingThreadId",
    "ThreadId",
    "IOFlags",
    "ExtraFlags",
];

fn dump_event(record: &EventRecord, schema: &Schema, parser: &Parser<'_, '_>) {
    let mut fields = String::new();

    for &name in DUMP_STRING_FIELDS {
        if let Ok(value) = parser.try_parse::<String>(name) {
            if !value.is_empty() {
                fields.push_str(&format!(" {name}=\"{value}\""));
            }
        }
    }
    for &name in DUMP_HEX_FIELDS {
        if let Some(value) = get_u64(parser, name) {
            fields.push_str(&format!(" {name}=0x{value:x}"));
        }
    }
    for &name in DUMP_DEC_FIELDS {
        if let Some(value) = get_u64(parser, name) {
            fields.push_str(&format!(" {name}={value}"));
        }
    }

    eprintln!(
        "[dump] id={} task={} opcode={} v={} pid={} tid={} kw=0x{:x}{}",
        record.event_id(),
        schema.task_name(),
        schema.opcode_name(),
        record.version(),
        record.process_id(),
        record.thread_id(),
        record.keyword(),
        fields
    );
}

// ================= 打印线程 =================

pub fn print_loop() {
    let interval = config().print_interval;
    let mut last = Instant::now();
    let mut last_rows: Option<Vec<(String, u64, u64)>> = None;

    while !shutdown_requested() {
        std::thread::sleep(Duration::from_millis(100));

        if MAP_OVERFLOW.swap(false, Ordering::Relaxed) {
            eprintln!(
                "警告: FileObject/FileKey 映射超过 {MAX_MAP_ENTRIES} 条，已清空以限制内存；部分路径可能暂时无法解析。"
            );
        }

        if last.elapsed() < interval {
            continue;
        }
        last = Instant::now();

        let rows = collect_rows();
        let stuck = scan_stuck();

        // 累计统计没有变化、也没有新卡死时，不重复打印上一屏。
        // 这样目标进程停止 I/O 后不会一直刷同样的内容。
        if stuck.is_empty() && last_rows.as_ref() == Some(&rows) {
            continue;
        }

        last_rows = Some(rows.clone());
        print_snapshot(&rows, &stuck);
    }
}

/// 取当前累计统计，按字节降序排序并截断到最多 15 行。
fn collect_rows() -> Vec<(String, u64, u64)> {
    let mut rows: Vec<(String, u64, u64)> = STATS
        .lock()
        .unwrap()
        .iter()
        .map(|(folder, (ops, bytes))| (folder.clone(), *ops, *bytes))
        .collect();
    rows.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    rows.truncate(MAX_PRINT_ROWS);
    rows
}

fn print_snapshot(rows: &[(String, u64, u64)], stuck: &[(f64, String, &'static str, u64)]) {
    println!();
    println!("========== {} ==========", local_time_string());
    println!("{:<14} {:>10}  {}", "累计字节", "操作数", "目录");
    println!("{}", "-".repeat(72));

    if rows.is_empty() {
        println!("(还没有捕获到 I/O 事件)");
    } else {
        // R4：按累计字节降序，最多 15 行。
        for (folder, ops, bytes) in rows {
            println!("{:<14} {:>10}  {}", human_bytes(*bytes), ops, folder);
        }
    }

    if !stuck.is_empty() {
        println!();
        println!(
            "--- 卡住的 I/O (>{}) ---",
            format_secs(config().stuck_threshold)
        );
        for (elapsed, folder, op, offset) in stuck {
            println!(
                "[STUCK {:>6.1}s] {:<6} {}  offset={}",
                elapsed, op, folder, offset
            );
        }
    }

    if config().dump {
        eprintln!(
            "[dump] 统计: 总事件={} 目标PID事件={} 未解析路径={} file_map={} key_map={}",
            EVENTS_TOTAL.load(Ordering::Relaxed),
            EVENTS_TARGET.load(Ordering::Relaxed),
            UNRESOLVED.load(Ordering::Relaxed),
            FILE_MAP.lock().unwrap().len(),
            KEY_MAP.lock().unwrap().len()
        );
    }

    io::stdout().flush().ok();
}

fn scan_stuck() -> Vec<(f64, String, &'static str, u64)> {
    let now = Instant::now();
    let threshold = config().stuck_threshold;
    let mut out = Vec::new();

    let pending = PENDING.lock().unwrap();
    let mut reported = REPORTED_STUCK.lock().unwrap();

    for (&irp, io) in pending.iter() {
        let elapsed = now.saturating_duration_since(io.start);
        if elapsed >= threshold && !reported.contains(&irp) {
            reported.insert(irp);
            out.push((elapsed.as_secs_f64(), io.folder.clone(), io.op, io.offset));
        }
    }

    // 安全网：pending 里已经没有的 Irp，不必再保留去重记录。
    reported.retain(|irp| pending.contains_key(irp));

    out.sort_by(|a, b| b.0.total_cmp(&a.0));
    out
}

// ================= 小工具 =================

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn format_secs(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if duration.subsec_nanos() == 0 {
        format!("{}s", duration.as_secs())
    } else if secs < 1.0 {
        format!("{secs:.2}s")
    } else {
        format!("{secs:.1}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_formats_units() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(1024), "1.0KB");
        assert_eq!(human_bytes(1536), "1.5KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0MB");
    }

    #[test]
    fn stuck_detection_reports_once_and_clears_on_completion() {
        set_config(Config {
            target_pid: 0,
            dump: false,
            print_interval: Duration::from_secs(2),
            stuck_threshold: Duration::from_secs(5),
        });

        let irp = 0x1234_5678;
        let start = Instant::now()
            .checked_sub(Duration::from_secs(6))
            .expect("Instant 支持回退 6 秒");

        PENDING.lock().unwrap().insert(
            irp,
            PendingIo {
                folder: r"D:\data".to_string(),
                offset: 4096,
                start,
                op: "READ",
            },
        );
        REPORTED_STUCK.lock().unwrap().remove(&irp);

        // 第一次扫描应报告一次。
        let first = scan_stuck();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1, r"D:\data");
        assert_eq!(first[0].2, "READ");
        assert_eq!(first[0].3, 4096);

        // 同一个 Irp 不应重复报告。
        assert!(scan_stuck().is_empty());

        // OperationEnd 到达后，pending 清空，去重记录也应随之清理。
        PENDING.lock().unwrap().remove(&irp);
        assert!(scan_stuck().is_empty());
        assert!(!REPORTED_STUCK.lock().unwrap().contains(&irp));
    }
}
