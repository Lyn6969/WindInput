//! 词序提升表：对指定 (code, text) 手工调整权重。
//!
//! 在自动权重计算与简码降权**之后**生效，是人工微调的最后一道闸——用来修正
//! 词频统计与实际使用习惯不符的个别条目。
//!
//! 格式（TAB 分隔）：`code<TAB>text[<TAB>adjust]`
//! - adjust 留空 → 顶置（设为该 code 当前最高权重 +1）
//! - adjust = `+N` / `-N` → 相对位置：上移 / 下移 N 位
//! - adjust = `N` → 绝对权重

use crate::entry::Entry;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BoostMode {
    /// 顶置到该编码的首位
    Top,
    /// 相对移动：正数上移、负数下移
    Relative(i64),
    /// 直接指定权重
    Absolute(i64),
}

#[derive(Debug, Clone)]
pub struct BoostRule {
    pub code: String,
    pub text: String,
    pub mode: BoostMode,
}

pub fn load_boost_rules(path: &Path) -> anyhow::Result<Vec<BoostRule>> {
    let f = std::fs::File::open(path)?;
    let mut rules = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line_no = i + 1;
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            anyhow::bail!("第 {line_no} 行格式错误（需 code<TAB>text [<TAB>adjust]）: {line}");
        }
        let code = parts[0].trim().to_string();
        let text = parts[1].trim().to_string();
        if code.is_empty() || text.is_empty() {
            anyhow::bail!("第 {line_no} 行 code/text 不可为空");
        }
        let mode = match parts.get(2).map(|s| s.trim()).unwrap_or("") {
            "" => BoostMode::Top,
            adj => {
                let n: i64 = adj
                    .parse()
                    .map_err(|_| anyhow::anyhow!("第 {line_no} 行 adjust 解析失败: {adj}"))?;
                // 带符号写法表示相对移动，裸数字是绝对权重
                if adj.starts_with('+') || adj.starts_with('-') {
                    BoostMode::Relative(n)
                } else {
                    BoostMode::Absolute(n)
                }
            }
        };
        rules.push(BoostRule { code, text, mode });
    }
    Ok(rules)
}

/// 按规则调整权重，返回 (生效条数, 未匹配条数)。
///
/// 同一 code 下多条规则按书写顺序依次生效，**后一条看到的是前一条改过的顺序**——
/// 所以每次改动后都要重排该 code 的候选列表。
pub fn apply_boost_rules(
    entries: &mut [Entry],
    rules: &[BoostRule],
    log: &mut dyn FnMut(String),
) -> (usize, usize) {
    if rules.is_empty() {
        return (0, 0);
    }
    let mut idx: HashMap<(String, String), usize> = HashMap::new();
    let mut by_code: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        idx.insert((e.code.clone(), e.text.clone()), i);
        by_code.entry(e.code.clone()).or_default().push(i);
    }

    let (mut applied, mut missing) = (0usize, 0usize);
    for r in rules {
        let Some(&i) = idx.get(&(r.code.clone(), r.text.clone())) else {
            log(format!(
                "        [警告] boost 未匹配: code={} text={}",
                r.code, r.text
            ));
            missing += 1;
            continue;
        };

        let list = by_code.get_mut(&r.code).expect("code 必然已登记");
        sort_by_weight_desc(list, entries);

        match r.mode {
            BoostMode::Absolute(w) => entries[i].weight = w,
            BoostMode::Top => {
                let top = entries[list[0]].weight;
                entries[i].weight = top + 1;
            }
            BoostMode::Relative(delta) => {
                let Some(cur) = list.iter().position(|&v| v == i) else {
                    missing += 1;
                    continue;
                };
                // +N 上移 → 目标索引减小
                let target = (cur as i64 - delta).clamp(0, list.len() as i64 - 1) as usize;
                if target == cur {
                    applied += 1;
                    continue;
                }
                if target < cur {
                    entries[i].weight = entries[list[target]].weight + 1;
                } else {
                    entries[i].weight = entries[list[target]].weight - 1;
                }
            }
        }

        // 该 code 的顺序已变，重排以便后续规则看到新次序
        let list = by_code.get_mut(&r.code).expect("code 必然已登记");
        sort_by_weight_desc(list, entries);
        applied += 1;
    }
    (applied, missing)
}

fn sort_by_weight_desc(list: &mut [usize], entries: &[Entry]) {
    list.sort_by(|&a, &b| {
        entries[b]
            .weight
            .cmp(&entries[a].weight)
            .then_with(|| entries[a].text.cmp(&entries[b].text))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn e(text: &str, code: &str, weight: i64) -> Entry {
        Entry::new(text.into(), code.into(), weight, 0)
    }

    fn rule(code: &str, text: &str, mode: BoostMode) -> BoostRule {
        BoostRule {
            code: code.into(),
            text: text.into(),
            mode,
        }
    }

    fn nolog() -> impl FnMut(String) {
        |_| {}
    }

    #[test]
    fn top_puts_entry_above_current_best() {
        let mut v = vec![e("甲", "abcd", 500), e("乙", "abcd", 900)];
        let mut l = nolog();
        let (applied, missing) =
            apply_boost_rules(&mut v, &[rule("abcd", "甲", BoostMode::Top)], &mut l);
        assert_eq!((applied, missing), (1, 0));
        assert_eq!(v[0].weight, 901, "顶置 = 当前最高 +1");
    }

    #[test]
    fn absolute_sets_weight_verbatim() {
        let mut v = vec![e("甲", "abcd", 500)];
        let mut l = nolog();
        apply_boost_rules(
            &mut v,
            &[rule("abcd", "甲", BoostMode::Absolute(1234))],
            &mut l,
        );
        assert_eq!(v[0].weight, 1234);
    }

    #[test]
    fn relative_up_inserts_above_target_position() {
        // 排序后: 丙900 > 乙700 > 甲500；把「甲」上移 1 位 → 越过乙
        let mut v = vec![
            e("甲", "abcd", 500),
            e("乙", "abcd", 700),
            e("丙", "abcd", 900),
        ];
        let mut l = nolog();
        apply_boost_rules(
            &mut v,
            &[rule("abcd", "甲", BoostMode::Relative(1))],
            &mut l,
        );
        assert_eq!(v[0].weight, 701, "上移到目标位置权重 +1");
    }

    #[test]
    fn relative_down_falls_below_target() {
        let mut v = vec![
            e("丙", "abcd", 900),
            e("乙", "abcd", 700),
            e("甲", "abcd", 500),
        ];
        let mut l = nolog();
        apply_boost_rules(
            &mut v,
            &[rule("abcd", "丙", BoostMode::Relative(-1))],
            &mut l,
        );
        assert_eq!(v[0].weight, 699, "下移到目标位置权重 -1");
    }

    #[test]
    fn relative_clamps_at_list_bounds() {
        let mut v = vec![e("甲", "abcd", 500), e("乙", "abcd", 900)];
        let mut l = nolog();
        // 上移 99 位，列表只有 2 条 → 夹到首位
        apply_boost_rules(
            &mut v,
            &[rule("abcd", "甲", BoostMode::Relative(99))],
            &mut l,
        );
        assert_eq!(v[0].weight, 901);
    }

    #[test]
    fn sequential_rules_on_same_code_see_prior_changes() {
        // 两条顶置规则依次作用：第二条应看到第一条的结果
        let mut v = vec![
            e("甲", "abcd", 500),
            e("乙", "abcd", 600),
            e("丙", "abcd", 900),
        ];
        let mut l = nolog();
        apply_boost_rules(
            &mut v,
            &[
                rule("abcd", "甲", BoostMode::Top),
                rule("abcd", "乙", BoostMode::Top),
            ],
            &mut l,
        );
        assert_eq!(v[0].weight, 901, "甲先顶置到 901");
        assert_eq!(v[1].weight, 902, "乙再顶置须越过甲");
    }

    #[test]
    fn unmatched_rule_is_counted_not_fatal() {
        let mut v = vec![e("甲", "abcd", 500)];
        let mut logs = Vec::new();
        let (applied, missing) = apply_boost_rules(
            &mut v,
            &[rule("abcd", "不存在", BoostMode::Top)],
            &mut |s| logs.push(s),
        );
        assert_eq!((applied, missing), (0, 1));
        assert!(logs.iter().any(|l| l.contains("未匹配")));
    }

    #[test]
    fn parses_three_adjust_forms() {
        let p = std::env::temp_dir().join("gen_dict_boost_test.txt");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "# 注释").unwrap();
        writeln!(f, "abcd\t甲").unwrap();
        writeln!(f, "abcd\t乙\t+2").unwrap();
        writeln!(f, "abcd\t丙\t-3").unwrap();
        writeln!(f, "abcd\t丁\t777").unwrap();
        drop(f);
        let r = load_boost_rules(&p).unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].mode, BoostMode::Top, "留空 = 顶置");
        assert_eq!(r[1].mode, BoostMode::Relative(2));
        assert_eq!(r[2].mode, BoostMode::Relative(-3));
        assert_eq!(r[3].mode, BoostMode::Absolute(777), "裸数字 = 绝对权重");
    }
}
