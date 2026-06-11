//! 模糊音配置与匹配
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/fuzzy.go` 对齐。
//! 允许用户输入时忽略常见发音混淆（如 z/zh, c/ch, s/sh, n/l）。

/// 模糊音配置
#[derive(Debug, Clone)]
pub struct FuzzyConfig {
    pub zh_z: bool,
    pub ch_c: bool,
    pub sh_s: bool,
    pub n_l: bool,
    pub f_h: bool,
    pub r_l: bool,
    pub an_ang: bool,
    pub en_eng: bool,
    pub in_ing: bool,
    pub ian_iang: bool,
    pub uan_uang: bool,
}

impl Default for FuzzyConfig {
    fn default() -> Self {
        Self {
            zh_z: false,
            ch_c: false,
            sh_s: false,
            n_l: false,
            f_h: false,
            r_l: false,
            an_ang: false,
            en_eng: false,
            in_ing: false,
            ian_iang: false,
            uan_uang: false,
        }
    }
}

/// 模糊音规则
struct FuzzyRule {
    from: &'static str,
    to: &'static str,
    flag: fn(&FuzzyConfig) -> bool,
}

/// 获取所有模糊音规则
fn fuzzy_rules() -> Vec<FuzzyRule> {
    vec![
        FuzzyRule { from: "zh", to: "z", flag: |c| c.zh_z },
        FuzzyRule { from: "z", to: "zh", flag: |c| c.zh_z },
        FuzzyRule { from: "ch", to: "c", flag: |c| c.ch_c },
        FuzzyRule { from: "c", to: "ch", flag: |c| c.ch_c },
        FuzzyRule { from: "sh", to: "s", flag: |c| c.sh_s },
        FuzzyRule { from: "s", to: "sh", flag: |c| c.sh_s },
        FuzzyRule { from: "n", to: "l", flag: |c| c.n_l },
        FuzzyRule { from: "l", to: "n", flag: |c| c.n_l },
        FuzzyRule { from: "f", to: "h", flag: |c| c.f_h },
        FuzzyRule { from: "h", to: "f", flag: |c| c.f_h },
        FuzzyRule { from: "r", to: "l", flag: |c| c.r_l },
        FuzzyRule { from: "l", to: "r", flag: |c| c.r_l },
    ]
}

/// 韵母模糊音规则
fn fuzzy_final_rules() -> Vec<FuzzyRule> {
    vec![
        FuzzyRule { from: "ang", to: "an", flag: |c| c.an_ang },
        FuzzyRule { from: "an", to: "ang", flag: |c| c.an_ang },
        FuzzyRule { from: "eng", to: "en", flag: |c| c.en_eng },
        FuzzyRule { from: "en", to: "eng", flag: |c| c.en_eng },
        FuzzyRule { from: "ing", to: "in", flag: |c| c.in_ing },
        FuzzyRule { from: "in", to: "ing", flag: |c| c.in_ing },
        FuzzyRule { from: "iang", to: "ian", flag: |c| c.ian_iang },
        FuzzyRule { from: "ian", to: "iang", flag: |c| c.ian_iang },
        FuzzyRule { from: "uang", to: "uan", flag: |c| c.uan_uang },
        FuzzyRule { from: "uan", to: "uang", flag: |c| c.uan_uang },
    ]
}

/// 模糊拼音匹配器
pub struct FuzzyMatcher;

impl FuzzyMatcher {
    /// 生成模糊变体列表
    ///
    /// 对于输入 "zhuo"，如果 zh_z 启用，返回 ["zuo"]。
    /// 对于输入 "zan"，如果 an_ang 启用，返回 ["zang"]。
    pub fn fuzzy_variants(input: &str, config: &FuzzyConfig) -> Vec<String> {
        let mut variants = Vec::new();

        // 声母模糊
        for rule in fuzzy_rules() {
            if !(rule.flag)(config) {
                continue;
            }
            if input.starts_with(rule.from) {
                let variant = format!("{}{}", rule.to, &input[rule.from.len()..]);
                if variant != input {
                    variants.push(variant);
                }
            }
        }

        // 韵母模糊
        for rule in fuzzy_final_rules() {
            if !(rule.flag)(config) {
                continue;
            }
            if let Some(pos) = input.find(rule.from) {
                let variant = format!(
                    "{}{}{}",
                    &input[..pos],
                    rule.to,
                    &input[pos + rule.from.len()..]
                );
                if variant != input {
                    variants.push(variant);
                }
            }
        }

        variants
    }

    /// 检查两个拼音是否模糊等价
    pub fn is_fuzzy_equal(a: &str, b: &str, config: &FuzzyConfig) -> bool {
        if a == b {
            return true;
        }

        let variants = Self::fuzzy_variants(a, config);
        variants.contains(&b.to_string())
    }
}
