//! 方案级 `[key_actions]` 的端到端分派测试。
//!
//! 用 `new_headless_with_override` 指定**临时** override 目录——`new_headless` 会让
//! `EngineManager` 取真实用户目录，测试写进去要污染用户配置，这个缺口曾让方案级
//! `[key_actions]` 的分派 bug 直接漏到真机上。

use std::path::PathBuf;
use wind_bridge::handler::{KeyAction, KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::EVENT_KEY_DOWN;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn has_schemas() -> bool {
    let d = data_dir();
    d.join("schemas/wubi86.schema.toml").exists() && d.join("schemas/pinyin.schema.toml").exists()
}

/// 建一个隔离的 override 目录，写入指定方案的 `[key_actions]`。
fn make_override(tag: &str, schema_id: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wind_ka_ov_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{schema_id}.toml")),
        format!("[key_actions]\n{body}\n"),
    )
    .unwrap();
    dir
}

fn cfg_for(active: &str) -> Config {
    let mut c = Config::default();
    c.schema.available = vec!["wubi86".into(), "pinyin".into()];
    c.schema.active = active.into();
    c.input.default.chinese_mode = true;
    c
}

fn key(vk: u32) -> KeyEventData {
    KeyEventData {
        key_code: vk,
        scan_code: 0,
        modifiers: 0,
        event_type: EVENT_KEY_DOWN,
        toggles: 0,
        event_seq: 0,
        prev_char: 0,
    }
}

const VK_OEM_1: u32 = 0xBA; // ;
const VK_Z: u32 = 0x5A;

/// `none`：本方案禁用该键的全局引导，既不进模式、也不回落全局 `trigger_keys`。
///
/// 现场：`;` 是 `quick_mix` 的全局触发键。方案里写 `semicolon = "none"` 后，空码按 `;`
/// 必须落普通输入（后续由标点流水线出分号），而不是进快捷输入。
#[test]
fn schema_none_blocks_global_trigger_key() {
    if !has_schemas() {
        return;
    }
    let ov = make_override("none", "wubi86", "semicolon = \"none\"");
    let mut cfg = cfg_for("wubi86");
    // 全局把 ; 配成 quick_mix 引导键（出厂即如此，这里显式写清前提）。
    cfg.schema.mix_modes[0].trigger_keys = vec!["semicolon".into()];
    let coord = Coordinator::new_headless_with_override(cfg, Some(&data_dir()), Some(ov.clone()));

    let act = coord.handle_key_event(&key(VK_OEM_1));
    // 进了 mix 会得到 UpdateComposition（组合区开前缀 ";"）；被 none 拦住则不会。
    if let KeyAction::UpdateComposition { text, .. } = &act {
        panic!("`;` 被 none 禁用后不该进快捷输入，实际开了组合区: {text:?}");
    }
    let _ = std::fs::remove_dir_all(&ov);
}

/// 对照组：不写 `none` 时，`;` 照常进快捷输入。
///
/// 没有这一条，上面那个用例在「`;` 本来就进不去」时也会绿。
#[test]
fn without_none_semicolon_still_enters_mix() {
    if !has_schemas() {
        return;
    }
    let ov = make_override("ctrl", "wubi86", "backslash = \"none\"");
    let mut cfg = cfg_for("wubi86");
    cfg.schema.mix_modes[0].trigger_keys = vec!["semicolon".into()];
    let coord = Coordinator::new_headless_with_override(cfg, Some(&data_dir()), Some(ov.clone()));

    let act = coord.handle_key_event(&key(VK_OEM_1));
    assert!(
        matches!(act, KeyAction::UpdateComposition { .. }),
        "未被 none 禁用时 `;` 应进快捷输入，实际: {act:?}"
    );
    let _ = std::fs::remove_dir_all(&ov);
}

/// `z_key_repeat` 只压得住**有夺取回路**的目标。
///
/// `temp_pinyin` 有 `try_z_fallback`：首键让位给 repeat 后，继续打字母仍会被夺取进临拼，
/// 两个功能共存。而 special / mix / 临英只支持首键进入——让位一次就是这个方案里再也进不去，
/// 尤其快符那种 `show_all_on_enter` 的模式，全部价值就在首键那一下。
///
/// 本用例先真上屏一次（喂出 repeat 历史），再按 z：绑 special 时必须照进不误。
#[test]
fn z_repeat_does_not_steal_targets_without_rescue_path() {
    if !has_schemas() {
        return;
    }
    // 目标取内置 quick_mix：它与快符同属「只支持首键进入、没有夺取回路」那一类，验证的是
    // 同一条判据。不用 special 是因为快符类方案不在 build_dev/data 里，`ensure_schema`
    // 门卫过不了，测出来的会是「方案缺失」而不是「被 repeat 抢走」——两者在结果上同形。
    let ov = make_override("zrepeat", "wubi86", "z = \"mix:quick_mix\"");
    let mut cfg = cfg_for("wubi86");
    cfg.schema.codetable.z_key_repeat = true;
    let coord = Coordinator::new_headless_with_override(cfg, Some(&data_dir()), Some(ov.clone()));

    // 先上屏一次，喂出 repeat 历史（无历史时 repeat 本就不生效，测不出让位）。
    for c in ['a', 'a'] {
        coord.handle_key_event(&key(c.to_ascii_uppercase() as u32));
    }
    coord.handle_key_event(&key(0x20)); // 空格上屏

    coord.handle_key_event(&key(VK_Z));
    // ⚠️ 判据必须是「进没进模式」，不能看 KeyAction 的形状——让位后 z 落普通输入、
    // buffer 变 "z"，返回的同样是 UpdateComposition，两种结局在那一层完全同形。
    assert_eq!(
        coord.debug_active_mode(),
        Some("mix"),
        "z 绑无夺取回路的目标时不该被 repeat 抢走：让位即这个方案里永久进不去"
    );
    let _ = std::fs::remove_dir_all(&ov);
}

/// 反向对照：绑 `temp_pinyin` 时 repeat **仍然**优先（它有 z-fallback 补救）。
///
/// 没有这条，上面那个用例在「repeat 整个失效」时也会绿。
#[test]
fn z_repeat_still_wins_for_temp_pinyin() {
    if !has_schemas() {
        return;
    }
    let ov = make_override("zrepeat_tp", "wubi86", "z = \"temp_pinyin\"");
    let mut cfg = cfg_for("wubi86");
    cfg.schema.codetable.z_key_repeat = true;
    cfg.input.temp_pinyin.enabled = true;
    let coord = Coordinator::new_headless_with_override(cfg, Some(&data_dir()), Some(ov.clone()));

    for c in ['a', 'a'] {
        coord.handle_key_event(&key(c.to_ascii_uppercase() as u32));
    }
    coord.handle_key_event(&key(0x20));

    coord.handle_key_event(&key(VK_Z));
    assert_eq!(
        coord.debug_active_mode(),
        None,
        "绑 temp_pinyin 时 repeat 仍优先，z 应落普通输入而非进模式（后续字母由 z-fallback 补救）"
    );
    let _ = std::fs::remove_dir_all(&ov);
}

/// 方案表里的 `z` 必须**压过**全局 `schema.codetable.z_key_action`。
///
/// 现场：全局配 `z_key_action = "temp_pinyin"`，方案表配 `z = "temp_english"`。
/// 按 z 应进临时英文——进了临拼就说明方案表没被优先。
#[test]
fn schema_table_overrides_global_z_key_action() {
    if !has_schemas() {
        return;
    }
    let ov = make_override("zover", "wubi86", "z = \"temp_english\"");
    let mut cfg = cfg_for("wubi86");
    cfg.schema.codetable.z_key_action = "temp_pinyin".into();
    cfg.input.temp_pinyin.enabled = true;
    cfg.input.temp_english.enabled = true;
    let coord = Coordinator::new_headless_with_override(cfg, Some(&data_dir()), Some(ov.clone()));

    let act = coord.handle_key_event(&key(VK_Z));
    assert!(
        matches!(act, KeyAction::UpdateComposition { .. }),
        "z 应进某个模式，实际: {act:?}"
    );
    // 临英缓冲吃字母原文：打 "ab" 后组合区应含 ab；临拼会把 ab 转成候选/拼音串。
    coord.handle_key_event(&key(0x41)); // a
    let act2 = coord.handle_key_event(&key(0x42)); // b
    if let KeyAction::UpdateComposition { text, .. } = &act2 {
        assert!(
            text.contains("ab"),
            "应进临时英文（缓冲存英文原文 ab），实际组合区: {text:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&ov);
}
