//! 残码前缀补全的上浮约束回归测试
//!
//! 背景：残码存在时（`meiy` 的 `y`），前缀补全候选故意不标 `is_prefix`，使其上浮到
//! 精确子串单字之前——否则「没有」会被数百个单字「没/每/美/…」压到十几页之后。
//!
//! 但该特权原本无条件给全部 30 条补全。双拼每 2 键 1 音节 → 奇数键必有残码，
//! 长输入下候选 2~5 位会被冷僻长词占满，并随每次按键在两种形态间反复跳动。
//! 现按「补全距离 + 置信度」约束（见 COMPLETION_NEAR_SYLLABLES / _FAR_WEIGHT_FLOOR）。
//!
//! 下列样本全部来自实测。**距离不能单独作判据**——`zhongguorenm`→「中国人民解放军」
//! 距离 +4 却是合理项，而同为 +4 的「…物权法」是噪音，判别力全在 weight。
//!
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
    cfg.schema.available = vec!["pinyin".to_string(), "shuangpin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    EngineManager::new(&cfg, Some(dir))
}

fn rank_of(mgr: &EngineManager, schema: &str, input: &str, text: &str) -> Option<usize> {
    mgr.convert_with(schema, input, 12)
        .candidates
        .iter()
        .position(|c| c.text == text)
}

/// 残码补全必须仍能上浮：这批是该机制存在的理由，全部须留在前列。
/// 含近距离（+1/+2）与远距离但高频（+4/+5）两类。
#[test]
fn test_useful_completions_still_float() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    for (input, want, note) in [
        ("meiy", "没有", "距离+1 w=339165"),
        ("nih", "你好", "距离+1 w=5328"),
        ("nihaom", "你好吗", "距离+1 w=166，低词频但近距离须豁免"),
        ("zhongguor", "中国人", "距离+1 w=21385"),
        ("beijingd", "北京大学", "距离+2 w=2010，阈值取1会被误杀"),
        ("jisuanjik", "计算机科学", "距离+2 w=1609，阈值取1会被误杀"),
        // ⚠️ `zhonghuar`→「中华人民共和国」曾在此列，现已移出：它的**音节** extra 是 4
        // （输入 3 音节、词 7 音节），超出 `schema.pinyin.completion.max_extra_syllables`
        // 的出厂值 3，默认不再产出。这是用户拍板的取舍 —— extra=4 与真机抱怨的
        // `nih`→「你会怎么做」(extra=3) 在音节维度上同形，weight 也分不开
        // （13330 vs 3113），只能靠这个旋钮按口味取。
        // 把 max_extra 调到 4 即恢复，由 `far_completion_returns_when_max_extra_raised` 守卫。
        (
            "zhongguorenm",
            "中国人民解放军",
            "距离+4 w=252，extra=3 恰好卡在出厂值上，是本档的下边界",
        ),
        ("zhonghuarenmingongheg", "中华人民共和国", "距离+1 w=3113"),
    ] {
        let rank = rank_of(&mgr, "pinyin", input, want);
        assert!(
            rank.is_some_and(|r| r < 6),
            "「{}」({}) 应仍在 {} 的前列，实际位置 {:?}",
            want,
            note,
            input,
            rank
        );
    }
}

/// 两个旋钮确实在起作用：`max_extra_syllables` 调大即恢复远距离补全，调小即收紧。
///
/// 以 `zhonghuar`→「中华人民共和国」（输入 3 音节、词 7 音节、extra=4）为标尺：
/// 出厂值 3 时不产出，调到 4 立刻回来。缺了这条，「出厂 3」这个决定就没有守卫 ——
/// 日后有人把过滤写死、旋钮变成摆设也不会有测试变红。
#[test]
fn far_completion_returns_when_max_extra_raised() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };

    let has = |max_extra: u32| {
        let mut cfg = Config::default();
        cfg.schema.available = vec!["pinyin".to_string()];
        cfg.schema.active = "pinyin".to_string();
        cfg.schema.pinyin.completion.max_extra_syllables = max_extra;
        EngineManager::new(&cfg, Some(&dir))
            .convert_with("pinyin", "zhonghuar", 300)
            .candidates
            .iter()
            .any(|c| c.text == "中华人民共和国")
    };

    assert!(!has(3), "出厂值 3 下 extra=4 的补全不该产出");
    assert!(has(4), "调到 4 后「中华人民共和国」必须回来");
}

/// 冷僻长词补全须沉底：它们是奇偶跳动的噪音源。
/// 「沉底」指排到精确匹配之后，不是从候选中消失。
#[test]
fn test_far_lowfreq_completions_are_demoted() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    let noise = [
        "中华人民共和国企业所得税",
        "中华人民共和国治安管理处罚法",
        "中华人民共和国道路交通安全法",
        "中华人民共和国物权法",
    ];
    let cands = mgr
        .convert_with("pinyin", "zhonghuarenmingongheg", 12)
        .candidates;
    for (i, c) in cands.iter().enumerate().take(6) {
        assert!(
            !noise.contains(&c.text.as_str()),
            "冷僻条文名「{}」不该出现在前 6 位（第 {} 位）",
            c.text,
            i + 1
        );
    }
}

/// 双拼奇偶键的候选形态须稳定：奇数键（残码）与相邻偶数键（完整音节）
/// 的前若干候选不应出现整批替换。这是用户报告的原始现象。
#[test]
fn test_shuangpin_parity_does_not_thrash() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    if !mgr.ensure_schema("shuangpin") {
        eprintln!("跳过：缺 shuangpin 方案");
        return;
    }

    // vshxrfmbgshego = zhong hua ren min gong he guo
    let odd = mgr.convert_with("shuangpin", "vshxrfmbgsheg", 6).candidates;
    let noise_in_odd = odd
        .iter()
        .take(5)
        .filter(|c| c.text.starts_with("中华人民共和国") && c.text.chars().count() > 7)
        .count();
    assert_eq!(
        noise_in_odd,
        0,
        "奇数键前 5 位不应被超长条文名占据，实际: {:?}",
        odd.iter().take(5).map(|c| &c.text).collect::<Vec<_>>()
    );
}
