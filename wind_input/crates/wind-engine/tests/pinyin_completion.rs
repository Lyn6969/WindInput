//! 残码前缀补全的上浮约束回归测试
//!
//! 背景：残码存在时（`meiy` 的 `y`），前缀补全候选故意不标 `is_prefix`，使其上浮到
//! 精确子串单字之前——否则「没有」会被数百个单字「没/每/美/…」压到十几页之后。
//!
//! 但该特权原本无条件给全部 30 条补全。双拼每 2 键 1 音节 → 奇数键必有残码，
//! 长输入下候选 2~5 位会被冷僻长词占满，并随每次按键在两种形态间反复跳动。
//! 现按「补全距离 + 置信度」约束（见 `COMPLETION_UNCONDITIONAL_FLOAT_SYLLABLES` /
//! `COMPLETION_FAR_WEIGHT_FLOOR`；⚠️ 不是 `COMPLETION_NEAR_SYLLABLES`，那个只管
//! **用户词**长词上浮，两者一度共用一个常量并因此串味）。
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
        // ⚠️ 这两条的注释原为「阈值取 1 会被误杀」，**已被实测推翻**：
        // `COMPLETION_UNCONDITIONAL_FLOAT_SYLLABLES` 收到 1 后它们只是改走
        // `COMPLETION_FAR_WEIGHT_FLOOR`(100) 那条路，而 2010/1609 远高于门槛，照样上浮
        // （实测位次 3 / 1）。真正被这次收紧挡掉的是 w=18 的「中华人民」——
        // 见 `low_freq_far_completion_does_not_outrank_sentence`。
        ("beijingd", "北京大学", "距离+2 w=2010，靠 FLOOR 放行"),
        ("jisuanjik", "计算机科学", "距离+2 w=1609，靠 FLOOR 放行"),
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

/// 补全折扣：同一上浮层内，未输入音节更多的候选须让位于更短的候选。
///
/// 用户报告的原始现象：打 `nih` 时首屏是「你会 → 你会发现 → 你好」，
/// 三、四字长词排在两字常用词之前。根因是 `is_promoted_completion` 是**布尔层级**，
/// 上浮的补全在层内**只比裸词频**，extra=1 与 extra=3 同等对待：
/// 「你会发现」(w=13330, extra=3) 因此压过「你好」(w=5328, extra=1)。
///
/// 修法＝`COMPLETION_WEIGHT_DISCOUNT`（0.5^extra），对齐 librime `kCompletionPenalty`
/// 与 fcitx5 `overLengthCost`。折后 你好 2664 > 你会发现 1666。
///
/// ⚠️ 断言用**相对位置**而非绝对位次：首选「你会」(w=22262) 赢「你好」纯粹是词库词频
/// 使然（unigram 无上下文，分不开「你会」这类句式碎片），不在本机制职责内 ——
/// 把它写进断言会让这条测试实际在守卫一个它治不了的东西。
#[test]
fn completion_discount_demotes_words_with_more_unentered_syllables() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    let cands = mgr.convert_with("pinyin", "nih", 12).candidates;
    let pos = |t: &str| cands.iter().position(|c| c.text == t);
    let texts = || cands.iter().map(|c| &c.text).collect::<Vec<_>>();

    let nihao = pos("你好").expect("「你好」应在 nih 的候选中");
    let faxian = pos("你会发现").expect("「你会发现」应仍在候选中（沉底而非消失）");
    assert!(
        nihao < faxian,
        "「你好」(extra=1) 须在「你会发现」(extra=3) 之前，实际: {:?}",
        texts()
    );

    // 4 音节噪音整体不该占据首屏前三。
    for (i, c) in cands.iter().enumerate().take(3) {
        assert!(
            c.text.chars().count() <= 2,
            "nih 首屏前三不该出现 {} 字词「{}」（第 {} 位），实际: {:?}",
            c.text.chars().count(),
            c.text,
            i + 1,
            texts()
        );
    }
}

/// step 6.5b：整句须让位于「恰好用完残码的补全」，且**只让给它**。
///
/// 现象：打 `nihaom` 时首选恒是整句「你好」——它把用户已按下的 `m` 丢掉了。实测该规律
/// 与音节数无关（2/3/4/6 音节一律如此），根因是当时整句的 `SENTENCE_WEIGHT_BASE`(3e7，已退役)
/// 无条件置顶，补全只有真实词频（个位数 ~ 1e4），差 4~7 个数量级。
///
/// 判据复刻 librime `has_exact_match_phrase`（`gear/script_translator.cc:387`：存在覆盖
/// 完整输入的精确词条时不生成整句）：**补全词音节数 == 已完成音节数 + 1**。
///
/// ⚠️ 反例与正例同等重要：`beijingdaxuex` 的「北京大学校长」是 6 音节 ≠ 4+1，**不该**触发
/// 让位 —— 它 w=4，一旦放进来就会顶掉「北京大学」。缺了这半边断言，把判据放宽成
/// 「extra ≤ 2」之类的一刀切也照样绿。
#[test]
fn sentence_yields_to_completion_that_exhausts_trailing_partial() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    let top = |input: &str| -> String {
        mgr.convert_with("pinyin", input, 6).candidates[0]
            .text
            .clone()
    };

    // 正例：补全恰好用完残码（音节数 == completed + 1）⇒ 整句让位。
    //
    // ⚠️ `nihaom`→你好吗 与 `zhongguor`→中国人 **已从本列表移出**，不是因为坏了，而是
    // step 2c（残码整句）落地后它们不再经过本机制：Viterbi 直接把残码补成那个字，产出的
    // 整句**就是**「你好吗」/「中国人」本身（`is_prefix=0`、`code` 含残码），既不是补全、
    // 也就无所谓让位（实测 `is_sentence_demoted=0`）。它们改由
    // `test_trailing_partial_completes_into_sentence` 覆盖。
    //
    // 6.5b 仍然必要且在此被守着：留下的两条是**Viterbi 选出的字与词库补全不一致**的情形
    // ——`zhongguorenm` 整句是「中国人吗」而补全是「中国人民」，此时才需要让位。
    for (input, want, note) in [
        ("zhongguorenm", "中国人民", "4 == 3+1，整句「中国人吗」让位"),
        (
            "zhonghuarenmingongheg",
            "中华人民共和国",
            "7 == 6+1，整句「中华人民共和」让位",
        ),
    ] {
        assert_eq!(top(input), want, "{input}：{note}");
    }

    // 反例：补全没用完残码就结束（音节数 > completed + 1）⇒ **不得**触发让位。
    // 「北京大学校长」bei|jing|da|xue|xiao|zhang = 6 ≠ 4+1，且 w=4 属冷僻预测词。
    //
    // ⚠️ 断言的是「校长没夺走首位」，不是「首选恰为北京大学」：step 2c 落地后残码 `x`
    // 会被补成一个字（实测「北京大学下」），它 consumed 满、按比较链 ⓪ 本就该在
    // 只消费 12/13 键的「北京大学」之前 —— 与 `buzhidaok`→「不知道看」同形态。
    // 写死具体文本会让这条反例实际在守一个它不负责的东西。
    assert_ne!(
        top("beijingdaxuex"),
        "北京大学校长",
        "beijingdaxuex：「北京大学校长」6 音节 ≠ 4+1，不该夺走首位"
    );

    // 无残码时本机制整个不启动（trailing_partial=false），整句照常居首。
    assert_eq!(top("nihao"), "你好", "无残码：整句不受影响");
    assert_eq!(top("nihaoma"), "你好吗", "无残码：整句不受影响");
}

/// 距离 ≥2 的补全必须过 `COMPLETION_FAR_WEIGHT_FLOOR`：低频长词不得靠「近距离豁免」登顶。
///
/// 现场（用户报「候选长度来回跳动」）：逐字符打「中华人民共和国」时，`zhonghuar` 的首选
/// 曾是 **w=18 的「中华人民」**，把整句「中华」压在后面；再多打两个字母又跳回 3 音节。
/// 根因是 `COMPLETION_UNCONDITIONAL_FLOAT_SYLLABLES` 当时取 2，使**距离 2 整档**白白豁免
/// 了 FLOOR —— 有残码时冷僻长词靠豁免登顶、无残码时整句 3e7 登顶，两套依据逐键切换。
///
/// ⚠️ 同时守着「收紧没有误伤高频远距离补全」：`beijingd`→北京大学(w=2010, 距离 2)、
/// `jisuanjik`→计算机科学(w=1609, 距离 2) 改走 FLOOR 那条路，仍须留在首屏 —— 这正是旧注释
/// 断言「取 1 会直接干掉这类极常见场景」的两个例子，实测它们只是换了条路进来。
#[test]
fn low_freq_far_completion_does_not_outrank_sentence() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    let cands = mgr.convert_with("pinyin", "zhonghuar", 6).candidates;
    let texts = || cands.iter().map(|c| &c.text).collect::<Vec<_>>();
    // ⚠️ 首选从「中华」改为「中华人」：step 2c 落地后残码 `r` 被补成一个待定音节，
    // 产出消费满 9 键的残码整句，按比较链 ⓪ 本就该在只消费 8 键的「中华」之前。
    // 本用例真正要守的是**下面那条**——w=0 的「种花人」与 w=18 的「中华人民」都不得
    // 靠上浮夺走首位，那与首选具体是哪条整句无关。
    assert_eq!(
        cands[0].text,
        "中华人",
        "zhonghuar 首选应是残码整句「中华人」，实际: {:?}",
        texts()
    );
    assert!(
        !cands.iter().take(3).any(|c| c.text == "中华人民"),
        "「中华人民」(w=18, 距离 2) 未过 FLOOR，不该进前 3，实际: {:?}",
        texts()
    );

    // 反向对照：高频远距离补全不得被这次收紧误伤。
    for (input, want) in [("beijingd", "北京大学"), ("jisuanjik", "计算机科学")] {
        let rank = rank_of(&mgr, "pinyin", input, want);
        assert!(
            rank.is_some_and(|r| r < 6),
            "{input}→「{want}」词频远高于 FLOOR，收紧后仍须在首屏，实际位置 {rank:?}"
        );
    }
}

/// step 6.5b 的置信度门槛：冷僻补全不得把整句顶掉，整句权重更不得被压成负数。
///
/// `zhonghuar` 下「种花人」(`zhonghuaren`) 音节数 3 == completed 2 + 1，**满足 6.5b 的音节
/// 判据**，但它 w=0。缺了 `SENTENCE_YIELD_WEIGHT_FLOOR` 时实测：首选变成「种花人」，
/// 整句「中华」被降到 **w=-1**。
///
/// librime 不需要这道门槛（整句与词条同轴，w=0 自然排不上去）；我们的整句拿 3e7 跨轴、
/// 让位只能做成二值开关，所以必须自己补回这条线。
#[test]
fn sentence_does_not_yield_to_low_confidence_completion() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    let cands = mgr.convert_with("pinyin", "zhonghuar", 6).candidates;
    let sentence = cands
        .iter()
        .find(|c| c.text == "中华")
        .expect("整句「中华」须在候选中");
    assert!(
        !sentence.is_sentence_demoted,
        "「种花人」(w=0) 不该触发整句让位，实际候选: {:?}",
        cands
            .iter()
            .map(|c| (&c.text, c.weight))
            .collect::<Vec<_>>()
    );
    assert!(
        sentence.weight > 0,
        "整句权重不得被降成 0/负数，实际 {}",
        sentence.weight
    );
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

/// step 2c：尾部残码作为「待定音节」入图，由 Viterbi 补出最优单字。
///
/// 用户报的核心问题：`buzhidaok` 在主流输入法给「不知道看」，而我们此前整句止步于
/// 「不知道」、末尾 `k` 无人认领 —— 且「不知道看」**在 147 条候选里根本不存在**，
/// 是生成层缺失而非排序问题（排序改到天上也排不出不存在的候选）。
#[test]
fn test_trailing_partial_completes_into_sentence() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    // 后三条从 `sentence_yields_to_completion_that_exhausts_trailing_partial` 移入：
    // 它们此前靠 6.5b「整句让位于补全」间接达成，现在由 step 2c 直接产出。
    // ⚠️「人」「吗」都是**实词/非虚词表成员**，而被它们取代的「让」「们」在虚词表里——
    // 这三条同时守着 `score_node_partial_final`（残码位不给虚词优待），去掉那个函数即变红。
    for (input, want) in [
        ("buzhidaok", "不知道看"),
        ("jisuanjik", "计算机看"),
        ("nihaom", "你好吗"),
        ("zhongguor", "中国人"),
        ("zhonghuar", "中华人"),
    ] {
        let cands = mgr.convert_with("pinyin", input, 300).candidates;
        let hit = cands.iter().find(|c| c.text == want).unwrap_or_else(|| {
            panic!(
                "{input} 应产出残码补全整句「{want}」，实际前 8: {:?}",
                cands.iter().take(8).map(|c| &c.text).collect::<Vec<_>>()
            )
        });
        assert!(hit.is_sentence, "「{want}」须带整句身份");
        assert_eq!(
            hit.consumed_length,
            input.len(),
            "残码整句必须解释**全部**输入（这正是它区别于 step 2 结果之处）"
        );
    }
}

/// 残码补全**不得把已完成的音节重新切开**。
///
/// 这条锁住 step 2c 与 `add_abbrev_nodes` 的分工：二者都是「补音节图给不出的节点」、
/// 代码形状几乎一样，但简拼节点会把整串按声母重切。实测放开简拼闸门让残码入图，
/// `buzhidaok` 产出的是「不直达欧卡」、`nihaom` 是「你黑暗欧美」—— `bu zhi dao` 被
/// 拆回 b/u/zh/i/d/a/o 去凑简拼了。若哪天有人把两条路径合并，本测试当场变红。
#[test]
fn test_partial_completion_preserves_completed_syllables() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    for (input, prefix) in [
        ("buzhidaok", "不知道"),
        ("nihaom", "你好"),
        ("qingfengs", "清风"),
    ] {
        let cands = mgr.convert_with("pinyin", input, 300).candidates;
        let sentences: Vec<_> = cands
            .iter()
            .filter(|c| c.is_sentence && c.consumed_length == input.len())
            .collect();
        // 前置：没有这一句，step 2c 一旦被整体关掉本测试就**真空假绿**（「不该出现 X」型
        // 断言在 X 一个都不产生时恒真）。实测有效性时正是这条露的馅。
        assert!(
            !sentences.is_empty(),
            "前置：{input} 应产出至少一条残码整句，否则本用例退化成空断言"
        );
        for c in sentences {
            assert!(
                c.text.starts_with(prefix),
                "{input} 的残码整句「{}」没有保住已完成音节「{prefix}」——\
                 已完成部分被重新切分了（简拼通道的特征）",
                c.text
            );
        }
    }
}

/// step 2c 的门槛：`syllables.len() >= 2`。
///
/// 1 音节 + 残码（`nim`）不走本路径——那种输入的正解是词库补全（你们/你没），
/// 残码整句「你吗」只会挤掉它。同 fcitx5 `partialLongWordLimit` 的精神：
/// 短输入不做激进的部分匹配。
#[test]
fn test_partial_completion_skips_single_syllable_input() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    let cands = mgr.convert_with("pinyin", "nim", 300).candidates;
    let sentences: Vec<_> = cands
        .iter()
        .filter(|c| c.is_sentence && c.consumed_length == 3)
        .map(|c| &c.text)
        .collect();
    assert!(
        sentences.is_empty(),
        "1 音节 + 残码不该走残码整句，实际产出: {sentences:?}"
    );
}

/// 残码整句必须带 `is_sentence_unanchored`。
///
/// **本用例断言的是标记本身，不是排序效果**，这是有意的：整句锚定已整体移除，置不置位当前
/// 都不改变任何顺序，任何按排序结果写的断言都测不出这个标记的存在与否。
///
/// 该字段现已无消费点，等同于一条待回收的死码 —— 保留本断言是为了在它被回收之前，
/// 「置位点被误删」这件事仍有东西守着。回收时连同本用例一并删除。
#[test]
fn partial_sentence_is_marked_unanchored() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);

    for (input, want) in [
        ("buzhidaok", "不知道看"),
        ("zhonghuar", "中华人"),
        ("nihaom", "你好吗"),
    ] {
        let cands = mgr.convert_with("pinyin", input, 300).candidates;
        let hit = cands
            .iter()
            .find(|c| c.text == want)
            .unwrap_or_else(|| panic!("{input} 应产出残码整句「{want}」"));
        assert!(
            hit.is_sentence && hit.is_sentence_unanchored,
            "「{want}」须同时带整句身份与 unanchored 标记（实际 sentence={} unanchored={}）",
            hit.is_sentence,
            hit.is_sentence_unanchored
        );
    }
}

/// `weight <= 0` 的词条不得获得「距离 1 无条件上浮」的特权。
///
/// w≤0 是词库对**存疑 / 非标准条目**的标记（`lattice.rs::score_node` 早就对它罚 -10），
/// 而无条件上浮那条原本没有任何权重下限 —— 于是最不可靠的词反而被提到最显眼处：
/// `zhonghuar` 的「种花人」(w=0、距离恰好 1) 排到第 2，压过 w=18 的「中华人民」
/// （后者距离 2、要过 `COMPLETION_FAR_WEIGHT_FLOOR` 而没过）。
///
/// librime 用 `log(w > 0 ? w : DBL_EPSILON)` 在结构上避免了这类条目参与竞争。
#[test]
fn zero_weight_completion_does_not_get_unconditional_float() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    let cands = mgr.convert_with("pinyin", "zhonghuar", 300).candidates;
    let pos = |t: &str| cands.iter().position(|c| c.text == t);
    let head = || {
        cands
            .iter()
            .take(6)
            .map(|c| format!("{}(w={})", c.text, c.weight))
            .collect::<Vec<_>>()
    };

    let zhonghuaren = pos("中华人民").expect("「中华人民」应在候选中");
    let zhonghuaren0 = pos("种花人").expect("「种花人」应仍在候选中（降级≠销毁）");
    assert!(
        zhonghuaren < zhonghuaren0,
        "w=18 的「中华人民」须排在 w=0 的「种花人」之前，实际: {:?}",
        head()
    );
}
