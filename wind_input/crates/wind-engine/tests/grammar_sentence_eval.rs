//! 语法模型（bigram）的**整句**评测。
//!
//! ## 为什么不能用 `pinyin_eval` 验收 bigram
//!
//! `pinyin_eval` 的 A/B/C 三类都是**单个词**的测试（输入一个词的拼音、期望该词），
//! 而 bigram 打的是**词与词之间的转移分**——单词场景下根本不存在跨词转移。
//! 实测 `weight=1.0` 时 A/B/C 的 top-1 变化为 +0.10 / 0 / 0，纯属噪音级别。
//! 唯一涉及多词的 D 类又是「随机拼两个词」造出来的，本身不是自然语言。
//!
//! ⇒ 要看 bigram 有没有用，必须拿**真实的多词句子**测。本文件就是那个集合。
//!
//! ## 用法
//!
//! ```text
//! cargo test -p wind-engine --test grammar_sentence_eval -- --ignored --nocapture
//! ```
//!
//! 默认对比 `weight=0`（基线）与 `weight=1.0`，可用 `WIND_GRAM_WEIGHT` 改后者。
//! 需要 `build_dev/data` 与 `build_dev/data/schemas/pinyin/grammar/*.gram`。

use std::path::{Path, PathBuf};

use wind_config::Config;
use wind_engine::EngineManager;

const TOP_N: usize = 10;

/// 真实多词句子。**期望值是人工判定的「自然中文」**，不是从词库合成的。
///
/// 选材原则：每条都要么在设计文档 §1 被点名，要么是日常高频表达——
/// 也就是「用户真会这么打、且上下文能帮上忙」的场景。
const CASES: &[(&str, &str)] = &[
    // —— 设计文档 §1 表格点名的定点 ——
    ("sixiang", "思想"),
    ("nihao", "你好"),
    // —— 双词搭配：上下文应能定夺 ——
    ("qihoutezheng", "气候特征"),
    ("zhengquangongsi", "证券公司"),
    ("xinlishang", "心理上"),
    ("dulizizhu", "独立自主"),
    ("jianyixiugai", "建议修改"),
    // —— 短句 ——
    ("woshizhongguoren", "我是中国人"),
    ("jintiantianqihenhao", "今天天气很好"),
    ("womenyiqiquchifan", "我们一起去吃饭"),
    ("xiexienidebangzhu", "谢谢你的帮助"),
    ("zhegewentihenzhongyao", "这个问题很重要"),
    ("qingwenxianzaijidian", "请问现在几点"),
    ("womenxuyaogengduoshijian", "我们需要更多时间"),
    ("tazhengzaikanshu", "他正在看书"),
    ("zhonghuarenmingongheguo", "中华人民共和国"),
    ("jisuanjikexue", "计算机科学"),
    ("rengongzhineng", "人工智能"),
    ("gongzuobaogao", "工作报告"),
    ("chifanlema", "吃饭了吗"),
    ("zhendehenbucuo", "真的很不错"),
    // —— 同音消歧：**正是 bigram 该拿分的地方**，靠上下文才分得出 ——
    ("mingtianzaijian", "明天再见"),            // 再见 / 在建
    ("shenghuozhongdexiaoshi", "生活中的小事"), // 小事 / 小时
    ("woyaoqushangban", "我要去上班"),
    ("tadeyisijiushi", "他的意思就是"),
    ("zheshiyigehaobanfa", "这是一个好办法"),
    // —— 更多日常整句：扩大样本以便看出基线本来就答错的那些 ——
    ("wobuzhidaozenmeban", "我不知道怎么办"),
    ("nizuotianqunalile", "你昨天去哪里了"),
    ("womingtianyaokaihui", "我明天要开会"),
    ("zhebenshuhenyouyisi", "这本书很有意思"),
    ("wodepengyoulaile", "我的朋友来了"),
    ("yijingwanchengle", "已经完成了"),
    ("xuyaoduoshaoqian", "需要多少钱"),
    ("qingnigaosuwo", "请你告诉我"),
    ("tashuodehendui", "他说得很对"),
    ("zheyangzuobutaihao", "这样做不太好"),
    ("womenzaiyiqigongzuo", "我们在一起工作"),
    ("zhegeshihouyinggai", "这个时候应该"),
    ("kanwandianyingyihou", "看完电影以后"),
    ("zuotianwanshangxiayu", "昨天晚上下雨"),
    ("womendeshijianbugou", "我们的时间不够"),
    ("zhegeshijianhenzhongyao", "这个事件很重要"),
    ("gongsideguidingshi", "公司的规定是"),
    ("qingtijiaoshenqing", "请提交申请"),
    ("xiawuliangdiankaishi", "下午两点开始"),
    ("tazaijiaxiuxi", "他在家休息"),
    ("womenyinggaizenmezuo", "我们应该怎么做"),
    ("zhejianshiqingbunan", "这件事情不难"),
    ("nihaishiyaoxiaoxin", "你还是要小心"),
    ("dajiadouzhidaole", "大家都知道了"),
];

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data");
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

fn manager(dir: &Path, weight: f64) -> EngineManager {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    cfg.schema.pinyin.grammar.weight = weight;
    // 换模型：bgc（字级 2-gram）与 bgw（词级）行为差别很大，标定时要能一键切换。
    if let Ok(m) = std::env::var("WIND_GRAM_MODEL") {
        cfg.schema.pinyin.grammar.model = m;
    }
    EngineManager::new(&cfg, Some(dir))
}

#[test]
#[ignore = "整句评测：依赖 build_dev 真实词库与 .gram 模型。用 --ignored 显式运行"]
fn grammar_sentence_report() {
    let Some(dir) = data_dir() else {
        eprintln!("!!! 跳过：build_dev 拼音词库不存在");
        return;
    };
    let gram = dir.join("schemas/pinyin/grammar");
    if !gram.exists() {
        eprintln!(
            "!!! 跳过：找不到 {}。\n\
             !!! 获取：curl -L -o zh-hans-bgc.gram \
             https://github.com/lotem/rime-octagram-data/raw/hans/zh-hans-t-essay-bgc.gram",
            gram.display()
        );
        return;
    }

    let weight = std::env::var("WIND_GRAM_WEIGHT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(1.0);

    let base = manager(&dir, 0.0);
    let with = manager(&dir, weight);

    let (mut base_hit, mut with_hit) = (0usize, 0usize);
    let mut changed: Vec<(&str, &str, String, String)> = Vec::new();

    for &(input, expect) in CASES {
        let b = base.convert_with("pinyin", input, TOP_N).candidates;
        let w = with.convert_with("pinyin", input, TOP_N).candidates;
        let bt = b.first().map(|c| c.text.clone()).unwrap_or_default();
        let wt = w.first().map(|c| c.text.clone()).unwrap_or_default();
        if bt == expect {
            base_hit += 1;
        }
        if wt == expect {
            with_hit += 1;
        }
        if bt != wt {
            changed.push((input, expect, bt, wt));
        }
    }

    let n = CASES.len();
    println!("\n=== 整句评测 (weight={weight}) ===");
    println!("样本 {n}");
    println!(
        "基线   top-1 命中 {base_hit}/{n} = {:.1}%",
        base_hit as f64 * 100.0 / n as f64
    );
    println!(
        "接模型 top-1 命中 {with_hit}/{n} = {:.1}%   ({:+})",
        with_hit as f64 * 100.0 / n as f64,
        with_hit as i64 - base_hit as i64
    );

    println!("\n--- 首选发生变化的样本 ({}) ---", changed.len());
    for (input, expect, bt, wt) in &changed {
        // 标注这次改动是修好了、弄坏了、还是两边都不对
        let tag = match (bt == expect, wt == expect) {
            (false, true) => "修好",
            (true, false) => "弄坏",
            _ => "都错",
        };
        println!("  [{tag}] {input:<26} 期望 {expect:<14} 基线 {bt:<14} 新 {wt}");
    }

    println!("\n--- 两边一致的样本里仍未命中的 ---");
    for &(input, expect) in CASES {
        let b = base.convert_with("pinyin", input, TOP_N).candidates;
        let bt = b.first().map(|c| c.text.clone()).unwrap_or_default();
        let w = with.convert_with("pinyin", input, TOP_N).candidates;
        let wt = w.first().map(|c| c.text.clone()).unwrap_or_default();
        if bt == wt && bt != expect {
            println!("  {input:<26} 期望 {expect:<14} 实得 {bt}");
        }
    }
}
