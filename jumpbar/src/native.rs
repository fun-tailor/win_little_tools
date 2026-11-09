use windows::Win32::Foundation::{HWND, LPARAM, RECT, POINT, WPARAM};
use windows::Win32::System::Threading::{GetCurrentThreadId,AttachThreadInput};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_SETTEXT: u32 = 0x000C;
pub const WM_GETTEXT: u32 = 0x000D;
pub const WM_GETTEXTLENGTH: u32 = 0x000E;

pub const DLG_FILE_NAME_COMBO: i32 = 0x047C;
pub const DLG_FILE_NAME_EDIT:  i32 = 0x0480;
pub const DLG_ADDRESS_BAND:    i32 = 0xA205;

/// 取窗口类名。跨进程 GetClassNameW 是纯本地调用，钩子回调里也安全。
pub unsafe fn class_name(hwnd: HWND) -> String {
    if hwnd.0.is_null() { return String::new(); }
    let mut buf = [0u16; 256];
    let n = GetClassNameW(hwnd, &mut buf);
    if n <= 0 { return String::new(); }
    String::from_utf16_lossy(&buf[..n as usize])
}

/// 读控件文本（WM_GETTEXT）。
pub unsafe fn read_text(hwnd: HWND) -> String {
    if hwnd.0.is_null() { return String::new(); }
    let len = SendMessageW(hwnd, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0)).0 as usize;
    if len == 0 { return String::new(); }
    let mut buf = vec![0u16; len + 1];
    SendMessageW(
        hwnd, WM_GETTEXT,
        WPARAM(buf.len()),
        LPARAM(buf.as_mut_ptr() as isize),
    );
    String::from_utf16_lossy(&buf[..len])
}

/// 一次性写控件文本（WM_SETTEXT），不产生逐字输入动画。
pub unsafe fn set_text(hwnd: HWND, text: &str) {
    if hwnd.0.is_null() { return; }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    SendMessageW(
        hwnd, WM_SETTEXT,
        WPARAM(0),
        LPARAM(wide.as_ptr() as isize),
    );
}

/// 当前焦点窗口（按目标窗口所属线程查，跨进程也能拿到）。
pub unsafe fn thread_focus(hwnd: HWND) -> HWND {
    let tid = GetWindowThreadProcessId(hwnd, None);
    if tid == 0 { return HWND::default(); }
    let mut info = GUITHREADINFO::default();
    info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    if GetGUIThreadInfo(tid, &mut info).is_ok() {
        info.hwndFocus
    } else {
        HWND::default()
    }
}

pub unsafe fn any_modifier_down() -> bool {
    let keys = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN];
    keys.iter().any(|k| (GetAsyncKeyState(k.0 as i32) as u16 & 0x8000) != 0)
}

pub unsafe fn is_window(hwnd: HWND) -> bool {
    IsWindow(hwnd).as_bool()
}

/// 把焦点送给跨进程控件：AttachThreadInput → SetForegroundWindow → SetFocus → Detach。
pub unsafe fn focus_control(target_window: HWND, control: HWND) -> bool {
    let mut pid = 0u32;
    let target_tid = GetWindowThreadProcessId(target_window, Some(&mut pid as *mut u32));
    let my_tid = GetCurrentThreadId();
    if target_tid == 0 { return false; }

    let attached = if target_tid != my_tid {
        AttachThreadInput(my_tid, target_tid, true).as_bool()
    } else {
        false
    };

    SetForegroundWindow(target_window);
    std::thread::sleep(std::time::Duration::from_millis(60));
    SetFocus(control);

    if attached {
        let _ = AttachThreadInput(my_tid, target_tid, false);
    }

    std::thread::sleep(std::time::Duration::from_millis(80));
    if thread_focus(target_window) == control { return true; }

    click_control(control)
}

/// 兜底：直接点击控件中心，用完还原鼠标。
unsafe fn click_control(control: HWND) -> bool {
    let mut rect = RECT::default();
    if GetWindowRect(control, &mut rect).is_err() { return false; }
    let x = (rect.left + rect.right) / 2;
    let y = (rect.top + rect.bottom) / 2;

    let mut origin = POINT::default();
    let had_origin = GetCursorPos(&mut origin).is_ok();

    let mut down = INPUT::default();
    down.r#type = INPUT_MOUSE;
    down.Anonymous.mi = MOUSEINPUT {
        dx: x, dy: y,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE
               | MOUSEEVENTF_LEFTDOWN | MOUSEEVENTF_LEFTUP,
        time: 0, dwExtraInfo: 0,
    };
    SendInput(&[down], std::mem::size_of::<INPUT>() as i32);
    std::thread::sleep(std::time::Duration::from_millis(250));

    if had_origin {
        let _ = SetCursorPos(origin.x, origin.y);
    }
    true
}

/// 模拟按下 + 抬起一个虚拟键（可带修饰键）。
pub unsafe fn press(keys: &[VIRTUAL_KEY]) {
    let mut inputs: Vec<INPUT> = Vec::new();
    for k in keys {
        inputs.push(kbd_input(*k, false));
    }
    for k in keys.iter().rev() {
        inputs.push(kbd_input(*k, true));
    }
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

fn kbd_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let mut input = INPUT::default();
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        wScan: 0,
        dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
        time: 0, dwExtraInfo: 0,
    };
    input
}

/// 逐 Unicode 字符注入（相当于 UnicodeInput.SendTextFast 的精简版）。
pub unsafe fn send_unicode_text(text: &str) {
    let mut inputs: Vec<INPUT> = Vec::new();
    for unit in text.encode_utf16() {
        inputs.push(uni_input(unit, false));
        inputs.push(uni_input(unit, true));
    }
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

fn uni_input(scan: u16, up: bool) -> INPUT {
    let mut input = INPUT::default();
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: VIRTUAL_KEY(0),
        wScan: scan,
        dwFlags: KEYEVENTF_UNICODE
               | if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
        time: 0, dwExtraInfo: 0,
    };
    input
}