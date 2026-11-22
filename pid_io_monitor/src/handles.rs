//! 枚举目标进程启动时已经打开的文件，建立 `FileObject -> 路径` 初始映射。
//!
//! ETW Kernel-File 的 Read/Write 事件只带 `FileObject`，不带文件名。
//! 如果文件在监控启动前就已经打开，监控收不到对应的 Create/NameCreate 事件，
//! 只能从目标进程的句柄表反查：
//!
//! 1. `NtQuerySystemInformation(SystemExtendedHandleInformation)` 拿到所有句柄，
//!    其中 `Object` 就是内核 `FILE_OBJECT` 指针（与 ETW 的 `FileObject` 一致）。
//! 2. `DuplicateHandle` 复制到本进程。
//! 3. `GetFinalPathNameByHandleW` 得到 Win32 路径。
//!
//! 这样即使监控在目标进程之后启动，也能解析已有的文件 I/O。

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, DuplicateHandle, BOOL, DUPLICATE_SAME_ACCESS, GENERIC_READ, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFinalPathNameByHandleW, FILE_ATTRIBUTE_NORMAL, FILE_NAME_NORMALIZED,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GETFINALPATHNAMEBYHANDLE_FLAGS,
    OPEN_EXISTING, VOLUME_NAME_DOS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_DUP_HANDLE,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

/// `SYSTEM_INFORMATION_CLASS::SystemExtendedHandleInformation`
const SYSTEM_EXTENDED_HANDLE_INFORMATION: u32 = 64;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
const MAX_HANDLE_BUFFER: usize = 256 * 1024 * 1024;

/// `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX`（x64 布局，40 字节）。
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemHandleEntry {
    object: usize,
    process_id: usize,
    handle: usize,
    granted_access: u32,
    creator_back_trace_index: u16,
    object_type_index: u16,
    handle_attributes: u32,
    reserved: u32,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtQuerySystemInformation(
        system_information_class: u32,
        system_information: *mut c_void,
        system_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

/// 返回目标进程当前打开文件的 `FILE_OBJECT 指针 -> Win32 路径`。
///
/// 失败时返回错误字符串，调用方可以忽略（退化成只能解析监控启动后新打开的
/// Create 事件）。
pub fn open_file_objects(pid: u32) -> Result<HashMap<u64, String>, String> {
    if pid == 0 {
        return Ok(HashMap::new());
    }

    // 先确定 File 对象的类型索引：不同 Windows 版本不保证固定值。
    let file_type_index =
        file_object_type_index().ok_or_else(|| "无法确定 File 对象类型索引".to_string())?;
    let buffer = query_handle_buffer()?;

    // SAFETY: OpenProcess 返回的句柄由我们负责关闭；失败时返回 Err。
    let process = unsafe {
        OpenProcess(
            PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
            BOOL(0),
            pid,
        )
    }
    .map_err(|error| format!("OpenProcess({pid}) 失败: {error}"))?;

    let mut result = HashMap::new();
    let count = handle_count(&buffer);
    // SAFETY: query_handle_buffer 返回的缓冲区至少包含 2 个 usize 头部，
    // handle_count 已把 count 限制在缓冲区能容纳的条目数内。
    let entries = unsafe { buffer.as_ptr().add(2 * size_of::<usize>()) };
    // SAFETY: GetCurrentProcess 返回当前进程的伪句柄，无需关闭。
    let current = unsafe { GetCurrentProcess() };

    for index in 0..count {
        // SAFETY: index < count，且 count 已按缓冲区长度校验；使用
        // read_unaligned 避免 Vec<u8> 的对齐问题。
        let entry = unsafe {
            ptr::read_unaligned(
                entries.add(index * size_of::<SystemHandleEntry>()) as *const SystemHandleEntry
            )
        };

        if entry.process_id != pid as usize
            || entry.object == 0
            || entry.handle == 0
            || entry.object_type_index != file_type_index
        {
            continue;
        }

        let mut duplicated = HANDLE::default();
        // SAFETY: process 是有效的进程句柄；entry.handle 来自同一进程的句柄表；
        // duplicated 是有效的输出指针。
        let duplicated_ok = unsafe {
            DuplicateHandle(
                process,
                HANDLE(entry.handle as isize),
                current,
                &mut duplicated,
                0,
                BOOL(0),
                DUPLICATE_SAME_ACCESS,
            )
        };
        if duplicated_ok.is_err() {
            continue;
        }

        if let Some(path) = final_path(duplicated) {
            result.insert(entry.object as u64, path);
        }
        // SAFETY: duplicated 由 DuplicateHandle 成功创建，必须关闭。
        let _ = unsafe { CloseHandle(duplicated) };
    }

    // SAFETY: process 由 OpenProcess 成功创建，必须关闭。
    let _ = unsafe { CloseHandle(process) };
    Ok(result)
}

fn handle_count(buffer: &[u8]) -> usize {
    let header = 2 * size_of::<usize>();
    if buffer.len() < header {
        return 0;
    }
    // SAFETY: 缓冲区至少包含 header 字节；read_unaligned 避免对齐问题。
    let count = unsafe { ptr::read_unaligned(buffer.as_ptr() as *const usize) };
    let max = (buffer.len() - header) / size_of::<SystemHandleEntry>();
    count.min(max)
}

fn query_handle_buffer() -> Result<Vec<u8>, String> {
    let mut size = 1 << 20;

    loop {
        let mut buffer = vec![0u8; size];
        let mut returned = 0u32;
        // SAFETY: buffer 是可写且长度正确的缓冲区；returned 是有效的输出指针。
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_EXTENDED_HANDLE_INFORMATION,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
                &mut returned,
            )
        };

        if status >= 0 {
            let header = 2 * size_of::<usize>();
            let used = (returned as usize).clamp(header, buffer.len());
            buffer.truncate(used);
            return Ok(buffer);
        }

        if status != STATUS_INFO_LENGTH_MISMATCH {
            return Err(format!("NtQuerySystemInformation 失败: 0x{status:08x}"));
        }

        let needed = (returned as usize).max(size * 2);
        if needed > MAX_HANDLE_BUFFER {
            return Err(format!("句柄表过大（需要 {needed} 字节）"));
        }
        size = needed;
    }
}

/// 打开自身 exe，在句柄表里找到它，读出 File 对象的类型索引。
fn file_object_type_index() -> Option<u16> {
    let exe = std::env::current_exe().ok()?;
    let wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: wide 是以 NUL 结尾的 UTF-16 字符串；返回的句柄由我们关闭。
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .ok()?;

    let buffer = query_handle_buffer().ok()?;
    let count = handle_count(&buffer);
    // SAFETY: 同 open_file_objects，count 已按缓冲区长度校验。
    let entries = unsafe { buffer.as_ptr().add(2 * size_of::<usize>()) };
    // SAFETY: GetCurrentProcessId 无前置条件。
    let pid = unsafe { GetCurrentProcessId() } as usize;
    let handle_value = handle.0 as usize;

    let mut found = None;
    for index in 0..count {
        // SAFETY: index < count，使用 read_unaligned。
        let entry = unsafe {
            ptr::read_unaligned(
                entries.add(index * size_of::<SystemHandleEntry>()) as *const SystemHandleEntry
            )
        };
        if entry.process_id == pid && entry.handle == handle_value {
            found = Some(entry.object_type_index);
            break;
        }
    }

    // SAFETY: handle 由 CreateFileW 成功创建，必须关闭。
    let _ = unsafe { CloseHandle(handle) };
    found
}

fn final_path(handle: HANDLE) -> Option<String> {
    let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let mut buffer = vec![0u16; 32768];
    // SAFETY: handle 是有效的文件句柄；buffer 是可写且长度正确的缓冲区。
    let mut len = unsafe { GetFinalPathNameByHandleW(handle, &mut buffer, flags) } as usize;
    if len == 0 {
        return None;
    }

    if len >= buffer.len() {
        buffer.resize(len + 1, 0);
        // SAFETY: 同上，缓冲区已扩容到 API 要求的长度。
        len = unsafe { GetFinalPathNameByHandleW(handle, &mut buffer, flags) } as usize;
        if len == 0 || len >= buffer.len() {
            return None;
        }
    }

    let path = String::from_utf16_lossy(&buffer[..len]);
    Some(normalize_path(&path))
}

/// `GetFinalPathNameByHandleW` 返回 `\\?\D:\...` 或 `\\?\UNC\...`，转成普通路径。
fn normalize_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_path;

    #[test]
    fn normalizes_handle_paths() {
        assert_eq!(normalize_path(r"\\?\D:\data\file.bin"), r"D:\data\file.bin");
        assert_eq!(
            normalize_path(r"\\?\UNC\server\share\f.txt"),
            r"\\server\share\f.txt"
        );
        assert_eq!(normalize_path(r"D:\data\file.bin"), r"D:\data\file.bin");
    }
}
