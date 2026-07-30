//! gen_dict 配置：TOML 解析与路径解析。
//!
//! 数值默认值与 WindInput-Go 的 `tools/dictgen/config.go: defaultConfig()` 逐项对应。
//! 改动这些默认值会直接改变发行词库的候选顺序，动前先读 gen_dict.toml 的头部说明。

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    // 输入（相对 --cache）
    pub jidian_path: String,
    pub unigram_path: String,

    // 输出（相对 --out）
    pub output_path: String,
    pub output_name: String,

    // 人工维护数据（相对配置文件所在目录）
    pub custom_words_path: String,
    pub custom_emoji_path: String,
    /// emoji 中文命名表（由 gen_emoji_names 从 CLDR 生成）：中文名反查五笔码，
    /// 与 `custom_emoji_path` 的固定 `emoj` 码是两条互不影响的通路。
    pub custom_emoji_named_path: String,
    pub boosts_path: String,

    // 分析报告（相对 --report）
    pub dropped_path: String,
    pub conflict_report_path: String,
    pub demotion_report_path: String,

    // 权重归一化
    pub target_median: i64,
    pub weight_max: i64,
    pub weight_min: i64,
    pub char_boost_factor: f64,
    pub fallback: FallbackWeights,

    // 普通词条权重上限，须低于最低简码权重
    pub regular_weight_max: i64,

    pub shortcodes: ShortcodeConfig,
    pub demotion: DemotionConfig,
    pub extra: ExtraConfig,
    pub filter: FilterConfig,

    #[serde(rename = "drop_rules")]
    pub drop_rules: Vec<DropRule>,

    /// 写入主词库头部的 `import_tables` 段；当前发行方案为空
    /// （emoji/extra/district 都作为独立 `[[dictionaries]]` 声明）。
    pub import_tables: Vec<String>,

    /// 上游原样词库（相对 `--cache`）：不重排，只复制到输出目录并清洗头部的 `sort:` 键。
    ///
    /// 用于 district 这类**条目顺序本身即数据**的词库（按行政区划层级排列），
    /// 按词频重排会破坏其语义。
    pub passthrough: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackWeights {
    pub priority_30: i64,
    pub priority_20: i64,
    pub priority_10: i64,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShortcodeConfig {
    pub enabled: bool,
    pub level1_weight: i64,
    pub level2_base_weight: i64,
    pub level3_base_weight: i64,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DemotionConfig {
    pub enabled: bool,
    pub filter_threshold: i64,
    pub single_char_promote_wt: i64,
    pub word_promote_wt: i64,
    pub max_gap_ratio_single: f64,
    pub max_gap_ratio_word: f64,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtraConfig {
    pub enabled: bool,
    pub input_path: String,
    pub default_weight: i64,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilterConfig {
    pub drop_z_code: bool,
    pub drop_dollar: bool,
    pub drop_emoji: bool,
    pub drop_pure_latin: bool,
    pub drop_pua: bool,
    pub require_cjk: bool,
    pub max_code_len: usize,
    pub max_text_len: usize,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct DropRule {
    pub code_prefix: String,
    pub code: String,
    pub reason: String,
    pub except_codes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            jidian_path: String::new(),
            unigram_path: String::new(),
            output_path: String::new(),
            output_name: "wubi86_jidian".into(),
            custom_words_path: String::new(),
            custom_emoji_path: String::new(),
            custom_emoji_named_path: String::new(),
            boosts_path: String::new(),
            dropped_path: String::new(),
            conflict_report_path: String::new(),
            demotion_report_path: String::new(),
            target_median: 1000,
            weight_max: 9999,
            weight_min: 1,
            char_boost_factor: 1.3,
            fallback: FallbackWeights::default(),
            regular_weight_max: 8999,
            shortcodes: ShortcodeConfig::default(),
            demotion: DemotionConfig::default(),
            extra: ExtraConfig::default(),
            filter: FilterConfig::default(),
            drop_rules: Vec::new(),
            import_tables: Vec::new(),
            passthrough: Vec::new(),
        }
    }
}

impl Default for FallbackWeights {
    fn default() -> Self {
        Self {
            priority_30: 180,
            priority_20: 150,
            priority_10: 120,
        }
    }
}

impl Default for ShortcodeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level1_weight: 9999,
            level2_base_weight: 9950,
            level3_base_weight: 9000,
        }
    }
}

impl Default for DemotionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            filter_threshold: 200,
            single_char_promote_wt: 1000,
            word_promote_wt: 800,
            max_gap_ratio_single: 0.60,
            max_gap_ratio_word: 0.65,
        }
    }
}

impl Default for ExtraConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            input_path: String::new(),
            default_weight: 100,
        }
    }
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            drop_z_code: true,
            drop_dollar: true,
            drop_emoji: true,
            drop_pure_latin: true,
            drop_pua: false,
            require_cjk: false,
            max_code_len: 4,
            max_text_len: 16,
        }
    }
}

/// 配置里的相对路径按各自的基准目录解析后的结果。
pub struct Paths {
    pub jidian: PathBuf,
    pub unigram: PathBuf,
    pub extra_input: Option<PathBuf>,
    pub output: PathBuf,
    /// (源文件, 输出文件) 对
    pub passthrough: Vec<(PathBuf, PathBuf)>,
    pub custom_words: Option<PathBuf>,
    pub custom_emoji: Option<PathBuf>,
    pub custom_emoji_named: Option<PathBuf>,
    pub boosts: Option<PathBuf>,
    pub dropped: Option<PathBuf>,
    pub conflict_report: Option<PathBuf>,
    pub demotion_report: Option<PathBuf>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("读取配置失败 {}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("解析配置失败 {}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 拦截会静默产出错误词库的配置组合。
    ///
    /// 尤其是 `regular_weight_max >= level3_base_weight`：普通词条会窜进简码权重区间，
    /// 结果是简码不再稳定占首选——这种错误在输出里看不出来，只有实际打字才发现。
    fn validate(&self) -> anyhow::Result<()> {
        if self.jidian_path.is_empty() {
            anyhow::bail!("jidian_path 不可为空");
        }
        if self.unigram_path.is_empty() {
            anyhow::bail!("unigram_path 不可为空");
        }
        if self.output_path.is_empty() || self.output_name.is_empty() {
            anyhow::bail!("output_path / output_name 不可为空");
        }
        if self.weight_min > self.weight_max {
            anyhow::bail!(
                "weight_min({}) 不可大于 weight_max({})",
                self.weight_min,
                self.weight_max
            );
        }
        if self.shortcodes.enabled && self.regular_weight_max >= self.shortcodes.level3_base_weight
        {
            anyhow::bail!(
                "regular_weight_max({}) 必须低于 level3_base_weight({})，否则普通词条会挤进简码权重区间、简码不再稳定占首选",
                self.regular_weight_max,
                self.shortcodes.level3_base_weight
            );
        }
        if self.target_median <= 0 {
            anyhow::bail!("target_median 必须为正数");
        }
        Ok(())
    }

    /// 按 CLI 给的目录把配置里的相对路径展开。
    ///
    /// 三个基准各不相同：输入相对 `--cache`、输出相对 `--out`、人工数据相对配置文件
    /// 自身所在目录（它们与配置同属版本控制，跟着配置走才不会因调用位置不同而失效）。
    pub fn resolve_paths(
        &self,
        config_dir: &Path,
        cache_dir: &Path,
        out_dir: &Path,
        report_dir: Option<&Path>,
    ) -> Paths {
        let opt = |s: &str, base: &Path| -> Option<PathBuf> {
            if s.is_empty() {
                None
            } else {
                Some(base.join(s))
            }
        };
        Paths {
            jidian: cache_dir.join(&self.jidian_path),
            unigram: cache_dir.join(&self.unigram_path),
            extra_input: if self.extra.enabled {
                opt(&self.extra.input_path, cache_dir)
            } else {
                None
            },
            output: out_dir.join(&self.output_path),
            passthrough: self
                .passthrough
                .iter()
                .map(|rel| {
                    let src = cache_dir.join(rel);
                    // 输出保持与源同名：这些文件被方案按文件名引用
                    let name = Path::new(rel).file_name().unwrap_or_default();
                    (src, out_dir.join(name))
                })
                .collect(),
            custom_words: opt(&self.custom_words_path, config_dir),
            custom_emoji: opt(&self.custom_emoji_path, config_dir),
            custom_emoji_named: opt(&self.custom_emoji_named_path, config_dir),
            boosts: opt(&self.boosts_path, config_dir),
            dropped: report_dir.and_then(|d| opt(&self.dropped_path, d)),
            conflict_report: report_dir.and_then(|d| opt(&self.conflict_report_path, d)),
            demotion_report: report_dir.and_then(|d| opt(&self.demotion_report_path, d)),
        }
    }

    /// 普通词条的权重上限：启用简码分层时压到 `regular_weight_max` 之下。
    pub fn regular_max(&self) -> i64 {
        if self.shortcodes.enabled
            && self.regular_weight_max > 0
            && self.regular_weight_max < self.weight_max
        {
            self.regular_weight_max
        } else {
            self.weight_max
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_go_dictgen() {
        // 与 WindInput-Go tools/dictgen/config.go defaultConfig() 逐项对齐
        let c = Config::default();
        assert_eq!(c.target_median, 1000);
        assert_eq!(c.weight_max, 9999);
        assert_eq!(c.weight_min, 1);
        assert!((c.char_boost_factor - 1.3).abs() < f64::EPSILON);
        assert_eq!(c.fallback.priority_30, 180);
        assert_eq!(c.fallback.priority_20, 150);
        assert_eq!(c.fallback.priority_10, 120);
        assert_eq!(c.regular_weight_max, 8999);
        assert_eq!(c.shortcodes.level1_weight, 9999);
        assert_eq!(c.shortcodes.level2_base_weight, 9950);
        assert_eq!(c.shortcodes.level3_base_weight, 9000);
        assert_eq!(c.demotion.filter_threshold, 200);
        assert_eq!(c.filter.max_code_len, 4);
        assert_eq!(c.filter.max_text_len, 16);
        assert!(!c.filter.drop_pua, "五笔字根字属合法生僻字，默认不过滤");
    }

    #[test]
    fn regular_max_capped_by_shortcodes() {
        let c = Config::default();
        assert_eq!(
            c.regular_max(),
            8999,
            "启用简码时普通词条上限被压到 regular_weight_max"
        );

        let c2 = Config {
            shortcodes: ShortcodeConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(c2.regular_max(), 9999, "关闭简码分层则放开到 weight_max");
    }

    #[test]
    fn rejects_regular_max_overlapping_shortcode_band() {
        let c = Config {
            jidian_path: "a".into(),
            unigram_path: "b".into(),
            output_path: "c".into(),
            regular_weight_max: 9000, // == level3_base_weight
            ..Default::default()
        };
        let err = c.validate().unwrap_err().to_string();
        assert!(
            err.contains("level3_base_weight"),
            "应指出与简码区间重叠: {err}"
        );
    }
}
