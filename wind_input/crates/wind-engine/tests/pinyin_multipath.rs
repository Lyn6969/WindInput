//! 多路径切分词图回归测试（Phase 3 / 方案 A）
//!
//! 背景：`Dag::maximum_match` 只给一条切分路径，于是「西安交通大学」的真值切分
//! `xi|an|jiao|tong|da|xue` 在词图里**根本不存在**——图里只有 `xian|jiao|tong|da|xue`。
//! 加上边界校验（Phase 2）后，这类词被逐出词图，实测 C 类（多音节含缩合音，4362 词）
//! top-1 从 87.20% 塌到 0.00%。
//!
//! 改造后词图直接消费 DAG 的**全部**路径，`xi|an|…` 与 `xian|…` 同时在图中，
//! 词条凭自带 `boundary` 走哪条路径由它自己决定。
//!
//! **必须用真实词库**：内联测试夹具（`CodetableDict::empty()` + `merge_single`）
//! 的 `boundary` 恒为 0，边界校验一律降级放行，等于没设防——测不出本文件关心的任何东西。
//! 词典缺失时自动跳过。

use std::path::PathBuf;
use wind_config::Config;
use wind_engine::EngineManager;

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("build_dev")
        .join("data");
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

fn manager(dir: &std::path::Path) -> EngineManager {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    EngineManager::new(&cfg, Some(dir))
}

/// 跨多音节、内部含缩合音的词必须能以**真值切分**入词图并夺得首选。
/// 这三例正是 Phase 1 里被边界校验误杀的形态（`先交通大学` / `且图表` / `连导演`）。
#[test]
fn test_contracted_syllable_words_win_top1() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    for (input, expect) in [
        ("xianjiaotongdaxue", "西安交通大学"), // xi|an|jiao|tong|da|xue (mm 给 xian|…)
        ("qietubiao", "企鹅图表"),             // qi|e|tu|biao       (mm 给 qie|…)
        ("xinanchu", "心安处"),               // xin|an|chu         (mm 给 xi|nan|chu)
        ("woqinaide", "我亲爱的"),             // wo|qin|ai|de       (mm 给 wo|qi|nai|de)
    ] {
        let got = mgr
            .convert_with("pinyin", input, 10)
            .candidates
            .first()
            .map(|c| c.text.clone())
            .unwrap_or_default();
        assert_eq!(got, expect, "{input} 首选应为 {expect}");
    }
}

/// 「N 字塌进单音节边」的畸形节点仍不得复活：`lian` 这条**单音节**边上不该出「李安」。
/// 多路径只是让 `li|an` 这条路径存在，不是放弃校验——「李安」现在只能从 li|an 走。
#[test]
fn test_single_syllable_span_still_rejects_contracted_word() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    // 单音节输入不跑 Viterbi（需 >= 2 音节），「李安」仍应作为普通候选可选中，只是不占首选。
    let r = mgr.convert_with("pinyin", "lian", 10);
    let top = r.candidates.first().map(|c| c.text.as_str()).unwrap_or("");
    assert_eq!(top, "连", "lian 首选应为高频单字");
    assert!(
        r.candidates.iter().any(|c| c.text == "李安"),
        "「李安」仍须在候选列表中（候选路径不受词图边界校验影响）"
    );
}

/// 整句候选的 `boundary` 必须是**解码器实际走的那条路径**，而非 `maximum_match`。
/// 用户造词/双拼校验都消费该字段，标错即为谎报。
#[test]
fn test_sentence_boundary_reflects_chosen_path() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    let r = mgr.convert_with("pinyin", "xianjiaotongdaxue", 10);
    let top = r.candidates.first().expect("应有候选");
    assert_eq!(top.text, "西安交通大学");
    // xi(0) an(2) jiao(4) tong(8) da(12) xue(14)
    let expect = (1u64 << 0) | (1 << 2) | (1 << 4) | (1 << 8) | (1 << 12) | (1 << 14);
    assert_eq!(
        top.boundary, expect,
        "整句边界应为 xi|an|jiao|tong|da|xue，而非 maximum_match 的 xian|jiao|tong|da|xue"
    );
    // 预编辑区跟随首选候选（用户拍板的策略）
    assert_eq!(r.preedit_display, "xi'an'jiao'tong'da'xue");
}

/// 分段上屏的消费长度在多路径下仍是**单一确定值**：所有切分路径都从 0 连续覆盖，
/// 覆盖长度恒等于「最远可达位置」，与走哪条路径无关。
#[test]
fn test_consumed_length_is_path_independent() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    for (input, expect) in [("xianjiaotongdaxue", 17usize), ("qietubiao", 9), ("nihao", 5)] {
        let r = mgr.convert_with("pinyin", input, 10);
        let c = r.candidates.first().expect("应有候选");
        assert_eq!(c.consumed_length, expect, "{input} 的整句应消费全部输入");
    }
}
