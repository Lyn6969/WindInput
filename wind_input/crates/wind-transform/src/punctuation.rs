//! 标点转换器
//!
//! 与 Go 版本 `wind_input/internal/transform/punctuation.go` 对齐。
//! 英文标点 → 中文标点；引号根据左右状态切换。

use std::collections::HashMap;

/// 标点转换器（持有引号左右状态 + 自定义映射表）
pub struct PunctuationConverter {
    single_quote_left: bool,
    double_quote_left: bool,
    custom_enabled: bool,
    /// key=源字符（引号用 `"1`/`"2`/`'1`/`'2`），value=[中半,英全,中全,英半]
    custom_mappings: HashMap<String, Vec<String>>,
}

impl Default for PunctuationConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl PunctuationConverter {
    pub fn new() -> Self {
        Self {
            single_quote_left: true,
            double_quote_left: true,
            custom_enabled: false,
            custom_mappings: HashMap::new(),
        }
    }

    /// 设置自定义标点映射（配置加载/热更时调用）。
    pub fn set_custom_mappings(&mut self, enabled: bool, mappings: HashMap<String, Vec<String>>) {
        self.custom_enabled = enabled;
        self.custom_mappings = mappings;
    }

    /// 重置引号状态（模式切换/清空时调用）。不清自定义映射（配置态）。
    pub fn reset(&mut self) {
        self.single_quote_left = true;
        self.double_quote_left = true;
    }

    /// 查找自定义映射。`col_idx`: 0=中文半角 1=英文全角 2=中文全角 3=英文半角。
    /// 引号按当前左右态选 `"1`/`"2`/`'1`/`'2` 作为 key；命中（非空）时切换引号态并返回。
    /// 未命中不切换状态。对齐 Go `PunctuationConverter.LookupCustom`。
    pub fn lookup_custom(&mut self, c: char, col_idx: usize) -> Option<String> {
        if !self.custom_enabled || self.custom_mappings.is_empty() {
            return None;
        }
        let (key, is_quote) = match c {
            '"' => (if self.double_quote_left { "\"1" } else { "\"2" }.to_string(), true),
            '\'' => (if self.single_quote_left { "'1" } else { "'2" }.to_string(), true),
            _ => (c.to_string(), false),
        };
        let vals = self.custom_mappings.get(&key)?;
        let v = vals.get(col_idx)?;
        if v.is_empty() {
            return None;
        }
        if is_quote {
            match c {
                '"' => self.double_quote_left = !self.double_quote_left,
                '\'' => self.single_quote_left = !self.single_quote_left,
                _ => {}
            }
        }
        Some(v.clone())
    }

    /// 预测 `c` 的中文标点产物但**不**改引号状态（智能符号武装/匹配用）。
    /// 对齐 Go `PeekChineseStr`。返回 None 表示该键无中文标点映射。
    pub fn peek_chinese_str(&self, c: char) -> Option<String> {
        match c {
            '\'' => Some(if self.single_quote_left { '\u{2018}' } else { '\u{2019}' }.to_string()),
            '"' => Some(if self.double_quote_left { '\u{201C}' } else { '\u{201D}' }.to_string()),
            _ => Self::static_chinese(c),
        }
    }

    /// 回退一次引号交替（智能符号吃掉一个引号后调用，使下次同引号仍从左引号开始）。
    /// 对齐 Go `RevertLastQuote`。
    pub fn revert_last_quote(&mut self, c: char) {
        match c {
            '\'' => self.single_quote_left = !self.single_quote_left,
            '"' => self.double_quote_left = !self.double_quote_left,
            _ => {}
        }
    }

    /// 无状态中文标点映射（非引号部分），供 to_chinese / peek 共用。
    fn static_chinese(c: char) -> Option<String> {
        match c {
            '^' => return Some("\u{2026}\u{2026}".to_string()), // ……
            '_' => return Some("\u{2014}\u{2014}".to_string()), // ——
            _ => {}
        }
        let mapped = match c {
            ',' => '\u{FF0C}',  // ，
            '.' => '\u{3002}',  // 。
            '?' => '\u{FF1F}',  // ？
            '!' => '\u{FF01}',  // ！
            ':' => '\u{FF1A}',  // ：
            ';' => '\u{FF1B}',  // ；
            '(' => '\u{FF08}',  // （
            ')' => '\u{FF09}',  // ）
            '[' => '\u{3010}',  // 【
            ']' => '\u{3011}',  // 】
            '{' => '\u{FF5B}',  // ｛
            '}' => '\u{FF5D}',  // ｝
            '<' => '\u{300A}',  // 《
            '>' => '\u{300B}',  // 》
            '~' => '\u{FF5E}',  // ～
            '$' => '\u{FFE5}',  // ￥
            '`' => '\u{00B7}',  // ·
            '\\' => '\u{3001}', // 、
            _ => return None,
        };
        Some(mapped.to_string())
    }

    /// 英文标点 → 中文标点；返回 None 表示该字符无中文标点映射。
    /// 结果可能是多字符（如 `^`→`……`），故返回 String。
    pub fn to_chinese(&mut self, c: char) -> Option<String> {
        // 引号需切换左右
        match c {
            '\'' => {
                let r = if self.single_quote_left { '\u{2018}' } else { '\u{2019}' };
                self.single_quote_left = !self.single_quote_left;
                return Some(r.to_string());
            }
            '"' => {
                let r = if self.double_quote_left { '\u{201C}' } else { '\u{201D}' };
                self.double_quote_left = !self.double_quote_left;
                return Some(r.to_string());
            }
            _ => {}
        }
        Self::static_chinese(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_punct() {
        let mut p = PunctuationConverter::new();
        assert_eq!(p.to_chinese(',').as_deref(), Some("，"));
        assert_eq!(p.to_chinese('.').as_deref(), Some("。"));
        assert_eq!(p.to_chinese('\\').as_deref(), Some("、"));
        assert_eq!(p.to_chinese('^').as_deref(), Some("……"));
        assert_eq!(p.to_chinese('a'), None);
    }

    #[test]
    fn test_custom_mapping() {
        let mut p = PunctuationConverter::new();
        let mut m = HashMap::new();
        // '/' 自定义：中半=、 英全=／ 中全=、 英半=/
        m.insert(
            "/".to_string(),
            vec!["、".into(), "／".into(), "、".into(), "/".into()],
        );
        // 引号：左右分键，仅配中文半角列（col 0）
        m.insert("\"1".to_string(), vec!["「".into()]);
        m.insert("\"2".to_string(), vec!["」".into()]);
        p.set_custom_mappings(true, m);
        assert_eq!(p.lookup_custom('/', 0).as_deref(), Some("、")); // 中文半角
        assert_eq!(p.lookup_custom('/', 1).as_deref(), Some("／")); // 英文全角
        assert_eq!(p.lookup_custom('/', 3).as_deref(), Some("/")); // 英文半角
        assert_eq!(p.lookup_custom('a', 0), None); // 无映射
        // 引号按左右交替选 key 并切换状态
        assert_eq!(p.lookup_custom('"', 0).as_deref(), Some("「")); // 左
        assert_eq!(p.lookup_custom('"', 0).as_deref(), Some("」")); // 右
    }

    #[test]
    fn test_custom_disabled() {
        let mut p = PunctuationConverter::new();
        let mut m = HashMap::new();
        m.insert("/".to_string(), vec!["、".into()]);
        p.set_custom_mappings(false, m);
        assert_eq!(p.lookup_custom('/', 0), None); // 未启用
    }

    #[test]
    fn test_peek_and_revert() {
        let mut p = PunctuationConverter::new();
        // peek 不改状态：连续 peek 同引号返回相同（左）
        assert_eq!(p.peek_chinese_str('"').as_deref(), Some("\u{201C}"));
        assert_eq!(p.peek_chinese_str('"').as_deref(), Some("\u{201C}"));
        assert_eq!(p.peek_chinese_str('.').as_deref(), Some("。"));
        // to_chinese 推进到右，revert 退回左
        assert_eq!(p.to_chinese('"').as_deref(), Some("\u{201C}")); // 左→推进
        p.revert_last_quote('"');
        assert_eq!(p.to_chinese('"').as_deref(), Some("\u{201C}")); // revert 后仍为左
    }

    #[test]
    fn test_quote_toggle() {
        let mut p = PunctuationConverter::new();
        assert_eq!(p.to_chinese('"').as_deref(), Some("\u{201C}")); // 左
        assert_eq!(p.to_chinese('"').as_deref(), Some("\u{201D}")); // 右
        assert_eq!(p.to_chinese('\'').as_deref(), Some("\u{2018}"));
        p.reset();
        assert_eq!(p.to_chinese('"').as_deref(), Some("\u{201C}")); // reset 后回到左
    }
}
