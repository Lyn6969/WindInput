//! 用户词库长词的**可见性**：上浮判据必须跟着 `max_extra_syllables` 走。
//!
//! ## 真机报障
//!
//! 用户库里有 11 音节的「清风输入法内测问题反馈」，`max_extra_syllables` 设为 10。
//! 打 `qingfengshurufa`（started 5、距词尾 6）**翻遍全部 16 页候选都找不到它**，
//! 必须打到 `qingfengshurufaneicewent`（started 9、距词尾 2）才出现。
//!
//! 分界点正是 `should_promote_user_completion` 里当时硬编码的 `2`
//! （旧常量 `COMPLETION_NEAR_SYLLABLES`）——它不读用户配置，于是把 `max_extra`
//! 调到 10 对用户词长词毫无作用。
//!
//! ## ⚠️ 为什么必须用 `limit` 参数化，而不是直接断言位次
//!
//! 不上浮的后果**不止「排在后面」**：候选落进前缀补全层、被首音节同音子短语整层
//! 压到最底，而引擎侧 `sort_by` 紧跟着 `truncate` —— **排到最底在候选数超过上限时
//! 等于被丢弃**，协调器再也收不到，`cmp_by_consumed` 那道补救无从谈起。
//!
//! 这意味着「降级」与「销毁」的界线不由排序决定，而由**候选总数有没有超过上限**决定，
//! 是个藏在数据规模里的开关。定位这个 bug 时，四轮探针（协调器级、真实用户数据重建、
//! 有无词频、有无边界）**全部报「正常」**——因为本仓测试词库在该输入下恰好只产出 142
//! 条，`limit=300` 时一条不丢，正好落在开关的安全侧；用户的大词库产出远超 300 就必然丢。
//!
//! ⇒ 本文件**刻意用很小的 `limit`** 把那个开关拨到危险侧。实测修复前：`limit=141`
//! 该词消失、`limit=142` 才刚好保住（它就在最后一位）。若改回按默认 limit 断言位次，
//! 这条用例会在本仓词库下永远绿，而线上照样丢词。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wind_config::Config;
use wind_engine::EngineManager;
use wind_store::Store;

/// 报障用户的真实词条（`dict export` 导出所得，11 音节、weight 1000）。
const WORD: &str = "清风输入法内测问题反馈";
const SYLS: &[&str] = &[
    "qing", "feng", "shu", "ru", "fa", "nei", "ce", "wen", "ti", "fan", "kui",
];

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data");
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

/// 扁平 code + 音节边界位图（各音节起始字节位）。
fn code_and_boundary() -> (String, u64) {
    let mut code = String::new();
    let mut b: u64 = 0;
    for s in SYLS {
        b |= 1u64 << code.len();
        code.push_str(s);
    }
    (code, b)
}

fn manager(dir: &Path, tag: &str, max_extra: u32) -> EngineManager {
    let root = std::env::temp_dir().join(format!("wind_uw_visible_{tag}"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    let store = Arc::new(Store::open(root.join("user_data.db")).expect("打开 store"));
    let (code, boundary) = code_and_boundary();
    store
        .add_user_word("pinyin", &code, WORD, 1000, boundary)
        .expect("写入用户词");

    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    cfg.schema.pinyin.completion.min_syllables = 4;
    cfg.schema.pinyin.completion.max_extra_syllables = max_extra;
    EngineManager::with_store_override(&cfg, Some(dir), Some(store), Some(root.join("ov")))
}

/// `max_extra` 足够大时，长词在**任何候选上限下**都必须活下来。
///
/// `qingfengshurufa` = qing feng shu ru fa ⇒ started 5，词 11 音节 ⇒ 距词尾 6 ≤ 10。
#[test]
fn long_user_word_survives_truncation_when_max_extra_allows() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir, "survive", 10);

    // 30 远小于该输入的候选总数（本仓词库约 142 条），modeling 用户的大词库场景。
    for limit in [30, 50, 100, 300] {
        let r = mgr.convert("qingfengshurufa", limit);
        let pos = r.candidates.iter().position(|c| c.text == WORD);
        assert!(
            pos.is_some(),
            "limit={limit}：距词尾 6 ≤ max_extra 10，长词须上浮进完整匹配层而不被截断丢弃\
             （修复前 limit ≤ 141 一律消失）；实际候选 {} 条",
            r.candidates.len()
        );
    }
}

/// 反向对照：`max_extra` 收紧到装不下时，长词**允许**沉下去。
///
/// 缺了这条，「让判据恒真」这种假修复也能让上面那条通过。取 2 ＝ 旧硬编码值，
/// 距词尾 6 > 2 ⇒ 不上浮 ⇒ 在小 limit 下被截断，正是报障现场的行为。
#[test]
fn long_user_word_sinks_when_max_extra_too_small() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir, "sink", 2);

    let r = mgr.convert("qingfengshurufa", 30);
    assert!(
        !r.candidates.iter().any(|c| c.text == WORD),
        "max_extra=2 时距词尾 6 超限，长词不该占用前 30 名（否则上面那条对照失效）"
    );
}

/// 打到距词尾 ≤ 2 时，即便 `max_extra` 很小也必须出现 —— 守住旧行为不被改坏。
#[test]
fn long_user_word_appears_near_word_end_regardless_of_max_extra() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir, "near_end", 2);

    // qing feng shu ru fa nei ce wen ti ⇒ started 9，距词尾 2。
    let r = mgr.convert("qingfengshurufaneicewenti", 30);
    assert!(
        r.candidates.iter().any(|c| c.text == WORD),
        "距词尾 2 是历史上唯一放行的档位，任何改动都不得破坏它"
    );
}
