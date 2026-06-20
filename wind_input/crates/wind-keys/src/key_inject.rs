//! 按键注入（cmdbar key.* 动作后端）。
//!
//! combo 解析（"Ctrl+Shift+End" / "Enter" / "vk:0x5D"）跨平台、可原生测试；
//! 实际注入用 Win32 `SendInput`，仅 `cfg(windows)`，非 Windows 返回错误降级。

use wind_cmdbar::KeyInjector;

/// 修饰键虚拟键码。
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12; // Alt
const VK_LWIN: u32 = 0x5B;

/// 解析按键组合 → (修饰键 vk 列表, 主键 vk)。无法解析返回 None。
/// 形如 `Ctrl+C` / `Shift+End` / `Enter` / `vk:0x5D`（直接十六进制 vk）。
pub(crate) fn parse_combo(combo: &str) -> Option<(Vec<u32>, u32)> {
    let parts: Vec<&str> = combo
        .split('+')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let (main, mod_parts) = parts.split_last()?;
    let mut mods = Vec::new();
    for m in mod_parts {
        let vk = match m.to_lowercase().as_str() {
            "ctrl" | "control" => VK_CONTROL,
            "shift" => VK_SHIFT,
            "alt" | "menu" => VK_MENU,
            "win" | "super" | "meta" | "cmd" => VK_LWIN,
            _ => return None,
        };
        mods.push(vk);
    }
    Some((mods, parse_key(main)?))
}

/// 解析单个主键名 → vk。支持 `vk:0xNN` 直写、常用功能键、F1-F12、数字、字母（回退 keymap）。
fn parse_key(name: &str) -> Option<u32> {
    let n = name.trim();
    if let Some(hex) = n.strip_prefix("vk:").or_else(|| n.strip_prefix("VK:")) {
        let hex = hex.trim().trim_start_matches("0x").trim_start_matches("0X");
        return u32::from_str_radix(hex, 16).ok();
    }
    let low = n.to_lowercase();
    let vk = match low.as_str() {
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "space" => 0x20,
        "backspace" | "bksp" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        _ => {
            // F1-F12
            if let Some(num) = low.strip_prefix('f')
                && let Ok(n) = num.parse::<u32>()
                && (1..=12).contains(&n)
            {
                return Some(0x70 + (n - 1));
            }
            // 单个数字
            if low.len() == 1 && low.as_bytes()[0].is_ascii_digit() {
                return Some(0x30 + (low.as_bytes()[0] - b'0') as u32);
            }
            // 字母（回退 keymap：'a'-'z' → 0x41-0x5A）等
            return crate::keymap::key_name_to_vk_with_letters(&low);
        }
    };
    Some(vk)
}

/// `SendInput` 后端的 KeyInjector（宿主经 wind-cmdbar 的 KeyInjector trait 使用）。
pub struct SysKeys;

impl KeyInjector for SysKeys {
    fn tap(&self, combo: &str) -> anyhow::Result<()> {
        let (mods, vk) =
            parse_combo(combo).ok_or_else(|| anyhow::anyhow!("无法解析按键组合 {:?}", combo))?;
        tap_combo(&mods, vk)
    }
    fn sequence(&self, combos: &[String]) -> anyhow::Result<()> {
        for c in combos {
            self.tap(c)?;
        }
        Ok(())
    }
    fn hold(&self, combo: &str) -> anyhow::Result<()> {
        let (mods, vk) =
            parse_combo(combo).ok_or_else(|| anyhow::anyhow!("无法解析按键组合 {:?}", combo))?;
        for m in &mods {
            send_key(*m, false)?;
        }
        send_key(vk, false)
    }
    fn release(&self, combo: &str) -> anyhow::Result<()> {
        let (mods, vk) =
            parse_combo(combo).ok_or_else(|| anyhow::anyhow!("无法解析按键组合 {:?}", combo))?;
        send_key(vk, true)?;
        for m in mods.iter().rev() {
            send_key(*m, true)?;
        }
        Ok(())
    }
    fn type_text(&self, text: &str) -> anyhow::Result<()> {
        type_unicode(text)
    }
}

/// 模拟单次组合：修饰键按下 → 主键按下抬起 → 修饰键反序抬起。
fn tap_combo(mods: &[u32], vk: u32) -> anyhow::Result<()> {
    for m in mods {
        send_key(*m, false)?;
    }
    send_key(vk, false)?;
    send_key(vk, true)?;
    for m in mods.iter().rev() {
        send_key(*m, true)?;
    }
    Ok(())
}

// ───────────────────────── Win32 SendInput（仅 windows）─────────────────────────

#[cfg(windows)]
fn send_key(vk: u32, up: bool) -> anyhow::Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
        VIRTUAL_KEY,
    };
    let flags = if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk as u16),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        anyhow::bail!("SendInput 失败 (vk={vk:#x}, up={up})");
    }
    Ok(())
}

#[cfg(windows)]
fn type_unicode(text: &str) -> anyhow::Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
        VIRTUAL_KEY,
    };
    for unit in text.encode_utf16() {
        for up in [false, true] {
            let mut flags = KEYEVENTF_UNICODE;
            if up {
                flags |= KEYEVENTF_KEYUP;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
            if sent == 0 {
                anyhow::bail!("SendInput(unicode) 失败");
            }
        }
    }
    Ok(())
}

// ───────────────────────── macOS 预留（待接入 Core Graphics）─────────────────────────
// macOS 按键注入用 Core Graphics 事件：
//   - send_key → `CGEventCreateKeyboardEvent(src, keycode, keydown)` + `CGEventPost(kCGHIDEventTap, ev)`
//     注意 macOS 用的是 CGKeyCode（ANSI 键位码），与此处的 Win32 VK 不同，需要一张 VK→CGKeyCode
//     映射表（或改用 keymap 直接产出 CGKeyCode）。
//   - type_unicode → `CGEventKeyboardSetUnicodeString(ev, len, buf)` 直接发 UTF-16。
// 接入时把下面两个桩替换为 `#[cfg(target_os = "macos")]` 的真实现（依赖 `core-graphics` crate）。

#[cfg(target_os = "macos")]
fn send_key(_vk: u32, _up: bool) -> anyhow::Result<()> {
    // TODO(macos): CGEventCreateKeyboardEvent + CGEventPost（需 VK→CGKeyCode 映射）。
    anyhow::bail!("key 注入：macOS 待接入 Core Graphics（CGEvent）")
}

#[cfg(target_os = "macos")]
fn type_unicode(_text: &str) -> anyhow::Result<()> {
    // TODO(macos): CGEventKeyboardSetUnicodeString。
    anyhow::bail!("key.type：macOS 待接入 Core Graphics（CGEvent）")
}

// 其他 Unix（Linux 等）：无统一按键注入通道，保持 no-op 桩。
#[cfg(not(any(windows, target_os = "macos")))]
fn send_key(_vk: u32, _up: bool) -> anyhow::Result<()> {
    anyhow::bail!("key 注入：当前平台暂未支持")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn type_unicode(_text: &str) -> anyhow::Result<()> {
    anyhow::bail!("key.type：当前平台暂未支持")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_keys() {
        assert_eq!(parse_combo("Enter"), Some((vec![], 0x0D)));
        assert_eq!(parse_combo("left"), Some((vec![], 0x25)));
        assert_eq!(parse_combo("F5"), Some((vec![], 0x74)));
        assert_eq!(parse_combo("3"), Some((vec![], 0x33)));
        assert_eq!(parse_combo("a"), Some((vec![], 0x41)));
    }

    #[test]
    fn parse_with_modifiers() {
        assert_eq!(parse_combo("Ctrl+C"), Some((vec![VK_CONTROL], 0x43)));
        assert_eq!(
            parse_combo("Ctrl+Shift+End"),
            Some((vec![VK_CONTROL, VK_SHIFT], 0x23))
        );
        assert_eq!(parse_combo("Alt+Tab"), Some((vec![VK_MENU], 0x09)));
        assert_eq!(parse_combo("Win+d"), Some((vec![VK_LWIN], 0x44)));
    }

    #[test]
    fn parse_raw_vk() {
        assert_eq!(parse_combo("vk:0x5D"), Some((vec![], 0x5D)));
        assert_eq!(parse_combo("Ctrl+vk:0x42"), Some((vec![VK_CONTROL], 0x42)));
    }

    #[test]
    fn parse_invalid() {
        assert_eq!(parse_combo("Bogus+X"), None); // 未知修饰键
        assert_eq!(parse_combo(""), None);
        assert_eq!(parse_combo("nonsuchkey"), None);
    }
}
