//! 配置片段（fragment）的解析、展平与校验——`config.previewPatch` / `config.applyPatch`
//! 的纯逻辑层。不碰文件系统：RPC 层负责取当前生效配置与经 `set_user_value` 落盘，
//! 本模块只回答「这段 TOML 拆成哪些键、每个键合不合法、应用后从什么值变成什么值」。
//!
//! 片段的键域 = `config_schema::REGISTRY` 登记键 ∪ [`ALLOWED_UNREGISTERED_KEYS`]。
//! 展平在这两类键处**停止下钻**：`StructList` 键（如 `schema.mix_modes`）整个子树就是一个
//! 配置值，切成伪键是错的语义——与 `prune_redundant` 用 `is_known_key` 把下钻子路径整体
//! 排除是同一道保险。
//!
//! **Map 键是唯一的例外，且只多下钻一层**（`keys.key_actions` / `keys.schema_hotkeys` /
//! `keys.session_actions` / `input.punct.custom_mappings`）：
//!
//! - 片段里 Map 键下的表**恒为逐条合并**（upsert：并入当前生效表，同名条目覆盖，其余保留）。
//!   片段**不能**整表替换、**不能**删条目——分发包带整表替换会清掉用户既有绑定，这正是本
//!   语义存在的理由。顺带消灭了「空表 = 清空」的脚枪：空表 = 无条目 = no-op。
//! - 条目名**不并入点分键**：`custom_mappings` 的条目名可以含 `.`（如 `"."`），拼进点分键
//!   就再也拆不回来。条目名由 [`PatchEntry::map_entry`] 独立承载，`key` 恒为父 Map 键。
//! - 落盘的最终键值由 [`writes`] 算出（父 Map 键一条，值 = 合并后整表）。
//!
//! 错误只有三类：整体 TOML 解析失败（[`parse_fragment`] 返回 `Err`，不产出任何条目）、
//! 未知配置键、类型或取值不合法（后两类落在条目的 `error` 字段，逐键/逐条目报告）。

use std::collections::HashMap;

use serde::Serialize;

use crate::config::Config;
use crate::config_schema;

/// 合法但刻意不进 REGISTRY 的配置键白名单。
///
/// 这些是 `Option<T>` 三态字段：出厂值恰是「键不存在」（`skip_serializing_if`），
/// 不出现在 `Config::default()` 的序列化键集里，登记进 REGISTRY 会被
/// `registry_covers_every_config_key` 判「多余」——见 REGISTRY 文档
/// 「三态键（`Option<T>`，默认 `None`）刻意不登记」一节。片段校验若只认登记键，
/// 用户合法手写的这些键就会被误报「未知配置键」，故在此显式列出。
///
/// 目前唯一的家族是模式级注释模板覆盖（[`crate::config::CommentTemplateOverride`]）。
/// `mix_modes` 条目内的同名字段不在此列：它们在 `schema.mix_modes`（StructList）子树内，
/// 展平时随整棵子树作一个值，不会以点分键形态出现。宁缺勿滥——拿不准的键不进名单，
/// 误报「未知」可改，误放行则绕过了 REGISTRY 这道门。
///
/// 守门测试（本文件 tests）保证名单不腐烂：每个键必须 (a) 不在 REGISTRY
/// （日后登记了就该从名单删除）、(b) 真实存在于 `Config` 结构（写入后能反序列化并原值读回）。
pub const ALLOWED_UNREGISTERED_KEYS: &[&str] = &[
    "input.temp_english.comment_template_vertical",
    "input.temp_english.comment_template_horizontal",
    "input.temp_pinyin.comment_template_vertical",
    "input.temp_pinyin.comment_template_horizontal",
    "input.url.comment_template_vertical",
    "input.url.comment_template_horizontal",
];

/// 预览条目：片段里的一个配置键及其应用效果。
///
/// Map 键的每个条目各占一条：`key` = 父 Map 键、[`Self::map_entry`] = 条目名。
#[derive(Debug, Clone, Serialize)]
pub struct PatchEntry {
    /// 点分配置键。Map 条目取**父 Map 键**（条目名不并入，见模块文档）。
    pub key: String,
    /// Map 条目名；`None` = 本条是普通标量/整值键。
    #[serde(rename = "mapEntry", skip_serializing_if = "Option::is_none")]
    pub map_entry: Option<String>,
    /// 当前生效值（按路径从传入的当前配置树取；Map 条目取表内该条目的值，
    /// 缺席 = 新增；白名单键未设置时无值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<toml::Value>,
    /// 片段给出的新值。
    pub next: toml::Value,
    /// 校验错误（未知配置键 / 类型或取值不合法）；`None` = 本条可应用。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 解析片段文本。失败即整体错误，调用方不应再走展平/校验。
pub fn parse_fragment(text: &str) -> Result<toml::Value, String> {
    toml::from_str::<toml::Value>(text).map_err(|e| format!("TOML 解析失败: {e}"))
}

/// 展平片段并逐条校验、取当前值。`current` 为当前生效配置的 TOML 值树
/// （RPC 层从 `Config::load` 序列化得到）。条目顺序 = 片段遍历顺序。
///
/// 点分键与嵌套表两种写法在 TOML 解析层就已归一为同一棵表树，本函数天然视其等价。
pub fn preview(fragment: &toml::Value, current: &toml::Value) -> Vec<PatchEntry> {
    let mut entries = Vec::new();
    flatten("", fragment, &mut entries);
    for e in &mut entries {
        // 未知键在展平期已定性，无当前值可取。
        if e.error.is_some() {
            continue;
        }
        let path: Vec<&str> = e.key.split('.').collect();
        match &e.map_entry {
            // Map 条目：校验「父键 = 只含本条目的单元素表」，当前值取表内同名条目。
            Some(name) => {
                e.error = validate_map_entry(&e.key, name, &e.next).err();
                e.current = crate::config::get_nested(current, &path)
                    .and_then(|v| v.as_table())
                    .and_then(|t| t.get(name))
                    .cloned();
            }
            None => {
                e.error = validate_value(&e.key, &e.next).err();
                e.current = crate::config::get_nested(current, &path).cloned();
            }
        }
    }
    entries
}

/// 该键是否登记为 `Map`（片段里其下的表逐条合并，见模块文档）。
fn is_map_key(key: &str) -> bool {
    matches!(
        config_schema::field(key).map(|f| f.ty),
        Some(config_schema::FieldType::Map)
    )
}

/// 递归展平：走到路径 `prefix` 时，登记键/白名单键 → 整子树为一个值、停止下钻
/// （Map 键除外：再下钻一层，逐条目产出）；否则表继续下钻；叶子而不是任何已知键 →
/// 记「未知配置键」。未知路径上的空表（如孤零零一行 `[input.foo]`）没有叶子可报，
/// 静默不产出条目；Map 键下的空表同理不产出条目（空表 = no-op，不是「清空」）。
fn flatten(prefix: &str, value: &toml::Value, out: &mut Vec<PatchEntry>) {
    let is_patch_key = !prefix.is_empty()
        && (config_schema::is_known_key(prefix) || ALLOWED_UNREGISTERED_KEYS.contains(&prefix));
    if is_patch_key {
        // Map 键 + 表 → 逐条目。非表值（如 `keys.key_actions = 5`）落回单条，
        // 由 validate 按 Map 类型报「类型应为 table」。
        if is_map_key(prefix)
            && let toml::Value::Table(t) = value
        {
            for (name, v) in t {
                out.push(PatchEntry {
                    key: prefix.to_string(),
                    map_entry: Some(name.clone()),
                    current: None,
                    next: v.clone(),
                    error: None,
                });
            }
            return;
        }
        out.push(PatchEntry {
            key: prefix.to_string(),
            map_entry: None,
            current: None,
            next: value.clone(),
            error: None,
        });
        return;
    }
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let child = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&child, v, out);
            }
        }
        _ => out.push(PatchEntry {
            key: prefix.to_string(),
            map_entry: None,
            current: None,
            next: value.clone(),
            error: Some("未知配置键".to_string()),
        }),
    }
}

/// 校验单个键值。登记键复用 `config.setItems` 的注册表校验；白名单键没有类型声明，
/// 唯一的真相是 `Config` 结构本身——把值合并进默认配置再整体反序列化，
/// 类型不符时 serde 会带字段路径报错。
fn validate_value(key: &str, value: &toml::Value) -> Result<(), String> {
    if config_schema::is_known_key(key) {
        return config_schema::validate(key, value).map_err(|e| e.to_string());
    }
    let mut base =
        toml::Value::try_from(Config::default()).map_err(|e| format!("默认配置序列化失败: {e}"))?;
    let path: Vec<&str> = key.split('.').collect();
    if let toml::Value::Table(t) = &mut base {
        crate::config::set_nested(t, &path, value.clone());
    }
    base.try_into::<Config>()
        .map(|_| ())
        .map_err(|e| format!("类型或取值不合法: {e}"))
}

/// 校验 Map 键下的单个条目值。REGISTRY 只声明父键是 `Map`（`value.is_table()`），
/// 对条目值类型一无所知，唯一的真相是 `Config` 结构本身——把「只含本条目的单元素表」
/// 并进默认配置的该 Map 键再整体反序列化，serde 会带字段路径报错。与
/// [`validate_value`] 的白名单分支同技巧。
fn validate_map_entry(key: &str, name: &str, value: &toml::Value) -> Result<(), String> {
    let mut base =
        toml::Value::try_from(Config::default()).map_err(|e| format!("默认配置序列化失败: {e}"))?;
    let mut one = toml::map::Map::new();
    one.insert(name.to_string(), value.clone());
    let path: Vec<&str> = key.split('.').collect();
    if let toml::Value::Table(t) = &mut base {
        crate::config::set_nested(t, &path, toml::Value::Table(one));
    }
    base.try_into::<Config>()
        .map(|_| ())
        .map_err(|e| format!("类型或取值不合法: {e}"))
}

/// 把预览条目折算成**实际落盘的键值**（`set_user_value` 的入参）。
///
/// - 标量/整值条目：原样 `(key, next)`。
/// - Map 条目：按父键分组，值 = 当前生效表 ∪ 片段条目（同名覆盖、其余保留），
///   每个父键**只产出一条**（整表写回是 `set_user_value` 唯一能表达的形态）。
///
/// 顺序 = 条目首次出现顺序。调用方须先确认无 `error` 条目（半应用不被允许）。
pub fn writes(entries: &[PatchEntry], current: &toml::Value) -> Vec<(String, toml::Value)> {
    let mut out: Vec<(String, toml::Value)> = Vec::new();
    // 父 Map 键 → out 中的下标，保证同一 Map 键的多个条目并进同一张表。
    let mut slot: HashMap<&str, usize> = HashMap::new();
    for e in entries {
        let Some(name) = &e.map_entry else {
            out.push((e.key.clone(), e.next.clone()));
            continue;
        };
        let idx = match slot.get(e.key.as_str()) {
            Some(i) => *i,
            None => {
                // 种子 = 当前生效表：合并的「其余条目保留」全靠这一步。
                let path: Vec<&str> = e.key.split('.').collect();
                let base = crate::config::get_nested(current, &path)
                    .and_then(|v| v.as_table())
                    .cloned()
                    .unwrap_or_default();
                out.push((e.key.clone(), toml::Value::Table(base)));
                slot.insert(e.key.as_str(), out.len() - 1);
                out.len() - 1
            }
        };
        if let toml::Value::Table(t) = &mut out[idx].1 {
            t.insert(name.clone(), e.next.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认配置序列化成的值树，充当测试里的「当前生效配置」。
    fn default_tree() -> toml::Value {
        toml::Value::try_from(Config::default()).expect("serialize default config")
    }

    fn preview_text(text: &str) -> Vec<PatchEntry> {
        let fragment = parse_fragment(text).expect("片段应能解析");
        preview(&fragment, &default_tree())
    }

    #[test]
    fn parse_failure_is_whole_fragment_error() {
        assert!(parse_fragment("= not toml =").is_err());
        assert!(parse_fragment("[unclosed\n").is_err());
    }

    /// Map 键（custom_mappings）逐条目产出：`key` 恒为父 Map 键，条目名走 `map_entry`，
    /// 值是条目自身的值（不是整表）。
    #[test]
    fn map_key_flattens_per_entry() {
        let entries =
            preview_text("[input.punct.custom_mappings]\n\"'1\" = [\"①\"]\n\"'2\" = [\"②\"]\n");
        assert_eq!(entries.len(), 2, "两个条目应产出两条");
        for e in &entries {
            assert_eq!(e.key, "input.punct.custom_mappings", "key 恒为父 Map 键");
            assert!(e.error.is_none(), "{:?}", e.error);
        }
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e.map_entry.as_deref().expect("Map 条目须带 map_entry"))
            .collect();
        assert_eq!(names, vec!["'1", "'2"]);
        assert_eq!(
            entries[0].next,
            toml::Value::Array(vec![toml::Value::String("①".into())]),
            "next 是条目自身的值,不是整表"
        );
    }

    /// 条目名可以含 `.`（`custom_mappings` 的源字符就是标点）。条目名并进点分键
    /// 就再也拆不回来,故必须由 `map_entry` 独立承载。
    #[test]
    fn map_entry_name_with_dot_survives() {
        let entries = preview_text("[input.punct.custom_mappings]\n\".\" = [\"。\"]\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "input.punct.custom_mappings");
        assert_eq!(entries[0].map_entry.as_deref(), Some("."));
        assert!(entries[0].error.is_none(), "{:?}", entries[0].error);
    }

    /// 条目值类型错 → 错误归到该条目（不是整表报错,也不牵连同表的合法条目）。
    #[test]
    fn map_entry_type_error_is_attributed_to_that_entry() {
        let entries = preview_text("[keys.key_actions]\nbacktick = \"english\"\nf4 = 5\n");
        assert_eq!(entries.len(), 2);
        let ok = entries
            .iter()
            .find(|e| e.map_entry.as_deref() == Some("backtick"))
            .unwrap();
        assert!(ok.error.is_none(), "合法条目不应被牵连: {:?}", ok.error);
        let bad = entries
            .iter()
            .find(|e| e.map_entry.as_deref() == Some("f4"))
            .unwrap();
        let err = bad.error.as_deref().expect("整数条目值应被拒绝");
        assert!(err.contains("类型或取值不合法"), "{err}");
    }

    /// Map 键下的空表 = no-op（不产出条目）。「空表 = 清空」是脚枪,片段没有删条目的语义。
    #[test]
    fn empty_map_table_is_noop() {
        assert!(preview_text("[keys.key_actions]\n").is_empty());
        assert!(preview_text("keys.key_actions = {}\n").is_empty());
    }

    /// Map 键给了非表值 → 落回单条,按 Map 类型报「类型应为 table」。
    #[test]
    fn map_key_with_non_table_value_reports_type_error() {
        let entries = preview_text("keys.key_actions = 5\n");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].map_entry.is_none());
        let err = entries[0].error.as_deref().expect("整数应被拒绝");
        assert!(err.contains("类型应为 table"), "{err}");
    }

    /// Map 条目的 current 取自当前表内的同名条目：缺席 = None（新增）,在场 = 原值。
    #[test]
    fn map_entry_current_comes_from_existing_table() {
        let mut tree = default_tree();
        if let toml::Value::Table(t) = &mut tree {
            let mut m = toml::map::Map::new();
            m.insert("backtick".into(), toml::Value::String("english".into()));
            crate::config::set_nested(t, &["keys", "key_actions"], toml::Value::Table(m));
        }
        let fragment =
            parse_fragment("[keys.key_actions]\nbacktick = \"半角\"\nf4 = \"english\"\n").unwrap();
        let entries = preview(&fragment, &tree);
        let old = entries
            .iter()
            .find(|e| e.map_entry.as_deref() == Some("backtick"))
            .unwrap();
        assert_eq!(
            old.current.as_ref().and_then(|v| v.as_str()),
            Some("english"),
            "已有条目应报当前值"
        );
        let new = entries
            .iter()
            .find(|e| e.map_entry.as_deref() == Some("f4"))
            .unwrap();
        assert!(new.current.is_none(), "缺席条目 = 新增,无当前值");
    }

    // ── writes()：落盘键值折算 ──

    /// Map 合并：当前表既有条目保留,同名条目被片段覆盖,父键只产出一条。
    #[test]
    fn writes_merges_map_keeping_existing_entries() {
        let mut tree = default_tree();
        if let toml::Value::Table(t) = &mut tree {
            let mut m = toml::map::Map::new();
            m.insert("backtick".into(), toml::Value::String("english".into()));
            m.insert("f2".into(), toml::Value::String("半角".into()));
            crate::config::set_nested(t, &["keys", "key_actions"], toml::Value::Table(m));
        }
        let fragment =
            parse_fragment("[keys.key_actions]\nbacktick = \"全角\"\nf4 = \"english\"\n").unwrap();
        let entries = preview(&fragment, &tree);
        let w = writes(&entries, &tree);
        assert_eq!(w.len(), 1, "同一 Map 父键只落一条,实际: {w:?}");
        assert_eq!(w[0].0, "keys.key_actions");
        let t = w[0].1.as_table().expect("Map 落盘值须是整表");
        assert_eq!(
            t.get("f2").and_then(|v| v.as_str()),
            Some("半角"),
            "未提及的条目保留"
        );
        assert_eq!(
            t.get("backtick").and_then(|v| v.as_str()),
            Some("全角"),
            "同名条目覆盖"
        );
        assert_eq!(
            t.get("f4").and_then(|v| v.as_str()),
            Some("english"),
            "新条目并入"
        );
        assert_eq!(t.len(), 3);
    }

    /// 当前树里该 Map 键缺席（或是空表）时,合并结果 = 片段条目本身。
    #[test]
    fn writes_seeds_empty_map_from_fragment_only() {
        let tree = default_tree();
        let fragment = parse_fragment("[keys.key_actions]\nf4 = \"english\"\n").unwrap();
        let entries = preview(&fragment, &tree);
        let w = writes(&entries, &tree);
        assert_eq!(w.len(), 1);
        let t = w[0].1.as_table().unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.get("f4").and_then(|v| v.as_str()), Some("english"));
    }

    /// 标量与多个 Map 键混合：标量原样一条,每个 Map 父键各合并成一条,顺序 = 首次出现顺序。
    #[test]
    fn writes_groups_scalars_and_multiple_maps() {
        let tree = default_tree();
        let fragment = parse_fragment(
            "ui.candidate.per_page = 9\n\
             [keys.key_actions]\nf4 = \"english\"\nf5 = \"半角\"\n\
             [keys.session_actions]\nf6 = \"english\"\n",
        )
        .unwrap();
        let entries = preview(&fragment, &tree);
        assert_eq!(entries.len(), 4, "条目仍逐条,实际: {entries:?}");
        let w = writes(&entries, &tree);
        let keys: Vec<&str> = w.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "keys.key_actions",
                "keys.session_actions",
                "ui.candidate.per_page"
            ],
            "每个 Map 父键一条 + 标量一条（顺序 = TOML 表遍历序）"
        );
        let ka = w
            .iter()
            .find(|(k, _)| k == "keys.key_actions")
            .unwrap()
            .1
            .as_table()
            .unwrap();
        assert_eq!(ka.len(), 2, "同一 Map 的两个条目并进同一张表");
        let per_page = w
            .iter()
            .find(|(k, _)| k == "ui.candidate.per_page")
            .unwrap();
        assert_eq!(per_page.1.as_integer(), Some(9), "标量原样落盘");
    }

    /// StructList 键（mix_modes）同理：数组整体是一个值，元素不展开。
    #[test]
    fn struct_list_stops_flatten() {
        let entries = preview_text("[[schema.mix_modes]]\nid = \"m1\"\nname = \"测试\"\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "schema.mix_modes");
        assert!(entries[0].error.is_none(), "{:?}", entries[0].error);
    }

    #[test]
    fn unknown_key_reported_per_entry() {
        let entries = preview_text("[input.foo]\nbar = 1\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "input.foo.bar");
        assert_eq!(entries[0].error.as_deref(), Some("未知配置键"));
        assert!(entries[0].current.is_none(), "未知键无当前值");
    }

    #[test]
    fn allowlist_key_passes_and_has_no_default_current() {
        let entries =
            preview_text("[input.temp_english]\ncomment_template_vertical = \"${dict}\"\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].key,
            "input.temp_english.comment_template_vertical"
        );
        assert!(entries[0].error.is_none(), "{:?}", entries[0].error);
        // 出厂值恰是「键不存在」，默认树里取不到当前值。
        assert!(entries[0].current.is_none());
    }

    #[test]
    fn allowlist_key_rejects_wrong_type() {
        let entries = preview_text("[input.temp_pinyin]\ncomment_template_vertical = 5\n");
        assert_eq!(entries.len(), 1);
        let err = entries[0].error.as_deref().expect("整数值应被拒绝");
        assert!(err.contains("类型或取值不合法"), "{err}");
    }

    #[test]
    fn registered_key_rejects_type_mismatch_and_enum_out_of_range() {
        let entries = preview_text("[ui.candidate]\nper_page = \"seven\"\nlayout = \"diagonal\"\n");
        assert_eq!(entries.len(), 2);
        let per_page = entries
            .iter()
            .find(|e| e.key == "ui.candidate.per_page")
            .unwrap();
        assert!(per_page.error.as_deref().unwrap().contains("类型应为"));
        let layout = entries
            .iter()
            .find(|e| e.key == "ui.candidate.layout")
            .unwrap();
        assert!(layout.error.as_deref().unwrap().contains("不在允许集合"));
    }

    /// diff 正确性：current 取自传入的当前值树，next 取自片段。
    #[test]
    fn diff_reports_current_and_next() {
        let entries = preview_text("[ui.candidate]\nper_page = 9\n");
        assert_eq!(entries.len(), 1);
        let default_per_page =
            crate::config::get_nested(&default_tree(), &["ui", "candidate", "per_page"]).cloned();
        assert_eq!(entries[0].current, default_per_page);
        assert_eq!(entries[0].next.as_integer(), Some(9));
        assert!(entries[0].error.is_none());
    }

    /// 点分键与嵌套表两种写法产出完全相同的条目集（TOML 解析层已归一）。
    #[test]
    fn dotted_and_nested_forms_are_equivalent() {
        let dotted = preview_text("ui.candidate.per_page = 9\ninput.auto_pair.chinese = false\n");
        let nested =
            preview_text("[ui.candidate]\nper_page = 9\n[input.auto_pair]\nchinese = false\n");
        let mut a: Vec<(String, toml::Value)> =
            dotted.into_iter().map(|e| (e.key, e.next)).collect();
        let mut b: Vec<(String, toml::Value)> =
            nested.into_iter().map(|e| (e.key, e.next)).collect();
        a.sort_by(|x, y| x.0.cmp(&y.0));
        b.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(a, b);
    }

    /// 未知路径上的空表不产出条目（无叶子可报），空片段同理。
    #[test]
    fn empty_fragment_and_empty_unknown_table_yield_no_entries() {
        assert!(preview_text("").is_empty());
        assert!(preview_text("[input.foo]\n").is_empty());
    }

    // ── ALLOWED_UNREGISTERED_KEYS 守门：名单不许腐烂 ──

    /// (a) 名单键必须不在 REGISTRY——日后登记了就该从名单删除，否则同一个键有两条校验路径。
    #[test]
    fn allowlist_keys_stay_out_of_registry() {
        for key in ALLOWED_UNREGISTERED_KEYS {
            assert!(
                !config_schema::is_known_key(key),
                "{key} 已进 REGISTRY，应从 ALLOWED_UNREGISTERED_KEYS 移除"
            );
        }
    }

    /// (b) 名单键必须真实存在于 `Config` 结构：写入样例值后能反序列化，且序列化回来
    /// 原值可读——`Config` 不 deny 未知字段，光「不报错」证明不了字段存在，
    /// 回读同值才能证明。样例值按当前名单全员 `Option<String>` 取字符串；
    /// 若日后加入其他类型的键，需为其单独给样例。
    #[test]
    fn allowlist_keys_deserialize_and_round_trip() {
        for key in ALLOWED_UNREGISTERED_KEYS {
            let mut base = toml::Value::try_from(Config::default()).unwrap();
            let path: Vec<&str> = key.split('.').collect();
            if let toml::Value::Table(t) = &mut base {
                crate::config::set_nested(t, &path, toml::Value::String("样例".into()));
            }
            let cfg: Config = base
                .try_into()
                .unwrap_or_else(|e| panic!("{key} 应能反序列化: {e}"));
            let back = toml::Value::try_from(cfg).unwrap();
            assert_eq!(
                crate::config::get_nested(&back, &path).and_then(|v| v.as_str()),
                Some("样例"),
                "{key} 写入后未能原值读回——名单里可能是不存在的键"
            );
        }
    }
}
