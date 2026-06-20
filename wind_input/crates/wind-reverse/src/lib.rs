//! 候选反查（编码反查 / 拆字 / 拼音）
//!
//! 与 Go 版本 `wind_input/internal/tooltip/` 对齐（简化版）。
//! 为悬停候选提供"如何输入"的提示：五笔编码（拆字）+ 拼音读音。
//!
//! 数据源：
//! - 拆字/五笔码：`schemas/wubi86/wubi86_chaizi.txt`（字\t字根\t五笔编码）
//! - 拼音：`pinyin_map.txt`（pinyin-data 格式：`U+4E00: yī  # 一`，多音字逗号分隔）
//!   由 wind-tools `gen_pinyin` 从 mozillazg/pinyin-data 合并生成。

use std::collections::HashMap;
use std::path::Path;

/// 反查表
#[derive(Default)]
pub struct ReverseLookup {
    /// 字 → 五笔编码
    code: HashMap<char, String>,
    /// 字 → 拼音读音（多音字按常用频率排序，最常用在前）
    pinyin: HashMap<char, Vec<String>>,
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
            if let (Some(c), None) = (chars.next(), chars.next())
                && let Some(code) = code
            {
                let code = code.trim();
                if !code.is_empty() {
                    self.code.insert(c, code.to_string());
                }
            }
        }
    }

    /// 载入拼音表（pinyin-data 格式：`U+4E00: yī  # 一`，多音字逗号分隔）。
    fn load_pinyin(&mut self, path: &Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for line in content.lines() {
            let mut line = line.trim();
            if !line.starts_with("U+") {
                continue;
            }
            // 去掉行内 `# 汉字` 注释
            if let Some(idx) = line.find('#') {
                line = line[..idx].trim();
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
            // 逗号分隔多音字读音，首项为最常用读音
            let readings: Vec<String> = rest
                .trim()
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            if !readings.is_empty() {
                self.pinyin.insert(c, readings);
            }
        }
    }

    /// 生成词的拼音编码（空格分隔、去声调小写；ü→v）。无读音的字跳过。
    /// 用于设置页 dict.genPinyin / 拼音方案加词自动出码。
    pub fn gen_pinyin(&self, text: &str) -> String {
        text.chars()
            .filter_map(|c| self.pinyin.get(&c).and_then(|r| r.first()))
            .map(|py| strip_tone(py))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// 五笔词组取码（86 版首码法）：1字=全码；2字=各取前2码；3字=前2字各首码+末字前2码；
    /// ≥4字=前3字首码+末字首码。用于码表方案加词自动出码。无码的字按空串跳过。
    pub fn wubi_word_code(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let firstn = |c: char, n: usize| -> String {
            self.code
                .get(&c)
                .map(|s| s.chars().take(n).collect())
                .unwrap_or_default()
        };
        match chars.len() {
            0 => String::new(),
            1 => self.code.get(&chars[0]).cloned().unwrap_or_default(),
            2 => format!("{}{}", firstn(chars[0], 2), firstn(chars[1], 2)),
            3 => format!(
                "{}{}{}",
                firstn(chars[0], 1),
                firstn(chars[1], 1),
                firstn(chars[2], 2)
            ),
            _ => {
                let last = *chars.last().unwrap();
                format!(
                    "{}{}{}{}",
                    firstn(chars[0], 1),
                    firstn(chars[1], 1),
                    firstn(chars[2], 1),
                    firstn(last, 1)
                )
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
                // 多音字读音以 "/" 连接（与 Go tooltip 一致）
                line.push_str("  ");
                line.push_str(&py.join("/"));
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

/// 去声调：带调号韵母 → 基本字母（ü→v，符合拼音输入习惯）。
fn strip_tone(py: &str) -> String {
    py.chars()
        .map(|c| match c {
            'ā' | 'á' | 'ǎ' | 'à' => 'a',
            'ō' | 'ó' | 'ǒ' | 'ò' => 'o',
            'ē' | 'é' | 'ě' | 'è' => 'e',
            'ī' | 'í' | 'ǐ' | 'ì' => 'i',
            'ū' | 'ú' | 'ǔ' | 'ù' => 'u',
            'ǖ' | 'ǘ' | 'ǚ' | 'ǜ' | 'ü' => 'v',
            other => other,
        })
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_tone() {
        assert_eq!(strip_tone("nǐ"), "ni");
        assert_eq!(strip_tone("hǎo"), "hao");
        assert_eq!(strip_tone("lǜ"), "lv");
    }

    #[test]
    fn test_wubi_word_code_rules() {
        let mut rl = ReverseLookup::default();
        rl.code.insert('工', "aaaa".into());
        rl.code.insert('人', "wwww".into());
        rl.code.insert('大', "dddd".into());
        rl.code.insert('小', "ihty".into());
        // 1字=全码
        assert_eq!(rl.wubi_word_code("工"), "aaaa");
        // 2字=各前2码
        assert_eq!(rl.wubi_word_code("工人"), "aaww");
        // 3字=前2字首码+末字前2码
        assert_eq!(rl.wubi_word_code("工人大"), "awdd");
        // ≥4字=前3字首码+末字首码
        assert_eq!(rl.wubi_word_code("工人大小"), "awdi");
    }

    #[test]
    fn test_tooltip_format() {
        let mut rl = ReverseLookup::default();
        rl.code.insert('好', "vbg".to_string());
        rl.pinyin.insert('好', vec!["hǎo".to_string()]);
        rl.code.insert('人', "w".to_string());
        let t = rl.tooltip_for("好人");
        assert!(t.contains("好"));
        assert!(t.contains("hǎo"));
        assert!(t.contains("vbg"));
        assert!(t.contains('\n'), "多字应多行");
        // 纯 ASCII 无反查
        assert_eq!(rl.tooltip_for("abc"), "");
    }

    #[test]
    fn test_gen_pinyin_uses_first_reading() {
        let mut rl = ReverseLookup::default();
        // 多音字"重"：首音 zhòng（最常用），次音 chóng
        rl.pinyin
            .insert('重', vec!["zhòng".to_string(), "chóng".to_string()]);
        rl.pinyin.insert('要', vec!["yào".to_string()]);
        assert_eq!(rl.gen_pinyin("重要"), "zhong yao");
    }

    #[test]
    fn test_tooltip_multi_reading_joined() {
        let mut rl = ReverseLookup::default();
        rl.pinyin
            .insert('重', vec!["zhòng".to_string(), "chóng".to_string()]);
        let t = rl.tooltip_for("重");
        assert!(t.contains("zhòng/chóng"), "多音字读音应以 / 连接: {t}");
    }

    #[test]
    fn test_load_pinyin_parses_multi_reading() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("wind-reverse-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pinyin_map.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "# 头部注释").unwrap();
        writeln!(f, "U+4E00: yī  # 一").unwrap();
        writeln!(f, "U+91CD: zhòng,chóng  # 重").unwrap();
        drop(f);

        let mut rl = ReverseLookup::default();
        rl.load_pinyin(&path);
        assert_eq!(rl.pinyin.get(&'一').unwrap(), &vec!["yī".to_string()]);
        assert_eq!(
            rl.pinyin.get(&'重').unwrap(),
            &vec!["zhòng".to_string(), "chóng".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
