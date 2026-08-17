//! 出厂农历短语（`zznl` / `date` / `zzrq`）的展开守门。
//!
//! 这些条目全部依赖 `$LF` 的「非节日展开成空串」语义，而 `zznl` 里**只有 `$LF`** 的
//! 那一条更进一步依赖「整条展开为空则丢弃」——两者任一回归，症状都很轻微：
//! 要么含 `$LF` 的条目在平常日子整组消失，要么 `zznl` 多出一条看不见的空候选。
//! 两种都不会报错，靠人是发现不了的。
//!
//! ⚠️ 读的是 `build_dev/data/`（部署产物）而**不是**仓库里的 `data/`。改完
//! `data/system.phrases.toml` 必须先同步过去，否则本测试拿旧文件跑、静默通过。
//! 另外 `build_dev` 在 `remote-build.ps1` 的 `$ExcludeDirs` 里**从不同步到编译机**，
//! 故本测试在远程跑等于验的是编译机上的旧数据——验出厂数据改动时须
//! `$env:WIND_NO_REMOTE = "1"` 走本机。
//!
//! ⚠️ 「耗时 0.00s = 静默跳过」那条判据在这里**不成立**：本测试只读一个 TOML、
//! 展开十几条模板，跑满也是 0.00s。要确认它真在跑，只能改一个期望值看它变红
//! （`builtin_phrase_reachability` 因为要加载整个码表词库才适用耗时判据）。

use chrono::{DateTime, Local, TimeZone};
use std::path::PathBuf;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn layer() -> Option<wind_phrase::PhraseLayer> {
    let p = data_dir().join("system.phrases.toml");
    p.is_file().then(|| wind_phrase::PhraseLayer::load(&p))
}

/// 2026-06-14：丙午年四月廿九，**非节日**。
fn ordinary_day() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 6, 14, 9, 0, 0).unwrap()
}

/// 2026-06-19：丙午年五月初五，**端午节**。
fn festival_day() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 6, 19, 9, 0, 0).unwrap()
}

fn texts(layer: &wind_phrase::PhraseLayer, code: &str, now: DateTime<Local>) -> Vec<String> {
    let host = wind_phrase::PhraseHost::empty();
    layer
        .lookup_at(code, now, &[], &host)
        .into_iter()
        .map(|h| h.text)
        .collect()
}

/// ★ `zznl` 在平常日子出 7 条（只含 `$LF` 的那条被丢弃），节日当天出 8 条。
///
/// 「节日当天多一条」是这组数据的核心行为，也是唯一能证明
/// 「整条展开为空则丢弃」真的接上了的观察点。
#[test]
fn zznl_expands_and_festival_only_entry_appears_only_on_festivals() {
    let Some(layer) = layer() else { return };

    assert_eq!(
        texts(&layer, "zznl", ordinary_day()),
        vec![
            "农历四月廿九",
            "四月廿九",
            "丙午马年四月廿九",
            "2026年四月廿九",
            "丙午年",
            "马年",
            // 只含 $LF 的那条在这里被丢弃——不是排在后面，是根本不出现
            "2026年6月14日 农历四月廿九",
        ],
        "平常日子：含 $LF 的条目照常出（只少节日名），只有 $LF 的那条整条消失"
    );

    assert_eq!(
        texts(&layer, "zznl", festival_day()),
        vec![
            "农历五月初五端午节",
            "五月初五",
            "丙午马年五月初五端午节",
            "2026年五月初五",
            "丙午年",
            "马年",
            "端午节", // ← 平常日子没有的那条
            "2026年6月19日 农历五月初五端午节",
        ],
        "端午当天：节日名追加到各条末尾，且「只有 $LF」那条自己冒出来"
    );
}

/// ★ `date` 与 `zzrq` 是同一组内容的两个入口，农历两条必须一致。
///
/// 两者的公历五条历来逐字相同；只给其中一个加农历是最容易犯的错，
/// 而用户完全可能只用其中一个入口，于是「另一个入口没有农历」很久都没人报。
#[test]
fn date_and_zzrq_carry_identical_lunar_entries() {
    let Some(layer) = layer() else { return };

    for now in [ordinary_day(), festival_day()] {
        let d = texts(&layer, "date", now);
        let z = texts(&layer, "zzrq", now);
        assert_eq!(d, z, "date 与 zzrq 的候选必须逐条相同");

        // 农历两条排在全部公历形态之后（weight 600 < 800）
        let tail = &d[d.len() - 2..];
        assert_eq!(
            tail[0],
            if now == festival_day() {
                "农历五月初五端午节"
            } else {
                "农历四月廿九"
            }
        );
        assert_eq!(
            tail[1],
            if now == festival_day() {
                "丙午马年五月初五端午节"
            } else {
                "丙午马年四月廿九"
            }
        );
    }
}
