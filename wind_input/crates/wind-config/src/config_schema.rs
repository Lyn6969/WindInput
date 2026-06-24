//! 配置字段注册表（声明式单一真相源）。
//!
//! 每个配置叶子键在此声明其值类型。通过测试与 [`Config`](crate::config::Config) 结构体
//! **反向对照**（注册表覆盖所有键、类型与默认值一致），并与系统预置 `data/config.toml`
//! 对照（无孤立键）。CLI、core 端校验、设置 UI 均由此注册表派生，杜绝多份手写真相源漂移。
//!
//! 注：本注册表只描述 **config 类**（用户可改配置）；运行状态（state）与分发数据（data）不在此。
//! 详见仓库根 `SETTINGS_REVAMP_PLAN.md` 的"数据三分准则"。

use crate::config::Config;

/// 配置字段值类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// 布尔。
    Bool,
    /// 整数（usize/i32 等）。
    Int,
    /// 浮点（f32 等）。
    Float,
    /// 任意字符串。
    Str,
    /// 受限字符串（合法值集合）。
    Enum(&'static [&'static str]),
    /// 字符串数组。
    StrList,
    /// 键值映射表（如自定义标点 mappings）。
    Map,
    /// 结构体数组（如 special_modes / mix_modes），整体作不透明叶子。
    StructList,
}

/// 单个配置字段的声明。
#[derive(Debug, Clone, Copy)]
pub struct ConfigField {
    /// 点分路径，如 `"ui.candidate.layout"`。
    pub key: &'static str,
    /// 值类型。
    pub ty: FieldType,
}

const fn f(key: &'static str, ty: FieldType) -> ConfigField {
    ConfigField { key, ty }
}

use FieldType::{Bool, Enum, Float, Int, Map, Str, StrList, StructList};

/// 候选无效按键三策（number_key/select_key/select_char_key 共用）。
const OVERFLOW_VALUES: &[&str] = &["ignore", "commit", "commit_and_input"];

/// 全部配置字段声明（单一真相源）。与 [`Config`] 经测试反向对照，保证零漂移。
static REGISTRY: &[ConfigField] = &[
    // ── general（基本/启动默认）──
    f("general.remember_last_state", Bool),
    f("general.default_chinese_mode", Bool),
    f("general.default_full_width", Bool),
    f("general.default_chinese_punct", Bool),
    // ── schema（方案）──
    f("schema.active", Str),
    f("schema.available", StrList),
    f("schema.primary_codetable", Str),
    f("schema.primary_pinyin", Str),
    // ── hotkeys（按键绑定）──
    f("hotkeys.toggle_mode_keys", StrList),
    f("hotkeys.commit_on_switch", Bool),
    f("hotkeys.switch_engine", Str),
    f("hotkeys.toggle_full_width", Str),
    f("hotkeys.toggle_punct", Str),
    f("hotkeys.toggle_toolbar", Str),
    f("hotkeys.open_settings", Str),
    f("hotkeys.add_word", Str),
    f("hotkeys.toggle_s2t", Str),
    f("hotkeys.activate_ime", Str),
    f("hotkeys.pin_candidate", Str),
    f("hotkeys.delete_candidate", Str),
    f("hotkeys.global_hotkeys", StrList),
    // ── input（输入行为）──
    f("input.punct_follow_mode", Bool),
    f("input.filter_mode", Str),
    f("input.select_key_groups", StrList),
    f("input.page_keys", StrList),
    f("input.highlight_keys", StrList),
    f("input.select_char_keys", StrList),
    f("input.smart_punct_after_digit", Bool),
    f("input.smart_punct_list", Str),
    f("input.smart_symbol_mode", Bool),
    f("input.smart_symbol_timeout_ms", Int),
    f("input.smart_symbol_chars", Str),
    f("input.enter_behavior", Str),
    f("input.space_on_empty_behavior", Str),
    f("input.numpad_behavior", Str),
    f("input.pinyin_separator", Str),
    f("input.punct_custom.enabled", Bool),
    f("input.punct_custom.mappings", Map),
    f("input.auto_pair.chinese", Bool),
    f("input.auto_pair.english", Bool),
    f("input.auto_pair.chinese_pairs", StrList),
    f("input.auto_pair.english_pairs", StrList),
    f("input.overflow.number_key", Enum(OVERFLOW_VALUES)),
    f("input.overflow.select_key", Enum(OVERFLOW_VALUES)),
    f("input.overflow.select_char_key", Enum(OVERFLOW_VALUES)),
    f("input.shift_temp_english.enabled", Bool),
    f("input.shift_temp_english.show_english_candidates", Bool),
    f(
        "input.shift_temp_english.shift_behavior",
        Enum(&["temp_english", "direct_commit"]),
    ),
    f("input.shift_temp_english.trigger_keys", StrList),
    f("input.shift_temp_english.allow_symbols", Bool),
    f("input.shift_temp_english.space_as_input", Bool),
    f("input.capslock.cancel_on_mode_switch", Bool),
    f("input.temp_pinyin.trigger_keys", StrList),
    f("input.url_input.enabled", Bool),
    f("input.url_input.prefixes", StrList),
    f("input.code_commit.auto_commit_at_full", Bool),
    f("input.code_commit.auto_commit_min_len", Int),
    f("input.code_commit.clear_on_empty_max", Bool),
    f("input.code_commit.top_code_commit", Bool),
    f("input.code_commit.auto_commit_block_on_pinyin", Bool),
    f("input.phrase.min_prefix_length", Int),
    f("input.phrase.max_display_chars", Int),
    // ── ui（外观）──
    f("ui.candidate.per_page", Int),
    f("ui.candidate.per_page_extended", Int),
    f("ui.candidate.layout", Enum(&["horizontal", "vertical"])),
    f(
        "ui.candidate.preedit_display",
        Enum(&["app_inline", "candidate_top", "candidate_inline"]),
    ),
    f("ui.candidate.hide_window", Bool),
    f("ui.candidate.font_size", Float),
    f("ui.candidate.font_size_follow_theme", Bool),
    f(
        "ui.candidate.pager_bar_display",
        Enum(&["", "hide", "auto", "always"]),
    ),
    f(
        "ui.candidate.page_number_display",
        Enum(&["", "show", "hide"]),
    ),
    f("ui.candidate.max_chars", Int),
    f("ui.candidate.index_labels", Str),
    f("ui.candidate.flip_when_above", Bool),
    f("ui.font.family", Str),
    f("ui.font.path", Str),
    f("ui.font.render_mode", Enum(&["directwrite", "gdi"])),
    f("ui.theme.name", Str),
    f("ui.theme.style", Str),
    f("ui.mode_indicator.style", Enum(&["short", "full", "none"])),
    f("ui.tooltip.delay", Int),
    f("ui.tooltip.code.enabled", Bool),
    f("ui.tooltip.pinyin.enabled", Bool),
    f("ui.tooltip.pinyin.heteronyms", Bool),
    f("ui.tooltip.pinyin.max_readings", Int),
    f("ui.tooltip.chaizi.enabled", Bool),
    f("ui.tooltip.debug.enabled", Bool),
    f("ui.status_indicator.enabled", Bool),
    f("ui.status_indicator.duration", Int),
    f(
        "ui.status_indicator.display_mode",
        Enum(&["temp", "always"]),
    ),
    f(
        "ui.status_indicator.schema_name_style",
        Enum(&["full", "short"]),
    ),
    f(
        "ui.status_indicator.position_mode",
        Enum(&["follow_caret", "fixed"]),
    ),
    f("ui.status_indicator.offset_x", Int),
    f("ui.status_indicator.offset_y", Int),
    f("ui.status_indicator.custom_x", Int),
    f("ui.status_indicator.custom_y", Int),
    f("ui.toolbar.visible", Bool),
    f("ui.toolbar.hide_in_fullscreen", Bool),
    // ── features（功能）──
    f("features.stats.enabled", Bool),
    f("features.stats.track_english", Bool),
    f("features.s2t.enabled", Bool),
    f("features.s2t.variant", Str),
    f("features.quick_input.enabled", Bool),
    f("features.quick_input.decimal_places", Int),
    f("features.quick_input.force_vertical", Bool),
    f("features.cmdbar.enabled", Bool),
    f("features.cmdbar.candidate_prefix", Str),
    f("features.special_modes", StructList),
    f("features.mix_modes", StructList),
    // ── compat（兼容）──
    f("compat.host_render_processes", StrList),
    // ── debug（调试）──
    f(
        "debug.log_level",
        Enum(&["trace", "debug", "info", "warn", "error"]),
    ),
    f("debug.perf_sampling", Bool),
    // ── pinyin（全局拼音）──
    f("pinyin.show_code_hint", Bool),
    f("pinyin.use_smart_compose", Bool),
    f("pinyin.candidate_order", Str),
    f("pinyin.fuzzy.enabled", Bool),
    f("pinyin.fuzzy.zh_z", Bool),
    f("pinyin.fuzzy.ch_c", Bool),
    f("pinyin.fuzzy.sh_s", Bool),
    f("pinyin.fuzzy.n_l", Bool),
    f("pinyin.fuzzy.f_h", Bool),
    f("pinyin.fuzzy.r_l", Bool),
    f("pinyin.fuzzy.an_ang", Bool),
    f("pinyin.fuzzy.en_eng", Bool),
    f("pinyin.fuzzy.in_ing", Bool),
    f("pinyin.fuzzy.ian_iang", Bool),
    f("pinyin.fuzzy.uan_uang", Bool),
];

/// 返回配置字段注册表。
pub fn registry() -> &'static [ConfigField] {
    REGISTRY
}

/// 按点分路径查注册表条目（未登记返回 None）。
pub fn field(key: &str) -> Option<&'static ConfigField> {
    REGISTRY.iter().find(|f| f.key == key)
}

/// 该键是否已在注册表登记。
pub fn is_known_key(key: &str) -> bool {
    field(key).is_some()
}

/// 配置值校验错误（按 registry 校验 setItems / CLI 写入时用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidateError {
    /// 键未在注册表登记。
    UnknownKey,
    /// 值类型与声明不符。
    TypeMismatch {
        expected: &'static str,
        got: &'static str,
    },
    /// 枚举值不在允许集合内。
    EnumOutOfRange {
        allowed: &'static [&'static str],
        got: String,
    },
}

impl std::fmt::Display for ValidateError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidateError::UnknownKey => write!(fmt, "未登记的配置键"),
            ValidateError::TypeMismatch { expected, got } => {
                write!(fmt, "类型应为 {expected}，实为 {got}")
            }
            ValidateError::EnumOutOfRange { allowed, got } => {
                write!(fmt, "值 {got:?} 不在允许集合 {allowed:?}")
            }
        }
    }
}

impl std::error::Error for ValidateError {}

fn toml_type_name(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::Boolean(_) => "bool",
        toml::Value::Integer(_) => "int",
        toml::Value::Float(_) => "float",
        toml::Value::String(_) => "string",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
        toml::Value::Datetime(_) => "datetime",
    }
}

fn type_label(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Bool => "bool",
        FieldType::Int => "int",
        FieldType::Float => "float",
        FieldType::Str => "string",
        FieldType::Enum(_) => "string(enum)",
        FieldType::StrList => "string[]",
        FieldType::Map => "table",
        FieldType::StructList => "array",
    }
}

/// 按注册表校验"键+值"。未登记键、类型不符、枚举越界均返回结构化错误。
/// 宽松点：`Float` 字段接受整数值（用户常输 18 而非 18.0）。
pub fn validate(key: &str, value: &toml::Value) -> Result<(), ValidateError> {
    let f = field(key).ok_or(ValidateError::UnknownKey)?;
    let type_ok = match f.ty {
        FieldType::Bool => value.is_bool(),
        FieldType::Int => value.is_integer(),
        FieldType::Float => value.is_float() || value.is_integer(),
        FieldType::Str => value.is_str(),
        FieldType::Enum(allowed) => {
            let s = value.as_str().ok_or(ValidateError::TypeMismatch {
                expected: "string",
                got: toml_type_name(value),
            })?;
            if !allowed.contains(&s) {
                return Err(ValidateError::EnumOutOfRange {
                    allowed,
                    got: s.to_string(),
                });
            }
            true
        }
        FieldType::StrList => value
            .as_array()
            .map(|a| a.iter().all(|e| e.is_str()))
            .unwrap_or(false),
        FieldType::Map => value.is_table(),
        FieldType::StructList => value.is_array(),
    };
    if type_ok {
        Ok(())
    } else {
        Err(ValidateError::TypeMismatch {
            expected: type_label(f.ty),
            got: toml_type_name(value),
        })
    }
}

/// 把 TOML 值展开为点分叶子键列表。
///
/// 规则：递归进入**非空表**；标量、数组、**空表**（如空 HashMap）均视为叶子。
/// 故 `[ui.candidate]`（非空表）会下钻，而 `input.punct_custom.mappings = {}`（空表）作叶子保留。
fn collect_leaf_keys(prefix: &str, value: &toml::Value, out: &mut Vec<String>) {
    match value {
        toml::Value::Table(t) if !t.is_empty() => {
            for (k, v) in t {
                let child = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                collect_leaf_keys(&child, v, out);
            }
        }
        _ => out.push(prefix.to_string()),
    }
}

/// 默认配置（[`Config::default`]）序列化后的全部叶子键（已排序去重）。
pub fn config_leaf_keys() -> Vec<String> {
    let value = toml::Value::try_from(Config::default()).expect("serialize default config");
    let mut out = Vec::new();
    collect_leaf_keys("", &value, &mut out);
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// 解析仓库内系统预置 `data/config.toml`。
    fn data_config_toml() -> toml::Value {
        // CARGO_MANIFEST_DIR = <repo>/wind_input/crates/wind-config
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/config.toml");
        let content = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("读取 {path} 失败（Stage1 注册表对照需要）: {e}"));
        toml::from_str(&content).expect("data/config.toml 解析失败")
    }

    fn leaf_keys_of(value: &toml::Value) -> Vec<String> {
        let mut out = Vec::new();
        collect_leaf_keys("", value, &mut out);
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn leaf_keys_drills_tables_and_keeps_arrays_maps_as_leaves() {
        let keys: BTreeSet<String> = config_leaf_keys().into_iter().collect();
        // 标量叶子
        assert!(keys.contains("ui.candidate.per_page"), "应含标量叶子");
        // 嵌套标量
        assert!(
            keys.contains("input.overflow.number_key"),
            "应含嵌套标量叶子"
        );
        // 空 HashMap 作叶子保留
        assert!(
            keys.contains("input.punct_custom.mappings"),
            "空表(mappings)应作叶子保留"
        );
        // 数组作叶子（不下钻元素）
        assert!(keys.contains("schema.available"), "数组应作叶子");
        assert!(
            keys.contains("features.mix_modes"),
            "结构体数组应作单一叶子，不展开元素"
        );
        // 中间表不应作为叶子出现
        assert!(!keys.contains("ui.candidate"), "中间表不应是叶子");
        assert!(!keys.contains("ui"), "顶层表不应是叶子");
    }

    #[test]
    fn registry_covers_every_config_key() {
        let struct_keys: BTreeSet<String> = config_leaf_keys().into_iter().collect();
        let registry_keys: BTreeSet<String> =
            registry().iter().map(|f| f.key.to_string()).collect();

        let missing: Vec<&String> = struct_keys.difference(&registry_keys).collect();
        let extra: Vec<&String> = registry_keys.difference(&struct_keys).collect();

        assert!(
            missing.is_empty() && extra.is_empty(),
            "注册表与 Config 不一致：\n  注册表缺失({}): {:?}\n  注册表多余({}): {:?}",
            missing.len(),
            missing,
            extra.len(),
            extra
        );
    }

    #[test]
    fn data_config_toml_has_no_orphan_keys() {
        let struct_keys: BTreeSet<String> = config_leaf_keys().into_iter().collect();
        let toml_keys = leaf_keys_of(&data_config_toml());
        let orphans: Vec<&String> = toml_keys
            .iter()
            .filter(|k| !struct_keys.contains(*k))
            .collect();
        assert!(
            orphans.is_empty(),
            "data/config.toml 含 {} 个孤立键（struct 无对应字段，会被静默丢弃）: {:?}",
            orphans.len(),
            orphans
        );
    }

    #[test]
    fn field_lookup_finds_registered_key_and_rejects_unknown() {
        let f = field("ui.candidate.layout").expect("已登记键应查到");
        assert_eq!(f.key, "ui.candidate.layout");
        assert!(matches!(f.ty, FieldType::Enum(_)));
        assert!(is_known_key("input.overflow.number_key"));
        assert!(!is_known_key("ui.candidate.bogus"), "未登记键应返回 None");
        assert!(!is_known_key("totally.made.up"));
    }

    /// 守卫：core 内部 setter 硬编码的 key 路径必须都在注册表中（防拼写/漂移）。
    /// 新增 `Config::set_user_*` 调用点时，把其路径加到这里。
    #[test]
    fn internal_setter_paths_are_registered() {
        const INTERNAL_PATHS: &[&str] = &[
            "schema.active",
            "ui.theme.style",
            "ui.theme.name",
            "ui.candidate.preedit_display",
            "ui.toolbar.visible",
        ];
        for p in INTERNAL_PATHS {
            assert!(is_known_key(p), "内部 setter 路径未在注册表登记: {p}");
        }
    }

    #[test]
    fn validate_accepts_correct_types() {
        assert!(validate("ui.candidate.per_page", &toml::Value::Integer(9)).is_ok());
        assert!(validate("ui.candidate.hide_window", &toml::Value::Boolean(true)).is_ok());
        assert!(
            validate(
                "ui.candidate.layout",
                &toml::Value::String("vertical".into())
            )
            .is_ok()
        );
        // Float 字段接受整数值（宽松）
        assert!(validate("ui.candidate.font_size", &toml::Value::Integer(18)).is_ok());
        assert!(validate("ui.candidate.font_size", &toml::Value::Float(18.0)).is_ok());
        // Enum 允许空串成员（pager_bar_display 含 ""）
        assert!(
            validate(
                "ui.candidate.pager_bar_display",
                &toml::Value::String("".into())
            )
            .is_ok()
        );
        // 数组 / 表
        assert!(
            validate(
                "schema.available",
                &toml::Value::Array(vec![toml::Value::String("wubi86".into())])
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_rejects_unknown_key() {
        assert_eq!(
            validate("ui.candidate.bogus", &toml::Value::Integer(1)),
            Err(ValidateError::UnknownKey)
        );
    }

    #[test]
    fn validate_rejects_type_mismatch() {
        let r = validate(
            "ui.candidate.per_page",
            &toml::Value::String("seven".into()),
        );
        assert!(
            matches!(r, Err(ValidateError::TypeMismatch { .. })),
            "{r:?}"
        );
        let r2 = validate("ui.candidate.hide_window", &toml::Value::Integer(1));
        assert!(
            matches!(r2, Err(ValidateError::TypeMismatch { .. })),
            "{r2:?}"
        );
    }

    #[test]
    fn validate_rejects_enum_out_of_range() {
        let r = validate(
            "ui.candidate.layout",
            &toml::Value::String("diagonal".into()),
        );
        assert!(
            matches!(r, Err(ValidateError::EnumOutOfRange { .. })),
            "{r:?}"
        );
    }

    #[test]
    fn validate_rejects_strlist_with_non_string_element() {
        let r = validate(
            "schema.available",
            &toml::Value::Array(vec![toml::Value::Integer(1)]),
        );
        assert!(
            matches!(r, Err(ValidateError::TypeMismatch { .. })),
            "{r:?}"
        );
    }

    #[test]
    fn registry_types_match_default_values() {
        let default = toml::Value::try_from(Config::default()).unwrap();
        for field in registry() {
            let value = navigate(&default, field.key)
                .unwrap_or_else(|| panic!("默认配置缺少注册表声明的键: {}", field.key));
            assert!(
                type_matches(field.ty, value),
                "键 {} 声明类型 {:?} 与默认值实际类型不符: {:?}",
                field.key,
                field.ty,
                value
            );
        }
    }

    /// 按点分路径在 TOML 表中导航取值。
    fn navigate<'a>(root: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
        let mut cur = root;
        for part in key.split('.') {
            cur = cur.as_table()?.get(part)?;
        }
        Some(cur)
    }

    fn type_matches(ty: FieldType, value: &toml::Value) -> bool {
        match ty {
            FieldType::Bool => value.is_bool(),
            FieldType::Int => value.is_integer(),
            FieldType::Float => value.is_float(),
            FieldType::Str | FieldType::Enum(_) => value.is_str(),
            FieldType::StrList | FieldType::StructList => value.is_array(),
            FieldType::Map => value.is_table(),
        }
    }
}
