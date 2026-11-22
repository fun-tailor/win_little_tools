//! Windows 相关的进程 / 控制台辅助函数。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, STILL_ACTIVE};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

static RUNNING: AtomicBool = AtomicBool::new(true);

/// 当前进程是否已提权。
pub fn is_admin() -> bool {
    // SAFETY: IsUserAnAdmin 无参数、无前置条件。
    unsafe { IsUserAnAdmin().as_bool() }
}

/// 用 UAC 重新启动自己（保留命令行参数）。
pub fn relaunch_elevated() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法获取程序路径: {e}"))?;
    let args = std::env::args()
        .skip(1)
        .map(|arg| quote_arg(&arg))
        .collect::<Vec<_>>()
        .join(" ");

    let exe_w = to_wide(exe.as_os_str());
    let args_w = to_wide(OsStr::new(&args));
    let runas_w = to_wide(OsStr::new("runas"));

    // SAFETY: 所有指针都是有效的、以 NUL 结尾的 UTF-16 字符串，且在调用返回前一直存活；
    // ShellExecuteW 不会保留这些指针。
    let result = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(runas_w.as_ptr()),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    let code = result.0;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("UAC 提权失败（ShellExecuteW 返回 {code}）"))
    }
}

/// 安装 Ctrl-C / 关闭控制台的处理函数：只设置退出标志，由主线程优雅收尾。
pub fn install_ctrl_handler() {
    // SAFETY: handler 是合法的 `extern "system"` 函数，生命周期覆盖整个进程。
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), BOOL(1));
    }
}

/// 请求所有线程退出（Ctrl-C 之外，目标进程退出 / --duration 到点也用它）。
pub fn request_shutdown() {
    RUNNING.store(false, Ordering::SeqCst);
}

pub fn shutdown_requested() -> bool {
    !RUNNING.load(Ordering::SeqCst)
}

/// 目标进程是否仍然存活。打不开（受保护 / 已退出）时返回 false。
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: OpenProcess 返回的句柄由我们负责关闭；每条成功路径都会 CloseHandle。
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, BOOL(0), pid) {
            Ok(handle) => {
                let mut exit_code = 0u32;
                let ok = GetExitCodeProcess(handle, &mut exit_code).is_ok();
                let _ = CloseHandle(handle);
                ok && exit_code == STILL_ACTIVE.0 as u32
            }
            Err(_) => false,
        }
    }
}

/// 本地时间 `HH:MM:SS`。
pub fn local_time_string() -> String {
    // SAFETY: GetLocalTime 会写入并返回一个有效的 SYSTEMTIME。
    let now = unsafe { GetLocalTime() };
    format!("{:02}:{:02}:{:02}", now.wHour, now.wMinute, now.wSecond)
}

unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
    RUNNING.store(false, Ordering::SeqCst);
    // 返回 TRUE：告诉系统我们已处理，不要立刻终止进程。
    BOOL(1)
}

fn to_wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Windows 命令行参数转义（CommandLineToArgvW 规则）。
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;

    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(ch);
                backslashes = 0;
            }
        }
    }

    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn quotes_arguments() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        // 不含空白/引号时无需加引号，尾部反斜杠原样保留。
        assert_eq!(quote_arg(r"a\"), r"a\");
        // 含空格且以反斜杠结尾时，尾部反斜杠要翻倍。
        assert_eq!(quote_arg("a b\\"), "\"a b\\\\\"");
    }
}
