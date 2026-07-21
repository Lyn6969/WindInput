//! 扩展词库拆分：把 jidian_extra 按字符类型分成 4 个独立文件。
//!
//! 拆分的意义是让用户能单独开关：emoji 和英文词条对纯中文输入是噪音，
//! 但对另一部分用户是刚需。分成独立词库后由方案的 `[[dictionaries]]` 各自控制。

use crate::config::Config;
use crate::entry::{Entry, has_cjk, has_emoji};
use crate::weight::{Unigram, compute_weight, fallback_weight};
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// 含 CJK 的中文词条（主类）
    Cjk,
    /// 含 emoji 的条目
    Emoji,
    /// 全 ASCII 且含字母的英文/品牌词
    English,
    /// 其余：非 CJK、非 emoji、非 ASCII 字母的特殊符号
    Symbol,
}

impl Category {
    pub fn suffix(self) -> &'static str {
        match self {
            Category::Cjk => "extra",
            Category::Emoji => "emoji",
            Category::English => "english",
            Category::Symbol => "symbols",
        }
    }

    pub const ALL: [Category; 4] = [
        Category::Cjk,
        Category::Emoji,
        Category::English,
        Category::Symbol,
    ];
}

/// 按 text 的字符构成归类。
///
/// 优先级 emoji > CJK > english > symbol：emoji 优先是为了让「🐶 + 中文备注」这类
/// 条目落进 emoji 桶而不是 CJK 桶——用户关掉 emoji 库时期待它一起消失。
pub fn classify(text: &str) -> Category {
    if has_emoji(text) {
        return Category::Emoji;
    }
    if has_cjk(text) {
        return Category::Cjk;
    }
    let mut only_ascii = true;
    let mut has_letter = false;
    for c in text.chars() {
        if !('\u{20}'..='\u{7E}').contains(&c) {
            only_ascii = false;
            break;
        }
        if c.is_ascii_alphabetic() {
            has_letter = true;
        }
    }
    if only_ascii && has_letter {
        Category::English
    } else {
        Category::Symbol
    }
}

/// 给各桶赋权：CJK 走 unigram 归一化，其余桶保留原权重、缺失则给兜底值。
pub fn assign_weights(
    buckets: &mut [(Category, Vec<Entry>)],
    unigram: &Unigram,
    log_median: f64,
    cfg: &Config,
) -> (usize, usize) {
    let default_weight = if cfg.extra.default_weight > 0 {
        cfg.extra.default_weight
    } else {
        100
    };
    let (mut cjk_hit, mut cjk_total) = (0usize, 0usize);

    for (cat, list) in buckets.iter_mut() {
        match cat {
            Category::Cjk => {
                cjk_total = list.len();
                for e in list.iter_mut() {
                    if log_median > 0.0
                        && let Some(&freq) = unigram.get(&e.text)
                    {
                        e.weight = compute_weight(freq, log_median, cfg);
                        cjk_hit += 1;
                        continue;
                    }
                    // 未命中：有原始优先级就按档保底，否则落最低档
                    e.weight = if e.weight > 0 {
                        fallback_weight(e.weight, cfg)
                    } else {
                        cfg.fallback.priority_10
                    };
                }
            }
            _ => {
                for e in list.iter_mut() {
                    if e.weight <= 0 {
                        e.weight = default_weight;
                    }
                }
            }
        }
    }
    (cjk_hit, cjk_total)
}

/// 加载自定义 emoji 列表：每行一个，按行序从 200 递减分配权重，编码固定 `emoj`。
///
/// 递减权重是这批常用表情的**展示顺序**载体（😂 > 🤣 > 😊…）。方案侧因此绝不能给
/// emoji 库配 `default_weight`——那会把整库抹平成文件序，毁掉这里的排序意图。
pub fn load_custom_emoji(path: Option<&Path>) -> anyhow::Result<Vec<Entry>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = std::fs::File::open(path)?;
    let mut emojis = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        emojis.push(line.to_string());
    }

    const BASE_WEIGHT: i64 = 200;
    Ok(emojis
        .into_iter()
        .enumerate()
        .map(|(i, emoji)| {
            let w = (BASE_WEIGHT - i as i64).max(1);
            Entry::new(emoji, "emoj".into(), w, 0)
        })
        .collect())
}

/// 把主输出路径里的 output_name 换成 `output_name_<suffix>`，其余部分保留。
///
/// 例：`.../wubi86_jidian.dict.yaml` + `emoji` → `.../wubi86_jidian_emoji.dict.yaml`
pub fn extra_output_path(main_path: &Path, output_name: &str, suffix: &str) -> std::path::PathBuf {
    let dir = main_path.parent().unwrap_or(Path::new("."));
    let base = main_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let new_base = base.replacen(output_name, &format!("{output_name}_{suffix}"), 1);
    dir.join(new_base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_wins_over_cjk_for_mixed_text() {
        // 关掉 emoji 库时，带中文备注的表情条目也该一起消失
        assert_eq!(classify("🐶狗"), Category::Emoji);
        assert_eq!(classify("狗"), Category::Cjk);
    }

    #[test]
    fn english_requires_letters_not_just_ascii() {
        assert_eq!(classify("api"), Category::English);
        assert_eq!(classify("+++"), Category::Symbol, "纯符号不算英文");
        assert_eq!(classify("№"), Category::Symbol, "非 ASCII 非 CJK 落符号桶");
    }

    #[test]
    fn custom_emoji_weights_descend_from_200() {
        let p = std::env::temp_dir().join("gen_dict_emoji_test.txt");
        std::fs::write(&p, "😂\n# 注释\n🤣\n😊\n").unwrap();
        let v = load_custom_emoji(Some(&p)).unwrap();
        assert_eq!(v.len(), 3, "注释行不计入");
        assert_eq!((v[0].weight, v[1].weight, v[2].weight), (200, 199, 198));
        assert!(v.iter().all(|e| e.code == "emoj"));
    }

    #[test]
    fn custom_emoji_weight_never_drops_below_one() {
        let p = std::env::temp_dir().join("gen_dict_emoji_many.txt");
        let content: String = (0..250).map(|_| "😀\n").collect();
        std::fs::write(&p, content).unwrap();
        let v = load_custom_emoji(Some(&p)).unwrap();
        assert!(v.iter().all(|e| e.weight >= 1), "超过 200 条后须夹在 1");
    }

    #[test]
    fn missing_custom_emoji_file_is_not_an_error() {
        assert!(
            load_custom_emoji(Some(Path::new("/definitely/not/here.txt")))
                .unwrap()
                .is_empty()
        );
        assert!(load_custom_emoji(None).unwrap().is_empty());
    }

    #[test]
    fn output_path_derives_sibling_names() {
        let p = Path::new("/out/wubi86_jidian.dict.yaml");
        assert_eq!(
            extra_output_path(p, "wubi86_jidian", "emoji"),
            Path::new("/out/wubi86_jidian_emoji.dict.yaml")
        );
    }

    #[test]
    fn cjk_bucket_falls_back_by_priority_when_unigram_misses() {
        let cfg = Config {
            jidian_path: "a".into(),
            unigram_path: "b".into(),
            output_path: "c".into(),
            ..Default::default()
        };
        let mut buckets = vec![(
            Category::Cjk,
            vec![
                Entry::new("罕见词".into(), "abcd".into(), 30, 0), // 有原始优先级
                Entry::new("无权重".into(), "abce".into(), 0, 1),  // 无原始权重
            ],
        )];
        assign_weights(&mut buckets, &Unigram::new(), 3.0, &cfg);
        assert_eq!(buckets[0].1[0].weight, 180, "priority_30 档");
        assert_eq!(buckets[0].1[1].weight, 120, "无原始权重落 priority_10");
    }

    #[test]
    fn non_cjk_buckets_keep_original_weight() {
        let cfg = Config {
            jidian_path: "a".into(),
            unigram_path: "b".into(),
            output_path: "c".into(),
            ..Default::default()
        };
        let mut buckets = vec![(
            Category::English,
            vec![
                Entry::new("api".into(), "abcd".into(), 555, 0),
                Entry::new("sdk".into(), "abce".into(), 0, 1),
            ],
        )];
        assign_weights(&mut buckets, &Unigram::new(), 3.0, &cfg);
        assert_eq!(buckets[0].1[0].weight, 555, "有原权重则保留");
        assert_eq!(buckets[0].1[1].weight, 100, "无权重给 extra.default_weight");
    }
}
