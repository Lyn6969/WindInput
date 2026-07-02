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

/// macOS：修饰键经 CGEvent flags 表达，而非单独 post 修饰键 keyDown。
///
/// macOS 应用识别组合键读的是**事件自带的 flags 字段**——单独 post 一个 kVK_Command
/// keyDown 不会让紧随其后新建的主键事件带上 Command 位，目标应用只会看到裸 `v`，
/// 导致 `key.tap("Cmd+v")`（clip.paste）等被当作普通字符而非快捷键（⌘V/⌘C 全失效）。
/// 故把 mods 折叠成 flags 设到主键 keyDown/keyUp 上，成对投递。
#[cfg(target_os = "macos")]
fn tap_combo(mods: &[u32], vk: u32) -> anyhow::Result<()> {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    let keycode = vk_to_cgkeycode(vk)
        .ok_or_else(|| anyhow::anyhow!("key 注入：vk={vk:#x} 无 macOS CGKeyCode 映射"))?;
    let mut flags = CGEventFlags::CGEventFlagNull;
    for m in mods {
        flags |= match *m {
            VK_LWIN => CGEventFlags::CGEventFlagCommand,
            VK_SHIFT => CGEventFlags::CGEventFlagShift,
            VK_CONTROL => CGEventFlags::CGEventFlagControl,
            VK_MENU => CGEventFlags::CGEventFlagAlternate,
            _ => CGEventFlags::CGEventFlagNull,
        };
    }
    for down in [true, false] {
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("CGEventSource 创建失败"))?;
        let event = CGEvent::new_keyboard_event(src, keycode, down)
            .map_err(|_| anyhow::anyhow!("CGEvent 键盘事件创建失败 (vk={vk:#x})"))?;
        event.set_flags(flags);
        event.post(CGEventTapLocation::HID);
    }
    Ok(())
}

/// 模拟单次组合：修饰键按下 → 主键按下抬起 → 修饰键反序抬起。
#[cfg(not(target_os = "macos"))]
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

/// Win32 虚拟键码 → macOS ANSI CGKeyCode（Carbon HIToolbox `kVK_*` 值）。
///
/// `parse_combo` 产出的是 Win32 VK，而 macOS CGEvent 用 ANSI 虚拟键位码（与 VK 不同），
/// 故注入前需经此表换算。覆盖 `parse_key` 能产出的全部键（修饰键 / 功能键 / F1-F12 /
/// 字母 / 数字）；未覆盖的 VK（如 OEM 符号键 0xBA..）返回 `None`，由调用方降级处理（不 panic）。
pub(crate) fn vk_to_cgkeycode(vk: u32) -> Option<u16> {
    let code: u16 = match vk {
        // 修饰键
        0x10 => 56, // VK_SHIFT   -> kVK_Shift
        0x11 => 59, // VK_CONTROL -> kVK_Control
        0x12 => 58, // VK_MENU    -> kVK_Option (Alt)
        0x5B => 55, // VK_LWIN    -> kVK_Command
        // 功能 / 编辑 / 导航键
        0x0D => 36,  // Enter      -> kVK_Return
        0x09 => 48,  // Tab        -> kVK_Tab
        0x1B => 53,  // Esc        -> kVK_Escape
        0x20 => 49,  // Space      -> kVK_Space
        0x08 => 51,  // Backspace  -> kVK_Delete
        0x2E => 117, // Delete     -> kVK_ForwardDelete
        0x24 => 115, // Home       -> kVK_Home
        0x23 => 119, // End        -> kVK_End
        0x21 => 116, // PageUp     -> kVK_PageUp
        0x22 => 121, // PageDown   -> kVK_PageDown
        0x25 => 123, // Left       -> kVK_LeftArrow
        0x26 => 126, // Up         -> kVK_UpArrow
        0x27 => 124, // Right      -> kVK_RightArrow
        0x28 => 125, // Down       -> kVK_DownArrow
        // F1-F12（CGKeyCode 不连续，按 kVK_F1..F12 表）
        0x70 => 122, // F1
        0x71 => 120, // F2
        0x72 => 99,  // F3
        0x73 => 118, // F4
        0x74 => 96,  // F5
        0x75 => 97,  // F6
        0x76 => 98,  // F7
        0x77 => 100, // F8
        0x78 => 101, // F9
        0x79 => 109, // F10
        0x7A => 103, // F11
        0x7B => 111, // F12
        // 字母 A-Z（0x41-0x5A）→ ANSI 键位码
        0x41 => 0,  // A
        0x42 => 11, // B
        0x43 => 8,  // C
        0x44 => 2,  // D
        0x45 => 14, // E
        0x46 => 3,  // F
        0x47 => 5,  // G
        0x48 => 4,  // H
        0x49 => 34, // I
        0x4A => 38, // J
        0x4B => 40, // K
        0x4C => 37, // L
        0x4D => 46, // M
        0x4E => 45, // N
        0x4F => 31, // O
        0x50 => 35, // P
        0x51 => 12, // Q
        0x52 => 15, // R
        0x53 => 1,  // S
        0x54 => 17, // T
        0x55 => 32, // U
        0x56 => 9,  // V
        0x57 => 13, // W
        0x58 => 7,  // X
        0x59 => 16, // Y
        0x5A => 6,  // Z
        // 数字 0-9（0x30-0x39）→ ANSI 数字键位码
        0x30 => 29, // 0
        0x31 => 18, // 1
        0x32 => 19, // 2
        0x33 => 20, // 3
        0x34 => 21, // 4
        0x35 => 23, // 5
        0x36 => 22, // 6
        0x37 => 26, // 7
        0x38 => 28, // 8
        0x39 => 25, // 9
        _ => return None,
    };
    Some(code)
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

// ───────────────────────── macOS Core Graphics（CGEvent）─────────────────────────
// macOS 用 ANSI CGKeyCode（≠ Win32 VK），经 `vk_to_cgkeycode` 换算后发 CGEvent：
//   - send_key → `CGEvent::new_keyboard_event(src, keycode, keydown)` + `event.post(HID)`
//   - type_unicode → 基础键盘事件上 `event.set_string(text)`（CGEventKeyboardSetUnicodeString 封装）
//     按下/抬起各发一次。
// `CGEventSource`/`CGEvent` 为 foreign_type（非 Clone），每次按下/抬起重建 source。
// 需「辅助功能」授权（系统设置 → 隐私与安全性 → 辅助功能）事件方能投递，否则被系统静默吞掉。

#[cfg(target_os = "macos")]
fn send_key(vk: u32, up: bool) -> anyhow::Result<()> {
    use core_graphics::event::{CGEvent, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    let keycode = vk_to_cgkeycode(vk)
        .ok_or_else(|| anyhow::anyhow!("key 注入：vk={vk:#x} 无 macOS CGKeyCode 映射"))?;
    let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource 创建失败"))?;
    let event = CGEvent::new_keyboard_event(src, keycode, !up)
        .map_err(|_| anyhow::anyhow!("CGEvent 键盘事件创建失败 (vk={vk:#x}, up={up})"))?;
    event.post(CGEventTapLocation::HID);
    Ok(())
}

#[cfg(target_os = "macos")]
fn type_unicode(text: &str) -> anyhow::Result<()> {
    use core_graphics::event::{CGEvent, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    for down in [true, false] {
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("CGEventSource 创建失败"))?;
        // keycode=0 占位；实际字符由 set_string 注入（CGEventKeyboardSetUnicodeString）。
        let event = CGEvent::new_keyboard_event(src, 0, down)
            .map_err(|_| anyhow::anyhow!("CGEvent 键盘事件创建失败 (type_unicode)"))?;
        event.set_string(text);
        event.post(CGEventTapLocation::HID);
    }
    Ok(())
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

    #[test]
    fn vk_to_cgkeycode_modifiers() {
        assert_eq!(vk_to_cgkeycode(0x10), Some(56)); // Shift
        assert_eq!(vk_to_cgkeycode(0x11), Some(59)); // Control
        assert_eq!(vk_to_cgkeycode(0x12), Some(58)); // Option(Alt)
        assert_eq!(vk_to_cgkeycode(0x5B), Some(55)); // Command(LWin)
    }

    #[test]
    fn vk_to_cgkeycode_function_keys() {
        assert_eq!(vk_to_cgkeycode(0x0D), Some(36)); // Return
        assert_eq!(vk_to_cgkeycode(0x09), Some(48)); // Tab
        assert_eq!(vk_to_cgkeycode(0x1B), Some(53)); // Escape
        assert_eq!(vk_to_cgkeycode(0x20), Some(49)); // Space
        assert_eq!(vk_to_cgkeycode(0x08), Some(51)); // Backspace -> Delete
        assert_eq!(vk_to_cgkeycode(0x2E), Some(117)); // Delete -> ForwardDelete
        assert_eq!(vk_to_cgkeycode(0x24), Some(115)); // Home
        assert_eq!(vk_to_cgkeycode(0x23), Some(119)); // End
        assert_eq!(vk_to_cgkeycode(0x21), Some(116)); // PageUp
        assert_eq!(vk_to_cgkeycode(0x22), Some(121)); // PageDown
        assert_eq!(vk_to_cgkeycode(0x25), Some(123)); // Left
        assert_eq!(vk_to_cgkeycode(0x26), Some(126)); // Up
        assert_eq!(vk_to_cgkeycode(0x27), Some(124)); // Right
        assert_eq!(vk_to_cgkeycode(0x28), Some(125)); // Down
    }

    #[test]
    fn vk_to_cgkeycode_f_keys() {
        assert_eq!(vk_to_cgkeycode(0x70), Some(122)); // F1
        assert_eq!(vk_to_cgkeycode(0x71), Some(120)); // F2
        assert_eq!(vk_to_cgkeycode(0x72), Some(99)); // F3
        assert_eq!(vk_to_cgkeycode(0x73), Some(118)); // F4
        assert_eq!(vk_to_cgkeycode(0x74), Some(96)); // F5
        assert_eq!(vk_to_cgkeycode(0x75), Some(97)); // F6
        assert_eq!(vk_to_cgkeycode(0x76), Some(98)); // F7
        assert_eq!(vk_to_cgkeycode(0x77), Some(100)); // F8
        assert_eq!(vk_to_cgkeycode(0x78), Some(101)); // F9
        assert_eq!(vk_to_cgkeycode(0x79), Some(109)); // F10
        assert_eq!(vk_to_cgkeycode(0x7A), Some(103)); // F11
        assert_eq!(vk_to_cgkeycode(0x7B), Some(111)); // F12
    }

    #[test]
    fn vk_to_cgkeycode_letters() {
        assert_eq!(vk_to_cgkeycode(0x41), Some(0)); // A
        assert_eq!(vk_to_cgkeycode(0x53), Some(1)); // S
        assert_eq!(vk_to_cgkeycode(0x5A), Some(6)); // Z
        assert_eq!(vk_to_cgkeycode(0x4D), Some(46)); // M
        assert_eq!(vk_to_cgkeycode(0x51), Some(12)); // Q
        assert_eq!(vk_to_cgkeycode(0x50), Some(35)); // P
    }

    #[test]
    fn vk_to_cgkeycode_digits() {
        assert_eq!(vk_to_cgkeycode(0x31), Some(18)); // 1
        assert_eq!(vk_to_cgkeycode(0x32), Some(19)); // 2
        assert_eq!(vk_to_cgkeycode(0x35), Some(23)); // 5
        assert_eq!(vk_to_cgkeycode(0x36), Some(22)); // 6
        assert_eq!(vk_to_cgkeycode(0x37), Some(26)); // 7
        assert_eq!(vk_to_cgkeycode(0x39), Some(25)); // 9
        assert_eq!(vk_to_cgkeycode(0x30), Some(29)); // 0
    }

    #[test]
    fn vk_to_cgkeycode_unknown() {
        assert_eq!(vk_to_cgkeycode(0xFFFF), None);
        assert_eq!(vk_to_cgkeycode(0xBA), None); // OEM_1 ; 未覆盖
        assert_eq!(vk_to_cgkeycode(0x00), None);
    }
}
