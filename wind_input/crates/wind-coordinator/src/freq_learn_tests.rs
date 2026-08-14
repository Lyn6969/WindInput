//! 词频路由 / 选词记账 / 自动造词 / 加词的 crate 内行为测试（白盒，零 RPC）。
//!
//! 原住 webdata 契约测试；webdata 独立成 crate 后按「是否用 web_data_rpc」分拣：
//! 用则留 wind-webdata（经公开面/debug_* 支撑），不用则属 coordinator 行为测试。

use std::sync::Arc;

use wind_candidate::CandidateSource;

use wind_config::Config;
use wind_store::Store;

use crate::coordinator::Coordinator;

/// 构造一个带临时 store 的无头 Coordinator（与 wind-webdata 契约测试同款 helper）。
fn coord(tag: &str) -> Arc<Coordinator> {
    let path = std::env::temp_dir().join(format!("wind_freqlearn_{tag}.redb"));
    let _ = std::fs::remove_file(&path);
    let store = Arc::new(Store::open(&path).unwrap());
    Coordinator::new_headless_with_store(Config::default(), None, store)
}

#[test]
fn record_input_stats_fallback_records_full_classes() {
    use wind_bridge::handler::KeyAction;
    use wind_store::stats::CommitSource;
    let c = coord("stats_fallback");
    // 顶层 fallback：上屏「你好abc，」→ 4 分类，含中文推测来源为候选。
    c.debug_record_input_stats(&KeyAction::InsertText {
        text: "你好abc，".to_string(),
        new_composition: None,
        mode_changed: false,
        chinese_mode: true,
        has_new_composition: false,
    });
    let day = c.stat_collector.as_ref().unwrap().get_today_stat();
    assert_eq!(day.chinese, 2);
    assert_eq!(day.english, 3);
    assert_eq!(day.punct, 1);
    assert_eq!(day.commit_count, 1);
    assert_eq!(day.by_source[CommitSource::Candidate.index()], 6);
}

#[test]
fn record_commit_captures_code_len_and_pos() {
    use wind_store::stats::CommitSource;
    let c = coord("stats_commit");
    c.debug_record_commit("你好", 4, 0, CommitSource::Candidate);
    let day = c.stat_collector.as_ref().unwrap().get_today_stat();
    assert_eq!(day.chinese, 2);
    assert_eq!(day.code_len_sum, 4);
    assert_eq!(day.code_len_count, 1);
    assert_eq!(day.cand_pos_dist[0], 1);
    assert!(
        c.debug_stat_recorded(),
        "record_commit 应置位 stat_recorded"
    );
}

#[test]
fn record_input_stats_skips_when_already_recorded() {
    use wind_bridge::handler::KeyAction;
    use wind_store::stats::CommitSource;
    let c = coord("stats_skip");
    // 具体路径已记录 → 顶层 fallback 应跳过，不重复计数。
    c.debug_record_commit("你好", 4, 0, CommitSource::Candidate);
    c.debug_record_input_stats(&KeyAction::InsertText {
        text: "你好".to_string(),
        new_composition: None,
        mode_changed: false,
        chinese_mode: true,
        has_new_composition: false,
    });
    let day = c.stat_collector.as_ref().unwrap().get_today_stat();
    assert_eq!(day.commit_count, 1, "已记录则 fallback 跳过");
}

/// P2d：构造带混输方案（primary=ct_test、secondary=py_test）的无头 Coordinator，
/// active=mx_test；返回 (coord, store) 供直查断言。
fn mixed_coord(tag: &str) -> (Arc<Coordinator>, Arc<Store>) {
    use std::io::Write;
    let base_dir = std::env::temp_dir().join(format!("wind_coord_p2d_{tag}"));
    let schemas = base_dir.join("schemas");
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&schemas).unwrap();
    {
        let mut f = std::fs::File::create(schemas.join("py_test.schema.toml")).unwrap();
        write!(
            f,
            "[schema]\nid = \"py_test\"\n[engine]\ntype = \"pinyin\"\n"
        )
        .unwrap();
    }
    {
        let mut f = std::fs::File::create(schemas.join("ct_test.schema.toml")).unwrap();
        write!(
            f,
            "[schema]\nid = \"ct_test\"\n[engine]\ntype = \"codetable\"\n"
        )
        .unwrap();
    }
    {
        let mut f = std::fs::File::create(schemas.join("mx_test.schema.toml")).unwrap();
        write!(
            f,
            "[schema]\nid = \"mx_test\"\n[engine]\ntype = \"mixed\"\n[engine.mixed]\nprimary_schema = \"ct_test\"\nsecondary_schema = \"py_test\"\n"
        )
        .unwrap();
    }
    let mut cfg = Config::default();
    cfg.schema.active = "mx_test".into();
    cfg.schema.available = vec!["mx_test".into(), "ct_test".into(), "py_test".into()];
    // 开启码表词频，供 apply_freq_rerank 测试生效（混输走码表 used-first 路径）。
    cfg.schema.codetable.frequency.enabled = true;
    // 开启码表自动造词，供 learn_phrase_on_commit 测试生效（混输继承主码表 auto_phrase）。
    cfg.schema.codetable.auto_phrase.enabled = true;

    let db_path = std::env::temp_dir().join(format!("wind_coord_p2d_{tag}.redb"));
    let _ = std::fs::remove_file(&db_path);
    let store = Arc::new(Store::open(&db_path).unwrap());
    let c = Coordinator::new_headless_with_store(cfg, Some(base_dir.as_path()), Arc::clone(&store));
    (c, store)
}

/// P2d Task 2：混输 active 下 record_selection 按候选来源落子方案键空间；无法归因跳过。
#[test]
fn mixed_record_selection_routes_by_source() {
    let (c, store) = mixed_coord("record_selection");

    // 码表候选 → 落 primary "ct_test"
    c.record_selection("aaaa", "工", CandidateSource::CodeTable);
    assert!(
        store.get_freq("ct_test", "aaaa", "工").unwrap().is_some(),
        "码表候选应落 primary ct_test 键空间"
    );
    assert!(
        store.get_freq("mx_test", "aaaa", "工").unwrap().is_none(),
        "不应落混输自身 id"
    );
    assert!(
        store.get_freq("pinyin", "aaaa", "工").unwrap().is_none(),
        "不应落 pinyin"
    );

    // 拼音候选 → 落 "pinyin"
    c.record_selection("nihao", "你好", CandidateSource::Pinyin);
    assert!(
        store.get_freq("pinyin", "nihao", "你好").unwrap().is_some(),
        "拼音候选应落 pinyin 共享键空间"
    );

    // 无法归因 → 三处键空间均无写入
    c.record_selection("x", "y", CandidateSource::None);
    assert!(store.get_freq("ct_test", "x", "y").unwrap().is_none());
    assert!(store.get_freq("mx_test", "x", "y").unwrap().is_none());
    assert!(store.get_freq("pinyin", "x", "y").unwrap().is_none());
}

/// P2d Task 2 回归：拼音方案 active 下 record_selection 忽略 source，仍折叠落 "pinyin"。
#[test]
fn pinyin_record_selection_ignores_source() {
    use std::io::Write;
    let base_dir = std::env::temp_dir().join("wind_coord_p2d_pinyin_active");
    let schemas = base_dir.join("schemas");
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&schemas).unwrap();
    {
        let mut f = std::fs::File::create(schemas.join("py_test.schema.toml")).unwrap();
        write!(
            f,
            "[schema]\nid = \"py_test\"\n[engine]\ntype = \"pinyin\"\n"
        )
        .unwrap();
    }
    let mut cfg = Config::default();
    cfg.schema.active = "py_test".into();
    cfg.schema.available = vec!["py_test".into()];
    // 开启拼音调频，供 record_selection 写入测试生效（默认关闭不落库）。
    cfg.schema.pinyin.frequency.enabled = true;
    let db_path = std::env::temp_dir().join("wind_coord_p2d_pinyin_active.redb");
    let _ = std::fs::remove_file(&db_path);
    let store = Arc::new(Store::open(&db_path).unwrap());
    let c = Coordinator::new_headless_with_store(cfg, Some(base_dir.as_path()), Arc::clone(&store));

    c.record_selection("nihao", "你好", CandidateSource::None);
    assert!(
        store.get_freq("pinyin", "nihao", "你好").unwrap().is_some(),
        "拼音方案忽略 source，落 pinyin"
    );
}

/// 回归：词频读写两端的 code 统一为**候选存储码**（全拼扁平域），而非输入缓冲（击键域）。
///
/// 现场：双拼下缓冲是击键 `siyr`、候选码是全拼 `siyuan`（实测 `convert("siyr")` 出的
/// 候选 code 恒为 `siyuan`）。写入端 `commit_selected` 用 `cand_code` → 键 `siyuan`；
/// 读取端曾用输入缓冲 → 键 `siyr`。二者永不相等，**双拼下词频重排整体失效、tooltip
/// 使用次数恒 0**。全拼带分隔符（`xi'an` → 码 `xian`）与前缀补全（`si` → 码 `sikao`）
/// 同形态。
///
/// 判据刻意让「码 ≠ 缓冲」：读侧若退回用缓冲查，`recs` 为空、`apply_freq_rerank` 提前
/// 返回，顺序不变 → 本用例挂。全仓 code 域标准（用户词库 key、造词码、加词码）皆为
/// 全拼扁平码，本测试同时锁住词频与它们对齐。
#[test]
fn freq_lookup_uses_candidate_code_not_input_buffer() {
    use std::io::Write;
    use wind_candidate::{Candidate, CandidateSource};
    let base_dir = std::env::temp_dir().join("wind_coord_freq_code_domain");
    let schemas = base_dir.join("schemas");
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&schemas).unwrap();
    {
        let mut f = std::fs::File::create(schemas.join("py_test.schema.toml")).unwrap();
        write!(
            f,
            "[schema]\nid = \"py_test\"\n[engine]\ntype = \"pinyin\"\n"
        )
        .unwrap();
    }
    let mut cfg = Config::default();
    cfg.schema.active = "py_test".into();
    cfg.schema.available = vec!["py_test".into()];
    cfg.schema.pinyin.frequency.enabled = true;
    let db_path = std::env::temp_dir().join("wind_coord_freq_code_domain.redb");
    let _ = std::fs::remove_file(&db_path);
    let store = Arc::new(Store::open(&db_path).unwrap());
    let c = Coordinator::new_headless_with_store(cfg, Some(base_dir.as_path()), Arc::clone(&store));

    // 双拼形态的候选：击键缓冲 4 字节 `siyr`，存储码 6 字节 `siyuan`。
    let mk = |t: &str| Candidate {
        text: t.to_string(),
        code: "siyuan".to_string(),
        source: CandidateSource::Pinyin,
        consumed_length: 4, // 消费整串击键（consumed_length 已回映射到原始输入空间）
        ..Default::default()
    };

    // ① 记账码来自 cand_code，不是缓冲。
    let picked = mk("思源");
    let code = Coordinator::cand_code("siyr", &picked);
    assert_eq!(code, "siyuan", "记账码须取候选存储码（全拼），不是击键缓冲");
    c.record_selection(&code, "思源", CandidateSource::Pinyin);
    assert!(
        store
            .get_freq("pinyin", "siyuan", "思源")
            .unwrap()
            .is_some(),
        "写入落在全拼码键空间"
    );
    assert!(
        store.get_freq("pinyin", "siyr", "思源").unwrap().is_none(),
        "击键码键空间不应有记录（若有，说明写入端也串了域）"
    );

    // ② 再次按击键缓冲取候选时，词频须读得到 → 「思源」软置前。
    let mut cands = vec![mk("寺院"), mk("思源")];
    c.apply_freq_rerank(&mut cands, "siyr");
    assert_eq!(
        cands[0].text,
        "思源",
        "读侧须按候选存储码查词频；实际: {:?}",
        cands.iter().map(|x| &x.text).collect::<Vec<_>>()
    );
}

/// P2d Task 3：混输 active 下 apply_freq_rerank 按候选来源读子方案词频。
/// 码表候选读 primary(ct_test)、拼音候选读 "pinyin"；命中记录者档内提权。
/// （若读侧仍走 mx_test 单一归属，则两处预置的记录都读不到，无提权 → 测试失败。）
#[test]
fn mixed_freq_rerank_reads_sub_schema() {
    use wind_candidate::{Candidate, CandidateSource};
    let (c, store) = mixed_coord("freq_rerank");
    // 预置：ct_test 名下「工」、pinyin 名下「好」各一条词频。
    store.record_freq("ct_test", "aaaa", "工").unwrap();
    store.record_freq("pinyin", "nihao", "好").unwrap();

    let mk = |t: &str, code: &str, s: CandidateSource| Candidate {
        text: t.to_string(),
        code: code.to_string(),
        source: s,
        ..Default::default()
    };

    // 码表档（tier 0，同 source 同码）：「工」有 ct_test 记录 → 浮到「他」前。
    let mut ct_cands = vec![
        mk("他", "aaaa", CandidateSource::CodeTable),
        mk("工", "aaaa", CandidateSource::CodeTable),
    ];
    c.apply_freq_rerank(&mut ct_cands, "aaaa");
    assert_eq!(
        ct_cands[0].text, "工",
        "码表候选应按 primary(ct_test) 词频提权"
    );

    // 拼音档（tier 3，同 source）：「好」有 pinyin 记录 → 浮到「你」前。
    let mut py_cands = vec![
        mk("你", "nihao", CandidateSource::Pinyin),
        mk("好", "nihao", CandidateSource::Pinyin),
    ];
    c.apply_freq_rerank(&mut py_cands, "nihao");
    assert_eq!(py_cands[0].text, "好", "拼音候选应按 pinyin 词频提权");
}

/// P2d Task 4：混输自动造词按"全段同源"路由——全段拼音落 pinyin 归属，混源跳过，
/// 全段码表同样跳过（拼接码无意义 + 与 auto_phrase 重复，见下方该段注释）。
#[test]
fn mixed_learn_phrase_same_source_only() {
    let (c, store) = mixed_coord("learn_phrase");

    // 全段拼音 → 临时词落 "pinyin"。
    {
        let mut st = c.state.lock().unwrap();
        st.committed_segs.clear();
        // 段各为单音节（段内边界 0b1）→ 自动造词拼出 nihao 时全局边界应为 ni|hao = 0b101。
        st.committed_segs.push((
            "ni".into(),
            "ni".into(),
            "你".into(),
            CandidateSource::Pinyin,
            0b1,
        ));
        st.committed_segs.push((
            "hao".into(),
            "hao".into(),
            "好".into(),
            CandidateSource::Pinyin,
            0b1,
        ));
        c.learn_phrase_on_commit(&st);
    }
    let py_words = store.get_temp_words("pinyin", "nihao").unwrap();
    let nihao = py_words
        .iter()
        .find(|w| w.text == "你好")
        .expect("全段拼音应落 pinyin 临时词");
    // 自动造词的边界：各段边界平移拼接（ni@0 + hao@2）→ ni|hao = 0b101。
    // 这条保证用户自造词从诞生起就带边界，而非「空洞」。
    assert_eq!(
        nihao.boundary, 0b101,
        "自动造词应把段边界平移拼接后落库，实际: {:#b}",
        nihao.boundary
    );

    // 混源（一码表一拼音）→ 三处键空间均无临时词。
    {
        let mut st = c.state.lock().unwrap();
        st.committed_segs.clear();
        // 码表段无音节概念（boundary=0）→ 整词边界作废（半截边界比没有更糟）。
        st.committed_segs.push((
            "aaaa".into(),
            "aaaa".into(),
            "工".into(),
            CandidateSource::CodeTable,
            0,
        ));
        st.committed_segs.push((
            "hao".into(),
            "hao".into(),
            "好".into(),
            CandidateSource::Pinyin,
            0b1,
        ));
        c.learn_phrase_on_commit(&st);
    }
    for schema in ["ct_test", "pinyin", "mx_test"] {
        assert!(
            store.get_temp_words(schema, "aaaahao").unwrap().is_empty(),
            "混源不应落任何临时词（{schema}）"
        );
    }

    // 全段码表 → **不造词**（本断言已反转，理由见下）。
    //
    // ① 语义与本文件下方已移除的 `codetable_learn_phrase_ignores_source` 完全同源：码表词组
    //    编码须按方案 `[[encoder.rules]]` 从各字**全码**取位（五笔「你好」= wqvb），各段码
    //    拼接（aa + bb = "aabb"）得到的串在词库里查不到 —— 正是自动造词历史上「完全不工作」
    //    的根因之一。码表侧造词已迁至 wind-coordinator 的 `auto_phrase` 连续单字缓冲。
    // ② 它当时测的是**现实中不可达**的分支：码表候选 `consumed_length` 恒 0 ⇒ 永不 partial
    //    ⇒ 单段即被 `reset_pinyin_composition` 清掉，混输下永远凑不满 2 段全码表。直到混输
    //    超码长回捞的前缀候选开始如实标注 `consumed_length`（见 `mixed/engine.rs` 的
    //    `convert_overflow`）这条路才第一次可达 —— 而可达之后产出的正是 ① 里那种错码，
    //    还会与 auto_phrase 对同一次输入重复造词。故 `learn_phrase_on_commit` 显式跳过。
    {
        let mut st = c.state.lock().unwrap();
        st.committed_segs.clear();
        st.committed_segs.push((
            "aa".into(),
            "aa".into(),
            "工".into(),
            CandidateSource::CodeTable,
            0,
        ));
        st.committed_segs.push((
            "bb".into(),
            "bb".into(),
            "人".into(),
            CandidateSource::CodeTable,
            0,
        ));
        c.learn_phrase_on_commit(&st);
    }
    for schema in ["ct_test", "pinyin", "mx_test"] {
        assert!(
            store.get_temp_words(schema, "aabb").unwrap().is_empty(),
            "全段码表不应落任何临时词（{schema}）——拼接码 aabb 在码表里查不到，\
             且码表侧造词归 auto_phrase 连续单字缓冲管"
        );
    }
}

// 【已移除】`codetable_learn_phrase_ignores_source`（P2d Task 4 回归）
//
// 该测试断言纯码表方案经 `committed_segs` 造词、编码为各段码**拼接**（aa + bb = "aabb"）。
// 两点使其不再成立：
//   ① 语义已判定为错。码表词组编码须按方案 `[[encoder.rules]]` 的公式从各字**全码**取位
//      （五笔「你好」= wqvb），拼接各段码得到的串在词库里查不到 —— 这正是自动造词
//      历史上「完全不工作」的根因之一。码表已迁至 wind-coordinator 的 `auto_phrase` 连续单字缓冲。
//   ② 它本就只在**引擎加载失败**时才通过。测试方案 `ct_test` 无 `dictionaries`，引擎加载不出，
//      `is_codetable()` 退化为 false，才落进非码表分支。真实码表方案不会走到这里。
//
// 替代覆盖：`tests/input_flow.rs` 的 `test_codetable_auto_phrase_*` 四条，用**真实 wubi86
// 方案与词库**端到端验证取码、终止信号时机与开关闸门。

/// P2d Task 5：混输 active 下手动加词（RPC dict.add）落主码表方案；primary 缺失则报错不 panic。
#[test]
fn mixed_manual_addword_goes_to_primary() {
    let (c, store) = mixed_coord("manual_addword");
    // 手动加词是码表语义 → 落 primary "ct_test"。
    c.cmd_dict_add("工", "aaaa").unwrap();
    assert!(
        store
            .get_user_words("ct_test", "aaaa")
            .unwrap()
            .iter()
            .any(|w| w.text == "工"),
        "混输手动加词应落 primary ct_test"
    );
    assert!(
        store.get_user_words("mx_test", "aaaa").unwrap().is_empty(),
        "不应落混输自身 id"
    );

    // primary 缺失的坏配置 → 返回 Err，不 panic，不写库。
    use std::io::Write;
    let base_dir = std::env::temp_dir().join("wind_coord_p2d_addword_bad");
    let schemas = base_dir.join("schemas");
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&schemas).unwrap();
    {
        let mut f = std::fs::File::create(schemas.join("mx_bad.schema.toml")).unwrap();
        // 混输但未配 primary_schema。
        write!(f, "[schema]\nid = \"mx_bad\"\n[engine]\ntype = \"mixed\"\n").unwrap();
    }
    let mut cfg = Config::default();
    cfg.schema.active = "mx_bad".into();
    cfg.schema.available = vec!["mx_bad".into()];
    let db_path = std::env::temp_dir().join("wind_coord_p2d_addword_bad.redb");
    let _ = std::fs::remove_file(&db_path);
    let bad_store = Arc::new(Store::open(&db_path).unwrap());
    let bc =
        Coordinator::new_headless_with_store(cfg, Some(base_dir.as_path()), Arc::clone(&bad_store));
    assert!(
        bc.cmd_dict_add("工", "aaaa").is_err(),
        "混输 primary 缺失应返回 Err"
    );
}
