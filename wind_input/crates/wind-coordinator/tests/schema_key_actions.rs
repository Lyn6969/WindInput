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

/// 造一个**自带 `zz` 开头编码**的临时方案数据目录。
///
/// 为什么不能用 build_dev/data 的 wubi86：真机上 `has_code_prefix("z")` 恒真是靠
/// `system.phrases.toml` 那 37 条 `zz*` 标点短语，而短语层要经 redb 建立，测试里
/// `store` 是 None、短语层为空 → z 成了死码，首键直接进模式，**根本走不到夺取路径**。
/// 这里改用码表自带 `zz` 编码来复现「z 是活码前缀」，与真机同构。
fn make_data_dir_with_z_code(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wind_ka_data_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    let schemas = dir.join("schemas");
    std::fs::create_dir_all(schemas.join("zt")).unwrap();
    std::fs::write(
        schemas.join("zt.schema.toml"),
        "[schema]\nid = \"zt\"\nname = \"Z测试\"\n\
         [engine]\ntype = \"codetable\"\n\
         [engine.codetable]\nmax_code_length = 4\n\
         [[dictionaries]]\nid = \"main\"\npath = \"zt/zt.dict.yaml\"\ndefault = true\n",
    )
    .unwrap();
    // rime .dict.yaml：`---` 头之后是 `文本\t编码`。zz* 模拟系统短语占位，a 保证词库非空可用。
    std::fs::write(
        schemas.join("zt/zt.dict.yaml"),
        "---\nname: zt\nversion: \"1\"\n...\n阿\ta\n甲\tzzbd\n乙\tzzsz\n",
    )
    .unwrap();
    dir
}

fn cfg_for_z_schema() -> Config {
    let mut c = Config::default();
    c.schema.available = vec!["zt".into()];
    c.schema.active = "zt".into();
    c.input.default.chinese_mode = true;
    c
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

/// z 夺取回路推广到 mix：首键让位（保住 `zz*` 系统短语），下一键破前缀时夺取进快捷输入。
///
/// 本项目 `system.phrases.toml` 出厂带 37 条 `zz*` 标点短语，`has_code_prefix("z")` 恒真，
/// 故首键 z **必然**被活码判据让位。不补这条夺取回路，`z = "mix:…"` 配了也永不生效。
#[test]
fn z_fallback_hijacks_into_mix() {
    let dd = make_data_dir_with_z_code("zmix");
    let ov = make_override("zmix", "zt", "z = \"mix:quick_mix\"");
    let coord =
        Coordinator::new_headless_with_override(cfg_for_z_schema(), Some(&dd), Some(ov.clone()));

    coord.handle_key_event(&key(VK_Z));
    assert_eq!(
        coord.debug_active_mode(),
        None,
        "首键 z 应让位（zz* 使 z 成活码前缀），否则下面测的就不是夺取路径了"
    );
    // r：zr 不是任何编码的前缀 → 破前缀，夺取。
    coord.handle_key_event(&key(0x52));
    assert_eq!(
        coord.debug_active_mode(),
        Some("mix"),
        "z + 破前缀字母应夺取进 mix"
    );
    let _ = std::fs::remove_dir_all(&ov);
    let _ = std::fs::remove_dir_all(&dd);
}

/// ★ 对照组：`zz` 仍走活码路径，**不**夺取——出厂那 37 条 `zz*` 标点短语必须照打。
///
/// 没有这一条，上面那个用例即便在「z 无条件夺取」的错误实现下也会绿，而那种实现会把
/// 所有用户的系统短语废掉。
#[test]
fn z_fallback_keeps_zz_system_phrases() {
    let dd = make_data_dir_with_z_code("zz");
    let ov = make_override("zz", "zt", "z = \"mix:quick_mix\"");
    let coord =
        Coordinator::new_headless_with_override(cfg_for_z_schema(), Some(&dd), Some(ov.clone()));

    coord.handle_key_event(&key(VK_Z));
    coord.handle_key_event(&key(VK_Z)); // zz —— 仍是活码前缀（zzbd/zzsz）
    assert_eq!(
        coord.debug_active_mode(),
        None,
        "zz 是 zzbd/zzsz 的前缀，必须留在正常输入流，不能被夺取（真机上对应那 37 条系统标点短语）"
    );
    let _ = std::fs::remove_dir_all(&ov);
    let _ = std::fs::remove_dir_all(&dd);
}

/// 夺取后退到边界再退格 → 还原正常码流，不是停在半残的模式里。
#[test]
fn z_fallback_into_mix_can_rewind() {
    let dd = make_data_dir_with_z_code("zrw");
    let ov = make_override("zrw", "zt", "z = \"mix:quick_mix\"");
    let coord =
        Coordinator::new_headless_with_override(cfg_for_z_schema(), Some(&dd), Some(ov.clone()));

    coord.handle_key_event(&key(VK_Z));
    coord.handle_key_event(&key(0x52)); // zr → 夺取进 mix，残余 "r"
    assert_eq!(coord.debug_active_mode(), Some("mix"));

    coord.handle_key_event(&key(0x08)); // Backspace：退到夺取边界
    assert_eq!(
        coord.debug_active_mode(),
        None,
        "退到边界应撤销夺取、回到正常码表输入流（active_hijack_buffer 与 rewind_hijack 都要认得 mix）"
    );
    let _ = std::fs::remove_dir_all(&ov);
    let _ = std::fs::remove_dir_all(&dd);
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

// ──────────────── 四期：修饰键 keyup 通路 + C 类 toggle_schema ────────────────

const VK_RSHIFT: u32 = 0xA1;

/// 修饰键的 keyup 事件（TSF 只在「干净单击」后转发这类事件，见 KeyEventSink.cpp）。
fn key_up(vk: u32) -> KeyEventData {
    KeyEventData {
        key_code: vk,
        scan_code: 0,
        modifiers: 0,
        event_type: wind_ipc::protocol::EVENT_KEY_UP,
        toggles: 0,
        event_seq: 0,
        prev_char: 0,
    }
}

/// C 类 `toggle_schema` 的往返：五笔按右 Shift 去拼音，再按回五笔。
///
/// ★ 回程**不要求目标方案配对称的绑定**——本例 pinyin 的 override 里没有任何
/// `key_actions`，回程仍然成立。这正是方案级只收 `toggle_schema`、不收单向
/// `switch_schema` 的理由：后者会把用户锁在目标方案里（见设计文档 §5）。
#[test]
fn toggle_schema_on_modifier_round_trips() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let ov = make_override("tgl_rt", "wubi86", "rshift = \"toggle_schema:pinyin\"");
    let coord = Coordinator::new_headless_with_override(
        cfg_for("wubi86"),
        Some(&data_dir()),
        Some(ov.clone()),
    );
    assert_eq!(coord.active_schema_id(), "wubi86");

    coord.handle_key_event(&key_up(VK_RSHIFT));
    assert_eq!(coord.active_schema_id(), "pinyin", "右 Shift 应切到 pinyin");

    // 回程靠运行时来源，与 pinyin 有没有配 rshift 无关。
    coord.handle_key_event(&key_up(VK_RSHIFT));
    assert_eq!(coord.active_schema_id(), "wubi86", "再按应回到来源方案");

    let _ = std::fs::remove_dir_all(&ov);
}

/// 修饰键的绑定必须进 `key_up` 转发集，否则 TSF 压根不发这个 keyup ——
/// 绑定在配置里躺着但永远不触发，是「配了不生效」里最难查的一种。
///
/// 断言的是**推给 C++ 的白名单**，不是内部结构：这是可达性的唯一来源。
#[test]
fn modifier_binding_enters_key_up_forward_set() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let ov = make_override("tgl_fwd", "wubi86", "rshift = \"toggle_schema:pinyin\"");
    let coord = Coordinator::new_headless_with_override(
        cfg_for("wubi86"),
        Some(&data_dir()),
        Some(ov.clone()),
    );
    let hashes = coord.debug_key_up_hotkeys();
    assert!(
        hashes.iter().any(|h| (h & 0xFFFF) == VK_RSHIFT),
        "rshift 应在 key_up 转发集里，实际: {hashes:?}"
    );
    let _ = std::fs::remove_dir_all(&ov);
}

/// 转发集取**所有方案的并集**：即便当前活跃的是 pinyin，wubi86 里绑的 rshift
/// 也要在集合里。按活跃方案裁剪就得在每次切方案后重推白名单，漏一次的表现是
/// 「刚切完方案这个键不灵、点下别的窗口又灵了」。
#[test]
fn key_up_forward_set_is_union_across_schemas() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let ov = make_override("tgl_union", "wubi86", "rshift = \"toggle_schema:pinyin\"");
    // 活跃方案是 pinyin，它自己没配任何 key_actions。
    let coord = Coordinator::new_headless_with_override(
        cfg_for("pinyin"),
        Some(&data_dir()),
        Some(ov.clone()),
    );
    let hashes = coord.debug_key_up_hotkeys();
    assert!(
        hashes.iter().any(|h| (h & 0xFFFF) == VK_RSHIFT),
        "别的方案绑的修饰键也要在转发集里，实际: {hashes:?}"
    );
    // 但**不动作**：活跃方案没绑，keyup 落回全局链。
    assert_eq!(coord.active_schema_id(), "pinyin");
    coord.handle_key_event(&key_up(VK_RSHIFT));
    assert_eq!(
        coord.active_schema_id(),
        "pinyin",
        "活跃方案没绑该键，不应切方案"
    );
    let _ = std::fs::remove_dir_all(&ov);
}

/// `toggle_schema` 绑在**有字符的键**上不生效（core 侧忽略 + warn）。
///
/// 不是遗漏：它必须在英文模式下也按得动（否则切到英文方案就回不来），而有字符的键
/// 走的 keydown 链在英文模式分水岭之后。设置页对这个组合给行内提示。
#[test]
fn toggle_schema_ignored_on_character_key() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let ov = make_override("tgl_char", "wubi86", "backslash = \"toggle_schema:pinyin\"");
    let coord = Coordinator::new_headless_with_override(
        cfg_for("wubi86"),
        Some(&data_dir()),
        Some(ov.clone()),
    );
    coord.handle_key_event(&key(0xDC)); // backslash
    assert_eq!(
        coord.active_schema_id(),
        "wubi86",
        "有字符的键上的 toggle_schema 应被忽略"
    );
    let _ = std::fs::remove_dir_all(&ov);
}

/// ★ 回程记录**用掉即失效**：连按三次是「去 → 回 → 再去」，不是在两边反复横跳时
/// 拿陈旧记录乱送。
///
/// 第三次按下时活跃方案已回到 wubi86，该方案里 rshift **有**绑定，故走的是正常去程；
/// 若回程记录没被 take 掉，这一次会被当成回程处理。
#[test]
fn return_authorization_is_consumed_once() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let ov = make_override("tgl_once", "wubi86", "rshift = \"toggle_schema:pinyin\"");
    let coord = Coordinator::new_headless_with_override(
        cfg_for("wubi86"),
        Some(&data_dir()),
        Some(ov.clone()),
    );

    coord.handle_key_event(&key_up(VK_RSHIFT));
    assert_eq!(coord.active_schema_id(), "pinyin", "第一次：去程");
    coord.handle_key_event(&key_up(VK_RSHIFT));
    assert_eq!(coord.active_schema_id(), "wubi86", "第二次：回程");
    coord.handle_key_event(&key_up(VK_RSHIFT));
    assert_eq!(
        coord.active_schema_id(),
        "pinyin",
        "第三次应是新的去程，而非拿用掉的记录再回一次"
    );
    let _ = std::fs::remove_dir_all(&ov);
}

// ──────────────── 五期：A 类状态切换 ────────────────

/// A 类绑在**有字符键**上：中文态按下即切换标点，不需要修饰键。
///
/// 与 C 类刻意不同——`toggle_punct` 本就只在中文态有意义（全局那份也带
/// CHINESE_ONLY），不存在「切过去回不来」的问题，故 keydown 路径可用。
#[test]
fn dispatch_action_on_character_key_toggles_punct() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let ov = make_override("act_punct", "wubi86", "backslash = \"toggle_punct\"");
    let coord = Coordinator::new_headless_with_override(
        cfg_for("wubi86"),
        Some(&data_dir()),
        Some(ov.clone()),
    );
    let before = coord.is_chinese_punct();
    coord.handle_key_event(&key(0xDC)); // backslash
    assert_ne!(
        coord.is_chinese_punct(),
        before,
        "绑在 backslash 上的 toggle_punct 应切换中英标点"
    );
    let _ = std::fs::remove_dir_all(&ov);
}

/// ★ A 类里「用来离开英文态」的那几个（`toggle_mode` / `switch_engine`）**限修饰键**。
///
/// 绑在有字符的键上是单程票：那条 keydown 链在英文模式分水岭之后，切到英文态就
/// 再也按不动了。core 侧忽略并 warn，判据见 `BoundAction::requires_modifier_key`。
#[test]
fn toggle_mode_ignored_on_character_key() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let ov = make_override("act_mode_char", "wubi86", "backslash = \"toggle_mode\"");
    let coord = Coordinator::new_headless_with_override(
        cfg_for("wubi86"),
        Some(&data_dir()),
        Some(ov.clone()),
    );
    assert!(coord.is_chinese_mode());
    coord.handle_key_event(&key(0xDC));
    assert!(
        coord.is_chinese_mode(),
        "有字符键上的 toggle_mode 应被忽略（否则切到英文就回不来）"
    );
    let _ = std::fs::remove_dir_all(&ov);
}

/// 同一个动作绑到**修饰键**上则生效，且能来回切——这正是「限修饰键」要保住的能力。
#[test]
fn toggle_mode_works_on_modifier_key() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let ov = make_override("act_mode_mod", "wubi86", "rshift = \"toggle_mode\"");
    let coord = Coordinator::new_headless_with_override(
        cfg_for("wubi86"),
        Some(&data_dir()),
        Some(ov.clone()),
    );
    assert!(coord.is_chinese_mode());
    coord.handle_key_event(&key_up(VK_RSHIFT));
    assert!(!coord.is_chinese_mode(), "右 Shift 应切到英文");
    // 回程：英文态下同一个键仍走 keyup 路径，按得动。
    coord.handle_key_event(&key_up(VK_RSHIFT));
    assert!(coord.is_chinese_mode(), "再按应切回中文");
    let _ = std::fs::remove_dir_all(&ov);
}

/// 缓冲非空时 A 类不接管：打字打到一半按下绑定键，意图多半是输入而非切状态。
#[test]
fn dispatch_action_yields_while_typing() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let ov = make_override("act_typing", "wubi86", "backslash = \"toggle_punct\"");
    let coord = Coordinator::new_headless_with_override(
        cfg_for("wubi86"),
        Some(&data_dir()),
        Some(ov.clone()),
    );
    coord.handle_key_event(&key(0x41)); // a：缓冲非空
    let before = coord.is_chinese_punct();
    coord.handle_key_event(&key(0xDC));
    assert_eq!(
        coord.is_chinese_punct(),
        before,
        "缓冲非空时不该被 A 类接管"
    );
    let _ = std::fs::remove_dir_all(&ov);
}
