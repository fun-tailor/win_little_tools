//! NT 设备路径 → Win32 路径转换，以及目录提取。

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::QueryDosDeviceW;

/// `\Device\HarddiskVolumeN` → `D:` 的映射。
pub static DEVICE_MAP: LazyLock<HashMap<String, String>> = LazyLock::new(build_device_map);

/// 把 ETW 里的 NT 路径转换成用户能看懂的 Win32 路径。
///
/// 例如 `\Device\HarddiskVolume3\foo\bar.txt` → `D:\foo\bar.txt`。
/// 找不到对应盘符时原样返回（例如 `\Device\Mup\...` 网络路径）。
pub fn nt_to_win(path: &str) -> String {
    // 有些路径会带 `\??\` 前缀。
    let path = path.strip_prefix(r"\??\").unwrap_or(path);

    // 选择最长匹配，避免 HarddiskVolume1 误匹配 HarddiskVolume10。
    let mut best: Option<(&str, &str)> = None;
    for (device, drive) in DEVICE_MAP.iter() {
        if path.len() >= device.len()
            && path.as_bytes()[..device.len()].eq_ignore_ascii_case(device.as_bytes())
            && best.map_or(true, |(best_device, _)| device.len() > best_device.len())
        {
            best = Some((device, drive));
        }
    }

    match best {
        Some((device, drive)) => format!("{}{}", drive, &path[device.len()..]),
        None => path.to_string(),
    }
}

/// 取文件所在目录。`D:\a\b.txt` → `D:\a`，`D:\b.txt` → `D:\`。
pub fn folder_of(path: &str) -> String {
    match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

fn build_device_map() -> HashMap<String, String> {
    let mut map = HashMap::new();

    for letter in b'A'..=b'Z' {
        let drive = format!("{}:", letter as char);
        let mut name: Vec<u16> = drive.encode_utf16().collect();
        name.push(0);
        let mut target = [0u16; 1024];

        // SAFETY: `name` 是以 NUL 结尾的 UTF-16 字符串；`target` 是长度正确的可写缓冲区。
        // QueryDosDeviceW 只会往 `target` 里写，不会保留指针。
        let len = unsafe { QueryDosDeviceW(PCWSTR(name.as_ptr()), Some(&mut target)) };
        if len == 0 {
            continue;
        }

        let raw = String::from_utf16_lossy(&target[..len as usize]);
        // 该 API 可能返回多个 NUL 分隔的字符串，盘符一般只有一个。
        if let Some(device) = raw.split('\0').next() {
            if !device.is_empty() {
                map.insert(device.to_string(), drive);
            }
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::folder_of;

    #[test]
    fn folder_extraction() {
        assert_eq!(folder_of(r"D:\a\b.txt"), r"D:\a");
        assert_eq!(folder_of(r"D:\b.txt"), r"D:\");
        assert_eq!(folder_of(r"\Device\Mup\share\f.txt"), r"\Device\Mup\share");
    }
}
