
use std::fs;
use std::path::PathBuf;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;

#[derive(Debug, Clone)]
pub struct Settings {
    pub hotkey_modifiers: u32,
    pub hotkey_key: u32,
    pub default_folder: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey_modifiers: MOD_CONTROL,
            hotkey_key: VK_G.0 as u32,
            default_folder: String::new(),
        }
    }
}

/// 加载顺序：exe 同目录 settings.ini → %APPDATA%\JumpBar\settings.ini → 默认值
pub fn load() -> Settings {
    let mut s = Settings::default();
    let mut path: Option<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("settings.ini")));
    if path.as_ref().map_or(true, |p| !p.exists()) {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let alt = PathBuf::from(appdata).join("JumpBar").join("settings.ini");
            if alt.exists() {
                path = Some(alt);
            }
        }
    }
    let Some(path) = path.filter(|p| p.exists()) else {
        return s;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return s;
    };

    for raw in content.lines() {
        let line = raw.trim().trim_start_matches('\u{FEFF}');
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let val = line[eq + 1..].trim();
        match key {
            "hotkey" => {
                if let Some((m, k)) = parse_hotkey_text(val) {
                    s.hotkey_modifiers = m;
                    s.hotkey_key = k;
                }
            }
            "hotkey_modifiers" => {
                if let Ok(n) = val.parse() {
                    s.hotkey_modifiers = n;
                }
            }
            "hotkey_key" => {
                if let Ok(n) = val.parse::<u32>() {
                    s.hotkey_key = n;
                }
            }
            "default_folder" => {
                if !val.is_empty() {
                    s.default_folder = val.to_string();
                }
            }
            _ => {}
        }
    }
    s
}

/// "Ctrl+Shift+J" → (MOD_CONTROL|MOD_SHIFT, 'J')
fn parse_hotkey_text(text: &str) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    let mut key: Option<u32> = None;
    for part in text.split('+') {
        let p = part.trim().to_ascii_lowercase();
        match p.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "meta" => mods |= MOD_WIN,
            "enter" | "return" => key = Some(VK_RETURN.0 as u32),
            "space" => key = Some(VK_SPACE.0 as u32),
            "tab" => key = Some(VK_TAB.0 as u32),
            k if k.len() == 1 => {
                let c = k.chars().next()?.to_ascii_uppercase();
                if c.is_ascii_alphanumeric() {
                    key = Some(c as u32); // 'A'..'Z' = 0x41..0x5A = VK_A..VK_Z
                } else {
                    return None;
                }
            }
            k if k.starts_with('f') && k.len() <= 3 => {
                let n: u32 = k[1..].parse().ok()?;
                if (1..=12).contains(&n) {
                    key = Some(VK_F1.0 as u32 + (n - 1));
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some((mods, key?))
}

/// 显示用："Ctrl+Shift+J"
pub fn format_hotkey(mods: u32, key: u32) -> String {
    let mut s = Vec::new();
    if mods & MOD_CONTROL != 0 { s.push("Ctrl"); }
    if mods & MOD_ALT != 0 { s.push("Alt"); }
    if mods & MOD_SHIFT != 0 { s.push("Shift"); }
    if mods & MOD_WIN != 0 { s.push("Win"); }
    let key_str = if (0x41..=0x5A).contains(&key) {
        char::from_u32(key).map(|c| c.to_string()).unwrap_or_default()
    } else if (VK_F1.0 as u32..=VK_F12.0 as u32).contains(&key) {
        format!("F{}", key - VK_F1.0 as u32 + 1)
    } else {
        format!("VK(0x{:X})", key)
    };
    s.push(&key_str);
    s.join("+")
}






