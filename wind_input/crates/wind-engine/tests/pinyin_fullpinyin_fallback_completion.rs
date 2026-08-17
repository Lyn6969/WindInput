//! 全拼降级支路（step 6.7）必须与主路径**同口径**地受词组补全配置约束。
//!
//! ## 背景
//!
//! 双拼方案开启「允许全拼输入」后，引擎会把击键串再当全拼读一遍（`recall_full_pinyin`）。
//! 该支路自带召回与排序，是主路径之外的**第二条产出通道** —— 于是主路径上每加一道
//! 与补全有关的判据，这里都得同步，否则同一个开关在两条流下表现不一致。
//!
//! 本文件锁住两项曾经漏掉的：
//!
//! | | 主路径 | 降级支路（修复前） |
//! |---|---|---|
//! | 召回门槛 | `search_prefix_with_boundary_syllable_capped(.., cap)` | `search_prefix_with_boundary(..)` **无 cap** |
//! | 音节数档位 | step 4 后回填 `completion_extra_syllables` | `..Default::default()` ⇒ **恒 0** |
//!
//! 实测后果：出厂 `min_syllables = 4` 下打 `beijingd`（started 3，上限应收紧到 3），
//! 主路径一条超音节候选都不给，降级支路却照样召回「北京大学」「北京地区」乃至
//! **7 音节的「北京大学出版社」**，且它们的 `extra` 全是 0 —— 与 3 音节的「北京的」
//! 同档竞争，协调器的 `cmp_completion_extra` 形同虚设。
//!
//! ## ⚠️ 音节数只能按全拼域算
//!
//! 本支路里全拼域与击键域**是同一个域**（支路的定义就是把击键当全拼读），故 `started`
//! 可以直接从 `syllables.len()` 与字节长度推出。主路径两域不同，`started_syllables` 必须
//! 走双拼域 —— 两处的算法看着像，混用会静默错配。

use std::path::PathBuf;
use wind_config::Config;
use wind_engine::EngineManager;

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data");
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

/// 双拼方案 + 允许全拼输入。
fn manager(dir: &std::path::Path, min_syl: u32, max_extra: u32) -> EngineManager {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["shuangpin".to_string()];
    cfg.schema.active = "shuangpin".to_string();
    cfg.schema.pinyin.shuangpin.allow_full_pinyin = true;
    cfg.schema.pinyin.completion.min_syllables = min_syl;
    cfg.schema.pinyin.completion.max_extra_syllables = max_extra;
    EngineManager::new(&cfg, Some(dir))
}

/// `beijingd` = bei jing + 残码 d ⇒ started 3。出厂 `min_syllables = 4` 未达门槛，
/// 上限收紧到 started 本身 ⇒ 降级支路也不得给出 4 音节及以上的补全。
#[test]
fn fallback_respects_min_syllables_gate() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir, 4, 5);
    let r = mgr.convert_with("shuangpin", "beijingd", 60);
    let texts: Vec<&str> = r.candidates.iter().map(|c| c.text.as_str()).collect();

    for over in ["北京大学", "北京地区", "北京大学出版社"] {
        assert!(
            !texts.contains(&over),
            "started=3 < min=4，降级支路不得召回超音节的「{over}」；实际前 12: {:?}",
            &texts[..texts.len().min(12)]
        );
    }
    // 反向：音节数对齐的必须还在，否则「没有超音节候选」可能只是整条支路空了。
    assert!(
        texts.contains(&"北京的") || texts.contains(&"背景的"),
        "3 音节候选应正常召回；实际前 12: {:?}",
        &texts[..texts.len().min(12)]
    );
}

/// 门槛放宽后超音节补全回来，且**带上正确的音节数档位**。
///
/// 档位错了不会让候选消失，只会让它与对齐候选同档 —— 是静默的排序退化，故必须直接
/// 断言字段值，不能只看候选在不在。
#[test]
fn fallback_tags_completion_extra_syllables() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir, 2, 5);
    let r = mgr.convert_with("shuangpin", "beijingd", 60);

    // started = 3（bei jing + 残码 d）
    for (text, want_extra) in [("北京的", 0u8), ("北京大学", 1), ("北京大学出版社", 4)]
    {
        let Some(c) = r.candidates.iter().find(|c| c.text == text) else {
            panic!("「{text}」应在候选中（min=2 已放开门槛）");
        };
        assert_eq!(
            c.completion_extra_syllables,
            want_extra,
            "「{text}」{} 音节、started=3 ⇒ extra 应为 {want_extra}（修复前恒 0）",
            c.boundary.count_ones()
        );
    }
}

/// 无残码的整音节输入同样成立：`nihao` ⇒ started 2。
#[test]
fn fallback_extra_without_trailing_partial() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir, 2, 5);
    let r = mgr.convert_with("shuangpin", "nihao", 60);

    for c in r
        .candidates
        .iter()
        .filter(|c| c.is_prefix && c.boundary != 0)
    {
        let want = c.boundary.count_ones().saturating_sub(2) as u8;
        assert_eq!(
            c.completion_extra_syllables,
            want,
            "「{}」{} 音节、started=2 ⇒ extra 应为 {want}",
            c.text,
            c.boundary.count_ones()
        );
    }
}
