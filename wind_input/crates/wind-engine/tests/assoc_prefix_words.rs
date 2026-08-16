//! 词语联想的取数口 `assoc_prefix_words` 在**真实词库**上的表现。
//!
//! # 为什么必须有这一层
//!
//! `ReverseIndex::texts_with_prefix` 的单元测试用的是手写的 8 条假数据——它能证明
//! 「按权重排、排除前缀自身、不越界」这些**算法性质**，但证明不了在十万词级的真实
//! 词库上打完「中」会得到什么。而后者才是用户看到的东西：真实词库里「中」开头的词
//! 有上千条，权重分布、有没有奇怪的长词占前排，只有真数据能回答。
//!
//! ```text
//! cargo test -p wind-engine --test assoc_prefix_words -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use wind_config::Config;
use wind_engine::EngineManager;

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data");
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

fn mgr(dir: &std::path::Path, schema: &str) -> EngineManager {
    let mut cfg = Config::default();
    cfg.schema.available = vec![schema.to_string()];
    cfg.schema.active = schema.to_string();
    EngineManager::new(&cfg, Some(dir))
}

/// 打完某个字之后，词语联想给出的前几条。
#[test]
#[ignore = "探针：依赖 build_dev 真实词库"]
fn prefix_words_on_real_dict() {
    let Some(dir) = data_dir() else {
        eprintln!("!!! 跳过：build_dev 词库不存在");
        return;
    };
    let m = mgr(&dir, "pinyin");
    println!("\n=== 词语联想：真实词库上的取数 ===");
    for probe in ["中", "北京", "输入", "我", "工作", "谢"] {
        let words = m.assoc_prefix_words("pinyin", probe, 9);
        let shown: Vec<String> = words.iter().map(|(w, wt)| format!("{w}({wt})")).collect();
        println!("{probe:<6} → {}", shown.join(" "));
    }
}

/// ★ **混输方案取不取得到词**——真机上最常见的活跃方案恰恰是混输。
///
/// 混输方案自身可能没有词库（它引用两个成员方案），那样反查索引会是空的，
/// 词语联想就一条也出不来。这条探针是来回答这个的，不是装饰。
#[test]
#[ignore = "探针：依赖 build_dev 真实词库"]
fn which_schema_actually_has_words() {
    let Some(dir) = data_dir() else {
        eprintln!("!!! 跳过：build_dev 词库不存在");
        return;
    };
    println!("\n=== 各方案的词语联想取数能力 ===");
    println!(
        "{:<16} {:>16} {:>4}  {}",
        "活跃方案", "解析出的词源", "条数", "样例"
    );
    for schema in ["wubi86_pinyin", "wubi86", "pinyin", "shuangpin"] {
        let m = mgr(&dir, schema);
        // 走真实解析口而非直接传 schema——被测的正是「混输能不能解析到有词库的成员」。
        let source = m.assoc_word_schema();
        let words = m.assoc_prefix_words(&source, "中", 5);
        let shown: Vec<String> = words.iter().map(|(w, wt)| format!("{w}({wt})")).collect();
        println!(
            "{schema:<16} {source:>16} {:>4}  {}",
            words.len(),
            if shown.is_empty() {
                "—— 取不到词！".to_string()
            } else {
                shown.join(" ")
            }
        );
        assert!(
            !words.is_empty(),
            "{schema} 解析出的词源 {source:?} 一条词都取不到——词语联想在该方案上是死的"
        );
    }
}

/// 后置条件：返回的词**必然**以 prefix 开头且严格更长。
///
/// 上屏时补的是 `word[prefix.len()..]`，这条不成立就会切出乱码
/// （或在多字节边界上 panic）。
#[test]
#[ignore = "探针：依赖 build_dev 真实词库"]
fn every_hit_is_a_strict_extension() {
    let Some(dir) = data_dir() else {
        eprintln!("!!! 跳过：build_dev 词库不存在");
        return;
    };
    let m = mgr(&dir, "pinyin");
    let mut checked = 0usize;
    for probe in ["中", "北京", "输入", "我", "工作", "谢", "一", "不"] {
        for (word, _) in m.assoc_prefix_words("pinyin", probe, 20) {
            assert!(
                word.starts_with(probe),
                "{word:?} 不以 {probe:?} 开头——上屏切片会切出乱码"
            );
            assert!(
                word.len() > probe.len(),
                "{word:?} 不比 {probe:?} 长——选中它等于上屏空串"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "一条都没查到：词库没加载，本用例等于空跑");
    println!("校验了 {checked} 条联想词");
}

/// 真机复现：wubi86 词库对**多字词**前缀还有没有更长的词。
#[test]
#[ignore = "探针：依赖 build_dev 真实词库"]
fn wubi_multichar_prefixes() {
    let Some(dir) = data_dir() else { return };
    let m = mgr(&dir, "wubi86");
    println!(
        "
=== wubi86 多字前缀 ==="
    );
    for probe in [
        "中", "中国", "我们", "输入", "工作", "谢谢", "问题", "时间", "可以", "的",
    ] {
        let w = m.assoc_prefix_words("wubi86", probe, 6);
        let shown: Vec<String> = w.iter().map(|(t, _)| t.clone()).collect();
        println!("{probe:<6} {:>3} 条  {}", w.len(), shown.join(" "));
    }
}

/// ★ 量化：**多字词上屏之后还有多少能出联想**。
///
/// 词语联想是纯前缀延伸，「我们」「谢谢」这类完整双字词在词库里往往没有更长的扩展。
/// 用户实际打字多半是打词而非打单字——若命中率过低，这个功能在真机上就近乎不可见，
/// 那不是 bug，是它的固有性质，必须量出来而不是猜。
#[test]
#[ignore = "探针：依赖 build_dev 真实词库"]
fn how_often_do_words_have_extensions() {
    let Some(dir) = data_dir() else { return };
    let m = mgr(&dir, "wubi86");
    let singles = [
        "我", "你", "他", "中", "工", "时", "上", "下", "大", "小", "人", "天", "地", "手", "口",
        "心", "水", "火", "日", "月",
    ];
    let doubles = [
        "我们", "你们", "他们", "中国", "工作", "时间", "问题", "谢谢", "可以", "什么", "这个",
        "没有", "知道", "现在", "因为", "所以", "输入", "电脑", "手机", "公司",
    ];
    for (name, list) in [("单字", &singles[..]), ("双字词", &doubles[..])] {
        let mut hit = 0;
        let mut detail = Vec::new();
        for w in list {
            let n = m.assoc_prefix_words("wubi86", w, 9).len();
            if n > 0 {
                hit += 1;
            }
            detail.push(format!("{w}:{n}"));
        }
        println!(
            "{name:<8} 有联想 {hit}/{}   {}",
            list.len(),
            detail.join(" ")
        );
    }
}
