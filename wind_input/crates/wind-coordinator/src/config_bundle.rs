//! [`ConfigBundle`]：配置 + 其轻量派生缓存的不可变快照，运行时整体原子替换以支持热重载。
//!
//! `build` 是**所有**配置生效的必经之路（启动、热重载、RPC 改配置、测试直接构造）。
//! （自 coordinator.rs 平移，纯搬运。）

use tracing::warn;
use wind_config::Config;
use wind_config::hotkey::{self, CompiledHotkeys};
use wind_engine::EngineManager;
use wind_keys::keymap;

/// 解析配对表（每项 2 字符 "（）"）为 (左,右) 字符对，忽略非法项。
pub(crate) fn parse_pairs(list: &[String]) -> Vec<(char, char)> {
    list.iter()
        .filter_map(|s| {
            let mut it = s.chars();
            match (it.next(), it.next(), it.next()) {
                (Some(l), Some(r), None) => Some((l, r)),
                _ => None,
            }
        })
        .collect()
}

/// 解析配对跳出键名 → VK 码集合。支持 tab / enter(return) / space / escape(esc)；
/// 大小写与首尾空白不敏感，未知名忽略。这些非可打印键不在 keymap 的 KEY_TABLE
/// （引导/触发用的 OEM 符号键）内，故在此单独映射。
pub(crate) fn parse_jump_out_keys(list: &[String]) -> std::collections::HashSet<u32> {
    list.iter()
        .filter_map(|s| match s.trim().to_lowercase().as_str() {
            "tab" => Some(keymap::VK_TAB),
            "enter" | "return" => Some(keymap::VK_RETURN),
            "space" => Some(keymap::VK_SPACE),
            "escape" | "esc" => Some(keymap::VK_ESCAPE),
            // `right_symbol` 不是键名（右符号是哪个键取决于配对表），由
            // `parse_jump_out_on_right_symbol` 单独解析成开关。
            _ => None,
        })
        .collect()
}

/// `jump_out_keys` 是否含「右符号键本身」这一特殊值 → 打 `）` 跳出已插入的 `（）`。
/// 与 VK 集合分开表示：右符号不是固定按键，取决于当前生效的配对表。
pub(crate) fn parse_jump_out_on_right_symbol(list: &[String]) -> bool {
    list.iter()
        .any(|s| s.trim().to_lowercase() == wind_config::config::JUMP_OUT_RIGHT_SYMBOL)
}

/// 配置 + 其轻量派生缓存的不可变快照；运行时整体原子替换以支持热重载。
/// 重型组件（引擎/方案/词典）不在内，仍需重启才能完全切换。
pub(crate) struct ConfigBundle {
    pub(crate) config: Config,
    pub(crate) compiled_hotkeys: CompiledHotkeys,
    /// 会话态按键绑定（`keys.session_actions` 编译一次）。**不只是导航**——二期起还装
    /// `cancel`，故不叫 `nav_keys`。动作值域在 `wind-config`，表在 `wind-keys`，两者由
    /// 本结构体所在的 crate 拼起来（唯一同时看得见两边的地方）。
    pub(crate) session_keys: keymap::KeyBinds<wind_config::SessionAction>,
    pub(crate) cn_pairs: Vec<(char, char)>,
    pub(crate) en_pairs: Vec<(char, char)>,
    /// 配对跳出键的 VK 码集合（预解析自 `auto_pair.jump_out_keys`，空=不启用）。
    pub(crate) jump_out_keys: std::collections::HashSet<u32>,
    /// 输入右符号本身是否跳出（`jump_out_keys` 含 `right_symbol`）。对称配对不受此项影响。
    pub(crate) jump_out_on_right_symbol: bool,
    /// 「英半列有自定义标点映射」的源字符集合（预解析自 `punct.custom_mappings`，空=英文模式
    /// 行为与历史一致）。这是 DLL 吃键与本侧出字的**同源判据**，且在英文标点键的热路径上每键
    /// 都要查——故预计算，别在按键时重新遍历 `custom_mappings`。有序集合使推送字节可复现。
    pub(crate) custom_en_punct_chars: std::collections::BTreeSet<char>,
}

/// 所有方案 `[key_actions]` 里绑过的纯修饰键 VK（并集）。
///
/// 取并集而非活跃方案那一份：`CompiledHotkeys` 随 activation 推给 C++，按活跃方案裁剪
/// 就得在每次切方案后重推，漏一次的表现是「刚切完方案这个键不灵、点下别的窗口又灵了」。
/// 并集是静态的，代价只是别的方案里多转发一个不动作的 keyup（keydown 侧纯修饰键一律
/// 放行，宿主无感）。理由详见 [`EngineManager::all_key_action_keys`]。
pub(crate) fn schema_bound_modifier_vks(mgr: &EngineManager) -> std::collections::BTreeSet<u32> {
    mgr.all_key_action_keys()
        .iter()
        .filter_map(|name| keymap::modifier_name_to_vk(name))
        .collect()
}

/// 加载期告警：`keys.session_actions` 里认不出的键名 / 动词。
///
/// ★ 静默忽略与「这个功能坏了」完全同形——用户无从分辨自己拼错了、还是该功能压根没实现。
/// 这是 `is_supported_key_action` 当初立的口径，本表沿用。
///
/// 分两条报而不是合并成一条：键名错与动词错的修法不同，合并后用户还要自己二选一去试。
fn warn_unknown_session_actions(config: &Config) {
    for (name, verb) in &config.keys.session_actions {
        if wind_config::SessionAction::parse_checked(verb).is_none() {
            warn!(
                "keys.session_actions[\"{name}\"] = \"{verb}\"：动词无法识别，该绑定被忽略。\
                 可选 page_prev / page_next / highlight_up / highlight_down / none",
            );
            continue;
        }
        if keymap::session_key_name_to_vk(name).is_none() {
            warn!(
                "keys.session_actions[\"{name}\"]：键名无法识别，该绑定被忽略。\
                 可选 tab / shift+tab / capslock / pageup / pagedown / up / down / left / \
                 right / home / end，以及符号键 minus / equal / lbracket / rbracket / \
                 comma / period / semicolon / quote / slash / backtick / backslash",
            );
        }
    }
}

impl ConfigBundle {
    /// `schema_bound_modifiers` = 所有方案 `[key_actions]` 里出现过的**纯修饰键** VK
    /// （见 [`Coordinator::schema_bound_modifier_vks`]）。它们要追加进 `key_up` 转发集，
    /// 否则 TSF 根本不把这些键的 keyup 送过来——`CompiledHotkeys` 编译自全局 config，
    /// 方案文件不在其中，这是 keyup 类绑定唯一的可达性来源。
    pub(crate) fn build(
        mut config: Config,
        schema_bound_modifiers: &std::collections::BTreeSet<u32>,
    ) -> Self {
        // 归一化 + 存量迁移。放在这里而不是只在 `Config::load()` 里：本函数是**所有**
        // 配置生效的必经之路（启动、热重载、RPC 改配置后的 `refresh_config_in_memory`、
        // 测试直接构造）。挂在 load 上会漏掉后三条——设置页保存一次就绕过了迁移，
        // 而消费点已改成只读新表，表现是「保存后引导键全失效」。`normalize` 幂等。
        config.normalize();
        let mut compiled_hotkeys = hotkey::Compiler::new(config.clone()).compile();
        // action 用专门的 `schema_bound` 而不是 `toggle_mode`：`is_toggle_mode_keycode` 按
        // action 过滤，混用会让「只在某方案里绑了 rshift」的键在所有方案里都切中英文
        // （与 `select_key_groups` 那次踩的是同一个坑，见该函数的 ⚠ 注释）。
        for vk in schema_bound_modifiers {
            // 修饰键的 hash 要带通用位+具体位，与 `compile_toggle_mode_key` 同构：
            // C++ `GetCurrentModifiers()` 对修饰键同时返回两者，只带一边匹配不上。
            if let Some(hash) = hotkey::compile_modifier_key_up_hash(*vk) {
                compiled_hotkeys.key_up.push(hotkey::HotkeyEntry {
                    tsf_hash: hash,
                    match_hash: hash,
                    action: "schema_bound".to_string(),
                });
            }
        }
        warn_unknown_session_actions(&config);
        // 会话态按键绑定。数据源是 `effective_session_actions()`＝四组键组配置的展开结果
        // ⊕ `session_actions`（后者优先）。
        //
        // ★ 合并只在这里发生，**配置文件里两套各自保持原样**——设置页的四个勾选框读的正是
        // 存储层，折算若写回存储，界面就永远显示为空。判据见该函数的文档。
        //
        // ★ 这里是两个 crate 的接缝：动作值域（`SessionAction`）在 `wind-config`，绑定表
        // （`KeyBinds`）在 `wind-keys`，而 `wind-config` 不能反向依赖 `wind-keys`（后者经
        // `wind-cmdbar` 依赖它，加进去成环）。本函数是唯一同时看得见两者的地方。
        //
        // 表**直接持有 `SessionAction`**，不再翻译成某个中间枚举——一期那层 `NavAction`
        // 映射在加 `cancel` 时立刻成了瓶颈（新动词没有对应的 `NavAction`）。
        // 显式 `none` 与写错的动词都在此过滤掉；后者由上一行的 `warn_unknown_session_actions`
        // 报出来，静默忽略与「功能坏了」完全同形。
        let effective_session = config.keys.effective_session_actions();
        let session_keys =
            keymap::KeyBinds::from_binds(effective_session.iter().filter_map(|(name, verb)| {
                let action = wind_config::SessionAction::parse(verb);
                action.is_enabled().then_some((name.as_str(), action))
            }));
        let cn_pairs = parse_pairs(&config.input.auto_pair.chinese_pairs);
        let en_pairs = parse_pairs(&config.input.auto_pair.english_pairs);
        let jump_out_keys = parse_jump_out_keys(&config.input.auto_pair.jump_out_keys);
        let jump_out_on_right_symbol =
            parse_jump_out_on_right_symbol(&config.input.auto_pair.jump_out_keys);
        // 英文模式下需要 DLL 吃下转发的标点键 = 「配了英半列自定义」∪「英文智能符号参与集」。
        // 两个来源都是「英文半角下 DLL 默认透传、core 却需要收到」的键，合并成一份推送即可
        // （DLL 侧判据是数据驱动的字符集查表，集合变大自动多吃，无需改 C++）。
        let custom_en_punct_chars: std::collections::BTreeSet<char> =
            wind_punct::custom_english_punct_chars(&config.input)
                .into_iter()
                .chain(wind_punct::english_smart_source_chars(&config.input))
                .collect();
        Self {
            config,
            compiled_hotkeys,
            session_keys,
            cn_pairs,
            en_pairs,
            jump_out_keys,
            jump_out_on_right_symbol,
            custom_en_punct_chars,
        }
    }
}

#[cfg(test)]
mod reload_tests {
    //! 热重载基础：验证 ConfigBundle 能从 Config 正确重建轻量派生缓存。
    //! （reload_user_config 走磁盘 IO 不在此测；这里测其核心——从配置重建派生状态。）
    use super::*;

    #[test]
    fn config_bundle_rebuilds_pairs_from_config() {
        let mut cfg = Config::default();
        cfg.input.auto_pair.chinese_pairs = vec!["（）".to_string(), "【】".to_string()];
        cfg.input.auto_pair.english_pairs = vec!["()".to_string()];
        let b = ConfigBundle::build(cfg, &Default::default());
        assert_eq!(b.cn_pairs, vec![('（', '）'), ('【', '】')]);
        assert_eq!(b.en_pairs, vec![('(', ')')]);
    }

    #[test]
    fn parse_jump_out_keys_maps_names_to_vk() {
        // 支持的键名（大小写/空白不敏感），未知名忽略。
        let set = parse_jump_out_keys(&[
            " Tab ".into(),
            "ENTER".into(),
            "space".into(),
            "esc".into(),
            "unknown".into(),
        ]);
        assert!(set.contains(&keymap::VK_TAB));
        assert!(set.contains(&keymap::VK_RETURN)); // enter → VK_RETURN
        assert!(set.contains(&keymap::VK_SPACE));
        assert!(set.contains(&keymap::VK_ESCAPE)); // esc → VK_ESCAPE
        assert_eq!(set.len(), 4); // "unknown" 被忽略
        // "return" 别名等价 enter
        assert!(parse_jump_out_keys(&["return".into()]).contains(&keymap::VK_RETURN));
        // 空配置 → 空集（不启用）
        assert!(parse_jump_out_keys(&[]).is_empty());
    }

    #[test]
    fn config_bundle_parses_jump_out_keys() {
        let mut cfg = Config::default();
        cfg.input.auto_pair.jump_out_keys = vec!["tab".into(), "enter".into()];
        let b = ConfigBundle::build(cfg, &Default::default());
        assert!(b.jump_out_keys.contains(&keymap::VK_TAB));
        assert!(b.jump_out_keys.contains(&keymap::VK_RETURN));
        assert_eq!(b.jump_out_keys.len(), 2);
    }

    #[test]
    fn config_bundle_carries_config_values() {
        // 改配置 → 重建 bundle → bundle.config 反映新值（热重载替换后读取生效的基础）。
        let mut cfg = Config::default();
        cfg.input.symbol.smart_mode = true;
        cfg.ui.candidate.per_page = 9;
        let b = ConfigBundle::build(cfg, &Default::default());
        assert!(b.config.input.symbol.smart_mode);
        assert_eq!(b.config.ui.candidate.per_page, 9);
    }
}
