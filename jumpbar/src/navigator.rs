use crate::native::*;
use std::thread::sleep;
use std::time::Duration;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::System::Threading::{GetCurrentThreadId,AttachThreadInput};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Debug, Clone, Copy, PartialEq)]
enum LayoutKind { None, Modern, Classic }

struct Layout {
    kind: LayoutKind,
    file_name_combo: HWND,
    file_name_edit: HWND,
    address_edit: HWND,
    has_rebar: bool,
    child_count: usize,
}

struct InspectCtx {
    children: Vec<HWND>,
    classes: Vec<String>,
}

unsafe extern "system" fn enum_cb(child: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut InspectCtx);
    ctx.children.push(child);
    ctx.classes.push(class_name(child));
    BOOL::from(true)
}

unsafe fn inspect(hwnd: HWND) -> Layout {
    let mut ctx = InspectCtx { children: Vec::new(), classes: Vec::new() };
    EnumChildWindows(hwnd, Some(enum_cb), LPARAM(&mut ctx as *mut _ as isize));

    let mut has_list = false;
    let mut has_tree = false;
    let mut has_rebar = false;
    for cls in &ctx.classes {
        match cls.as_str() {
            "SysListView32" | "DirectUIHWND" | "DUIViewWndClassName" | "SHELLDLL_DefView" => has_list = true,
            "SysTreeView32" => has_tree = true,
            "ReBarWindow32" => has_rebar = true,
            _ => {}
        }
    }

    let combo = GetDlgItem(hwnd, DLG_FILE_NAME_COMBO).unwrap_or_default();
    let edit  = find_file_name_edit(hwnd, combo);
    let addr  = find_address_edit(&ctx.children, &ctx.classes);

    let kind = if has_rebar || !addr.0.is_null() {
        LayoutKind::Classic
    } else if has_list && !edit.0.is_null() {
        LayoutKind::Modern
    } else if has_tree && edit.0.is_null() {
        LayoutKind::None // BrowseTree 暂不实现
    } else {
        LayoutKind::None
    };

    Layout {
        kind,
        file_name_combo: combo,
        file_name_edit: edit,
        address_edit: addr,
        has_rebar: has_rebar,
        child_count: ctx.children.len(),
    }
}

unsafe fn find_file_name_edit(hwnd: HWND, combo: HWND) -> HWND {
    if !combo.0.is_null() {
        if class_name(combo) == "Edit" { return combo; }
        let inside = find_edit_child(combo);
        if !inside.0.is_null() { return inside; }
    }
    let edit = GetDlgItem(hwnd, DLG_FILE_NAME_EDIT).unwrap_or_default();
    if !edit.0.is_null() && class_name(edit) == "Edit" { return edit; }
    HWND::default()
}

struct FindEditCtx { found: HWND }

unsafe extern "system" fn find_edit_cb(child: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut FindEditCtx);
    if class_name(child) == "Edit" {
        ctx.found = child;
        return BOOL::from(false);
    }
    BOOL::from(true)
}

unsafe fn find_edit_child(parent: HWND) -> HWND {
    if parent.0.is_null() { return HWND::default(); }
    let mut ctx = FindEditCtx { found: HWND::default() };
    EnumChildWindows(parent, Some(find_edit_cb), LPARAM(&mut ctx as *mut _ as isize));
    ctx.found
}

unsafe fn find_address_edit(children: &[HWND], classes: &[String]) -> HWND {
    for (i, c) in classes.iter().enumerate() {
        if c != "Edit" { continue; }
        if GetDlgCtrlID(children[i]) == DLG_ADDRESS_BAND {
            return children[i];
        }
    }
    HWND::default()
}

// ─────────────────────────── 公开入口 ───────────────────────────

/// 对当前前台对话框执行导航。返回是否成功。
pub unsafe fn navigate(hwnd: HWND, folder: &str) -> bool {
    if hwnd.0.is_null() || folder.is_empty() { return false; }

    let layout = inspect(hwnd);
    eprintln!(
        "[nav] class={} kind={:?} children={} rebar={} target={}",
        class_name(hwnd), layout.kind, layout.child_count, layout.has_rebar, folder
    );

    if layout.kind == LayoutKind::None {
        eprintln!("[nav] not a folder dialog → ignored");
        return false;
    }

    let path = folder.trim_end_matches('\\');
    if path.is_empty() { return false; }

    if !ensure_foreground(hwnd) {
        eprintln!("[nav] foreground check failed");
        return false;
    }

    wait_modifiers_released();
    if GetForegroundWindow() != hwnd {
        eprintln!("[nav] foreground lost while waiting modifiers");
        return false;
    }

    if layout.kind == LayoutKind::Classic {
        if try_address_bar(hwnd, &layout, path) {
            eprintln!("[nav] address bar success");
            return true;
        }
        eprintln!("[nav] address bar failed → fallback to file name box");
    }

    let ok = try_file_name_box(hwnd, &layout, path);
    eprintln!("[nav] file name box = {}", ok);
    ok
}

fn ensure_foreground(hwnd: HWND) -> bool {
    unsafe {
        if GetForegroundWindow() == hwnd { return true; }
        SetForegroundWindow(hwnd);
        sleep(Duration::from_millis(60));
        GetForegroundWindow() == hwnd
    }
}

fn wait_modifiers_released() {
    unsafe {
        for _ in 0..30 {
            if !any_modifier_down() { return; }
            sleep(Duration::from_millis(20));
        }
    }
}

// ─────────────────────────── 文件名框策略 ───────────────────────────

unsafe fn try_file_name_box(hwnd: HWND, layout: &Layout, path: &str) -> bool {
    let edit = layout.file_name_edit;
    if edit.0.is_null() { return false; }

    let original = read_text(edit);
    if !focus_control(hwnd, edit) { return false; }

    set_text(edit, path);
    let written = read_text(edit).to_lowercase().contains(&path.to_lowercase());

    if !written {
        // 少数自绘控件不吃 WM_SETTEXT：退回键盘输入
        press(&[VK_CONTROL, VK_A]);
        sleep(Duration::from_millis(60));
        send_unicode_text(path);
        sleep(Duration::from_millis(250));
        if !read_text(edit).to_lowercase().contains(&path.to_lowercase()) {
            set_text(edit, &original);
            return false;
        }
    }

    // 回车前最后一刻确认前台 + 焦点
    if GetForegroundWindow() != hwnd || thread_focus(hwnd) != edit {
        set_text(edit, &original);
        return false;
    }

    press(&[VK_RETURN]);
    sleep(Duration::from_millis(500));

    if !is_window(hwnd) { return false; }

    let after = read_text(edit);
    let navigated = !after.eq_ignore_ascii_case(path);
    if !navigated {
        set_text(edit, &original);
        return false;
    }

    if !original.is_empty() && !original.eq_ignore_ascii_case(path) {
        set_text(edit, &original);
    }
    true
}

// ─────────────────────────── 地址栏策略 ───────────────────────────

unsafe fn try_address_bar(hwnd: HWND, layout: &Layout, path: &str) -> bool {
    let focus_before = thread_focus(hwnd);

    let shortcuts: [&[VIRTUAL_KEY]; 2] = [
        &[VK_MENU, VK_D],      // Alt+D
        &[VK_CONTROL, VK_L],   // Ctrl+L
    ];

    for keys in shortcuts.iter() {
        press(keys);

        let mut focused = HWND::default();
        for _ in 0..12 {
            sleep(Duration::from_millis(50));
            if GetForegroundWindow() != hwnd { break; }
            let cand = thread_focus(hwnd);
            if is_address_bar(cand, layout) { focused = cand; break; }
        }
        if focused.0.is_null() { continue; }

        set_text(focused, path);

        let mut ready = false;
        for _ in 0..30 {
            sleep(Duration::from_millis(60));
            if GetForegroundWindow() != hwnd || thread_focus(hwnd) != focused {
                return false;
            }
            let typed = read_text(focused);
            if typed.to_lowercase().contains(&path.to_lowercase()) {
                ready = true;
                break;
            }
        }
        if !ready { return false; }

        if GetForegroundWindow() != hwnd || thread_focus(hwnd) != focused {
            return false;
        }

        press(&[VK_RETURN]);
        sleep(Duration::from_millis(120));
        restore_focus(hwnd, focus_before);
        return true;
    }
    false
}

unsafe fn is_address_bar(focused: HWND, layout: &Layout) -> bool {
    if focused.0.is_null() { return false; }
    let cls = class_name(focused);
    if cls != "Edit" && cls != "ComboBox" && cls != "ComboBoxEx32" { return false; }

    let id = GetDlgCtrlID(focused);
    if id == 1 || id == 2 || id == DLG_FILE_NAME_COMBO || id == DLG_FILE_NAME_EDIT {
        return false;
    }
    if id == DLG_ADDRESS_BAND { return true; }
    if focused == layout.file_name_edit || focused == layout.file_name_combo { return false; }

    // 兜底：往上找 Address Band / ReBar / Breadcrumb 祖先
    let mut parent = focused;
    for _ in 0..12 {
        parent = GetParent(parent).unwrap_or_default();
        if parent.0.is_null() { break; }
        let pc = class_name(parent);
        if pc.contains("Address Band") || pc.contains("ReBarWindow32") || pc.contains("Breadcrumb") {
            return true;
        }
        if pc == "#32770" { break; }
    }
    false
}

unsafe fn restore_focus(target_window: HWND, previous: HWND) {
    if previous.0.is_null() || !is_window(previous) { return; }
    let mut pid = 0u32;
    let target_tid = GetWindowThreadProcessId(target_window, Some(&mut pid as *mut u32));
    let my_tid = GetCurrentThreadId();
    if target_tid == 0 || target_tid == my_tid { return; }

    let attached = AttachThreadInput(my_tid, target_tid, true).as_bool();
    SetFocus(previous);
    if attached {
        let _ = AttachThreadInput(my_tid, target_tid, false);
    }
}