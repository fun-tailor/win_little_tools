//! pid_io_monitor：监控指定 Windows 进程的磁盘 I/O，按目录聚合，并检测卡住的 I/O。
//!
//! 用法：pid_io_monitor.exe <PID> [--dump] [--no-elevate]
//!                              [--interval <秒>] [--stuck <秒>] [--duration <秒>]

mod handles;
mod monitor;
mod paths;
mod winutil;

use std::env;
use std::error::Error;
use std::process;
use std::thread;
use std::time::{Duration, Instant};

use monitor::Config;

const DEFAULT_PRINT_INTERVAL: Duration = Duration::from_secs(2);
const DEFAULT_STUCK_THRESHOLD: Duration = Duration::from_secs(5);

struct Args {
    pid: u32,
    dump: bool,
    no_elevate: bool,
    interval: Duration,
    stuck: Duration,
    duration: Option<Duration>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("错误: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = match parse_args()? {
        Some(args) => args,
        None => return Ok(()),
    };

    // Kernel-File provider 强制要求管理员权限。默认自动弹 UAC。
    if !winutil::is_admin() {
        if args.no_elevate {
            eprintln!("警告: 当前进程不是管理员，Microsoft-Windows-Kernel-File 事件很可能收不到。");
        } else {
            match winutil::relaunch_elevated() {
                Ok(()) => {
                    println!(
                        "已请求管理员权限，请在弹出的新窗口中继续监控 PID {}。",
                        args.pid
                    );
                    return Ok(());
                }
                Err(error) => {
                    eprintln!("{error}");
                    eprintln!(
                        "请右键“以管理员身份运行”，或加 --no-elevate 继续（通常收不到事件）。"
                    );
                    return Err("需要管理员权限".into());
                }
            }
        }
    }

    monitor::set_config(Config {
        target_pid: args.pid,
        dump: args.dump,
        print_interval: args.interval,
        stuck_threshold: args.stuck,
    });

    println!(
        "监控 PID {}，打印间隔 {}，卡死阈值 {}。Ctrl-C 退出。",
        args.pid,
        format_secs(args.interval),
        format_secs(args.stuck)
    );
    if args.dump {
        println!("--dump 已开启：目标进程的原始事件会打印到 stderr。");
    }
    println!();

    winutil::install_ctrl_handler();

    let trace = monitor::start_trace().map_err(|error| -> Box<dyn Error> { error.into() })?;
    println!("ETW 会话已启动，开始收集 Microsoft-Windows-Kernel-File 事件。\n");

    let printer = thread::spawn(monitor::print_loop);

    let started = Instant::now();
    let mut ever_alive = winutil::process_alive(args.pid);
    if !ever_alive {
        eprintln!(
            "警告: 当前无法打开 PID {}（可能不存在或受保护），仍会继续监听事件。",
            args.pid
        );
    }

    // 主线程只负责等待退出条件；打印在独立线程，避免 ETW 回调被阻塞。
    while !winutil::shutdown_requested() {
        if let Some(duration) = args.duration {
            if started.elapsed() >= duration {
                break;
            }
        }

        let alive = winutil::process_alive(args.pid);
        if alive {
            ever_alive = true;
        } else if ever_alive {
            println!("\n目标进程 {} 已退出，监控结束。", args.pid);
            break;
        }

        thread::sleep(Duration::from_millis(200));
    }

    winutil::request_shutdown();
    drop(trace); // 停止 ETW session
    let _ = printer.join();

    println!("\n已停止监控。");
    Ok(())
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut pid = None;
    let mut dump = false;
    let mut no_elevate = false;
    let mut interval = DEFAULT_PRINT_INTERVAL;
    let mut stuck = DEFAULT_STUCK_THRESHOLD;
    let mut duration = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            "--dump" => dump = true,
            "--no-elevate" => no_elevate = true,
            "--interval" => interval = parse_secs(args.next(), "--interval")?,
            "--stuck" => stuck = parse_secs(args.next(), "--stuck")?,
            "--duration" => duration = Some(parse_secs(args.next(), "--duration")?),
            other if other.starts_with('-') => {
                return Err(format!("未知参数: {other}（用 --help 查看用法）"));
            }
            other => {
                if pid.is_some() {
                    return Err(format!("多余的参数: {other}"));
                }
                pid = Some(
                    other
                        .parse::<u32>()
                        .map_err(|_| format!("PID 必须是数字: {other}"))?,
                );
            }
        }
    }

    let pid = pid.ok_or("缺少 PID 参数（用 --help 查看用法）")?;

    Ok(Some(Args {
        pid,
        dump,
        no_elevate,
        interval,
        stuck,
        duration,
    }))
}

fn parse_secs(value: Option<String>, name: &str) -> Result<Duration, String> {
    let value = value.ok_or_else(|| format!("{name} 需要一个秒数"))?;
    let seconds: f64 = value
        .parse()
        .map_err(|_| format!("{name} 的值不是数字: {value}"))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!("{name} 必须是正数: {value}"));
    }
    // 下限 50ms，避免 --interval 0 把 CPU 打满。
    Ok(Duration::from_secs_f64(seconds.max(0.05)))
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

fn print_help() {
    println!("pid_io_monitor - 按目录实时监控指定进程的磁盘 I/O");
    println!();
    println!("用法:");
    println!("  pid_io_monitor.exe <PID> [选项]");
    println!();
    println!("选项:");
    println!("  --dump              把目标进程的每条原始事件打到 stderr（校准事件 ID / 字段名）");
    println!("  --no-elevate        不自动弹 UAC（通常收不到 Kernel-File 事件）");
    println!("  --interval <秒>     打印间隔，默认 2（可带小数，最小 0.05）");
    println!("  --stuck <秒>        卡死阈值，默认 5（可带小数，最小 0.05）");
    println!("  --duration <秒>     运行指定秒数后退出（默认一直运行）");
    println!("  -h, --help          显示本帮助");
    println!();
    println!("示例:");
    println!("  pid_io_monitor.exe 1234");
    println!("  pid_io_monitor.exe 1234 --dump");
    println!("  pid_io_monitor.exe 1234 --interval 1 --stuck 2 --duration 20");
}
