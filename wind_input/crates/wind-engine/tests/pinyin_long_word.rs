//! 长词整句回归测试
//!
//! 背景：`zhonghuarenmingongheguo` 首选曾是「中华人民共和过」。根因是 lattice
//! `max_word_len` 为 6，把 7 音节的「中华人民共和国」挡在词图外，却放行了它的
//! 语义碎片「中华人民共和」(freq=2)，于是 Viterbi 只能在
//! 「中华人民共和」(-18.615) + 「过」(-5.708) 这类错误切分里挑最优；而词典里
//! 精确命中的「中华人民共和国」只带原始词频 3113，在 weight 维度必然输给
//! 30M 基座的整句。
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
    cfg.schema.available = vec!["pinyin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    EngineManager::new(&cfg, Some(dir))
}

fn first(mgr: &EngineManager, input: &str) -> String {
    mgr.convert_with("pinyin", input, 10)
        .candidates
        .first()
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

/// 7 音节长专名须整词命中，不得被「碎片 + 虚词」切分挤掉首选。
#[test]
fn test_long_proper_noun_wins_over_fragment_split() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    for (input, expect) in [
        ("zhonghuarenmingongheguo", "中华人民共和国"),
        ("renmingongheguo", "人民共和国"),
        ("gongheguo", "共和国"),
        ("zhonghuarenmin", "中华人民"),
    ] {
        assert_eq!(first(&mgr, input), expect, "输入 {} 的首选不对", input);
    }
}

/// 词图上限内的普通精确整词不得被授予 `is_sentence` 身份，否则会被 freq_rerank
/// 锚定在顶部而永久失去词频学习能力。
/// 「共和」自身是 Viterbi 整句故允许带该标记；同码的「恭贺」「共贺」不该被带进去。
#[test]
fn test_short_exact_words_stay_out_of_sentence_anchor() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    let cands = mgr.convert_with("pinyin", "gonghe", 10).candidates;
    assert_eq!(cands.first().map(|c| c.text.as_str()), Some("共和"));

    for c in cands.iter().filter(|c| c.text != "共和") {
        assert!(
            !c.is_sentence,
            "候选「{}」被误标为整句解，将被锚定而失去词频学习",
            c.text
        );
    }
}

/// 超过词图上限（10 音节）的超长词：Viterbi 无法整词命中，须由 step 1.5 的
/// 等价分兜底，使词典精确整词排在拼接整句之前。
///
/// 下列用例经实测确认**依赖** step 1.5——禁用该段后首选会退化成括号中的错误切分，
/// 挑用例时务必用这一类（「中华人民共和国道路交通安全法」等则不依赖它，
/// Viterbi 拼出的整句恰好等于词本身，拿来断言会假绿）。
#[test]
fn test_over_limit_long_word_falls_back_to_dict_exact() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    for (input, expect) in [
        // 罐装动脉…
        (
            "guanzhuangdongmaizhouyangyinghuaxingxinzangbing",
            "冠状动脉粥样硬化性心脏病",
        ),
        // 大不列颠几倍爱尔兰…
        (
            "dabuliedianjibeiaierlanlianhewangguo",
            "大不列颠及北爱尔兰联合王国",
        ),
        // 里昂好的开端…
        (
            "lianghaodekaiduanshichenggongdeyiban",
            "良好的开端是成功的一半",
        ),
        // 塔什库尔干他即可自治县
        ("tashikuergantajikezizhixian", "塔什库尔干塔吉克自治县"),
        // …责任强制保险调理
        (
            "jidongchejiaotongshiguzerenqiangzhibaoxiantiaoli",
            "机动车交通事故责任强制保险条例",
        ),
    ] {
        assert_eq!(first(&mgr, input), expect, "超长词兜底失效: {}", input);
    }
}
