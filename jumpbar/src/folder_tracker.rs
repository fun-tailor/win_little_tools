
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::{Interface, VARIANT};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};

use crate::native::class_name;

/// 前后台线程共享的"当前跟踪目录"。
pub type Shared = Arc<Mutex<Option<PathBuf>>>;

pub struct FolderTracker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FolderTracker {
    pub fn start(shared: Shared) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = thread::spawn(move || poll_loop(shared, s));
        Self { stop, handle: Some(handle) }
    }
}

impl Drop for FolderTracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn poll_loop(shared: Shared, stop: Arc<AtomicBool>) {
    unsafe { let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); }
    while !stop.load(Ordering::Relaxed) {
        if let Some(p) = unsafe { read_active_explorer_path() } {
            let mut g = shared.lock().unwrap();
            if g.as_ref() != Some(&p) {
                eprintln!("[track] {}", p.display());
                *g = Some(p);
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    unsafe { CoUninitialize(); }
}

/// 只在"前台是资源管理器"时返回 Some，其他情况返回 None。
unsafe fn read_active_explorer_path() -> Option<PathBuf> {
    let fg = GetForegroundWindow();
    if fg.0.is_null() { return None; }
    if !is_explorer(&class_name(fg)) { return None; }
    read_path_for_hwnd(fg)
}


/// 返回该窗口**活动标签页**的路径。多标签时按标题匹配。
pub unsafe fn read_path_for_hwnd(target: HWND) -> Option<PathBuf> {
    let sw: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
    let count = sw.Count().ok()?;
    let target_raw = target.0 as isize;

    // 先收集所有同 HWND 的标签：(显示名, 路径)
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for i in 0..count {
        let idx = VARIANT::from(i);
        let Ok(disp) = sw.Item(&idx) else { continue };
        let Ok(browser) = disp.cast::<IWebBrowser2>() else { continue };
        let Ok(hwnd_sh) = browser.HWND() else { continue };
        if (hwnd_sh.0 as isize) != target_raw { continue }
        let Ok(url) = browser.LocationURL() else { continue };
        let Ok(name) = browser.LocationName() else { continue };
        if let Some(p) = file_url_to_path(&url.to_string()) {
            candidates.push((name.to_string(), p));
        }
    }

    if candidates.is_empty() { return None; }
    if candidates.len() == 1 { return candidates.pop().map(|(_, p)| p); }

    // 多标签：从窗口标题取活动标签名去匹配 LocationName
    let title = get_window_title(target);
    let active = extract_active_tab_name(&title);
    if !active.is_empty() {
        for (name, path) in &candidates {
            if name.eq_ignore_ascii_case(&active) {
                return Some(path.clone());
            }
        }
    }
    // 兜底：C# 同款——取最后一个
    candidates.pop().map(|(_, p)| p)
}

unsafe fn get_window_title(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 { return String::new(); }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n as usize])
}

/// 「文档 和 1 个其他选项卡 - 文件资源管理器」→「文档」
fn extract_active_tab_name(title: &str) -> String {
    const MARKERS: [&str; 5] = [
        " 和 ", " - 文件资源管理器", " - File Explorer", " - Windows Explorer", " and ",
    ];
    for m in MARKERS {
        if let Some(idx) = title.find(m) {
            if idx > 0 { return title[..idx].trim().to_string(); }
        }
    }
    String::new()
}

pub fn is_explorer(cls: &str) -> bool {
    cls == "CabinetWClass" || cls == "ExploreWClass"
}

/// file:///D:/foo%20bar -> D:\foo bar
/// file://server/share/f  -> \\server\share\f
fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    if let Some(local) = rest.strip_prefix('/') {
        let trimmed = local.trim_end_matches('/');
        Some(PathBuf::from(percent_decode(trimmed).replace('/', "\\")))
    } else {
        let trimmed = rest.trim_end_matches('/');
        Some(PathBuf::from(format!(
            "\\\\{}",
            percent_decode(trimmed).replace('/', "\\")
        )))
    }
}

/// UTF-8 安全的 percent decode（中文路径必须走 Vec<u8>，不要 `byte as char`）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}













