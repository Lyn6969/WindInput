//! 全角/半角转换
//!
//! 与 Go 版本 `wind_input/internal/transform/fullwidth.go` 对齐。

/// ASCII 转全角
pub fn to_full_width(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '!'..='~' => char::from_u32(c as u32 - 0x21 + 0xFF01).unwrap(),
            ' ' => '\u{3000}',
            _ => c,
        })
        .collect()
}

/// 全角转半角
pub fn to_half_width(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFF01 + 0x21).unwrap(),
            '\u{3000}' => ' ',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_width() {
        assert_eq!(to_full_width("abc"), "ａｂｃ");
        assert_eq!(to_full_width(" "), "　");
    }

    #[test]
    fn test_half_width() {
        assert_eq!(to_half_width("ａｂｃ"), "abc");
        assert_eq!(to_half_width("　"), " ");
    }
}
