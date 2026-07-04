//! 配置字段注册表（声明式单一真相源）。
//!
//! 每个配置叶子键在此声明其值类型。通过测试与 [`Config`](crate::config::Config) 结构体
//! **反向对照**（注册表覆盖所有键、类型与默认值一致），并与系统预置 `data/config.toml`
//! 对照（无孤立键）。CLI、core 端校验、设置 UI 均由此注册表派生，杜绝多份手写真相源漂移。
//!
//! 注：本注册表只描述 **config 类**（用户可改配置）；运行状态（state）与分发数据（data）不在此。
//! 详见仓库根 `SETTINGS_REVAMP_PLAN.md` 的"数据三分准则"，键名映射见 `docs/config-key-migration.md`。

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

/// 码表词频应用策略。
const FREQ_STRATEGY_VALUES: &[&str] = &["top", "step"];

/// 全部配置字段声明（单一真相源）。与 [`Config`] 经测试反向对照，保证零漂移。
/// 域划分见 `docs/config-key-migration.md`（不做向后兼容，旧键已弃）。
static REGISTRY: &[ConfigField] = &[
    // -- schema（方案 + 拼音 + 模式）--
    f("schema.active", Str),
    f("schema.available", StrList),
    f("schema.primary_codetable", Str),
    f("schema.primary_pinyin", Str),
    // 全局码表（公共基线；方案经 schema_overrides 覆盖）
    f("schema.codetable.top_code_commit", Bool),
    f("schema.codetable.clear_on_empty_max", Bool),
    f("schema.codetable.auto_commit_at_full", Bool),
    f("schema.codetable.auto_commit_min_len", Int),
    f("schema.codetable.punct_commit", Bool),
    f("schema.codetable.show_code_hint", Bool),
    f("schema.codetable.single_code_input", Bool),
    f("schema.codetable.single_code_complete", Bool),
    f("schema.codetable.z_key_repeat", Bool),
    f("schema.codetable.frequency.enabled", Bool),
    f("schema.codetable.frequency.protect_top_n", Int),
    f(
        "schema.codetable.frequency.strategy",
        Enum(FREQ_STRATEGY_VALUES),
    ),
    f("schema.codetable.auto_phrase.enabled", Bool),
    f("schema.codetable.auto_phrase.min_phrase_len", Int),
    f("schema.codetable.auto_phrase.max_phrase_len", Int),
    f("schema.codetable.auto_phrase.promote_count", Int),
    // 全局拼音
    f("schema.pinyin.show_code_hint", Bool),
    f("schema.pinyin.use_smart_compose", Bool),
    f("schema.pinyin.separator", Str),
    f("schema.pinyin.fuzzy.enabled", Bool),
    f("schema.pinyin.fuzzy.zh_z", Bool),
    f("schema.pinyin.fuzzy.ch_c", Bool),
    f("schema.pinyin.fuzzy.sh_s", Bool),
    f("schema.pinyin.fuzzy.n_l", Bool),
    f("schema.pinyin.fuzzy.f_h", Bool),
    f("schema.pinyin.fuzzy.r_l", Bool),
    f("schema.pinyin.fuzzy.an_ang", Bool),
    f("schema.pinyin.fuzzy.en_eng", Bool),
    f("schema.pinyin.fuzzy.in_ing", Bool),
    f("schema.pinyin.fuzzy.ian_iang", Bool),
    f("schema.pinyin.fuzzy.uan_uang", Bool),
    f("schema.pinyin.frequency.enabled", Bool),
    f("schema.pinyin.frequency.half_life", Float),
    f("schema.pinyin.frequency.base_scale", Float),
    f("schema.pinyin.frequency.recency_peak", Float),
    f("schema.pinyin.auto_learn.enabled", Bool),
    f("schema.pinyin.auto_learn.min_word_length", Int),
    f("schema.pinyin.auto_learn.promote_count", Int),
    // 全局混输（融合策略）
    f("schema.mix.show_source_hint", Bool),
    f("schema.mix.enable_english", Bool),
    f("schema.mix.pinyin_only_overflow", Bool),
    f("schema.mix.top_code_override_pinyin", Bool),
    f("schema.mix.auto_commit_block_on_pinyin", Bool),
    f("schema.mix.min_pinyin_length", Int),
    f("schema.quick_input.enabled", Bool),
    f("schema.quick_input.decimal_places", Int),
    f("schema.quick_input.force_vertical", Bool),
    f("schema.special_modes", StructList),
    f("schema.mix_modes", StructList),
    // -- input（输入行为）--
    f("input.filter_mode", Str),
    f("input.enter_behavior", Str),
    f("input.space_on_empty_behavior", Str),
    f("input.numpad_behavior", Str),
    // 启动默认状态（原 general 域）
    f("input.default.remember_last_state", Bool),
    f("input.default.state_scope", Enum(&["global", "app"])),
    f("input.default.chinese_mode", Bool),
    f("input.default.full_width", Bool),
    f("input.default.chinese_punct", Bool),
    f("input.punct.follow_mode", Bool),
    f("input.punct.smart_after_digit", Bool),
    f("input.punct.smart_list", Str),
    f("input.punct.custom_enabled", Bool),
    f("input.punct.custom_mappings", Map),
    f("input.symbol.smart_mode", Bool),
    f("input.symbol.smart_timeout_ms", Int),
    f("input.symbol.smart_chars", Str),
    f(
        "input.symbol.smart_method",
        Enum(&["delete_replace", "hold_composition"]),
    ),
    f("input.auto_pair.chinese", Bool),
    f("input.auto_pair.english", Bool),
    f("input.auto_pair.chinese_pairs", StrList),
    f("input.auto_pair.english_pairs", StrList),
    f("input.temp_english.enabled", Bool),
    f("input.temp_english.show_candidates", Bool),
    f(
        "input.temp_english.shift_behavior",
        Enum(&["temp_english", "direct_commit"]),
    ),
    f("input.temp_english.trigger_keys", StrList),
    f("input.temp_english.allow_symbols", Bool),
    f("input.temp_english.space_as_input", Bool),
    f("input.capslock.cancel_on_mode_switch", Bool),
    f("input.temp_pinyin.enabled", Bool),
    f("input.temp_pinyin.schema", Str),
    f("input.temp_pinyin.trigger_keys", StrList),
    f("input.url.enabled", Bool),
    f("input.url.prefixes", StrList),
    f("input.s2t.enabled", Bool),
    f("input.s2t.variant", Str),
    f("input.cmdbar.enabled", Bool),
    f("input.cmdbar.candidate_prefix", Str),
    // 短语前缀列举（原 dict.phrase）
    f("input.phrase.min_prefix", Int),
    f("input.phrase.max_display_chars", Int),
    // -- keys（全部按键，扁平；overflow 保留一层）--
    f("keys.toggle_mode_keys", StrList),
    f("keys.commit_on_switch", Bool),
    f("keys.switch_engine", Str),
    f("keys.toggle_full_width", Str),
    f("keys.toggle_punct", Str),
    f("keys.toggle_toolbar", Str),
    f("keys.open_settings", Str),
    f("keys.add_word", Str),
    f("keys.toggle_s2t", Str),
    f("keys.activate_ime", Str),
    f("keys.pin_candidate", Str),
    f("keys.delete_candidate", Str),
    f("keys.take_screenshot", Str),
    f("keys.global_hotkeys", StrList),
    f("keys.select_key_groups", StrList),
    f("keys.page_keys", StrList),
    f("keys.highlight_keys", StrList),
    f("keys.select_char_keys", StrList),
    f("keys.overflow.number_key", Enum(OVERFLOW_VALUES)),
    f("keys.overflow.select_key", Enum(OVERFLOW_VALUES)),
    f("keys.overflow.select_char_key", Enum(OVERFLOW_VALUES)),
    // -- ui（外观）--
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
    f("ui.tooltip.code_enabled", Bool),
    f("ui.tooltip.pinyin_enabled", Bool),
    f("ui.tooltip.pinyin_heteronyms", Bool),
    f("ui.tooltip.pinyin_max_readings", Int),
    f("ui.tooltip.chaizi_enabled", Bool),
    f("ui.tooltip.debug_enabled", Bool),
    f("ui.status.enabled", Bool),
    f("ui.status.duration", Int),
    f("ui.status.display_mode", Enum(&["temp", "always"])),
    f("ui.status.schema_name_style", Enum(&["full", "short"])),
    f("ui.status.position_mode", Enum(&["follow_caret", "fixed"])),
    f("ui.status.offset_x", Int),
    f("ui.status.offset_y", Int),
    f("ui.status.custom_x", Int),
    f("ui.status.custom_y", Int),
    f("ui.toolbar.visible", Bool),
    f("ui.toolbar.hide_in_fullscreen", Bool),
    // -- stats（统计，原 features.stats 升顶级）--
    f("stats.enabled", Bool),
    f("stats.track_english", Bool),
    // -- compat（兼容）--
    f("compat.host_render_processes", StrList),
    // -- debug（调试）--
    f(
        "debug.log_level",
        Enum(&["trace", "debug", "info", "warn", "error"]),
    ),
    f("debug.log_max_size_mb", Int),
    f("debug.log_max_files", Int),
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
/// 故 `[ui.candidate]`（非空表）会下钻，而 `input.punct.custom_mappings = {}`（空表）作叶子保留。
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

fn collect_leaf_entries(prefix: &str, value: &toml::Value, out: &mut Vec<(String, toml::Value)>) {
    match value {
        toml::Value::Table(t) if !t.is_empty() => {
            for (k, v) in t {
                let child = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                collect_leaf_entries(&child, v, out);
            }
        }
        _ => out.push((prefix.to_string(), value.clone())),
    }
}

/// 把任意 TOML 表展开为 `(点分键, 叶子值)` 列表（叶子规则同 [`config_leaf_keys`]）。
/// 供 `config import` 把一份 TOML 拍平成逐字段 setItems。
pub fn leaf_entries(value: &toml::Value) -> Vec<(String, toml::Value)> {
    let mut out = Vec::new();
    collect_leaf_entries("", value, &mut out);
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
            keys.contains("keys.overflow.number_key"),
            "应含嵌套标量叶子"
        );
        // 空 HashMap 作叶子保留
        assert!(
            keys.contains("input.punct.custom_mappings"),
            "空表(custom_mappings)应作叶子保留"
        );
        // 数组作叶子（不下钻元素）
        assert!(keys.contains("schema.available"), "数组应作叶子");
        assert!(
            keys.contains("schema.mix_modes"),
            "结构体数组应作单一叶子，不展开元素"
        );
        // 中间表不应作为叶子出现
        assert!(!keys.contains("ui.candidate"), "中间表不应是叶子");
        assert!(!keys.contains("ui"), "顶层表不应是叶子");
    }

    #[test]
    fn leaf_entries_flattens_table_to_key_value_pairs() {
        let v: toml::Value = toml::from_str(
            "[ui.candidate]\nper_page = 9\nlayout = \"vertical\"\n[input.auto_pair]\nchinese = false\n",
        )
        .unwrap();
        let entries = leaf_entries(&v);
        assert!(
            entries
                .iter()
                .any(|(k, val)| k == "ui.candidate.per_page" && val.as_integer() == Some(9))
        );
        assert!(
            entries
                .iter()
                .any(|(k, val)| k == "ui.candidate.layout" && val.as_str() == Some("vertical"))
        );
        assert!(
            entries
                .iter()
                .any(|(k, val)| k == "input.auto_pair.chinese" && val.as_bool() == Some(false))
        );
        // 不应出现中间表键
        assert!(!entries.iter().any(|(k, _)| k == "ui.candidate"));
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
        assert!(is_known_key("keys.overflow.number_key"));
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

    /// data/config.toml 每个已登记叶子键的值，必须通过 registry 校验（类型 / enum 合法）。
    /// 注意：config.toml 作为系统预置可合法覆盖 code default，故此处只校验「合法」而非「等于默认」。
    #[test]
    fn data_config_toml_values_pass_validation() {
        let toml_val = data_config_toml();
        let mut bad = Vec::new();
        for (key, value) in leaf_entries(&toml_val) {
            // 未登记键由 data_config_toml_has_no_orphan_keys 守护，这里只校验已登记项的值
            if field(&key).is_none() {
                continue;
            }
            if let Err(e) = validate(&key, &value) {
                bad.push(format!("{key}: {e}"));
            }
        }
        assert!(
            bad.is_empty(),
            "data/config.toml 含非法值（类型/enum 不符 registry）:\n{}",
            bad.join("\n")
        );
    }
}
