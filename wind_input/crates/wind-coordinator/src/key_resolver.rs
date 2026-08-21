//! 按键解析层：把分层、分作用域的按键配置解析成消费点能直接用的单一视图。
//!
//! 设计见 `docs/design/key-resolver-unification.md`。本模块是该文档 §7 第 1 步的落点。
//!
//! ## 为什么这层必须在 `wind-coordinator`
//!
//! 动作值域（[`BoundAction`]）在 `wind-config`，键名→VK 的解析（`keymap`）在 `wind-keys`，
//! 而 `wind-config` **不能**反向依赖 `wind-keys`（后者经 `wind-cmdbar` 依赖它，加进去成环）。
//! `wind-engine` 也不依赖 `wind-keys`，故方案级表的 VK 预编译同样落不到 `EngineManager`。
//! 本 crate 是唯一同时看得见两边的地方——与 `ConfigBundle::session_keys` 同源的理由。
//!
//! ## 当前范围（第 1 步）
//!
//! 只收**全局层**（`keys.key_actions`）。方案级 `[key_actions]` 仍留在
//! `Coordinator::bound_action_with_source`：它按活跃方案查表，需要 `EngineManager`。
//!
//! ⚠️ **方案层不在这里加缓存**。`EngineManager` 已有 `key_actions_cache`（随
//! `invalidate_schema` 失效）；在其上再叠一层 VK 预编译，就多出一个必须同步失效的镜像态。
//! 现成的 `schema_generation` **不能**当失效判据——它只在**活跃方案改变**时 +1，
//! 而设置页改 `schema_overrides` 走的是 `invalidate_schema`，不 bump 它。用它做判据的表现
//! 是「设置页改了不生效、重启才生效」，本仓已有同型教训。要做方案层预编译，得先给
//! `EngineManager` 加一个在 `invalidate_schema` 里 bump 的独立代际。

use std::collections::HashMap;

use wind_config::hotkey::{KeyActionRoute, route_of_key_action};
use wind_config::{BoundAction, Config};
use wind_keys::keymap;

/// 分层按键配置的解析器。
///
/// 目标形态是 `lead(schema, vk)` / `session(schema, vk)` / `reachability()` 三个方法同源，
/// 当前只实现了 [`Self::global_lead`]（见模块文档「当前范围」）。
pub(crate) struct KeyResolver {
    /// 全局 `keys.key_actions` 的引导键条目，**预编译**：键名已解析成 VK、动词已解析成
    /// [`BoundAction`]。
    ///
    /// 值含 [`BoundAction::None`]（用户显式写的 `none`）：查得到就是「全局层表了态」，
    /// 表态为禁用同样**不再往下回落**到 `z_key_action`。故本表不能用「查不到」表示禁用。
    global_lead: HashMap<u32, BoundAction>,
}

impl KeyResolver {
    /// 从配置预编译。随 `ConfigBundle` 重建（热重载、RPC 改配置、测试构造都经它）。
    pub(crate) fn build(config: &Config) -> Self {
        Self {
            global_lead: compile_global_lead(&config.keys.key_actions),
        }
    }

    /// 全局 `keys.key_actions` 里这个键绑了什么。
    ///
    /// 只含**单键**条目（引导键 + 纯修饰键）：组合键由热键通路消费，在这里再认一次
    /// 就是同一个键两条路都触发。过滤在预编译期完成。
    pub(crate) fn global_lead(&self, key_code: u32) -> Option<BoundAction> {
        self.global_lead.get(&key_code).cloned()
    }
}

/// 键名 → VK。两张表叠加，缺一不可。
///
/// 修饰键的键名**不在** `KEY_TABLE` 里（那是引导键的解析口，走 keydown，修饰键在那条路上
/// 不工作），故必须显式并进来。少了这一层的表现是「转发集里有这个键、TSF 也发了 keyup，
/// 但查表查不到、什么都不发生」——已在测试里复现过一次。
pub(crate) fn key_action_name_to_vk(name: &str) -> Option<u32> {
    keymap::key_name_to_vk_with_letters(name).or_else(|| keymap::modifier_name_to_vk(name))
}

/// 把 `keys.key_actions` 编译成 VK 表。
///
/// ★ **先到先得，与折算前的线性遍历同序**：源表是 `BTreeMap`，遍历即按键名字典序，
/// 原实现用 `for` + 首个 VK 命中即 `return`。两个不同键名解析到同一个 VK 时（如大小写
/// 写法不一，`key_name_to_vk_with_letters` 走 `to_lowercase`），必须仍由字典序在前的那条
/// 获胜——故撞键时 `continue` 保留先到者，**不是**后写覆盖。
/// 直接 `insert` 会让胜者反转，且静默：这是「行为不变的重构」里最容易混进来的语义变更。
fn compile_global_lead(
    table: &std::collections::BTreeMap<String, String>,
) -> HashMap<u32, BoundAction> {
    let mut out: HashMap<u32, BoundAction> = HashMap::new();
    // 撞键时报出来：分散查表时「谁赢」是遍历顺序的副产物，没有任何一处代码有资格报错；
    // 收敛成单表后构建期天然拿到全集，冲突检测从「要额外实现」变成「顺手就有」。
    // 判据同 `warn_unknown_session_actions`——静默忽略与「这个功能坏了」完全同形。
    let mut winners: HashMap<u32, &str> = HashMap::new();
    for (name, action) in table {
        // 只收单键。组合键走热键通路（`Compiler::compile` 已按形态分流）。
        if !matches!(
            route_of_key_action(name),
            Some(KeyActionRoute::LeadingKey) | Some(KeyActionRoute::ModifierKeyUp)
        ) {
            continue;
        }
        let Some(vk) = key_action_name_to_vk(name) else {
            continue;
        };
        if let Some(prev) = winners.get(&vk) {
            tracing::warn!(
                "key_actions: 键名 {name:?} 与 {prev:?} 解析到同一个键（vk=0x{vk:02X}），\
                 只有 {prev:?} 生效。删掉其中一条以消除歧义。"
            );
            continue;
        }
        winners.insert(vk, name.as_str());
        out.insert(vk, BoundAction::parse(action));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 单键进表、组合键不进表——两条通路的分流在预编译期就该完成。
    #[test]
    fn only_single_keys_are_compiled() {
        let t = compile_global_lead(&table(&[
            ("backtick", "temp_pinyin"),
            ("ctrl+alt+e", "temp_english"),
        ]));
        let backtick = key_action_name_to_vk("backtick").expect("backtick 应能解析");
        assert_eq!(t.get(&backtick), Some(&BoundAction::TempPinyin));
        assert_eq!(t.len(), 1, "组合键不该进引导键表：{t:?}");
    }

    /// 纯修饰键必须进表——它的键名不在 `KEY_TABLE` 里，只能靠 `modifier_name_to_vk`。
    /// 少这一层的表现是「TSF 发了 keyup 但查表查不到」。
    #[test]
    fn modifier_keys_are_compiled() {
        let t = compile_global_lead(&table(&[("rshift", "toggle_schema:english")]));
        let rshift = keymap::modifier_name_to_vk("rshift").expect("rshift 应能解析");
        assert_eq!(
            t.get(&rshift),
            Some(&BoundAction::ToggleSchema("english".to_string()))
        );
    }

    /// ★ 显式 `none` 必须**留在表里**而不是被过滤掉：它表示「全局层表了态：禁用」，
    /// 查得到才能阻止往下回落到 `z_key_action`。若过滤掉，配了 `z = "none"` 的用户
    /// 会落回 z 专用字段，等于开关失灵。
    #[test]
    fn explicit_none_stays_in_table() {
        let t = compile_global_lead(&table(&[("z", "none")]));
        let z = key_action_name_to_vk("z").expect("z 应能解析");
        assert_eq!(t.get(&z), Some(&BoundAction::None), "显式 none 不能被丢掉");
    }

    /// ★ 撞键时由**字典序在前**者获胜，与折算前的 `for` + 首个命中即 return 同序。
    /// 用 `insert` 代替 `or_insert` 会让后者覆盖前者——反向且静默。
    #[test]
    fn duplicate_vk_first_wins_by_name_order() {
        // BTreeMap 里 "Z" < "z"（大写 ASCII 在前），而键名解析大小写不敏感 ⇒ 两条撞同一个 VK。
        //
        // ★ 前置条件写成**断言**而不是 `if` 包住断言：后者在解析规则变成大小写敏感的那天
        // 会静默跳过、用例照绿，而它恰恰是本用例唯一想测的东西。
        let z = key_action_name_to_vk("z").expect("z 应能解析");
        assert_eq!(
            key_action_name_to_vk("Z"),
            Some(z),
            "前置条件：两种写法须撞同一个 VK，否则本用例根本测不到撞键"
        );
        let t = compile_global_lead(&table(&[("Z", "temp_pinyin"), ("z", "temp_english")]));
        assert_eq!(
            t.get(&z),
            Some(&BoundAction::TempPinyin),
            "撞键应由字典序在前的 \"Z\" 获胜（与折算前的线性遍历同序）"
        );
    }

    /// 认不出的键名跳过，不 panic 也不进表。
    #[test]
    fn unknown_key_name_is_skipped() {
        let t = compile_global_lead(&table(&[("no_such_key", "temp_pinyin")]));
        assert!(t.is_empty(), "认不出的键名不该进表：{t:?}");
    }
}
