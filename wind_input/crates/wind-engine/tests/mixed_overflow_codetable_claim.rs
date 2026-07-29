//! 混输超码长归属：`pinyin_only_overflow`（⓪）的例外口 `codetable_owns_overflow`。
//!
//! 真机现象：五笔混输下 `yijg`（唯一全码「就是」）**再打任何一个字母**，候选里「就是」整条
//! 消失、只剩拼音「以」——且把上屏否决开关（①`auto_commit_block_on_pinyin` /
//! ②`block_commit_on_pinyin_word` / ③`auto_commit_block_on_english`）全关也无济于事。
//!
//! 根因是超码长走了另一套逻辑，由 ⓪ 独占裁决，卡死两个出口：
//! - `handle_top_code` 的 ⓪ 判据只看「拼音有没有候选」，不读任何否决开关；
//! - `convert_overflow` 里唯一能捞回码表候选的逃生口 `has_full_input_match(input) ||
//!   has_longer_code(input)` 问的是**整串**，而五笔 4 码封顶、不存在 5 码词条 ⇒ 恒假。
//!
//! 修法：两处共用 `codetable_owns_overflow` = 「前 N 码是精确全码」且「拼音主张不了整串」。
//!
//! ⚠️ 全部用例走**真 `PinyinEngine`**（不用 fake）：本判据的分水岭是
//! `is_possible_pinyin_sequence` 与候选 `consumed_length` 的真实取值，fake 引擎给什么就是什么，
//! 用 fake 锁不住真机行为（`youyo` 在 fake 下靠「消费整串」偶然通过，真机靠的是「还没打完」）。

use std::sync::Arc;
use wind_dict::cached::CachedDict;
use wind_dict::codetable::CodetableDict;
use wind_dict::{DictManager, SystemDictLayer};
use wind_engine::codetable::{CodeTableEngine, CommitOptions};
use wind_engine::mixed::{MixConfig, MixedEngine};
use wind_engine::pinyin::Config as PinyinConfig;
use wind_engine::{Engine, PinyinEngine};

/// 五笔侧（4 码封顶）：`yijg`=就是（本例主角）、`youy`=变凉（youyoud 回归对照）。
/// 两者都是**精确全码**，故「前缀是精确全码」这一条对两个用例同时成立 —— 唯一变量只剩
/// 「拼音主张不主张整串」。
fn wubi() -> Box<dyn Engine> {
    let mut d = CodetableDict::empty();
    d.merge_single("yijg".into(), "就是".into(), 5000, 0);
    d.merge_single("youy".into(), "变凉".into(), 864, 1);
    let dm = DictManager::new();
    dm.register_layer(Box::new(SystemDictLayer::new(CachedDict::Memory(d), "sys")));
    Box::new(CodeTableEngine::new(
        4,
        CommitOptions {
            top_code_commit: true,
            ..Default::default()
        },
        Arc::new(dm),
    ))
}

fn pinyin() -> PinyinEngine {
    let mut d = CodetableDict::empty();
    d.merge_single("yi".into(), "以".into(), 9000, 0);
    d.merge_single("you".into(), "悠".into(), 9000, 1);
    d.merge_single("ni".into(), "你".into(), 9000, 2);
    PinyinEngine::new(PinyinConfig::default(), CachedDict::Memory(d))
}

/// 「否决选项全关」＝ ①②③ 全关，⓪ 保持出厂 `true`（用户设置页里关得掉的就是 ①②③）。
fn all_vetoes_off() -> MixConfig {
    MixConfig {
        auto_commit_block_on_pinyin: false,
        block_commit_on_pinyin_word: false,
        auto_commit_block_on_english: false,
        pinyin_only_overflow: true,
        top_code_override_pinyin: false,
        ..Default::default()
    }
}

fn mixed(cfg: MixConfig) -> MixedEngine {
    MixedEngine::new(wubi(), Some(Box::new(pinyin())), None, cfg)
}

fn texts(e: &MixedEngine, input: &str) -> Vec<String> {
    e.convert(input, 20)
        .unwrap()
        .candidates
        .into_iter()
        .map(|c| c.text)
        .collect()
}

/// 前置事实：`yijg` 本身（未超码长）一切正常——五笔精确全码排第一。
/// 锁住「问题只发生在第 5 键」，免得后续有人误改等长合并分支。
#[test]
fn full_code_itself_is_unaffected() {
    let e = mixed(all_vetoes_off());
    assert_eq!(
        texts(&e, "yijg").first().map(String::as_str),
        Some("就是"),
        "4 码满码时五笔精确全码本就该排第一"
    );
}

/// 核心诉求：`yijg` + 字母 → 顶码放行，候选里「就是」回来且排第一。
#[test]
fn codetable_reclaims_overflow_when_pinyin_cannot_explain() {
    // 前提：本例确实落在「拼音主张不了整串」那一侧（否则下面测的是别的分支）。
    let py = pinyin();
    assert!(
        !py.is_possible_pinyin_sequence("yijga"),
        "前置：jg 不是音节前缀 ⇒ 拼音没在打这串"
    );
    assert_eq!(
        py.convert("yijga", 1).unwrap().candidates[0].consumed_length,
        2,
        "前置：拼音首选只解释 yi（2/5），够不上接管整串"
    );

    let e = mixed(all_vetoes_off());

    assert_eq!(
        e.handle_top_code("yijga"),
        Some(("就是".to_string(), "a".to_string())),
        "前 N 码是精确全码且拼音只解释得了 yi → ⓪ 不得再拦顶码"
    );

    let c = texts(&e, "yijga");
    assert_eq!(
        c.first().map(String::as_str),
        Some("就是"),
        "顶码关闭时也须由候选兜住：「就是」应排第一，实际 {c:?}"
    );
    assert!(c.iter().any(|t| t == "以"), "拼音候选仍应保留供选择：{c:?}");
}

/// 「+**任何**字母」都成立（用户原话）：拼音切分只认开头的 `yi`，与后续字母无关。
#[test]
fn holds_for_every_trailing_letter() {
    let e = mixed(all_vetoes_off());
    for ch in "abcdefghijklmnopqrstuvwxyz".chars() {
        let s = format!("yijg{ch}");
        let c = texts(&e, &s);
        assert_eq!(
            c.first().map(String::as_str),
            Some("就是"),
            "{s}: 首选应为「就是」，实际 {c:?}"
        );
    }
}

/// 反向锁（youyoud 回归）：`youyo` 与 `yijga` **只差一个变量**——两者前 N 码（`youy`/`yijg`）
/// 都是精确全码，但 `youyo` = you + `yo`，`yo` 是合法音节前缀 ⇒ 拼音还没打完、主张整串。
/// 此时 ⓪ 必须继续拦，否则第 5 键就把「悠悠的」顶成「变凉」。
#[test]
fn pinyin_still_owns_overflow_when_it_may_continue() {
    // 前提：拦截必须来自「拼音还没打完」这一位，而不是「前缀不是精确全码」——否则本例
    // 与 yijga 就不再是单一变量对照，youyoud 回归也就没被真正锁住。
    assert!(
        pinyin().is_possible_pinyin_sequence("youyo"),
        "前置：you + yo（合法音节前缀）⇒ 拼音还在打"
    );
    assert!(
        wubi().has_full_input_match("youy"),
        "前置：前 N 码 youy 确是精确全码（与 yijg 同侧），唯一变量只剩拼音主张与否"
    );

    let e = mixed(all_vetoes_off());
    assert_eq!(
        e.handle_top_code("youyo"),
        None,
        "拼音还没打完（you+yo）→ ⓪ 照常抑制顶码"
    );
    let c = texts(&e, "youyo");
    assert!(
        !c.iter().any(|t| t == "变凉"),
        "码表候选不得回捞，否则「悠悠的」被五笔截胡：{c:?}"
    );
    assert!(c.iter().any(|t| t == "悠"), "应保持纯拼音候选：{c:?}");
}

/// 反向锁（拼音打错字母）：`nihxo` 的拼音同样主张不了整串（`hx` 不是音节前缀），
/// 但前 N 码 `nihx` 在码表**没有精确全码** ⇒ 例外口不成立，仍归拼音。
/// 这一条保证「打错一个字母就被五笔顶码截胡」不会发生。
#[test]
fn no_reclaim_when_prefix_is_not_an_exact_code() {
    // 前提：拦截必须来自「前缀不是精确全码」这一位。若拼音其实主张得了这串，本例就退化成
    // 与上一例重复的测试，"前缀精确全码" 这一半判据将无人看守。
    let py = pinyin();
    assert!(
        !py.is_possible_pinyin_sequence("nihxo"),
        "前置：hx 不是音节前缀 ⇒ 拼音已打岔（与 yijga 同侧）"
    );
    assert!(
        py.convert("nihxo", 1).unwrap().candidates[0].consumed_length < 5,
        "前置：拼音首选解释不了整串"
    );
    assert!(
        !wubi().has_full_input_match("nihx"),
        "前置：前 N 码 nihx 在码表无精确全码 —— 这才是本例唯一的拦截理由"
    );

    let e = mixed(all_vetoes_off());
    assert_eq!(
        e.handle_top_code("nihxo"),
        None,
        "前缀非精确全码 → 码表没资格主张，顶码仍抑制"
    );
    assert!(
        texts(&e, "nihxo").iter().any(|t| t == "你"),
        "仍应交出拼音候选"
    );
}

/// 回捞的前缀候选不得被当成**本次输入**的精确匹配：`is_exact_code` 归一到完整输入。
/// 下游（协调器 `candidate_display_order` / `freq_rerank::freq_tier`）一律以完整输入为准，
/// 不归一会把只匹配前缀的候选提拔进精确档。
#[test]
fn reclaimed_prefix_candidate_is_not_marked_exact() {
    let e = mixed(all_vetoes_off());
    let r = e.convert("yijga", 20).unwrap();
    let jiushi = r
        .candidates
        .iter()
        .find(|c| c.text == "就是")
        .expect("前置：「就是」应已回捞");
    assert_eq!(jiushi.code, "yijg", "候选码是前 N 码");
    assert!(
        !jiushi.is_exact_code,
        "code(yijg) != input(yijga)，不得标为精确匹配"
    );
}

/// 关掉 ⓪ 的老路径不受影响（混合 overflow：前 N 码 + 拼音整串竞争）。
#[test]
fn overflow_off_path_unchanged() {
    let e = mixed(MixConfig {
        pinyin_only_overflow: false,
        ..all_vetoes_off()
    });
    assert_eq!(
        e.handle_top_code("yijga"),
        Some(("就是".to_string(), "a".to_string()))
    );
    assert_eq!(texts(&e, "yijga").first().map(String::as_str), Some("就是"));
}

/// ★ 出厂默认下的完整表现：**候选回捞、但不上屏**。这是本修复「默认安全」的全部依据。
///
/// `handle_top_code` 里 ⓪ 通过后还要过 ①②（`pinyin_vetoes_commit`），而 ①
/// `auto_commit_block_on_pinyin` 出厂即开 + `yijga` 确有拼音候选 ⇒ 顶码仍被拦下。
/// 于是默认用户看到的只是「就是」回到候选首位，按空格才上屏；只有像本文件其余用例那样
/// **主动关掉 ①** 的用户（即报这个 bug 的场景）才会拿到顶码直接上屏。
///
/// ⚠️ 反过来说：⓪ 的例外口**不是**上屏放行口。若日后有人为了「让五笔更爽快」把这条例外
/// 挪到 ①② 之后或让它压制 ①②，默认配置下的顶码行为就会突变，务必先重估此例。
#[test]
fn default_guards_reclaim_candidates_but_still_block_commit() {
    let e = mixed(MixConfig::default());

    let c = texts(&e, "yijga");
    assert_eq!(
        c.first().map(String::as_str),
        Some("就是"),
        "候选装配只受 ⓪ 管，默认开关下同样应回捞：{c:?}"
    );
    assert!(c.iter().any(|t| t == "以"), "拼音候选仍在列：{c:?}");

    assert_eq!(
        e.handle_top_code("yijga"),
        None,
        "① 出厂即开 ⇒ 顶码仍被 pinyin_vetoes_commit 拦下，默认用户不会遭遇自动上屏"
    );
}
