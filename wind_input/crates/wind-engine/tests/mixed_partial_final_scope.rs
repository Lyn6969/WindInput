//! 混输「残码整句」的**作用域**：超码长开、码长内关（`ConvertOptions::allow_partial_final`）。
//!
//! 用户现象：`zaiyebuj` 在混输下尾字母 `j` 不参与组句，打不出「在也不就」，而纯拼音方案
//! 一直打得出。根因是 `manager.rs` 那行 `enable_partial_final: mix_pinyin.is_none()` ——
//! 混输把整句里的残码补全**整体**关掉了。
//!
//! 关掉它的理由（真机 `aaw`，本意五笔 `aawt`→「工作」，首选被拼音整句「啊啊我」抢走）
//! **只在码长内成立**：定长码表（五笔 4 码）之外的串不可能是码表码，那里已是纯拼音语境。
//! ⇒ 判据从「是不是混输」改为「这串还可能是码表码吗」。
//!
//! 本文件的三条用例正好是这条判据的三个面：超码长该出、码长内不该出、纯拼音两侧都该出。
//! ⚠️ 依赖 `build_dev/data`（整句解码要真实词库与词频），缺失时静默跳过——判据是耗时与
//! eprintln。

use std::path::PathBuf;
use wind_config::Config;
use wind_engine::EngineManager;

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../build_dev/data")
        .canonicalize()
        .ok()?;
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

/// 返回 (候选文本, 该候选是否消费整串)。
fn candidates(active: &str, input: &str) -> Option<Vec<(String, bool)>> {
    let dir = data_dir()?;
    let mut cfg = Config::default();
    cfg.schema.available = vec![
        "wubi86".to_string(),
        "pinyin".to_string(),
        "wubi86_pinyin".to_string(),
    ];
    cfg.schema.active = active.to_string();
    let root = std::env::temp_dir().join(format!("wind_mix_pf_scope_{active}_{input}"));
    let _ = std::fs::remove_dir_all(&root);
    let mgr =
        EngineManager::with_store_override(&cfg, Some(&dir), None, Some(root.join("overrides")));
    let len = input.len();
    Some(
        mgr.convert(input, 300)
            .candidates
            .into_iter()
            .map(|c| {
                let full = c.consumed_length == 0 || c.consumed_length >= len;
                (c.text, full)
            })
            .collect(),
    )
}

/// 超码长（8 键 > 五笔 4 码）：残码 `j` 参与组句，「在也不就」出得来且**消费整串**。
///
/// 「消费整串」是本用例的要害——改动前 `zaiyebuj` 也有一堆候选（「在也不」等），但没有一条
/// 解释得完整个输入，用户按空格上屏后 `j` 会留在缓冲里。
#[test]
fn overflow_lets_trailing_partial_join_the_sentence() {
    let Some(cands) = candidates("wubi86_pinyin", "zaiyebuj") else {
        eprintln!("跳过 overflow_lets_trailing_partial_join_the_sentence：build_dev/data 不存在");
        return;
    };
    let hit = cands.iter().find(|(t, _)| t == "在也不就");
    let texts: Vec<&String> = cands.iter().map(|(t, _)| t).take(8).collect();
    let (_, full) =
        hit.unwrap_or_else(|| panic!("超码长应出残码整句「在也不就」，实际: {texts:?}"));
    assert!(*full, "「在也不就」必须消费整串，否则尾部 j 仍会留在缓冲里");
}

/// 码长内（3 键 ≤ 五笔 4 码）：残码整句**保持关闭**，`aaw` 不出「啊啊我」。
///
/// ★ 这是本轮改动的反向对照，守的是被修复过的那次真机事故：`aaw` 本意是五笔 `aawt`→「工作」，
/// 整句「啊啊我」一旦生成就会消费满 3/3 键，从而合法地跨过 `is_pinyin_exact_tier` 的
/// 「消费整串」闸门、抢走首位。放开作用域时若把码长内一起放开，这条立刻红。
#[test]
fn in_code_len_keeps_partial_final_off() {
    let Some(cands) = candidates("wubi86_pinyin", "aaw") else {
        eprintln!("跳过 in_code_len_keeps_partial_final_off：build_dev/data 不存在");
        return;
    };
    let texts: Vec<&String> = cands.iter().map(|(t, _)| t).collect();
    assert!(
        !texts.iter().any(|t| *t == "啊啊我"),
        "码长内不该出残码整句（五笔码会被当拼音读），实际: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| *t == "工作"),
        "前置：码表候选应在场，否则上一条断言可能只是词库没加载: {texts:?}"
    );
}

/// 纯拼音方案两侧都不受影响——关的是「混输的码长内」这一格，不是整个功能。
///
/// 没有这条，把 `enable_partial_final` 整个删掉也能让上面两条绿。
#[test]
fn pure_pinyin_keeps_partial_final_everywhere() {
    let Some(short) = candidates("pinyin", "aaw") else {
        eprintln!("跳过 pure_pinyin_keeps_partial_final_everywhere：build_dev/data 不存在");
        return;
    };
    assert!(
        short.iter().any(|(t, _)| t == "啊啊我"),
        "纯拼音短串仍应出残码整句: {:?}",
        short.iter().map(|(t, _)| t).take(8).collect::<Vec<_>>()
    );
    let long = candidates("pinyin", "zaiyebuj").expect("词库已确认存在");
    assert!(
        long.iter().any(|(t, full)| t == "在也不就" && *full),
        "纯拼音长串的残码整句也应在且消费整串"
    );
}
