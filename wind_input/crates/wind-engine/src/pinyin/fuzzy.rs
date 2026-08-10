//! 模糊音配置与匹配
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/fuzzy.go` 对齐。
//! 允许用户输入时忽略常见发音混淆（如 z/zh, c/ch, s/sh, n/l）。

/// 笛卡尔积展开的组合数上限（超出即放弃扩展，避免组合爆炸）。
pub const MAX_FUZZY_COMBOS: usize = 64;

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

impl FuzzyConfig {
    /// 是否有任一模糊组开启。供调用方在全关时跳过整段展开逻辑（含其前置的切分求解）。
    ///
    /// 注意**不能**用「对整串求 `fuzzy_variants` 得空」来代替这个判断：那既漏掉非首音节的
    /// 变体，又要求先拿到 code；本判断只看配置，可在任何前置计算之前短路。
    pub fn any_enabled(&self) -> bool {
        self.zh_z
            || self.ch_c
            || self.sh_s
            || self.n_l
            || self.f_h
            || self.r_l
            || self.an_ang
            || self.en_eng
            || self.in_ing
            || self.ian_iang
            || self.uan_uang
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
        FuzzyRule {
            from: "zh",
            to: "z",
            flag: |c| c.zh_z,
        },
        FuzzyRule {
            from: "z",
            to: "zh",
            flag: |c| c.zh_z,
        },
        FuzzyRule {
            from: "ch",
            to: "c",
            flag: |c| c.ch_c,
        },
        FuzzyRule {
            from: "c",
            to: "ch",
            flag: |c| c.ch_c,
        },
        FuzzyRule {
            from: "sh",
            to: "s",
            flag: |c| c.sh_s,
        },
        FuzzyRule {
            from: "s",
            to: "sh",
            flag: |c| c.sh_s,
        },
        FuzzyRule {
            from: "n",
            to: "l",
            flag: |c| c.n_l,
        },
        FuzzyRule {
            from: "l",
            to: "n",
            flag: |c| c.n_l,
        },
        FuzzyRule {
            from: "f",
            to: "h",
            flag: |c| c.f_h,
        },
        FuzzyRule {
            from: "h",
            to: "f",
            flag: |c| c.f_h,
        },
        FuzzyRule {
            from: "r",
            to: "l",
            flag: |c| c.r_l,
        },
        FuzzyRule {
            from: "l",
            to: "r",
            flag: |c| c.r_l,
        },
    ]
}

/// 韵母模糊音规则
fn fuzzy_final_rules() -> Vec<FuzzyRule> {
    vec![
        FuzzyRule {
            from: "ang",
            to: "an",
            flag: |c| c.an_ang,
        },
        FuzzyRule {
            from: "an",
            to: "ang",
            flag: |c| c.an_ang,
        },
        FuzzyRule {
            from: "eng",
            to: "en",
            flag: |c| c.en_eng,
        },
        FuzzyRule {
            from: "en",
            to: "eng",
            flag: |c| c.en_eng,
        },
        FuzzyRule {
            from: "ing",
            to: "in",
            flag: |c| c.in_ing,
        },
        FuzzyRule {
            from: "in",
            to: "ing",
            flag: |c| c.in_ing,
        },
        FuzzyRule {
            from: "iang",
            to: "ian",
            flag: |c| c.ian_iang,
        },
        FuzzyRule {
            from: "ian",
            to: "iang",
            flag: |c| c.ian_iang,
        },
        FuzzyRule {
            from: "uang",
            to: "uan",
            flag: |c| c.uan_uang,
        },
        FuzzyRule {
            from: "uan",
            to: "uang",
            flag: |c| c.uan_uang,
        },
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

    /// 逐音节展开模糊变体的笛卡尔积，拼成完整 code 列表（**含全原音节的原码本身**）。
    ///
    /// **必须逐音节调用 [`Self::fuzzy_variants`]，不可对多音节拼接串整体调用**：声母规则用
    /// `input.starts_with(rule.from)`、韵母规则用 `input.find(rule.from)`，对整串只能改到
    /// **第一个音节的声母**与**第一处**韵母匹配。`zhongzou`→`zhongzhou`（中州）这类
    /// 非首音节模糊会整片丢失。切分信息在两个调用点都是现成的——`mod.rs` 有 DAG 的
    /// `syllables`，`lattice.rs` 有 `graph.any_path` 的 `offsets`（还紧接着用
    /// `slice_syllables` 切过一次）——本函数即为收口这两处而抽出。
    ///
    /// 组合数超 [`MAX_FUZZY_COMBOS`] 时返回空（放弃扩展），避免组合爆炸。
    /// 返回 `(变体码, 被模糊的音节数)`。**第二项是惩罚的计量单位**：librime 的
    /// `kFuzzySpellingPenalty` 与 libime 的 `fuzzyCost` 都是「每个模糊拼写 log(0.5)」并
    /// 逐个累加，即概率域按模糊音节数**累乘 0.5**。我们此前两处惩罚（词图 −0.5、候选层
    /// ×0.01）都是**一次性固定值**，`beijinsi`（2 个模糊音节）与 `si`（1 个）同等对待。
    pub fn expand_syllables(syllables: &[String], config: &FuzzyConfig) -> Vec<(String, usize)> {
        let per_syllable: Vec<Vec<String>> = syllables
            .iter()
            .map(|s| {
                // opts[0] 恒为原音节，故「下标 > 0」即「该音节被模糊了」。
                let mut opts = vec![s.clone()];
                opts.extend(Self::fuzzy_variants(s, config));
                opts
            })
            .collect();

        // 预估组合数，超限直接放弃扩展。
        let mut combo_count: usize = 1;
        for opts in &per_syllable {
            combo_count = combo_count.saturating_mul(opts.len());
            if combo_count > MAX_FUZZY_COMBOS {
                return Vec::new();
            }
        }

        let mut codes: Vec<(String, usize)> = vec![(String::new(), 0)];
        for opts in &per_syllable {
            let mut next: Vec<(String, usize)> = Vec::with_capacity(codes.len() * opts.len());
            for (prefix, fuzzy_count) in &codes {
                for (i, opt) in opts.iter().enumerate() {
                    next.push((format!("{prefix}{opt}"), fuzzy_count + usize::from(i > 0)));
                }
            }
            codes = next;
        }
        codes
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(f: impl FnOnce(&mut FuzzyConfig)) -> FuzzyConfig {
        let mut c = FuzzyConfig::default();
        f(&mut c);
        c
    }

    fn syls(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ---------------------------------------------------------------- any_enabled

    #[test]
    fn any_enabled_reflects_each_group() {
        assert!(!FuzzyConfig::default().any_enabled(), "默认全关");
        assert!(cfg(|c| c.zh_z = true).any_enabled());
        assert!(cfg(|c| c.uan_uang = true).any_enabled(), "末位组也须被算上");
    }

    // ---------------------------------------------------------------- expand_syllables

    /// 测试辅助：只取变体码，丢掉模糊音节数。
    fn codes(out: &[(String, usize)]) -> Vec<String> {
        out.iter().map(|(c, _)| c.clone()).collect()
    }

    /// 测试辅助：查某个变体码对应的模糊音节数。
    fn fuzzy_count_of(out: &[(String, usize)], code: &str) -> Option<usize> {
        out.iter().find(|(c, _)| c == code).map(|(_, k)| *k)
    }

    /// 全原音节组合恒排第一、且模糊音节数为 0（调用方据 `variant == code` 跳过精确命中，
    /// 依赖此性质）。
    #[test]
    fn expand_first_combo_is_original_code() {
        let c = cfg(|c| {
            c.zh_z = true;
            c.sh_s = true;
        });
        let out = FuzzyMatcher::expand_syllables(&syls(&["zhong", "guo"]), &c);
        assert_eq!(out[0].0, "zhongguo", "首个组合须是原码，实际: {out:?}");
        assert_eq!(out[0].1, 0, "原码的模糊音节数须为 0");
    }

    #[test]
    fn expand_disabled_yields_only_original() {
        let out = FuzzyMatcher::expand_syllables(&syls(&["si", "jin"]), &FuzzyConfig::default());
        assert_eq!(
            out,
            vec![("sijin".to_string(), 0)],
            "全关时只应有原码，且计数为 0"
        );
    }

    #[test]
    fn expand_single_syllable_degrades_to_variants() {
        let out = FuzzyMatcher::expand_syllables(&syls(&["si"]), &cfg(|c| c.sh_s = true));
        assert!(codes(&out).contains(&"si".to_string()));
        assert_eq!(
            fuzzy_count_of(&out, "shi"),
            Some(1),
            "单音节 s→sh 须计 1 个模糊音节，实际: {out:?}"
        );
    }

    /// **本次修复的核心**：非首音节的**声母**变体必须能生成。
    /// `zhongzou` → 「中州」需要第 2 音节 zou→zhou。
    #[test]
    fn expand_covers_non_initial_syllable_initial() {
        let out = FuzzyMatcher::expand_syllables(&syls(&["zhong", "zou"]), &cfg(|c| c.zh_z = true));
        assert_eq!(
            fuzzy_count_of(&out, "zhongzhou"),
            Some(1),
            "第 2 音节 zou→zhou 须被展开并计 1，实际: {out:?}"
        );
    }

    /// 非首音节的**韵母**变体：`beijin` → `beijing`（第 2 音节 in→ing）。
    #[test]
    fn expand_covers_non_initial_syllable_final() {
        let out = FuzzyMatcher::expand_syllables(&syls(&["bei", "jin"]), &cfg(|c| c.in_ing = true));
        assert_eq!(
            fuzzy_count_of(&out, "beijing"),
            Some(1),
            "第 2 音节 in→ing 须被展开并计 1，实际: {out:?}"
        );
    }

    /// **多处音节同时模糊**（笛卡尔积的意义）：`beijinsi` → `beijingshi`（北京市）
    /// 需要第 2 音节 in→ing **且** 第 3 音节 s→sh。
    ///
    /// ★ 计数须为 **2** —— 惩罚按模糊音节数累乘（`0.5^2`），对齐 librime
    /// `kFuzzySpellingPenalty` 与 libime `fuzzyCost` 的逐个累加。写成一次性固定折扣时，
    /// 本串与单音节模糊同等对待，置信度差异被抹平。
    #[test]
    fn expand_covers_multiple_syllables_at_once() {
        let c = cfg(|c| {
            c.in_ing = true;
            c.sh_s = true;
        });
        let out = FuzzyMatcher::expand_syllables(&syls(&["bei", "jin", "si"]), &c);
        assert_eq!(
            fuzzy_count_of(&out, "beijingshi"),
            Some(2),
            "第 2、3 音节同时变体须计 2，实际: {out:?}"
        );
        // 同一次展开里，只改一个音节的组合计 1 —— 与上面合看才说明计数真在数音节，
        // 而非「只要有模糊就置 1」或「恒等于音节总数」。
        assert_eq!(fuzzy_count_of(&out, "beijingsi"), Some(1));
        assert_eq!(fuzzy_count_of(&out, "beijinshi"), Some(1));
    }

    /// **回归守卫（钉死旧 bug）**：对多音节**拼接串**整体调 `fuzzy_variants` 拿不到
    /// 非首音节的声母变体——声母规则是 `starts_with`。谁把 `expand_syllables` 改回
    /// 整串调用，这条就会挂。
    #[test]
    fn whole_string_variants_miss_non_initial_initials() {
        let c = cfg(|c| {
            c.in_ing = true;
            c.sh_s = true;
        });
        let whole = FuzzyMatcher::fuzzy_variants("beijinsi", &c);
        assert!(
            !whole.contains(&"beijingshi".to_string()),
            "整串调用本就拿不到非首音节声母变体（已知限制），实际: {whole:?}"
        );

        // 而逐音节展开可以——两者的差集正是本次修复的价值。
        let per_syllable = FuzzyMatcher::expand_syllables(&syls(&["bei", "jin", "si"]), &c);
        assert!(codes(&per_syllable).contains(&"beijingshi".to_string()));
    }

    /// 组合数超上限时整体放弃（避免爆炸），而非截断出半份结果。
    #[test]
    fn expand_gives_up_beyond_combo_limit() {
        let c = cfg(|c| c.in_ing = true);
        // 每个 "jin" 有 2 个选项（jin / jing）→ 2^7 = 128 > MAX_FUZZY_COMBOS(64)
        let many = syls(&["jin"; 7]);
        assert!(
            FuzzyMatcher::expand_syllables(&many, &c).is_empty(),
            "超 {MAX_FUZZY_COMBOS} 组合须返回空"
        );
        // 2^6 = 64，恰好不超限
        assert!(!FuzzyMatcher::expand_syllables(&syls(&["jin"; 6]), &c).is_empty());
    }

    #[test]
    fn expand_empty_input_is_safe() {
        let out = FuzzyMatcher::expand_syllables(&[], &cfg(|c| c.zh_z = true));
        assert_eq!(
            out,
            vec![(String::new(), 0)],
            "空音节列表只产出空串，交调用方跳过"
        );
    }
}
