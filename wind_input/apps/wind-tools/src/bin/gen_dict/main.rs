//! gen_dict：五笔主词库生成与扩展词库拆分。
//!
//! 从极点五笔 rime-wubi86-jidian 原始词库出发，结合 unigram 真实词频重新赋权排序，
//! 输出 WindInput 所用的 rime YAML 词库。
//!
//! 用法：
//!   gen_dict --cache <.cache 目录> --out <词库输出目录> [--report <报告目录>]
//!            [--config <gen_dict.toml>] [--version-date YYYY-MM-DD]
//!
//! 由 WindInput-Go 的 `tools/dictgen` 移植而来。权重与排序的每个数值都会直接改变
//! 发行词库的候选顺序，改动前先读 `data/gen_dict/gen_dict.toml` 的头部说明。

mod boost;
mod config;
mod entry;
mod extra;
mod parse;
mod reverse;
mod shortcode;
mod weight;
mod writer;

use config::Config;
use entry::Entry;
use std::path::{Path, PathBuf};

struct Args {
    config: PathBuf,
    cache: PathBuf,
    out: PathBuf,
    report: Option<PathBuf>,
    version_date: Option<String>,
}

fn parse_args() -> anyhow::Result<Args> {
    let argv: Vec<String> = std::env::args().collect();
    let mut config = None;
    let mut cache = None;
    let mut out = None;
    let mut report = None;
    let mut version_date = None;

    let mut i = 1;
    while i < argv.len() {
        let need = |i: usize| -> anyhow::Result<String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{} 缺少参数值", argv[i]))
        };
        match argv[i].as_str() {
            "--config" => {
                config = Some(PathBuf::from(need(i)?));
                i += 2;
            }
            "--cache" => {
                cache = Some(PathBuf::from(need(i)?));
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(need(i)?));
                i += 2;
            }
            "--report" => {
                report = Some(PathBuf::from(need(i)?));
                i += 2;
            }
            "--version-date" => {
                version_date = Some(need(i)?);
                i += 2;
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => anyhow::bail!("未知参数: {other}（-h 查看用法）"),
        }
    }

    let config = config.unwrap_or_else(|| {
        // 相对 crate 源码位置的默认配置，便于直接 cargo run
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/gen_dict/gen_dict.toml")
    });
    Ok(Args {
        config,
        cache: cache.ok_or_else(|| anyhow::anyhow!("缺少 --cache <目录>（-h 查看用法）"))?,
        out: out.ok_or_else(|| anyhow::anyhow!("缺少 --out <目录>（-h 查看用法）"))?,
        report,
        version_date,
    })
}

fn print_usage() {
    eprintln!(
        "gen_dict：生成五笔主词库并拆分扩展词库\n\
         \n\
         用法:\n  \
           gen_dict --cache <dir> --out <dir> [选项]\n\
         \n\
         必需:\n  \
           --cache <dir>          源数据目录（含 rime-wubi/ 与 pinyin-frost/）\n  \
           --out <dir>            词库输出目录\n\
         \n\
         选项:\n  \
           --config <path>        配置文件，默认 data/gen_dict/gen_dict.toml\n  \
           --report <dir>         分析报告输出目录，省略则不写报告\n  \
           --version-date <date>  版本戳 YYYY-MM-DD，默认当前 UTC 日期\n"
    );
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let cfg = Config::load(&args.config)?;
    let config_dir = args.config.parent().unwrap_or(Path::new("."));
    let paths = cfg.resolve_paths(config_dir, &args.cache, &args.out, args.report.as_deref());
    let version = args.version_date.clone().unwrap_or_else(writer::today_utc);

    eprintln!("已加载配置: {}", args.config.display());
    eprintln!("  jidian   : {}", paths.jidian.display());
    eprintln!("  unigram  : {}", paths.unigram.display());
    eprintln!("  输出路径 : {}", paths.output.display());
    eprintln!("  版本戳   : {version}");
    eprintln!(
        "  权重归一化: 中位→{}  上限={}  下限={}  单字提权×{:.1}",
        cfg.target_median, cfg.weight_max, cfg.weight_min, cfg.char_boost_factor
    );
    if cfg.shortcodes.enabled {
        eprintln!(
            "  简码分层 : 一级={}  二级基={}  三级基={}  普通上限={}",
            cfg.shortcodes.level1_weight,
            cfg.shortcodes.level2_base_weight,
            cfg.shortcodes.level3_base_weight,
            cfg.regular_weight_max
        );
    }
    eprintln!();

    run(&cfg, &paths, &version)
}

fn run(cfg: &Config, paths: &config::Paths, version: &str) -> anyhow::Result<()> {
    let mut log = |s: String| eprintln!("{s}");

    // ── 1. unigram ────────────────────────────────────
    let size_mb = std::fs::metadata(&paths.unigram)
        .map(|m| m.len() / 1024 / 1024)
        .unwrap_or(0);
    eprintln!("[1/4] 加载 unigram.txt ({size_mb} MB)...");
    let unigram = weight::load_unigram(&paths.unigram)?;
    eprintln!("      加载完成: {} 条词频记录", unigram.len());

    // ── 2. jidian ─────────────────────────────────────
    eprintln!("[2/4] 加载 jidian 词典...");
    let jidian = parse::parse_jidian(&paths.jidian)?;
    eprintln!("      {}: {} 条", paths.jidian.display(), jidian.len());
    if jidian.is_empty() {
        anyhow::bail!("jidian 词库为空，检查 --cache 路径与源数据是否完整");
    }

    // 单字→首选编码反查表：供自定义词反查与 extra 非法编码修正复用
    let char_codes = reverse::build_char_code_map(&jidian);

    // ── 3. 过滤 + 赋权 ────────────────────────────────
    eprintln!("[3/4] 过滤 + 补充词频...");
    let mut kept: Vec<Entry> = Vec::with_capacity(jidian.len());
    let mut dropped: Vec<(&'static str, Entry)> = Vec::new();
    let mut filter_stats: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for e in jidian {
        match entry::should_keep(&e, cfg) {
            Ok(()) => kept.push(e),
            Err(reason) => {
                *filter_stats.entry(reason).or_default() += 1;
                dropped.push((reason, e));
            }
        }
    }
    eprintln!("      保留: {}  过滤: {}", kept.len(), dropped.len());
    let mut stats: Vec<_> = filter_stats.iter().collect();
    stats.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (reason, n) in stats {
        eprintln!("        - {reason}: {n}");
    }

    // 简码分层必须先于 unigram 赋权：赋权阶段靠 shortcode_level 跳过这些条目
    if cfg.shortcodes.enabled {
        shortcode::assign_shortcode_weights(&mut kept, cfg);
        eprintln!(
            "      简码分层: 一级={}  二级={}  三级={}",
            shortcode::count_level(&kept, 1),
            shortcode::count_level(&kept, 2),
            shortcode::count_level(&kept, 3)
        );
    }

    // 归一化基准只取 jidian 过滤后的词条，自定义词不参与（避免少量词拉偏中位）
    let median_raw = weight::median_raw_freq(&kept, &unigram);
    let log_median = (median_raw + 1.0).log10();
    let hit = kept
        .iter()
        .filter(|e| unigram.contains_key(&e.text))
        .count();
    eprintln!(
        "      unigram 命中: {hit} ({}%)  未命中: {}",
        hit * 100 / kept.len(),
        kept.len() - hit
    );
    eprintln!("      中位原始频次: {median_raw:.0}  (log10={log_median:.3})");

    if let Some(p) = &paths.custom_words
        && p.exists()
    {
        eprintln!("      加载自定义词表: {}", p.display());
        match reverse::load_custom_words(p, &char_codes, &unigram, log_median, cfg, &mut log) {
            Ok(v) => {
                eprintln!("      自定义词条: {} 条", v.len());
                kept.extend(v);
            }
            Err(e) => eprintln!("      [警告] 自定义词表加载失败: {e}"),
        }
    }

    let regular_max = cfg.regular_max();
    let mut weight_buckets: std::collections::BTreeMap<String, usize> = Default::default();
    for e in kept.iter_mut() {
        if e.shortcode_level > 0 {
            *weight_buckets.entry("简码".into()).or_default() += 1;
            continue;
        }
        let is_char = e.is_single_char();
        // 先读原始优先级再覆写：读到已覆写的值会把生僻字全打成同一档
        let orig_priority = e.weight;
        match unigram.get(&e.text) {
            Some(&freq) => {
                let mut w = weight::compute_weight(freq, log_median, cfg);
                if is_char && cfg.char_boost_factor != 1.0 {
                    w = weight::clamp_weight(
                        (w as f64 * cfg.char_boost_factor).round() as i64,
                        cfg,
                    );
                }
                w = w.min(regular_max);
                e.weight = w;
                let lo = (w / 500) * 500;
                *weight_buckets
                    .entry(format!("{lo}-{}", lo + 499))
                    .or_default() += 1;
            }
            None => {
                e.weight = weight::fallback_weight(orig_priority, cfg);
                *weight_buckets.entry("<200(生僻)".into()).or_default() += 1;
            }
        }
    }

    eprintln!("\n      权重分布预览:");
    let mut buckets: Vec<(&String, &usize)> = weight_buckets.iter().collect();
    buckets.sort_by_key(|(k, _)| {
        if k.as_str() == "<200(生僻)" {
            -1
        } else if k.as_str() == "简码" {
            i64::MAX
        } else {
            k.split('-')
                .next()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0)
        }
    });
    for (k, cnt) in buckets {
        let bar = "█".repeat(cnt * 30 / kept.len());
        eprintln!("        {k:>15}: {cnt:>6}  {bar}");
    }

    // 降权前的冲突快照：降权后的权重已是调整结果，拿它调参会自我循环
    let pre_demotion = if cfg.shortcodes.enabled {
        shortcode::analyze_conflicts(&kept)
    } else {
        Vec::new()
    };
    if cfg.shortcodes.enabled && cfg.demotion.enabled {
        let n = shortcode::apply_demotion(&mut kept, cfg);
        if n > 0 {
            eprintln!("\n      简码降权: {n} 条简码字被降权（第二候选满足权重+gap条件）");
        } else {
            eprintln!("\n      简码降权: 无符合条件的降权条目");
        }
    }

    // 词序提升在自动权重与降权之后，是人工微调的最后一道闸
    if let Some(p) = &paths.boosts
        && p.exists()
    {
        eprintln!("\n      加载词序提升表: {}", p.display());
        match boost::load_boost_rules(p) {
            Ok(rules) if !rules.is_empty() => {
                let (applied, missing) = boost::apply_boost_rules(&mut kept, &rules, &mut log);
                eprintln!("      词序提升: {applied} 条生效，{missing} 条未匹配");
            }
            Ok(_) => {}
            Err(e) => eprintln!("      [警告] boost 解析失败: {e}"),
        }
    }

    // 编码升序 → 同码权重降序。**不加 text 末级**：同码同权靠稳定排序保持 jidian 行序，
    // 加了会把大量同权条目按字典序重排。
    kept.sort_by(|a, b| a.code.cmp(&b.code).then_with(|| b.weight.cmp(&a.weight)));

    // ── 4. 写出 ───────────────────────────────────────
    eprintln!("\n[4/4] 写出到 {} ...", paths.output.display());
    writer::write_main_dict(&paths.output, &kept, cfg, version)?;
    let size_kb = std::fs::metadata(&paths.output)
        .map(|m| m.len() / 1024)
        .unwrap_or(0);
    eprintln!("      完成: {} 条，{size_kb} KB", kept.len());

    if cfg.extra.enabled
        && let Err(e) = process_extra(cfg, paths, &unigram, log_median, &char_codes, version)
    {
        eprintln!("      [警告] extra 处理失败: {e}");
    }

    // 原样透传的词库：只清洗头部，条目顺序即数据，不得重排
    for (src, dst) in &paths.passthrough {
        if !src.exists() {
            eprintln!("      [警告] 透传源不存在，跳过: {}", src.display());
            continue;
        }
        match writer::passthrough_stripping_sort(src, dst) {
            Ok(stripped) => {
                let note = if stripped {
                    "，已清除 sort: 键"
                } else {
                    ""
                };
                eprintln!("      [透传] {} → {}{note}", src.display(), dst.display());
            }
            Err(e) => eprintln!("      [警告] 透传失败 {}: {e}", src.display()),
        }
    }

    if cfg.shortcodes.enabled {
        let conflicts = shortcode::analyze_conflicts(&kept);
        eprintln!("      简码避让冲突: 共 {} 处（降权后）", conflicts.len());
        if let Some(p) = &paths.conflict_report {
            match writer::write_conflict_report(p, &conflicts) {
                Ok(()) => eprintln!("      冲突报告: {}", p.display()),
                Err(e) => eprintln!("      [警告] 冲突报告写出失败: {e}"),
            }
        }
        if let Some(p) = &paths.demotion_report {
            let source = if pre_demotion.is_empty() {
                &conflicts
            } else {
                &pre_demotion
            };
            match writer::write_demotion_report(p, source) {
                Ok(()) => eprintln!("      降权报告: {}（降权前快照）", p.display()),
                Err(e) => eprintln!("      [警告] 降权报告写出失败: {e}"),
            }
        }
    }

    if !dropped.is_empty()
        && let Some(p) = &paths.dropped
    {
        match writer::write_dropped(p, &dropped) {
            Ok(()) => eprintln!("      过滤条目已写出: {}", p.display()),
            Err(e) => eprintln!("      [警告] 过滤条目写出失败: {e}"),
        }
    }

    eprintln!("\n✓ 完成");
    Ok(())
}

/// 扩展词库：按字符类型拆成 4 个文件，各自赋权后写出。
fn process_extra(
    cfg: &Config,
    paths: &config::Paths,
    unigram: &weight::Unigram,
    log_median: f64,
    char_codes: &reverse::CharCodes,
    version: &str,
) -> anyhow::Result<()> {
    let Some(input) = &paths.extra_input else {
        anyhow::bail!("extra.input_path 未配置");
    };
    if !input.exists() {
        eprintln!("\n[extra] 跳过：输入文件不存在 ({})", input.display());
        return Ok(());
    }

    eprintln!("\n[extra] 处理扩展词库: {}", input.display());
    let mut log = |s: String| eprintln!("{s}");
    let entries = parse::parse_extra_dict(input, char_codes, &mut log)?;
    eprintln!("      读取 {} 条原始条目", entries.len());

    let mut buckets: Vec<(extra::Category, Vec<Entry>)> = extra::Category::ALL
        .iter()
        .map(|c| (*c, Vec::new()))
        .collect();
    for e in entries {
        let cat = extra::classify(&e.text);
        let slot = buckets
            .iter_mut()
            .find(|(c, _)| *c == cat)
            .expect("四类已穷举");
        slot.1.push(e);
    }

    let (cjk_hit, cjk_total) = extra::assign_weights(&mut buckets, unigram, log_median, cfg);

    // 自定义 emoji 置于 emoji 桶最前：它们是手工维护的常用表情快捷入口
    match extra::load_custom_emoji(paths.custom_emoji.as_deref()) {
        Ok(custom) if !custom.is_empty() => {
            let n = custom.len();
            let slot = buckets
                .iter_mut()
                .find(|(c, _)| *c == extra::Category::Emoji)
                .unwrap();
            let mut merged = custom;
            merged.append(&mut slot.1);
            slot.1 = merged;
            eprintln!("      [custom_emoji] 注入 {n} 条 emoj 编码条目");
        }
        Ok(_) => {}
        Err(e) => eprintln!("      [custom_emoji] 加载失败，跳过: {e}"),
    }

    // CLDR 命名表：中文名反查五笔码，与上面的固定 `emoj` 码是两条独立通路。
    //
    // 与 custom_emoji 一样**直接注入 emoji 桶、不经 classify**：`has_emoji` 的区间表
    // 漏判 ⭐(U+2B50)、国旗(U+1F1E6..)、keycap(ASCII+U+20E3) 等 76 个字形，若改成走
    // classify，这些条目会落进 symbols/english 桶而从 emoji 库里消失。
    match extra::load_named_emoji(paths.custom_emoji_named.as_deref(), char_codes, &mut log) {
        Ok(named) if !named.is_empty() => {
            let n = named.len();
            let slot = buckets
                .iter_mut()
                .find(|(c, _)| *c == extra::Category::Emoji)
                .expect("emoji 桶必然存在");
            slot.1.extend(named);
            eprintln!("      [emoji_named] 注入 {n} 条五笔码 emoji 条目");
        }
        Ok(_) => {}
        Err(e) => eprintln!("      [emoji_named] 加载失败，跳过: {e}"),
    }

    // 去重放在所有注入之后：既清掉上游自带的重复，也合并 named 与上游的重合项
    if let Some((_, list)) = buckets
        .iter_mut()
        .find(|(c, _)| *c == extra::Category::Emoji)
    {
        let removed = extra::dedup_emoji_entries(list);
        if removed > 0 {
            eprintln!("      [emoji] 去重移除 {removed} 条（同码同 emoji，保留权重最高者）");
        }
    }

    for (cat, list) in buckets.iter_mut() {
        let name = format!("{}_{}", cfg.output_name, cat.suffix());
        let path = extra::extra_output_path(&paths.output, &cfg.output_name, cat.suffix());
        writer::write_extra_dict(&path, list, &name, *cat, version)?;
        eprintln!(
            "      [{}] {} 条 → {}",
            cat.suffix(),
            list.len(),
            path.display()
        );
    }

    if log_median > 0.0 && cjk_total > 0 {
        eprintln!(
            "      CJK unigram 命中率: {cjk_hit}/{cjk_total} ({:.1}%)",
            100.0 * cjk_hit as f64 / cjk_total as f64
        );
    }
    Ok(())
}
