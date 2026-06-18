//! wind-punct: 标点转换纯逻辑（从 wind-coordinator 抽出，可原生测试）。
//!
//! 与 Go `wind_input/internal/coordinator/handle_punctuation.go` 对齐。所有函数无副作用
//! （除经 `&mut PunctuationConverter` 推进引号状态机外），输入为 `&InputConfig` 配置 +
//! 当前模式布尔（chinese_punct / full_width），便于单测。
//!
//! 转换优先级（对齐 Go `convertPunct`）：自定义映射 → 数字后智能 → 中文标点 → 全半角。

use wind_config::config::InputConfig;
use wind_transform::fullwidth::to_full_width;
use wind_transform::punctuation::PunctuationConverter;

/// 数字后智能标点：中文标点模式下，若 ch 在智能标点列表且光标前一字符为数字，
/// 则该标点按英文（半角）输出（如 "3." 不转 "3。"）。`prev_char` 为 UTF-16 单元（0=不可用）。
pub fn is_smart_punct_after_digit(cfg: &InputConfig, ch: char, prev_char: u16) -> bool {
    if !cfg.smart_punct_after_digit {
        return false;
    }
    let list = &cfg.smart_punct_list;
    let in_list = if list.is_empty() {
        ch == '.' || ch == ','
    } else {
        list.contains(ch)
    };
    if !in_list {
        return false;
    }
    // 数字 '0'..='9' = 0x30..=0x39
    (0x30..=0x39).contains(&prev_char)
}

/// 纯查表读自定义标点映射的指定列（不碰转换器引号状态），供无副作用计算用。
/// 四状态列：中半 0 / 英全 1 / 中全 2 / 英半 3。
pub fn custom_lookup(cfg: &InputConfig, ch: char, col_idx: usize) -> Option<String> {
    let vals = cfg.punct_custom.mappings.get(&ch.to_string())?;
    let v = vals.get(col_idx)?;
    if v.is_empty() { None } else { Some(v.clone()) }
}

/// 标点转换单点流水线（对齐 Go `convertPunct`）。`conv` 推进引号状态机故取 `&mut`。
pub fn convert_punct(
    conv: &mut PunctuationConverter,
    cfg: &InputConfig,
    chinese_punct: bool,
    full_width: bool,
    ch: char,
    prev_char: u16,
) -> String {
    let smart_en = chinese_punct && is_smart_punct_after_digit(cfg, ch, prev_char);
    let is_chinese_punct = chinese_punct && !smart_en;

    // 1. 自定义映射优先（四状态均可配置）。
    if cfg.punct_custom.enabled {
        let col_idx = if is_chinese_punct && full_width {
            2 // 中文全角
        } else if is_chinese_punct {
            0 // 中文半角
        } else if full_width {
            1 // 英文全角
        } else {
            3 // 英文半角
        };
        if let Some(text) = conv.lookup_custom(ch, col_idx) {
            return text;
        }
    }

    // 2~4. 默认转换：中文标点（含引号状态机）→ 全半角。
    let mut piece = ch.to_string();
    if is_chinese_punct && let Some(c) = conv.to_chinese(ch) {
        piece = c;
    }
    if full_width {
        piece = to_full_width(&piece);
    }
    piece
}

/// 无副作用地计算 `ch` 在当前模式下的标点产物，**镜像** `convert_punct` 优先级。
/// `chinese=true` 算中文标点产物（引号经 peek 预测不改状态）；`chinese=false` 算英文产物
/// （替换用）。引号有状态、键名特殊，保守跳过自定义、走标准引号/英文产物。
pub fn compute_punct_str_pure(
    conv: &PunctuationConverter,
    cfg: &InputConfig,
    full_width: bool,
    ch: char,
    chinese: bool,
) -> Option<String> {
    let is_quote = ch == '\'' || ch == '"';

    if !is_quote && cfg.punct_custom.enabled {
        let col_idx = if chinese && full_width {
            Some(2) // 中文全角
        } else if chinese {
            Some(0) // 中文半角
        } else if full_width {
            Some(1) // 英文全角
        } else {
            None // 英文半角：pure 计算走原样
        };
        if let Some(ci) = col_idx
            && let Some(v) = custom_lookup(cfg, ch, ci)
        {
            return Some(v);
        }
    }

    let mut s = ch.to_string();
    if chinese {
        s = conv.peek_chinese_str(ch)?;
    }
    if full_width {
        s = to_full_width(&s);
    }
    Some(s)
}

/// 中文标点串 `cn` 是否在用户配置的智能符号参与集合内（子串包含匹配）。
pub fn participates(cfg: &InputConfig, cn: &str) -> bool {
    !cn.is_empty() && cfg.smart_symbol_chars.contains(cn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> InputConfig {
        InputConfig::default()
    }

    #[test]
    fn smart_punct_after_digit_default_list() {
        let c = cfg(); // 默认 smart_punct_after_digit=true, list=".,:"
        // '.' 在列表 + 前字符是数字 '5'(0x35) → true
        assert!(is_smart_punct_after_digit(&c, '.', 0x35));
        // 前字符非数字 → false
        assert!(!is_smart_punct_after_digit(&c, '.', b'a' as u16));
        // 不在列表的标点 → false
        assert!(!is_smart_punct_after_digit(&c, '!', 0x35));
    }

    #[test]
    fn convert_punct_chinese_and_fullwidth() {
        let mut conv = PunctuationConverter::new();
        let c = cfg();
        // 中文标点模式：'.' → '。'
        assert_eq!(convert_punct(&mut conv, &c, true, false, '.', 0), "。");
        // 英文标点模式 + 全角：'.' 走全半角 → '．'
        let out = convert_punct(&mut conv, &c, false, true, '.', 0);
        assert_ne!(out, "."); // 全角化
    }

    #[test]
    fn convert_punct_smart_digit_forces_english() {
        let mut conv = PunctuationConverter::new();
        let c = cfg();
        // 中文模式但前字符是数字 → '.' 按英文输出（不转 '。'）。
        assert_eq!(convert_punct(&mut conv, &c, true, false, '.', 0x33), ".");
    }

    #[test]
    fn compute_pure_mirrors_chinese() {
        let conv = PunctuationConverter::new();
        let c = cfg();
        // 中文产物：'.' → '。'（peek 不改状态）。
        assert_eq!(
            compute_punct_str_pure(&conv, &c, false, '.', true).as_deref(),
            Some("。")
        );
    }

    #[test]
    fn participates_substring_match() {
        let mut c = cfg();
        c.smart_symbol_chars = "。，".to_string();
        assert!(participates(&c, "。"));
        assert!(!participates(&c, "！"));
        assert!(!participates(&c, ""));
    }

    #[test]
    fn custom_lookup_empty_is_none() {
        let c = cfg(); // 默认无自定义映射
        assert_eq!(custom_lookup(&c, '.', 0), None);
    }
}
