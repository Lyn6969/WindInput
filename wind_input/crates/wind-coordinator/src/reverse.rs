//! 候选反查（编码反查 / 拆字 / 拼音）
//!
//! 与 Go 版本 `wind_input/internal/tooltip/` 对齐（简化版）。
//! 为悬停候选提供"如何输入"的提示：五笔编码（拆字）+ 拼音读音。
//!
//! 数据源：
//! - 拆字/五笔码：`schemas/wubi86/wubi86_chaizi.txt`（字\t字根\t五笔编码）
//! - 拼音：`pinyin_map.txt`（kMandarin 格式：`U+4E00: yī  # 一`）

use std::collections::HashMap;
use std::path::Path;

/// 反查表
#[derive(Default)]
pub struct ReverseLookup {
    /// 字 → 五笔编码
    code: HashMap<char, String>,
    /// 字 → 拼音
    pinyin: HashMap<char, String>,
}

impl ReverseLookup {
    pub fn load(data_dir: Option<&Path>) -> Self {
        let mut rl = Self::default();
        if let Some(dir) = data_dir {
            rl.load_chaizi(&dir.join("schemas/wubi86/wubi86_chaizi.txt"));
            rl.load_pinyin(&dir.join("pinyin_map.txt"));
        }
        rl
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty() && self.pinyin.is_empty()
    }

    /// 载入五笔拆字库（字\t字根\t编码）；取编码列。
    fn load_chaizi(&mut self, path: &Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for line in content.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split('\t');
            let (Some(ch), _radicals, code) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let mut chars = ch.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                if let Some(code) = code {
                    let code = code.trim();
                    if !code.is_empty() {
                        self.code.insert(c, code.to_string());
                    }
                }
            }
        }
    }

    /// 载入拼音表（kMandarin 格式：`U+4E00: yī  # 一`）。
    fn load_pinyin(&mut self, path: &Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for line in content.lines() {
            let line = line.trim();
            if !line.starts_with("U+") {
                continue;
            }
            let Some((hexpart, rest)) = line.split_once(':') else {
                continue;
            };
            let hex = hexpart.trim_start_matches("U+").trim();
            let Ok(cp) = u32::from_str_radix(hex, 16) else {
                continue;
            };
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            // 取第一个非标记 token 作为读音
            let py = rest
                .split_whitespace()
                .find(|t| *t != "->" && *t != "?" && *t != "<-" && !t.starts_with('#'));
            if let Some(py) = py {
                if !py.is_empty() {
                    self.pinyin.insert(c, py.to_string());
                }
            }
        }
    }

    /// 为候选文本生成反查提示（逐字一行："字  编码  拼音"）。无可用信息返回空串。
    pub fn tooltip_for(&self, text: &str) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut lines = Vec::new();
        for c in text.chars() {
            // 跳过 ASCII / 非汉字
            if (c as u32) < 0x3400 {
                continue;
            }
            let code = self.code.get(&c);
            let py = self.pinyin.get(&c);
            if code.is_none() && py.is_none() {
                continue;
            }
            let mut line = c.to_string();
            if let Some(py) = py {
                line.push_str("  ");
                line.push_str(py);
            }
            if let Some(code) = code {
                line.push_str("  ");
                line.push_str(code);
            }
            lines.push(line);
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tooltip_format() {
        let mut rl = ReverseLookup::default();
        rl.code.insert('好', "vbg".to_string());
        rl.pinyin.insert('好', "hǎo".to_string());
        rl.code.insert('人', "w".to_string());
        let t = rl.tooltip_for("好人");
        assert!(t.contains("好"));
        assert!(t.contains("hǎo"));
        assert!(t.contains("vbg"));
        assert!(t.contains('\n'), "多字应多行");
        // 纯 ASCII 无反查
        assert_eq!(rl.tooltip_for("abc"), "");
    }
}
