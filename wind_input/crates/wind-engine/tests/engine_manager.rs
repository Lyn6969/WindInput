//! 引擎管理器端到端测试
//!
//! 用仓库内真实 schema 构建 EngineManager，验证五笔/拼音转换产出候选。
//! 词典缺失时自动跳过。

use std::path::PathBuf;
use wind_config::Config;
use wind_engine::EngineManager;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build_dev/data")
}

fn make_config(schemas: &[&str]) -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = schemas.iter().map(|s| s.to_string()).collect();
    cfg.schema.active = schemas[0].to_string();
    cfg
}

fn schema_exists(dir: &std::path::Path, id: &str) -> bool {
    dir.join(format!("schemas/{}.schema.toml", id)).exists()
        || dir.join(format!("schemas/{}.schema.yaml", id)).exists()
}

/// english 方案（隐藏，懒加载）：ensure_schema 应可加载，convert_with 前缀查词命中，
/// 候选来源标记为 English，且无自动上屏。词库/schema 缺失时跳过。
#[test]
fn test_english_schema_lazy_loads_and_converts() {
    // build_dev 可能位于 wind_input/build_dev（data_dir()）或产品仓根 build_dev；两处都试。
    let dir = [
        data_dir(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data"),
    ]
    .into_iter()
    .find(|d| d.join("schemas/english/en.dict.yaml").exists())
    .unwrap_or_else(data_dir);
    if !schema_exists(&dir, "english") || !dir.join("schemas/english/en.dict.yaml").exists() {
        eprintln!("跳过：english schema/词库不存在");
        return;
    }
    // 活跃方案用 wubi86；english 仅作隐藏方案懒加载（不在 available）。
    let cfg = make_config(&["wubi86"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    assert!(mgr.ensure_schema("english"), "english 方案应可懒加载");

    let result = mgr.convert_with("english", "hel", 50);
    assert!(!result.candidates.is_empty(), "english 'hel' 应产出前缀候选");
    assert!(
        result
            .candidates
            .iter()
            .any(|c| c.text.eq_ignore_ascii_case("hello")),
        "应包含 hello，实际前几个: {:?}",
        result
            .candidates
            .iter()
            .take(5)
            .map(|c| &c.text)
            .collect::<Vec<_>>()
    );
    assert!(
        result
            .candidates
            .iter()
            .all(|c| c.source == wind_candidate::CandidateSource::English),
        "english 候选来源应全部标记为 English"
    );
    assert!(!result.should_commit, "english 不应自动上屏");
}

#[test]
fn test_wubi_engine_candidates() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") {
        eprintln!("跳过：wubi86 schema 不存在");
        return;
    }
    let cfg = make_config(&["wubi86"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    let result = mgr.convert("aaaa", 9);
    assert!(!result.candidates.is_empty(), "五笔 'aaaa' 应产出候选");
    assert!(
        result.candidates.iter().any(|c| c.text == "恭恭敬敬"),
        "应包含 恭恭敬敬，实际: {:?}",
        result
            .candidates
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
    );
    assert!(!mgr.is_pinyin(), "wubi86 不应判定为拼音");
}

#[test]
fn test_wubi_extra_dict_loaded() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") {
        eprintln!("跳过：wubi86 schema 不存在");
        return;
    }
    // 删除旧 combined 缓存，强制重新合并多库
    let _ = std::fs::remove_file(dir.join("schemas/wubi86/wubi86_jidian.dict.combined.wdb"));
    let cfg = make_config(&["wubi86"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // "甘蓝菜"(aaae) 仅存在于扩展库 wubi86_jidian_extra；主库没有。
    // 能查到即证明扩展库已被合并加载。
    let r = mgr.convert("aaae", 20);
    assert!(
        r.candidates.iter().any(|c| c.text == "甘蓝菜"),
        "扩展库词 '甘蓝菜'(aaae) 应能查到，实际: {:?}",
        r.candidates
            .iter()
            .take(10)
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
    );

    // 主库词仍在
    let a = mgr.convert("aaaa", 20);
    assert!(
        a.candidates.iter().any(|c| c.text == "恭恭敬敬"),
        "主库词应仍在"
    );
}

/// 后台预热 + single-flight 构建锁：并发预热同一方案不重复构建/不死锁，最终就绪。
#[test]
fn test_prewarm_single_flight() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") || !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：需要 wubi86 + pinyin schema");
        return;
    }
    let cfg = make_config(&["wubi86", "pinyin"]); // active=wubi86
    let mgr = std::sync::Arc::new(EngineManager::new(&cfg, Some(&dir)));

    assert!(mgr.is_loaded("wubi86"), "活跃方案应已同步加载");
    assert!(!mgr.is_loaded("pinyin"), "非活跃方案初始未加载");

    // 4 线程并发预热同一方案：single-flight 应只构建一次、不死锁、全部成功返回。
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let m = std::sync::Arc::clone(&mgr);
            std::thread::spawn(move || m.prewarm_schema("pinyin"))
        })
        .collect();
    let oks: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert!(oks.iter().all(|&b| b), "并发预热应全部成功: {oks:?}");
    assert!(mgr.is_loaded("pinyin"), "预热后 pinyin 应已加载");
    assert!(!mgr.is_building("pinyin"), "加载完成后不应再报构建中");
}

/// 扩展词库 **live 热插拔**：对已加载引擎翻 enabled 标志即时改候选，无需重建。
#[test]
fn test_codetable_extra_hot_toggle() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") {
        eprintln!("跳过：wubi86 schema 不存在");
        return;
    }
    let cfg = make_config(&["wubi86"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // 扩展词库 id（非默认、有 path）
    let Some(merged) = mgr.schema_merged("wubi86") else {
        eprintln!("跳过：无法读取 wubi86");
        return;
    };
    let extra_ids: Vec<String> = merged
        .dictionaries
        .iter()
        .filter(|d| !d.default && !d.path.is_empty())
        .map(|d| d.id.clone())
        .collect();
    if extra_ids.is_empty() {
        eprintln!("跳过：wubi86 无扩展词库");
        return;
    }

    // 触发引擎加载并确认扩展库词 '甘蓝菜'(aaae) 初始可见
    let has_extra = |m: &EngineManager| {
        m.convert("aaae", 20)
            .candidates
            .iter()
            .any(|c| c.text == "甘蓝菜")
    };
    if !has_extra(&mgr) {
        eprintln!("跳过：扩展库词 '甘蓝菜' 不在该数据集");
        return;
    }

    // 热关闭全部扩展（live，不重建）→ '甘蓝菜' 消失
    for id in &extra_ids {
        assert!(
            mgr.set_dict_enabled_live("wubi86", id, false),
            "已加载引擎应即时命中扩展层: {id}"
        );
    }
    assert!(
        !has_extra(&mgr),
        "热关闭扩展后 '甘蓝菜' 应消失（live，未重建）"
    );
    assert!(
        mgr.convert("aaaa", 20)
            .candidates
            .iter()
            .any(|c| c.text == "恭恭敬敬"),
        "主库词不受扩展开关影响"
    );

    // 热重新开启 → '甘蓝菜' 回来
    for id in &extra_ids {
        assert!(mgr.set_dict_enabled_live("wubi86", id, true));
    }
    assert!(has_extra(&mgr), "热开启扩展后 '甘蓝菜' 应回来");
}

#[test]
fn test_pinyin_engine_candidates() {
    let dir = data_dir();
    if !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    assert!(mgr.is_pinyin(), "pinyin 应判定为拼音");

    let result = mgr.convert("nihao", 9);
    assert!(!result.candidates.is_empty(), "拼音 'nihao' 应产出候选");
    let top10: Vec<&str> = result
        .candidates
        .iter()
        .take(10)
        .map(|c| c.text.as_str())
        .collect();
    // 整句应在首位（SENTENCE_WEIGHT_BASE 置顶）。
    assert_eq!(
        result.candidates[0].text, "你好",
        "首候选应为 你好，实际: {top10:?}"
    );
    // 前缀子候选「你」应存在并标注只消费「ni」（分段上屏）。
    let ni = result.candidates.iter().find(|c| c.text == "你");
    assert!(ni.is_some(), "应包含前缀候选 你，实际: {top10:?}");
    assert_eq!(ni.unwrap().consumed_length, 2, "你 应只消费 ni 两字节");
    // 非前缀子串「好」（来自 hao 段）不应作为 nihao 的直接候选出现。
    assert!(
        !result.candidates.iter().any(|c| c.text == "好"),
        "不应包含非前缀子候选 好，实际: {top10:?}"
    );
}

#[test]
fn test_pinyin_long_sentence() {
    let dir = data_dir();
    if !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // 长拼音串：Viterbi 整句解码应产出一个 >=4 字的合理句子候选
    let r = mgr.convert("woaizhongguo", 20);
    let longest = r
        .candidates
        .iter()
        .map(|c| c.text.chars().count())
        .max()
        .unwrap_or(0);
    eprintln!(
        "woaizhongguo 候选: {:?}",
        r.candidates
            .iter()
            .take(8)
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        longest >= 4,
        "长句应产出 >=4 字候选（Viterbi+unigram），最长仅 {} 字",
        longest
    );
    assert!(
        r.candidates.iter().any(|c| c.text == "我爱中国"),
        "应能整句解码出 我爱中国"
    );
}

#[test]
fn test_mixed_wubi_priority_and_consistency() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86_pinyin") {
        eprintln!("跳过：wubi86_pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["wubi86_pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // cang：五笔精确全码「駏」(+10M tier) 应压过拼音「藏」(/100)，首候选=駏（五笔优先）。
    let r = mgr.convert("cang", 9);
    let top: Vec<&str> = r
        .candidates
        .iter()
        .take(6)
        .map(|c| c.text.as_str())
        .collect();
    assert_eq!(
        r.candidates[0].text, "駏",
        "cang 首候选应为五笔精确码 駏，实际: {top:?}"
    );
    // 一致性：若放行全码自动上屏，commit_text 必等于显示首候选（杜绝显示/上屏漂移）。
    if r.should_commit {
        assert_eq!(
            r.commit_text, r.candidates[0].text,
            "全码上屏文本应与首候选一致"
        );
    }
    // 拼音「藏」仍在候选中（可选），只是不在首位。
    assert!(
        r.candidates.iter().any(|c| c.text == "藏"),
        "藏 应仍可选: {top:?}"
    );
}

#[test]
fn test_mixed_multisyllable_pinyin_preedit_separated() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86_pinyin") {
        eprintln!("跳过：wubi86_pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["wubi86_pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));
    // 多音节拼音：组合区应带音节分隔（"ni'hao"），而非连写。
    let r = mgr.convert("nihao", 9);
    assert!(
        r.preedit_display.contains('\''),
        "多音节拼音组合区应有音节分隔，实际 preedit: {:?}",
        r.preedit_display
    );
    // 混输高亮跟随：拼音拆分形态须单独留存（供协调器在高亮拼音候选时取用、高亮五笔候选时
    // 改回原始码）。多音节拼音应填充且含 ' 分隔。
    assert!(
        r.preedit_pinyin.contains('\''),
        "混输应留存拼音拆分形态 preedit_pinyin，实际: {:?}",
        r.preedit_pinyin
    );
}

#[test]
fn test_pinyin_trailing_partial_keeps_sentence() {
    let dir = data_dir();
    if !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // 尾部多打一个不成音节的残码「m」：整句「你好」仍应排首位（bug①），
    // 残码不计入消费（consumed_length=5，留「m」在缓冲续输）。
    let r = mgr.convert("nihaom", 9);
    let top: Vec<&str> = r
        .candidates
        .iter()
        .take(8)
        .map(|c| c.text.as_str())
        .collect();
    assert_eq!(
        r.candidates[0].text, "你好",
        "首候选应为 你好（残码不破坏整句），实际: {top:?}"
    );
    assert_eq!(
        r.candidates[0].consumed_length, 5,
        "你好 应只消费 nihao 五字节，残码 m 留缓冲"
    );
}

#[test]
fn test_schema_cycle() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") || !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let cfg = make_config(&["wubi86", "pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    assert_eq!(mgr.active_schema_id(), "wubi86");
    let next = mgr.cycle_schema();
    assert_eq!(next.as_deref(), Some("pinyin"));
    assert_eq!(mgr.active_schema_id(), "pinyin");
    assert!(mgr.is_pinyin());
}

/// available 中夹杂构建失败的方案时，循环应跳过它找到下一个已加载方案。
#[test]
fn test_schema_cycle_skips_unloaded() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") || !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：缺少 schema");
        return;
    }
    // 中间插入一个不存在的方案 → 构建失败、不会进入 engines
    let cfg = make_config(&["wubi86", "__nonexistent__", "pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));
    assert_eq!(mgr.active_schema_id(), "wubi86");
    let next = mgr.cycle_schema();
    assert_eq!(
        next.as_deref(),
        Some("pinyin"),
        "应跳过未加载方案直达 pinyin"
    );
}

/// 方案显示名取自 schema.name（friendly），未知方案回退 id。
#[test]
fn test_schema_name_from_meta() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") || !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let cfg = make_config(&["wubi86"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));
    assert_eq!(mgr.schema_name("wubi86"), "五笔");
    assert_eq!(mgr.schema_name("pinyin"), "全拼");
    // 未知方案：回退 id 本身
    assert_eq!(mgr.schema_name("__nonexistent__"), "__nonexistent__");
}

/// 配置热重载：切换活跃方案、更新可用列表，无需重建 EngineManager。
#[test]
fn test_reload_from_config_switches_active() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") || !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let cfg = make_config(&["wubi86", "pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));
    assert_eq!(mgr.active_schema_id(), "wubi86");
    assert!(!mgr.is_pinyin());

    // 新配置：活跃切到 pinyin（顺序也变）
    let mut cfg2 = Config::default();
    cfg2.schema.available = vec!["pinyin".to_string(), "wubi86".to_string()];
    cfg2.schema.active = "pinyin".to_string();
    let changed = mgr.reload_from_config(&cfg2);
    assert!(changed, "活跃方案应从 wubi86 切到 pinyin");
    assert_eq!(mgr.active_schema_id(), "pinyin");
    assert!(mgr.is_pinyin(), "重载后应为拼音引擎");
    assert_eq!(
        mgr.available_schemas(),
        vec!["pinyin".to_string(), "wubi86".to_string()],
        "可用列表应反映新配置"
    );

    // 相同配置再次重载：活跃未变 → false
    let again = mgr.reload_from_config(&cfg2);
    assert!(!again, "活跃方案未变时应返回 false");
    assert_eq!(mgr.active_schema_id(), "pinyin");
}

/// 简拼（声母缩写）经 wdat 独立 AbbrevSection 产出候选：bzd→不知道 / bj→北京 等。
/// 这是「简拼能力」的回归保护（迁 wdat 前简拼完全失效，返回空）。
#[test]
fn test_pinyin_abbrev() {
    let dir = data_dir();
    if !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));
    let has = |input: &str, want: &str| -> bool {
        mgr.convert(input, 20)
            .candidates
            .iter()
            .any(|c| c.text == want)
    };
    assert!(has("bzd", "不知道"), "简拼 bzd 应含 不知道");
    assert!(has("bj", "北京"), "简拼 bj 应含 北京");
    assert!(has("nh", "你好"), "简拼 nh 应含 你好");
    assert!(has("zg", "中国"), "简拼 zg 应含 中国");
    assert!(has("zgr", "中国人"), "三字简拼 zgr 应含 中国人");
    // 全拼仍正常（简拼区段不影响全拼查询）。
    assert!(has("nihao", "你好"), "全拼 nihao 应含 你好");
}
