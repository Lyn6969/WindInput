//! 存量 `keys.schema_hotkeys` 的端到端迁移：**从 TOML 文本读起**。
//!
//! 单测里直接 `cfg.keys.legacy_schema_hotkeys.insert(...)` 验不到最要紧的那一环——
//! 字段已改名为 `legacy_schema_hotkeys`，只要 `#[serde(rename)]` 写错或漏掉，用户
//! config.toml 里的那一段就被静默丢弃：**热键全部消失，且没有任何报错**。
//! 本用例从真实 TOML 文本解析，把 rename 也钉在契约里。

use wind_config::Config;

#[test]
fn legacy_schema_hotkeys_parse_and_migrate_from_toml_text() {
    let toml = r#"
[keys]
[keys.schema_hotkeys]
wubi86 = "ctrl+shift+r"
english = "ctrl+shift+n"
"#;
    let mut cfg: Config = toml::from_str(toml).expect("旧配置应解析成功");
    assert_eq!(
        cfg.keys.legacy_schema_hotkeys.len(),
        2,
        "serde rename 丢了 ⇒ 用户那一段被静默丢弃，热键全消失且无报错"
    );

    cfg.normalize();

    assert!(
        cfg.keys.legacy_schema_hotkeys.is_empty(),
        "折算后旧表须清空"
    );
    assert_eq!(
        cfg.keys.key_actions.get("ctrl+shift+r").map(String::as_str),
        Some("switch_schema:wubi86")
    );
    assert_eq!(
        cfg.keys.key_actions.get("ctrl+shift+n").map(String::as_str),
        Some("switch_schema:english")
    );

    // 折算后不再写出旧键：`skip_serializing_if` 保证设置页全量写回时它自然消失。
    let out = toml::to_string(&cfg).expect("序列化");
    assert!(!out.contains("schema_hotkeys"), "旧键不该再被写回配置文件");
}
