//! 词频等效权重的**量纲基准**回归（`docs/design/freq-weight-model.md` §5.3 / §7）。
//!
//! `DEFAULT_FREQ_SAT_COUNT = 11765` 是拿「全库最高权重占总权重 6.32%」换算出来的。若
//! `max_dict_weight()` 取到的值与标定时不符，整个词频模型会静默偏移一个数量级——而这类
//! 偏移的表象只是「候选顺序不太对」，极难归因。本文件把那个数钉死。
//!
//! **换词库或改 `import_tables` 后本测试会红**，这是有意的：它提示重新标定，而不是让人
//! 直接改掉期望值。重新标定的方法见设计文档附录。
//!
//! 词典缺失时自动跳过。
//!
//! ⚠️ **本文件不适用「耗时判据」**。仓库惯例是「依赖真实词库的测试若秒过即为静默跳过」，
//! 但这里全部通过也只需 ~0.05s——`max_weight()` 读的是 wdat v6 MaxW 段的根节点，O(1)，
//! mmap 不把词库读进内存。要确认没走跳过分支，请看 `--nocapture` 下有无「跳过」输出，
//! 不要按耗时下结论。

use std::path::PathBuf;
use wind_config::Config;
use wind_engine::EngineManager;

/// 标定时的实测值：`8105.dict.yaml` 里的「的」。
const EXPECTED_PINYIN_MAX: i32 = 15_378_475;

/// 混输对拼音候选整体 `/= PINYIN_TIER_SCALE`。
const PINYIN_TIER_SCALE: i32 = 100;

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
        .or_else(|| {
            // build_dev/data 是 d1 的复制产出、可能不在场（并发重建时尤其）；退到 build/data
            let q = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("..")
                .join("build")
                .join("data");
            q.join("schemas/pinyin/cn_dicts/base.dict.yaml")
                .exists()
                .then_some(q)
        })
}

fn manager(dir: &std::path::Path, schema: &str) -> EngineManager {
    let mut cfg = Config::default();
    cfg.schema.available = vec![schema.to_string()];
    cfg.schema.active = schema.to_string();
    EngineManager::new(&cfg, Some(dir))
}

/// 纯拼音的量纲基准必须等于词库最高权重。
#[test]
fn pinyin_max_dict_weight_matches_calibration() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev/data 与 build/data 均不存在");
        return;
    };
    let mgr = manager(&dir, "pinyin");
    // 触发懒加载：未加载的引擎按设计返回 None（不触发读盘）
    let _ = mgr.convert_with("pinyin", "de", 5);

    let got = mgr.loaded_max_dict_weight("pinyin");
    assert_eq!(
        got,
        Some(EXPECTED_PINYIN_MAX),
        "量纲基准与标定值不符。若确实换过词库，需按设计文档附录重新标定 \
         DEFAULT_FREQ_SAT_COUNT，而不是直接改本期望值"
    );
}

/// 基准必须真的来自词库，不是常数回显——换个方案应得到不同的值。
///
/// **反向对照**：缺了它，`loaded_max_dict_weight` 即使写死返回 15378475 也能让上面那条通过。
#[test]
fn max_dict_weight_is_derived_not_hardcoded() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：数据目录不存在");
        return;
    };
    let mgr = manager(&dir, "pinyin");
    let _ = mgr.convert_with("pinyin", "de", 5);
    let pinyin = mgr.loaded_max_dict_weight("pinyin");

    // 未加载的方案必须返回 None（而非兜底常数）
    assert_eq!(
        mgr.loaded_max_dict_weight("nonexistent_schema_xyz"),
        None,
        "未加载的方案不得回退到任何常数——那会让混输/码表拿到错误量纲"
    );
    assert!(pinyin.is_some_and(|w| w > 0), "已加载的拼音方案必须有基准");
}

/// 混输的基准必须是**降档后**的量纲。
///
/// 这是整个模型最容易错的一处：按纯拼音标定的词频分在混输下会碾压全部拼音候选
/// （降档后 p99 只有 69）。
#[test]
fn mixed_max_dict_weight_is_scaled_down() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：数据目录不存在");
        return;
    };
    // 混输方案 id 依赖 build 数据里的方案定义，缺失即跳过（不是失败）
    for schema in ["mixed", "wubi_pinyin", "wubi86_pinyin"] {
        let mgr = manager(&dir, schema);
        let _ = mgr.convert_with(schema, "de", 5);
        let Some(w) = mgr.loaded_max_dict_weight(schema) else {
            continue;
        };
        assert!(
            w < EXPECTED_PINYIN_MAX,
            "混输方案 {schema} 的基准 {w} 必须低于纯拼音量纲——它应已除以 PINYIN_TIER_SCALE"
        );
        assert_eq!(
            w,
            EXPECTED_PINYIN_MAX / PINYIN_TIER_SCALE,
            "混输方案 {schema} 的基准应恰为拼音基准降档值"
        );
        return; // 找到一个可用的混输方案即可
    }
    eprintln!("跳过：未找到可用的混输方案");
}

/// `dict_weight` 必须在整句同文合并时留存原值——contested 归一（步骤 2）要靠它。
///
/// 现场 `siyuan`：「寺院」词库里 491，经同文合并被抬成 `SENTENCE_WEIGHT_BASE + log_offset`
/// （真机调试信息显示 `权 29984561`）。若不留存，归一时无从取回可比的量级，
/// 3e7 会碾压同码的「思源」，词频维度对该编码整体失效。
#[test]
fn sentence_merge_preserves_dict_weight() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：数据目录不存在");
        return;
    };
    let mgr = manager(&dir, "pinyin");
    let r = mgr.convert_with("pinyin", "siyuan", 30);

    let Some(temple) = r.candidates.iter().find(|c| c.text == "寺院") else {
        eprintln!("跳过：候选中无「寺院」（词库版本差异）");
        return;
    };

    assert!(
        temple.is_sentence,
        "「寺院」应经同文合并继承整句身份，实际 is_sentence=false"
    );
    assert!(
        temple.weight > 20_000_000,
        "整句加成后应是 3e7 量级，实际 {}",
        temple.weight
    );
    let dw = temple
        .dict_weight
        .expect("被整句加成覆盖过的候选必须留存 dict_weight");
    assert!(
        dw > 0 && dw < 10_000,
        "留存的应是词库原值（标定时实测 491），实际 {dw}"
    );
    assert!(
        dw < temple.weight,
        "留存值必须是加成前的、严格小于当前 weight"
    );
}

/// **反向对照**：未被整句加成覆盖的候选，`dict_weight` 必须是 `None`。
///
/// 缺了它，「无条件给每个候选都填上 weight」也能让上面那条通过，而那会让归一逻辑
/// 在普通候选上误用一个本不该存在的值。
#[test]
fn ordinary_candidate_has_no_dict_weight() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：数据目录不存在");
        return;
    };
    let mgr = manager(&dir, "pinyin");
    let r = mgr.convert_with("pinyin", "siyuan", 30);

    let Some(product) = r.candidates.iter().find(|c| c.text == "思源") else {
        eprintln!("跳过：候选中无「思源」");
        return;
    };
    assert!(!product.is_sentence, "「思源」不应是整句解");
    assert_eq!(
        product.dict_weight, None,
        "未被整句加成覆盖的候选不得有 dict_weight，实际 {:?}",
        product.dict_weight
    );
}
