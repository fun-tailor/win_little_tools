mod folder_tracker;
mod native;
mod navigator;
mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

static TARGET: OnceLock<Arc<Mutex<Option<PathBuf>>>> = OnceLock::new();

static HOTKEY_MODS: OnceLock<u32> = OnceLock::new();
static HOTKEY_KEY:  OnceLock<u32> = OnceLock::new();

enum FolderSource {
    Fixed(PathBuf),
    Track,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // 无参数 = 双击 / 快捷方式启动 → 默认 --watch --track
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("--watch");
    let sub: Vec<&str> = if args.len() >= 2 {
        args[2..].iter().map(|s| s.as_str()).collect()
    } else {
        vec!["--track"]
    };

    match cmd {
        "--navigate" if !sub.is_empty() => {
            let fg = unsafe { GetForegroundWindow() };
            let ok = unsafe { navigator::navigate(fg, sub[0]) };
            std::process::exit(if ok { 0 } else { 1 });
        }
        "--watch" => {
            let cfg = settings::load();
            HOTKEY_MODS.set(cfg.hotkey_modifiers).ok();
            HOTKEY_KEY.set(cfg.hotkey_key).ok();

            let source = match sub.first().copied() {
                Some("--track") | None => FolderSource::Track,
                Some(p) => FolderSource::Fixed(PathBuf::from(p)),
            };
            let initial = match &source {
                FolderSource::Fixed(p) => Some(p.clone()),
                FolderSource::Track if !cfg.default_folder.is_empty() => {
                    Some(PathBuf::from(&cfg.default_folder))
                }
                FolderSource::Track => None,
            };
            TARGET.set(Arc::new(Mutex::new(initial))).ok();

            let _tracker = if matches!(source, FolderSource::Track) {
                Some(folder_tracker::FolderTracker::start(
                    TARGET.get().unwrap().clone(),
                ))
            } else {
                None
            };

            run_watch(&source, &cfg);
        }
        "--inspect" => unsafe { inspect_foreground() },
        "-h" | "--help" => print_usage(),
        _ => print_usage(),
    }
}

fn print_usage() {
    eprintln!("用法:");
    eprintln!("  jumpbar --navigate <folder>   对当前前台对话框一次性跳转");
    eprintln!("  jumpbar --watch    <folder>   常驻，固定跳转到 <folder>");
    eprintln!("  jumpbar --watch --track       常驻，自动跟踪资源管理器当前目录");
    eprintln!("  jumpbar --watch               等价 --track");
    eprintln!("  jumpbar --inspect             打印前台窗口结构（或列出所有资源管理器路径）");
}

/// ─── inspect 调试模式 ───
unsafe fn inspect_foreground() {
    let fg = GetForegroundWindow();
    eprintln!("foreground hwnd={:?}", fg);
    let cls = native::class_name(fg);
    eprintln!("  class = {}", cls);

    let is_dialog = cls == "#32770"
        || cls.to_lowercase().starts_with("bosa_sdm_")
        || cls.to_lowercase().starts_with("nuidialog");

    if is_dialog {
        let mut kids = Vec::new();
        unsafe extern "system" fn collect(c: HWND, l: LPARAM) -> BOOL {
            let v = &mut *(l.0 as *mut Vec<HWND>);
            v.push(c);
            BOOL::from(true)
        }
        EnumChildWindows(fg, Some(collect), LPARAM(&mut kids as *mut _ as isize));
        for k in &kids {
            eprintln!(
                "  child {:?} id=0x{:X} class={} text=[{}]",
                k, GetDlgCtrlID(*k), native::class_name(*k), native::read_text(*k)
            );
        }
    } else {
        eprintln!("\n[explorer scan] 列出所有 CabinetWClass / ExploreWClass 的当前路径");
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mut hwnds: Vec<HWND> = Vec::new();
        unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
            let v = &mut *(l.0 as *mut Vec<HWND>);
            v.push(h);
            BOOL::from(true)
        }
        let _ = EnumWindows(Some(cb), LPARAM(&mut hwnds as *mut _ as isize));
        for h in hwnds {
            if folder_tracker::is_explorer(&native::class_name(h)) {
                let path = folder_tracker::read_path_for_hwnd(h);
                eprintln!("  explorer hwnd={:?} path={:?}", h, path);
            }
        }
        CoUninitialize();
    }
}


fn run_watch(source: &FolderSource, cfg: &settings::Settings) {
    let hotkey_str = settings::format_hotkey(cfg.hotkey_modifiers, cfg.hotkey_key);
    unsafe {
        let hmod = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), hmod, 0)
            .expect("SetWindowsHookExW failed");

        let where_str = match source {
            FolderSource::Fixed(p) => format!("固定跳转 {}", p.display()),
            FolderSource::Track if !cfg.default_folder.is_empty() =>
                format!("跟踪模式（无资源管理器时默认 {}）", cfg.default_folder),
            FolderSource::Track =>
                "跟踪模式（尚无目标：先打开资源管理器，或配置 settings.ini 的 default_folder）"
                    .to_string(),
        };
        eprintln!("[watch] 已安装键盘钩子，热键={}, {}", hotkey_str, where_str);
        eprintln!("[watch] 日志路径: stderr（后台运行请看开始菜单/快捷方式的重定向）");

        // ★ 关键：hook 装好 + 日志写完，再隐藏控制台
        // let folder_opt = TARGET
        //     .get()
        //     .and_then(|a| a.lock().ok().and_then(|g| g.clone()));
        // let Some(folder) = folder_opt else {
        //     // Track 模式但还没目标：一次性提示（用 static 防刷屏）
        //     use std::sync::atomic::{AtomicBool, Ordering};
        //     static WARNED: AtomicBool = AtomicBool::new(false);
        //     if !WARNED.swap(true, Ordering::Relaxed) {
        //         eprintln!("[watch] 尚无目标目录：切到资源管理器一次，或配置 settings.ini 的 default_folder");
        //     }
        //     return CallNextHookEx(None, code, wparam, lparam);
        // };
        try_hide_console();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let is_keydown = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
    if !is_keydown {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);

    if (kb.flags.0 & LLKHF_INJECTED.0) != 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    // 目标键匹配
    let want_key = HOTKEY_KEY.get().copied().unwrap_or(VK_G.0 as u32);
    if kb.vkCode != want_key {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    // 修饰键匹配（严格四态）
    let want_mods = HOTKEY_MODS.get().copied().unwrap_or(settings::MOD_CONTROL);
    if !modifiers_match(want_mods) {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let fg = GetForegroundWindow();
    if !is_candidate_dialog(fg) {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    // 读 TARGET（锁定时间 < 1µs，符合 SPEC §4.2）
    let folder_opt = TARGET
        .get()
        .and_then(|a| a.lock().ok().and_then(|g| g.clone()));
    let Some(folder) = folder_opt else {
        // Track 模式但 tracker 还没扫到任何资源管理器窗口 → 静默跳过
        return CallNextHookEx(None, code, wparam, lparam);
    };
    let folder_str = folder.to_string_lossy().into_owned();

    // HWND 不是 Send：转 isize 再搬到工作线程重建
    let fg_raw = fg.0 as isize;
    std::thread::spawn(move || {
        let fg = HWND(fg_raw as *mut _);
        unsafe { navigator::navigate(fg, &folder_str); }
    });

    LRESULT(1)
}

unsafe fn is_candidate_dialog(hwnd: HWND) -> bool {
    if hwnd.0.is_null() { return false; }
    let cls = native::class_name(hwnd);
    cls == "#32770"
        || cls.to_lowercase().starts_with("bosa_sdm_")
        || cls.to_lowercase().starts_with("nuidialog")
}

fn modifiers_match(expected: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    unsafe {
        let ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
        let alt  = (GetAsyncKeyState(VK_MENU.0 as i32)    as u16 & 0x8000) != 0;
        let shift= (GetAsyncKeyState(VK_SHIFT.0 as i32)   as u16 & 0x8000) != 0;
        let win  = ((GetAsyncKeyState(VK_LWIN.0 as i32) as u16 & 0x8000) != 0)
                || ((GetAsyncKeyState(VK_RWIN.0 as i32) as u16 & 0x8000) != 0);
        ctrl  == (expected & settings::MOD_CONTROL != 0)
            && alt   == (expected & settings::MOD_ALT != 0)
            && shift == (expected & settings::MOD_SHIFT != 0)
            && win   == (expected & settings::MOD_WIN != 0)
    }
}

/// 只在"独占 console"（双击 exe / 任务计划程序启动）时隐藏窗口；
/// 若从已有 cmd 启动（共享 console），不动 —— 避免影响用户的 cmd。
unsafe fn try_hide_console() {
    use windows::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

    let console = GetConsoleWindow();
    if console.0.is_null() { return; }

    let mut pids = [0u32; 4];
    let count = GetConsoleProcessList(&mut pids);
    if count == 1 {
        let _ = ShowWindow(console, SW_HIDE);
    }
}
