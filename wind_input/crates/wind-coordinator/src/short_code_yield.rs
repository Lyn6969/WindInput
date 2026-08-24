//! 出简让全：有简码的字，在更长的码位上把首选让给词语。
//!
//! 五笔的标准功能——「路」的三简是 `kht`，用户已经能用 3 键打出它，那么 4 码 `khtk` 的
//! 首选位就该给词语（`khtk` 下的「路上」之类），否则等于浪费一个码位。
//!
//! # 为什么在这里做，而不是在词库里
//!
//! 这件事原先由 `gen_dict::apply_demotion` 在**词库生成阶段**做，两个偏差：
//! 判定烘进权重，用户关不掉；且触发条件是「第二候选权重够高且 gap 够小」的有条件降权，
//! 不是标准的出简让全语义。搬到运行时后它成为一个开关，关掉即回到词库原序。
//!
//! # 判据从哪来：记录，而不是查询
//!
//! 「这个字有没有更短的码」看似要 `text → code` 反查，其实不用——五笔简码恒是全码的前缀，
//! 所以问题等价于「输入码的某个前缀上，首选是不是它」。而输入是渐进的：打 `khtk` 必然
//! 逐键经过 `k` → `kh` → `kht`，每一步都跑完整的候选生成。把每一步的首选记下来，到全码
//! 时信息已经齐了，**零查询**。
//!
//! 记的必须是**用户实际看到的**首条，不是词典层的序——后者不含用户调频（`apply_freq_rerank`）、
//! 候选调整（`apply_shadow`）、检索范围过滤（`apply_filter`）与用户词层的效果。用户把某字
//! 调频顶到 `kht` 首位，词典层序毫无变化，而用户确实已经能一键打出它了。
//!
//! # 档位
//!
//! `level` = 参与让位的简码级别上限：0 关闭，2 = 一二级简码置后，3 = 全部简码置后。
//! 判据是「当前码长 > level」而**不是**「当前码长 == 全码长」——后者要知道方案有几码，
//! 换到 5 码方案就错位；前者对任何码长的方案都成立。

use wind_candidate::{Candidate, CandidateSource};

/// 记录表长度：简码最多三级（一简/二简/三简）。
pub(crate) const MAX_LEVEL: usize = 3;

/// 各级简码位的首选：下标 0/1/2 = 码长 1/2/3，值为 `(该级的码, 首选文本)`。
pub(crate) type ShortcodeTops = [Option<(String, String)>; MAX_LEVEL];

/// 淘汰与当前输入码无关的陈旧记录。
///
/// **失效靠拉取而非推送**：`input_buffer.clear()` 在协调器里有十余个散落调用点，逐个接线
/// 迟早漏一处（本仓栽过多次）。改成用时校验前缀关系后，缓冲清空、光标中间编辑、方案切换
/// 全被这一条覆盖——记录的码不再是当前输入的前缀，它就不成立了。
///
/// `kht` 改成 `kxt` 时缓冲长度没变，只有前缀校验抓得住；「退格时按长度截断」抓不住。
pub(crate) fn evict_stale(tops: &mut ShortcodeTops, input: &str) {
    for slot in tops.iter_mut() {
        let stale = slot
            .as_ref()
            .is_none_or(|(code, _)| !input.starts_with(code.as_str()));
        if stale {
            *slot = None;
        }
    }
}

/// 记录本级简码位的首选。码长超出 [`MAX_LEVEL`] 或列表为空时不记。
///
/// 记的是 `candidates[0]`——**不限定来源**。出简让全问的是「用户打这个短码能不能一键得到
/// 这个字」，混输下若首选是拼音候选，那答案就是不能，此时记下拼音那条、后续比对不相等、
/// 于是不让位，正是想要的保守行为。
pub(crate) fn record_top(tops: &mut ShortcodeTops, input: &str, candidates: &[Candidate]) {
    let n = input.chars().count();
    if n == 0 || n > MAX_LEVEL {
        return;
    }
    let Some(first) = candidates.first() else {
        return;
    };
    // 只记单字：让位的对象恒是字，记词没有任何消费者。
    if first.text.chars().count() != 1 {
        tops[n - 1] = None;
        return;
    }
    tops[n - 1] = Some((input.to_string(), first.text.clone()));
}

/// 施加让位。返回是否真的动了顺序。
///
/// 调用位置有硬约束：**必须在 `apply_freq_rerank` 之后**。4 码位的
/// `ProtectPolicy.fallback` 是 0（不保护首选），先让位会被调频原样顶回去。
///
/// 缺记录一律不让位（走过临时英文/临时拼音/特殊模式/URL 模式的输入会缺级）——宁可少让，
/// 不可让错。
///
/// `user_pinned` = 本码有置顶规则就位（`apply_shadow` 的返回值）。**候选调整优先于出简
/// 让全**：用户在这个码上手动排过序，自动让位就不再插手，见下方 `user_pinned` 分支。
pub(crate) fn apply(
    candidates: &mut [Candidate],
    input: &str,
    tops: &ShortcodeTops,
    level: usize,
    user_pinned: bool,
) -> bool {
    let level = level.min(MAX_LEVEL);
    if level == 0 {
        return false;
    }
    // ── 候选调整优先 ──────────────────────────────────────────────────────────
    //
    // 用户右键调过这个码的顺序，让位就整码停手。理由不是「尊重用户」这种软话，而是
    // **`ShadowPin.position` 是绝对下标**：它记的是用户右键当时**所见列表**里的位次，
    // 而用户所见正是让位之后的列表。若让位继续在 shadow 之后动手，用户按下标 N 存进去、
    // 回放时被挪到别处，那个下标就再也表达不了任何东西——position 的语义先垮了。
    //
    // 停的是**整码**而不只是被沉底的那个字：让位的两步 rotate 会让接位词之后的候选各前移
    // 一位，「调到第 4 位」照样会变成第 3 位。只赦免首条治不了这一半。
    //
    // 代价（明确的取舍，不是遗漏）：用户只是把某个词往后挪一挪，也会连带让首选从词变回
    // 字。可预测性优先——「这个码我排过序，就按我排的来」比「有些调整生效有些不生效」
    // 好解释得多。不接受的用户把那条调整「恢复默认」即可。
    //
    // 只看置顶不看删除：删除说的是「这条别出现」，与「谁排第一」不是同一维度。判据由
    // `apply_shadow` 一处交出（见其文档），让位侧不得自己再查规则。
    if user_pinned {
        return false;
    }
    let n = input.chars().count();
    // 简码位自己不让位：只有比参与档位更长的码才谈得上「出简让全」。
    if n <= level {
        return false;
    }
    let Some(first) = candidates.first() else {
        return false;
    };
    // 首选须是码表来源的单字。让拼音/短语候选降位是跨来源排序（`source_tier`）的地盘，
    // 本功能不碰——否则混输下会与那套规则在同一个位置上打架。
    if first.source != CandidateSource::CodeTable
        || first.is_command
        || first.text.chars().count() != 1
    {
        return false;
    }
    // 扫**全部**参与级，不能只看最近一级。「路」若在二简 `kh` 就是首选，`kht` 会因此让位，
    // 于是 `kht` 的首选不再是「路」——只看最近一级的话，`khtk` 就会判成「无简码」而不让位，
    // 判据把自己擦掉了。症状是「有的字让了有的没让」，从现象极难倒推。
    let has_shortcode = tops.iter().take(level).any(|slot| {
        slot.as_ref()
            .is_some_and(|(code, top)| input.starts_with(code.as_str()) && top == &first.text)
    });
    if !has_shortcode {
        return false;
    }
    // 接位者：第一个码表来源的多字词。
    //
    // `is_scope_filtered` 必须排除——那是检索范围临时放宽才放进来的候选，「追加在末尾、
    // 原有顺序纹丝不动」是它的硬约束，提到首位会直接违背。
    let Some(pos) = candidates.iter().position(|c| {
        c.source == CandidateSource::CodeTable
            && !c.is_command
            && !c.is_scope_filtered
            && c.text.chars().count() > 1
    }) else {
        return false;
    };
    // 让位的字**沉到本码所有候选之后**，不是降一位——用户已经能用简码一键打出它，
    // 它在这个码位上就该彻底靠后，而不是继续占着第二位。同码的其它全码字（`踟` 这类
    // 没有简码的）都排在它前面：那些字**只能**在这个码位打出来，它不能挡着。
    //
    //   路 | 昤 | 路上 | 路口  →  路上 | 昤 | 路口 | 路
    //
    // 沉底的边界是**正常候选段**而非整个列表：`is_scope_filtered` 是检索范围临时放宽才
    // 补进来的，「追加在末尾、原有顺序纹丝不动」是它的硬约束，让位的字不能沉到它们后面。
    let normal_len = candidates
        .iter()
        .take_while(|c| !c.is_scope_filtered)
        .count();
    // 两步：接位者转到首位（字随之落到下标 1），再把字从下标 1 沉到正常段末尾。
    //
    // ⚠️ 不能只做沉底那一步——`路 | 昤 | 路上` 直接沉底会得到 `昤 | 路上 | 路`，
    // 首选成了生僻字。「让给词语」和「沉到最后」是两件事，必须都做。
    candidates[..=pos].rotate_right(1);
    candidates[1..normal_len].rotate_left(1);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(text: &str, source: CandidateSource) -> Candidate {
        Candidate {
            text: text.to_string(),
            source,
            ..Default::default()
        }
    }

    fn ct(text: &str) -> Candidate {
        cand(text, CandidateSource::CodeTable)
    }

    fn tops_of(pairs: &[(usize, &str, &str)]) -> ShortcodeTops {
        let mut t: ShortcodeTops = [const { None }; MAX_LEVEL];
        for &(lv, code, top) in pairs {
            t[lv - 1] = Some((code.to_string(), top.to_string()));
        }
        t
    }

    fn texts(c: &[Candidate]) -> Vec<&str> {
        c.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn yields_full_code_top_to_the_first_word() {
        let mut c = vec![ct("路"), ct("路上"), ct("路口")];
        assert!(apply(
            &mut c,
            "khtk",
            &tops_of(&[(3, "kht", "路")]),
            3,
            false
        ));
        assert_eq!(texts(&c), ["路上", "路口", "路"], "字沉到本码所有候选之后");
    }

    /// **候选调整优先**：与 `yields_full_code_top_to_the_first_word` 逐字同参，只把
    /// `user_pinned` 翻成 true —— 两条必须合看，单看任何一条都证明不了让路判据接进来了。
    ///
    /// 顺序必须**纹丝不动**（不只是「首选还是字」）：让位的两步 rotate 会把接位词之后的
    /// 候选各前移一位，只断言首选的话，那半个失效抓不到。
    #[test]
    fn user_pinned_stops_the_yield_entirely() {
        let mut c = vec![ct("路"), ct("路上"), ct("路口")];
        assert!(
            !apply(&mut c, "khtk", &tops_of(&[(3, "kht", "路")]), 3, true),
            "用户调过这个码的顺序，让位须整码停手"
        );
        assert_eq!(texts(&c), ["路", "路上", "路口"], "顺序须纹丝不动");
    }

    /// 让位的字要沉到**所有**同码候选之后，包括没有简码的全码字——那些字只能在这个
    /// 码位打出来，有简码的字不该挡着它们。其余候选相对次序不变。
    #[test]
    fn the_yielding_char_sinks_below_other_full_code_chars() {
        let mut c = vec![ct("路"), ct("昤"), ct("路上"), ct("路口")];
        assert!(apply(
            &mut c,
            "khtk",
            &tops_of(&[(3, "kht", "路")]),
            3,
            false
        ));
        assert_eq!(texts(&c), ["路上", "昤", "路口", "路"]);
    }

    /// 只沉底而不提词，首选会变成生僻字——两步缺一不可。
    #[test]
    fn word_is_promoted_even_when_a_rare_char_sits_between() {
        let mut c = vec![ct("路"), ct("昤"), ct("路上")];
        assert!(apply(
            &mut c,
            "khtk",
            &tops_of(&[(3, "kht", "路")]),
            3,
            false
        ));
        assert_eq!(
            c.first().map(|c| c.text.as_str()),
            Some("路上"),
            "首选须是词，不能是被沉底顺带顶上来的生僻字"
        );
        assert_eq!(texts(&c), ["路上", "昤", "路"]);
    }

    /// 最隐蔽的一条：二简位已是首选 ⇒ 三简位会让位 ⇒ 三简位首选不再是该字。
    /// 判定只看最近一级就会在全码位判成「无简码」，让位链自我拆台。
    #[test]
    fn scans_all_levels_so_chained_yield_does_not_erase_its_own_premise() {
        // kht 已经让位给词，故三简那级记的是「路上」而不是「路」。
        let tops = tops_of(&[(2, "kh", "路"), (3, "kht", "路上")]);
        let mut c = vec![ct("路"), ct("路口")];
        assert!(
            apply(&mut c, "khtk", &tops, 3, false),
            "二简位的记录仍在，全码位应当照样让位"
        );
        assert_eq!(texts(&c), ["路口", "路"]);
    }

    #[test]
    fn holds_when_no_record_for_this_char() {
        // 走过临时拼音等旁路时该级缺记录 —— 保守不让位。
        let mut c = vec![ct("路"), ct("路上")];
        assert!(!apply(&mut c, "khtk", &tops_of(&[]), 3, false));
        assert_eq!(texts(&c), ["路", "路上"]);
    }

    /// 记录的码必须是当前输入的前缀。光标中间改码时缓冲长度不变，只有前缀校验抓得住。
    #[test]
    fn holds_when_record_belongs_to_a_different_code() {
        let mut c = vec![ct("路"), ct("路上")];
        // 记录来自 kht，当前输入却是 kxtk。
        assert!(!apply(
            &mut c,
            "kxtk",
            &tops_of(&[(3, "kht", "路")]),
            3,
            false
        ));
        assert_eq!(texts(&c), ["路", "路上"]);
    }

    #[test]
    fn holds_when_nobody_can_take_over() {
        // 同码只有单字，没有词可以接位。
        let mut c = vec![ct("路"), ct("昤")];
        assert!(!apply(
            &mut c,
            "khtk",
            &tops_of(&[(3, "kht", "路")]),
            3,
            false
        ));
        assert_eq!(texts(&c), ["路", "昤"]);
    }

    /// 档位边界：level=2 时三简字不参与让位。
    #[test]
    fn level_two_ignores_third_level_shortcodes() {
        let tops = tops_of(&[(3, "kht", "路")]);
        let mut c = vec![ct("路"), ct("路上")];
        assert!(
            !apply(&mut c, "khtk", &tops, 2, false),
            "三简记录在档位 2 下不算数"
        );
        // 同一份候选，档位 3 就让。
        assert!(apply(&mut c, "khtk", &tops, 3, false));
    }

    #[test]
    fn level_two_still_yields_for_a_second_level_shortcode() {
        let mut c = vec![ct("大"), ct("大厦")];
        assert!(apply(
            &mut c,
            "dddd",
            &tops_of(&[(2, "dd", "大")]),
            2,
            false
        ));
        assert_eq!(texts(&c), ["大厦", "大"]);
    }

    #[test]
    fn level_zero_is_a_full_stop() {
        let mut c = vec![ct("路"), ct("路上")];
        assert!(!apply(
            &mut c,
            "khtk",
            &tops_of(&[(3, "kht", "路")]),
            0,
            false
        ));
        assert_eq!(texts(&c), ["路", "路上"]);
    }

    /// 简码位自身不让位——`kht` 在档位 3 下是简码位，不是「更长的码」。
    #[test]
    fn shortcode_position_itself_never_yields() {
        let mut c = vec![ct("路"), ct("路上")];
        assert!(!apply(
            &mut c,
            "kht",
            &tops_of(&[(2, "kh", "路")]),
            3,
            false
        ));
        assert_eq!(texts(&c), ["路", "路上"]);
    }

    /// 让位只在码表来源内做：拼音候选的次序归 `source_tier` 管。
    #[test]
    fn does_not_touch_non_codetable_candidates() {
        let mut c = vec![cand("路", CandidateSource::Pinyin), ct("路上")];
        assert!(
            !apply(&mut c, "khtk", &tops_of(&[(3, "kht", "路")]), 3, false),
            "首选不是码表来源时不让位"
        );
        // 反过来，接位者也必须是码表来源。
        let mut c2 = vec![ct("路"), cand("路上", CandidateSource::Pinyin)];
        assert!(!apply(
            &mut c2,
            "khtk",
            &tops_of(&[(3, "kht", "路")]),
            3,
            false
        ));
        assert_eq!(texts(&c2), ["路", "路上"]);
    }

    /// 检索范围临时放宽补进来的候选沉底是硬约束，不能被提到首位。
    #[test]
    fn scope_relaxed_candidates_never_take_over() {
        let mut relaxed = ct("路上");
        relaxed.is_scope_filtered = true;
        let mut c = vec![ct("路"), relaxed];
        assert!(!apply(
            &mut c,
            "khtk",
            &tops_of(&[(3, "kht", "路")]),
            3,
            false
        ));
        assert_eq!(texts(&c), ["路", "路上"]);
    }

    /// 沉底的边界是**正常候选段**：让位的字沉到放宽候选之前，不能沉到整个列表最后。
    /// 否则「放宽候选恒在末尾」这条硬约束就被这个功能破掉了。
    #[test]
    fn the_yielding_char_sinks_above_scope_relaxed_candidates() {
        let mut relaxed = ct("踟");
        relaxed.is_scope_filtered = true;
        let mut c = vec![ct("路"), ct("路上"), relaxed];
        assert!(apply(
            &mut c,
            "khtk",
            &tops_of(&[(3, "kht", "路")]),
            3,
            false
        ));
        assert_eq!(texts(&c), ["路上", "路", "踟"]);
    }

    #[test]
    fn records_only_single_char_tops_within_shortcode_range() {
        let mut t: ShortcodeTops = [const { None }; MAX_LEVEL];
        record_top(&mut t, "kht", &[ct("路"), ct("路上")]);
        assert_eq!(t[2], Some(("kht".into(), "路".into())));
        // 首选是词 ⇒ 该级作废（不是保留旧值）。
        record_top(&mut t, "kht", &[ct("路上"), ct("路")]);
        assert_eq!(t[2], None);
        // 超出简码级别不记。
        record_top(&mut t, "khtk", &[ct("路")]);
        assert_eq!(t, [None, None, None]);
    }

    #[test]
    fn evicts_records_that_are_no_longer_a_prefix() {
        let mut t = tops_of(&[(1, "k", "口"), (2, "kh", "路"), (3, "kht", "路")]);
        evict_stale(&mut t, "kxt");
        assert_eq!(t[0], Some(("k".into(), "口".into())), "k 仍是 kxt 的前缀");
        assert_eq!(t[1], None, "kh 不是 kxt 的前缀");
        assert_eq!(t[2], None);
        // 缓冲清空后重新输入，旧记录全部作废——不需要在任何 clear() 点接线。
        evict_stale(&mut t, "a");
        assert_eq!(t, [None, None, None]);
    }
}
