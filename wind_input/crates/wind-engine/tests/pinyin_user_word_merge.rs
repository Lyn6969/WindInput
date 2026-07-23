//! 用户词与系统词同文时的**合并**行为（原为整条丢弃）。
//!
//! 旧行为：`convert` step 6 遇到「系统候选已有同文」时 `continue`，用户词整条被丢弃，
//! 其 `weight` 从不参与比较 —— 用户把「自激」配到 20 亿，名次纹丝不动，最终 weight
//! 仍是系统的那个小值。「加词提权」这个动作在词已存在时是无效操作。
//!
//! **必须用真实词库**：内联夹具（`CodetableDict::empty()` + `merge_single`）里系统词典
//! 是空的，「系统已有同文」这个前提根本构造不出来，测不出本文件关心的任何东西。
//! 词库缺失时自动跳过。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wind_config::Config;
use wind_engine::EngineManager;
use wind_store::Store;

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

/// 每个用例独立的 redb 与 override 目录，避免串扰与污染真实用户目录。
fn tmp_root(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("wind_pinyin_user_merge_{tag}"));
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::create_dir_all(&p);
    p
}

/// `words` = (code, text, weight)，为空则不挂任何用户词（基线）。
fn manager(dir: &Path, tag: &str, words: &[(&str, &str, i32)]) -> EngineManager {
    let root = tmp_root(tag);
    let store = Arc::new(Store::open(root.join("user_data.db")).expect("打开 store"));
    for (code, text, weight) in words {
        store
            .add_user_word("pinyin", code, text, *weight, 0)
            .expect("写入用户词");
    }
    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    EngineManager::with_store_override(
        &cfg,
        Some(dir),
        Some(store),
        Some(root.join("schema_overrides")),
    )
}

/// 返回 (名次, weight)；找不到则 None。
fn find(mgr: &EngineManager, input: &str, text: &str) -> Option<(usize, i32)> {
    mgr.convert_with("pinyin", input, 40)
        .candidates
        .iter()
        .position(|c| c.text == text)
        .and_then(|i| {
            mgr.convert_with("pinyin", input, 40)
                .candidates
                .get(i)
                .map(|c| (i, c.weight))
        })
}

const INPUT: &str = "ziji";
const WORD: &str = "自激";

/// 系统已有词 + 用户高权重 → 名次上升，最终 weight 为用户值。
#[test]
fn user_high_weight_merges_into_existing_system_word() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    const USER_W: i32 = 2_000_000_000;

    let base = manager(&dir, "high_base", &[]);
    let (rank_before, w_before) = find(&base, INPUT, WORD).expect("系统词典应有「自激」");
    assert!(
        w_before < USER_W,
        "前提不成立：系统权重 {w_before} 已不低于用户值，本用例失去意义"
    );

    let with_user = manager(&dir, "high_user", &[(INPUT, WORD, USER_W)]);
    let (rank_after, w_after) = find(&with_user, INPUT, WORD).expect("合并后仍应在候选中");

    println!("[高权重] 「{WORD}」 rank {rank_before}→{rank_after}  weight {w_before}→{w_after}");
    assert_eq!(w_after, USER_W, "同文合并应取 max(系统, 用户) = 用户值");
    assert!(
        rank_after < rank_before,
        "用户提权后名次应上升：{rank_before} → {rank_after}"
    );
}

/// 用户权重低于系统值 → 保留系统值，名次不变。
/// 用户加词的意图是提权而非降权，合并取 max 而非「用户覆盖」。
#[test]
fn user_low_weight_does_not_demote_system_word() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };

    let base = manager(&dir, "low_base", &[]);
    let (rank_before, w_before) = find(&base, INPUT, WORD).expect("系统词典应有「自激」");
    assert!(w_before > 1, "前提不成立：系统权重 {w_before} 已是最低值");

    let with_user = manager(&dir, "low_user", &[(INPUT, WORD, 1)]);
    let (rank_after, w_after) = find(&with_user, INPUT, WORD).expect("合并后仍应在候选中");

    println!("[低权重] 「{WORD}」 rank {rank_before}→{rank_after}  weight {w_before}→{w_after}");
    assert_eq!(w_after, w_before, "用户权重更低时应保留系统值");
    assert_eq!(rank_after, rank_before, "名次不应变化");
}

/// 对照组：系统没有的词 + 用户词 → 行为不变（仍作为独立候选整条加入）。
/// 这条用于捕获「合并分支写错、把新词也当同文吞掉」。
#[test]
fn user_only_word_still_appended_unchanged() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    const CODE: &str = "miaomiaoceshi";
    const TEXT: &str = "喵喵测试专用词";
    const W: i32 = 4242;

    let base = manager(&dir, "only_base", &[]);
    assert!(
        find(&base, CODE, TEXT).is_none(),
        "前提不成立：该词已在系统词典中，不是对照组"
    );

    let with_user = manager(&dir, "only_user", &[(CODE, TEXT, W)]);
    let (_, w) = find(&with_user, CODE, TEXT).expect("用户独有词应作为候选出现");
    assert_eq!(w, W, "用户独有词的 weight 应原样保留");
}
