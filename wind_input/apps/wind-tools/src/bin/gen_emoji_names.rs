//! 从 Unicode CLDR 注解生成 emoji 中文命名表，供 gen_dict 反查五笔码。
//!
//! 产出的 `custom_emoji_named.txt` 入库（可 review、可 diff），由 gen_dict 的
//! `load_named_emoji` 消费——它把每个中文名按五笔 86 词组取码规则反查成编码，
//! 于是「⚽ 足球」自动得到 `khgf`，与上游 rime-wubi 手工编的码完全一致。
//!
//! 三个输入各自不可替代：
//!   - `annotations/zh.xml`        1584 个 emoji 的 tts 主名 + keywords
//!   - `annotationsDerived/zh.xml` 再补 326 个（几乎全是国旗），tts 形如「旗: 阿富汗」
//!   - `emoji-test.txt`            白名单：定义什么才算 emoji，并给出带 VS16 的规范形态
//!
//! 数据许可证 Unicode-3.0（允许再分发与修改），见 NOTICE.md。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 肤色修饰符：`👋🏻` 这类变体不入表——五笔码无法区分肤色，只会让同码堆积翻 5 倍。
const SKIN_TONES: (u32, u32) = (0x1F3FB, 0x1F3FF);
const VS16: char = '\u{FE0F}';

/// 内置上位词黑名单：CLDR 用来给 emoji 归类的分类标签，不是具名描述。
///
/// 括号里是实测共享度（被多少个 emoji 共用），取自 annotations + annotationsDerived
/// 合并后的统计。阈值取 39——下一档是「日本」31、「家庭」30，语义相关性强，保留。
///
/// 只过滤 keywords，**不过滤 tts 主名称**：CLDR 的 tts 实测彼此完全不重复
/// （1584 个主名零共享），是每个 emoji 的唯一入口，剔掉会让该 emoji 彻底消失。
const DEFAULT_STOPWORDS: &[&str] = &[
    "旗",   // 265
    "脸",   // 128
    "女",   // 71
    "男",   // 69
    "食物", // 45
    "动物", // 40
    "手",   // 39
    "男人", // 39
    "女人", // 39
    "按键", // 39
];

struct Args {
    cldr: PathBuf,
    stopwords: Option<PathBuf>,
    out: PathBuf,
}

fn parse_args() -> anyhow::Result<Args> {
    let (mut cldr, mut stopwords, mut out) = (None, None, None);
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--cldr" => cldr = it.next().map(PathBuf::from),
            "--stopwords" => stopwords = it.next().map(PathBuf::from),
            "--out" => out = it.next().map(PathBuf::from),
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => anyhow::bail!("未知参数: {other}（--help 查看用法）"),
        }
    }
    Ok(Args {
        cldr: cldr.ok_or_else(|| anyhow::anyhow!("缺 --cldr <dir>"))?,
        stopwords,
        out: out.ok_or_else(|| anyhow::anyhow!("缺 --out <file>"))?,
    })
}

fn print_usage() {
    eprintln!(
        "用法: gen_emoji_names --cldr <dir> --out <file> [--stopwords <file>]\n\
         \n\
         --cldr       CLDR 数据目录，需含 zh.xml / zh_derived.xml / emoji-test.txt\n\
         --out        输出的 emoji 命名表（TSV: emoji<TAB>中文名<TAB>tts|kw）\n\
         --stopwords  可选，覆盖内置上位词黑名单（只过滤 keywords，不过滤 tts）"
    );
}

// ── 工具函数 ──────────────────────────────────────────

fn strip_vs16(s: &str) -> String {
    s.chars().filter(|&c| c != VS16).collect()
}

/// 是否纯汉字。非汉字名（「O型血」「按键: 9」「:D」）一律丢弃：
/// 五笔码表只能由汉字反查，混入拉丁/数字只会让 encode_phrase 失败。
fn is_all_han(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

/// 括号与连接符只是排版，剥掉符号本身、**保留内容**再判纯汉字。
///
/// 不这么做，「科科斯（基林）群岛」「刚果（金）」「特里斯坦-达库尼亚」这类国名
/// 会因含标点被整条丢弃。保留内容才能让「刚果金」与「刚果布」仍然可区分——
/// 若连内容一起剥掉，两面不同的国旗会塌成同一个「刚果」。
fn clean_name(raw: &str) -> Option<String> {
    const PUNCT: &[char] = &[
        '（', '）', '(', ')', '·', '-', '‑', '–', '—', '　', ' ', '、',
    ];
    let cleaned: String = raw.trim().chars().filter(|c| !PUNCT.contains(c)).collect();
    is_all_han(&cleaned).then_some(cleaned)
}

/// 解码 XML 实体。CLDR 用 `&amp;` 等转义，不解码会让含 & 的名字带上字面量。
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(semi) = tail.find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse::<u32>().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 取 tts 主名称。derived 的 tts 形如「旗: 阿富汗」「按键: 9」——
/// **必须切冒号取后半**，否则 260 个国旗因含 `:` 全部反查失败而静默消失。
fn tts_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if let Some((_, tail)) = raw.split_once(':')
        && let Some(name) = clean_name(tail)
    {
        return Some(name);
    }
    clean_name(raw)
}

// ── 输入解析 ──────────────────────────────────────────

/// 解析 emoji-test.txt，返回 剥VS16形态 → 规范形态（带 VS16）。
///
/// 只收 `fully-qualified`：minimally-qualified / unqualified 是同一 emoji 的缺 VS16
/// 变体，component 是肤色/发色修饰符本身，都不该独立成为候选。
fn load_whitelist(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", path.display()))?;
    let mut map = BTreeMap::new();
    let mut skin = 0usize;
    for line in text.lines() {
        if line.starts_with('#') || !line.contains("; fully-qualified") {
            continue;
        }
        let Some(cps) = line.split(';').next() else {
            continue;
        };
        let mut seq = String::new();
        let mut bad = false;
        for tok in cps.split_whitespace() {
            match u32::from_str_radix(tok, 16).ok().and_then(char::from_u32) {
                Some(c) => seq.push(c),
                None => {
                    bad = true;
                    break;
                }
            }
        }
        if bad || seq.is_empty() {
            continue;
        }
        if seq
            .chars()
            .any(|c| (SKIN_TONES.0..=SKIN_TONES.1).contains(&(c as u32)))
        {
            skin += 1;
            continue;
        }
        map.entry(strip_vs16(&seq)).or_insert(seq);
    }
    eprintln!(
        "      白名单 {} 个 emoji（跳过肤色变体 {skin} 条）",
        map.len()
    );
    Ok(map)
}

#[derive(Default)]
struct Annotations {
    /// 剥VS16 的 cp → tts 主名称
    tts: BTreeMap<String, String>,
    /// 剥VS16 的 cp → keywords（保留 CLDR 原序）
    keywords: BTreeMap<String, Vec<String>>,
}

/// 解析 CLDR annotation XML。格式极规整，手工扫描即可，不必引入 XML 依赖：
///   `<annotation cp="⚽">足球 | 球 | 运动</annotation>`
///   `<annotation cp="⚽" type="tts">足球</annotation>`
///
/// 先解析的文件优先（annotations 优先于 derived），故合并时用 `or_insert`。
fn load_annotations(path: &Path, into: &mut Annotations) -> anyhow::Result<usize> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", path.display()))?;
    let mut n = 0usize;
    for chunk in text.split("<annotation ").skip(1) {
        let Some(end) = chunk.find("</annotation>") else {
            continue;
        };
        let (head, body) = match chunk[..end].split_once('>') {
            Some(v) => v,
            None => continue,
        };
        let Some(cp_start) = head.find("cp=\"") else {
            continue;
        };
        let after = &head[cp_start + 4..];
        let Some(cp_end) = after.find('"') else {
            continue;
        };
        let cp = strip_vs16(&decode_entities(&after[..cp_end]));
        if cp.is_empty() {
            continue;
        }
        let is_tts = head.contains("type=\"tts\"");
        let body = decode_entities(body);
        if is_tts {
            into.tts
                .entry(cp)
                .or_insert_with(|| body.trim().to_string());
        } else {
            into.keywords.entry(cp).or_insert_with(|| {
                body.split('|')
                    .map(|w| w.trim().to_string())
                    .filter(|w| !w.is_empty())
                    .collect()
            });
        }
        n += 1;
    }
    Ok(n)
}

/// 无 `--stopwords` 时用内置常量；给了文件则整体替换（不是叠加）。
fn load_stopwords(path: Option<&Path>) -> anyhow::Result<HashSet<String>> {
    let Some(path) = path else {
        return Ok(DEFAULT_STOPWORDS.iter().map(|s| s.to_string()).collect());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

// ── 主流程 ────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(2);
        }
    };

    eprintln!("[1/3] 加载输入...");
    let whitelist = load_whitelist(&args.cldr.join("emoji-test.txt"))?;
    let mut ann = Annotations::default();
    for (file, label) in [("zh.xml", "annotations"), ("zh_derived.xml", "derived")] {
        let p = args.cldr.join(file);
        if !p.exists() {
            anyhow::bail!("缺少 {}（运行构建脚本的 gen-data 下载 CLDR）", p.display());
        }
        let n = load_annotations(&p, &mut ann)?;
        eprintln!("      {label}: {n} 条注解");
    }
    let stop = load_stopwords(args.stopwords.as_deref())?;
    eprintln!(
        "      上位词黑名单: {} 个（{}）",
        stop.len(),
        if args.stopwords.is_some() {
            "外部文件"
        } else {
            "内置"
        }
    );

    eprintln!("[2/3] 生成命名表...");
    // (规范emoji, 中文名, 是否主名) —— BTreeSet 保证输出稳定，便于 diff
    let mut rows: Vec<(String, String, bool)> = Vec::new();
    let mut covered = BTreeSet::new();
    let (mut no_name, mut dropped_stop, mut dropped_nonhan) = (0usize, 0usize, 0usize);

    for (stripped, canonical) in &whitelist {
        let mut names: Vec<(String, bool)> = Vec::new();
        let mut seen = BTreeSet::new();

        if let Some(raw) = ann.tts.get(stripped) {
            match tts_name(raw) {
                Some(name) => {
                    seen.insert(name.clone());
                    names.push((name, true));
                }
                None => dropped_nonhan += 1,
            }
        }
        for w in ann.keywords.get(stripped).into_iter().flatten() {
            // 黑名单比对用原文：表里写的是 CLDR 原词，清洗后再比会漏掉带标点的变体
            if stop.contains(w) {
                dropped_stop += 1;
                continue;
            }
            let Some(name) = clean_name(w) else {
                dropped_nonhan += 1;
                continue;
            };
            if !seen.insert(name.clone()) {
                continue;
            }
            names.push((name, false));
        }

        if names.is_empty() {
            no_name += 1;
            continue;
        }
        covered.insert(canonical.clone());
        for (name, is_tts) in names {
            rows.push((canonical.clone(), name, is_tts));
        }
    }

    // 稳定排序：emoji 码位序 → 主名优先 → 名称序
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.1.cmp(&b.1))
    });

    eprintln!(
        "      {} 条命名，覆盖 {} / {} 个 emoji（{:.1}%）",
        rows.len(),
        covered.len(),
        whitelist.len(),
        covered.len() as f64 / whitelist.len() as f64 * 100.0
    );
    eprintln!("      剔除: 上位词 {dropped_stop} 条 / 非纯汉字 {dropped_nonhan} 条");
    if no_name > 0 {
        let missing: Vec<&str> = whitelist
            .iter()
            .filter(|(_, v)| !covered.contains(*v))
            .map(|(_, v)| v.as_str())
            .take(20)
            .collect();
        eprintln!(
            "      {no_name} 个 emoji 无可用中文名（CLDR 未翻译）: {}",
            missing.join(" ")
        );
    }

    eprintln!("[3/3] 写入 {}...", args.out.display());
    if let Some(dir) = args.out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut buf = Vec::new();
    writeln!(
        buf,
        "# emoji 中文命名表 —— 由 gen_emoji_names 从 Unicode CLDR 生成，请勿手工编辑"
    )?;
    writeln!(buf, "#")?;
    writeln!(buf, "# 每行: emoji <TAB> 中文名 <TAB> tts|kw")?;
    writeln!(
        buf,
        "#   tts = CLDR 主名称，同码内权重最高；kw = keywords 补充入口"
    )?;
    writeln!(
        buf,
        "# 编码不在本表内——由 gen_dict 按五笔 86 词组取码规则反查中文名得到。"
    )?;
    writeln!(buf, "#")?;
    writeln!(
        buf,
        "# 来源: unicode-org/cldr common/annotations{{,Derived}}/zh.xml"
    )?;
    writeln!(
        buf,
        "#       unicode.org/Public/emoji/latest/emoji-test.txt"
    )?;
    writeln!(buf, "# 许可证: Unicode-3.0（见 NOTICE.md）")?;
    writeln!(
        buf,
        "# 统计: {} 条命名 / {} 个 emoji / 上位词剔除 {} 条",
        rows.len(),
        covered.len(),
        dropped_stop
    )?;
    for (emoji, name, is_tts) in &rows {
        writeln!(
            buf,
            "{emoji}\t{name}\t{}",
            if *is_tts { "tts" } else { "kw" }
        )?;
    }
    std::fs::write(&args.out, &buf)?;
    eprintln!("      完成: {} 行", rows.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tts_splits_derived_prefix() {
        // derived 的国旗 tts 带「旗: 」前缀，不切就因含 ':' 被判非汉字而全灭
        assert_eq!(tts_name("旗: 阿富汗").as_deref(), Some("阿富汗"));
        assert_eq!(tts_name("足球").as_deref(), Some("足球"));
    }

    #[test]
    fn tts_rejects_non_han_even_after_split() {
        assert_eq!(tts_name("按键: 9"), None, "切出数字须丢弃");
        assert_eq!(tts_name("O型血"), None);
        assert_eq!(tts_name(""), None);
    }

    #[test]
    fn tts_falls_back_to_whole_when_tail_not_han() {
        // 冒号后不是汉字但整体是汉字时，用整体（防御 CLDR 里出现「甲:乙」式命名）
        assert_eq!(tts_name("旗帜").as_deref(), Some("旗帜"));
    }

    #[test]
    fn clean_name_strips_punct_but_keeps_content() {
        // 剥符号保内容：两个刚果国旗才不会塌成同一个名字
        assert_eq!(clean_name("刚果（金）").as_deref(), Some("刚果金"));
        assert_eq!(clean_name("刚果（布）").as_deref(), Some("刚果布"));
        assert_eq!(
            clean_name("科科斯（基林）群岛").as_deref(),
            Some("科科斯基林群岛")
        );
        assert_eq!(
            clean_name("特里斯坦-达库尼亚").as_deref(),
            Some("特里斯坦达库尼亚")
        );
        assert_eq!(clean_name("足球").as_deref(), Some("足球"));
    }

    #[test]
    fn clean_name_still_rejects_latin_and_digits() {
        assert_eq!(clean_name("O型血"), None, "剥标点不等于放行拉丁字母");
        assert_eq!(clean_name("9"), None);
        assert_eq!(clean_name("（）"), None, "剥完为空须丢弃");
    }

    #[test]
    fn tts_derived_flag_names_survive_cleaning() {
        assert_eq!(tts_name("旗: 刚果（金）").as_deref(), Some("刚果金"));
        assert_eq!(
            tts_name("旗: 特里斯坦-达库尼亚").as_deref(),
            Some("特里斯坦达库尼亚")
        );
    }

    #[test]
    fn han_check_rejects_mixed() {
        assert!(is_all_han("双手合十"));
        assert!(!is_all_han("O型血"));
        assert!(!is_all_han("按键 9"));
        assert!(!is_all_han(""));
    }

    #[test]
    fn vs16_is_stripped_for_keys_only() {
        assert_eq!(strip_vs16("\u{26BD}\u{FE0F}"), "\u{26BD}");
        assert_eq!(strip_vs16("\u{26BD}"), "\u{26BD}");
    }

    #[test]
    fn entities_decoded() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&lt;x&gt;"), "<x>");
        assert_eq!(decode_entities("&#x1F600;"), "\u{1F600}");
        assert_eq!(decode_entities("no entity"), "no entity");
        assert_eq!(decode_entities("bare & amp"), "bare & amp", "裸 & 原样保留");
    }

    #[test]
    fn annotation_parser_reads_both_forms() {
        let xml = r#"<ldml><annotations>
            <annotation cp="⚽">足球 | 球 | 运动</annotation>
            <annotation cp="⚽" type="tts">足球</annotation>
        </annotations></ldml>"#;
        let dir = std::env::temp_dir().join("gen_emoji_names_test_ann.xml");
        std::fs::write(&dir, xml).unwrap();
        let mut ann = Annotations::default();
        let n = load_annotations(&dir, &mut ann).unwrap();
        assert_eq!(n, 2);
        assert_eq!(ann.tts.get("⚽").map(String::as_str), Some("足球"));
        assert_eq!(
            ann.keywords.get("⚽").unwrap(),
            &vec!["足球".to_string(), "球".to_string(), "运动".to_string()]
        );
    }

    #[test]
    fn earlier_file_wins_on_conflict() {
        // annotations 先加载，derived 不得覆盖它
        let a = std::env::temp_dir().join("gen_emoji_a.xml");
        let b = std::env::temp_dir().join("gen_emoji_b.xml");
        std::fs::write(&a, r#"<annotation cp="⚽" type="tts">足球</annotation>"#).unwrap();
        std::fs::write(&b, r#"<annotation cp="⚽" type="tts">皮球</annotation>"#).unwrap();
        let mut ann = Annotations::default();
        load_annotations(&a, &mut ann).unwrap();
        load_annotations(&b, &mut ann).unwrap();
        assert_eq!(ann.tts.get("⚽").map(String::as_str), Some("足球"));
    }

    #[test]
    fn stopwords_strip_inline_comments() {
        let p = std::env::temp_dir().join("gen_emoji_stop.txt");
        std::fs::write(&p, "# 头部注释\n旗      # 265\n脸\n\n").unwrap();
        let s = load_stopwords(Some(&p)).unwrap();
        assert_eq!(s.len(), 2);
        assert!(s.contains("旗") && s.contains("脸"));
    }

    #[test]
    fn stopwords_default_to_builtin_list() {
        let s = load_stopwords(None).unwrap();
        assert_eq!(s.len(), DEFAULT_STOPWORDS.len());
        assert!(s.contains("旗"), "共享度最高的 265 必须在内置表里");
        assert!(!s.contains("日本"), "共享度 31 属保留档，不该被剔除");
    }
}
