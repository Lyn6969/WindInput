//! 常用字表的**用户覆盖**（候选右键「设为生僻字 / 设为常用字」）端到端。
//!
//! 装置沿用 `codetable_filter_scope_consistency.rs` 的现场：五笔 `sivg` 码位上坐着
//! 常用的「档」与生僻的「桜」，智能档只放行前者。把「档」标成生僻之后，这个码位就
//! **没有常用字了**，于是「桜」按孤儿码规则被放出来——一条断言同时验到了
//! 写库、镜像回灌、候选重建、过滤联动四步。
//!
//! ⚠️ `build_dev/data` 不存在时**整族静默跳过而计数照绿**（判据是耗时：正常秒级，
//! 跳过是 0.0x s）。恢复命令 `.\scripts\dev.ps1 gd`。

use std::path::PathBuf;
use std::sync::Arc;
use wind_bridge::handler::{KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::EVENT_KEY_DOWN;
use wind_ui_types::CandidateOp;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn dict_ready(d: &std::path::Path) -> bool {
    d.join("schemas/wubi86/wubi86_jidian.dict.yaml").exists()
}

fn key_event(key_code: u32) -> KeyEventData {
    KeyEventData {
        key_code,
        scan_code: 0,
        modifiers: 0,
        event_type: EVENT_KEY_DOWN,
        toggles: 0,
        event_seq: 0,
        prev_char: 0,
    }
}

/// 每个测试独立库：redb 单写者，共用文件会让并发测试互相阻塞。
fn store(tag: &str) -> Arc<wind_store::Store> {
    let p = std::env::temp_dir().join(format!(
        "wind_common_override_{}_{}.redb",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_file(&p);
    Arc::new(wind_store::Store::open(&p).unwrap())
}

fn coord(tag: &str) -> Arc<Coordinator> {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into()];
    cfg.schema.active = "wubi86".into();
    cfg.input.default.chinese_mode = true;
    cfg.input.filter_mode = "smart".into();
    Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store(tag))
}

/// 按键走**生产入口** `handle_key_event_policed`，不是内部的 `handle_key_event`——
/// 后者绕过若干收口，等于验证一条真实不存在的路径。
fn press(coord: &Coordinator, code: &str) {
    for c in code.chars() {
        coord.handle_key_event_policed(&key_event((c.to_ascii_uppercase() as u32) & 0xFF));
    }
}

fn clear(coord: &Coordinator) {
    for _ in 0..12 {
        coord.handle_key_event_policed(&key_event(0x08)); // Backspace
    }
}

fn cands(coord: &Coordinator) -> Vec<String> {
    coord.debug_all_candidate_texts()
}

/// 打一串码并取候选。
fn cands_of(coord: &Coordinator, code: &str) -> Vec<String> {
    clear(coord);
    press(coord, code);
    cands(coord)
}

/// 页内第 n 项的位置（用于对着某条候选点右键）。
fn index_of(list: &[String], text: &str) -> Option<usize> {
    list.iter().position(|t| t == text)
}

/// ★ 核心链路：把同码位唯一的常用字标成生僻，被它压着的生僻字当场露出来。
///
/// 走完整条路：写 redb → 回灌内存镜像 → 重建候选 → 智能过滤重算。任何一步断掉，
/// 「桜」都不会出现。
#[test]
fn marking_the_common_char_rare_releases_the_suppressed_one() {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：build_dev/data 缺失");
        return;
    }
    let c = coord("release");

    let before = cands_of(&c, "sivg");
    assert!(
        before.contains(&"档".to_string()),
        "前置不成立：sivg 应有常用字「档」，实际 {before:?}"
    );
    assert!(
        !before.contains(&"桜".to_string()),
        "前置不成立：智能档本应压住生僻的「桜」，实际 {before:?}"
    );

    // 对着「档」点右键 →「设为生僻字（全局）」。
    let idx = index_of(&before, "档").expect("「档」应在候选里");
    assert_eq!(
        c.debug_common_char_mark(idx),
        Some(('档', true)),
        "菜单侧应认「档」为当前判常用（据此给「设为生僻字」）"
    );
    c.debug_candidate_op(CandidateOp::ToggleCommon, idx);

    let after = cands(&c);
    assert!(
        after.contains(&"桜".to_string()),
        "「档」降级后 sivg 再无常用字，生僻的「桜」应按孤儿码放行，实际 {after:?}"
    );
    // 菜单文案的取值随之翻面。
    let idx2 = index_of(&after, "档").expect("「档」自身仍在（降级不等于隐藏）");
    assert_eq!(
        c.debug_common_char_mark(idx2),
        Some(('档', false)),
        "再次右键应给「设为常用字」"
    );
}

/// 覆盖是**全局**的：换一个码位打同一个字，判定跟着走。
///
/// 这正是它与 shadow 的分界——shadow 键含输入码，只在那个码下生效。
#[test]
fn override_applies_across_codes() {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：build_dev/data 缺失");
        return;
    }
    let c = coord("global");

    // 「档」的简码 siv 与全码 sivg 是两个不同的输入码。
    let full = cands_of(&c, "sivg");
    let idx = index_of(&full, "档").expect("sivg 应有「档」");
    c.debug_candidate_op(CandidateOp::ToggleCommon, idx);

    // 换到简码 siv 上再看：同一个字，判定必须已经是生僻。
    let short = cands_of(&c, "siv");
    let idx2 = index_of(&short, "档").expect("siv 也应有「档」");
    assert_eq!(
        c.debug_common_char_mark(idx2),
        Some(('档', false)),
        "覆盖不带输入码，换个码打同一个字判定应一致，实际候选 {short:?}"
    );
}

/// 点回出厂方向 = **删掉覆盖**，而不是写一条同向记录。
///
/// 库里因此永远只留「与出厂不同」的字：词库管理界面列出来的就是一份干净的
/// 「我改过的」，出厂表升版时没被碰过的字自动跟随。
#[test]
fn toggling_back_removes_the_row_instead_of_writing_a_redundant_one() {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：build_dev/data 缺失");
        return;
    }
    let st = store("roundtrip");
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into()];
    cfg.schema.active = "wubi86".into();
    cfg.input.default.chinese_mode = true;
    cfg.input.filter_mode = "smart".into();
    let c = Coordinator::new_headless_with_store(cfg, Some(&d), Arc::clone(&st));

    let list = cands_of(&c, "sivg");
    let idx = index_of(&list, "档").expect("sivg 应有「档」");

    c.debug_candidate_op(CandidateOp::ToggleCommon, idx);
    assert_eq!(
        st.get_common_char_override('档').unwrap(),
        Some(false),
        "第一次点击应写下一条与出厂相反的覆盖"
    );

    // 再点一次：目标方向（常用）恰好等于出厂判定 ⇒ 删覆盖，而不是存 true。
    let list2 = cands(&c);
    let idx2 = index_of(&list2, "档").expect("「档」仍在");
    c.debug_candidate_op(CandidateOp::ToggleCommon, idx2);
    assert_eq!(
        st.get_common_char_override('档').unwrap(),
        None,
        "点回出厂方向应删掉那条记录，而不是写一条同向的冗余覆盖"
    );
    assert!(
        st.list_common_char_overrides().unwrap().is_empty(),
        "库里不该留下任何痕迹"
    );

    // 行为也回到原样：生僻的「桜」重新被压住。
    let back = cands_of(&c, "sivg");
    assert!(
        !back.contains(&"桜".to_string()),
        "恢复出厂后应回到智能档的原始表现，实际 {back:?}"
    );
}

/// 词组不给标记：「常用」是**字**级属性，给词组存覆盖，读端逐字判定时永远看不到它。
#[test]
fn phrases_are_not_markable() {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：build_dev/data 缺失");
        return;
    }
    let c = coord("phrase");

    // ggg = 五笔「王王王」类多字词区；找一条真正的多字候选来断言。
    let list = cands_of(&c, "wgg");
    let multi = list.iter().position(|t| t.chars().count() > 1);
    if let Some(i) = multi {
        assert_eq!(
            c.debug_common_char_mark(i),
            None,
            "多字候选 {:?} 不该给标记项",
            list[i]
        );
    }
    // 单字候选则必须给。
    let single = list
        .iter()
        .position(|t| t.chars().count() == 1)
        .expect("wgg 应有单字候选");
    assert!(
        c.debug_common_char_mark(single).is_some(),
        "单字候选 {:?} 应可标记",
        list[single]
    );
}

/// 覆盖在**重启后**仍然生效——装载走 `build`，而不是只在 `new()` 里。
///
/// 这条钉的是回灌落点：若装载写在 `new()` 里，`new_headless_with_store` 这条路
/// （直接走 build）就恒看不到已存在的覆盖，而那正是本测试模拟的「重启」。
#[test]
fn overrides_survive_restart() {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：build_dev/data 缺失");
        return;
    }
    let st = store("restart");
    st.set_common_char_override('档', false).unwrap();

    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into()];
    cfg.schema.active = "wubi86".into();
    cfg.input.default.chinese_mode = true;
    cfg.input.filter_mode = "smart".into();
    // 全新协调器，模拟重启：覆盖已在库里，必须在构造期被装载。
    let c = Coordinator::new_headless_with_store(cfg, Some(&d), Arc::clone(&st));

    let list = cands_of(&c, "sivg");
    assert!(
        list.contains(&"桜".to_string()),
        "重启后覆盖应照旧生效（「档」判生僻 ⇒ 放行「桜」），实际 {list:?}"
    );
}
