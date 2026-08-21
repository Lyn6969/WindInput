//! 残留 `keys.schema_hotkeys` 的处置：**从 TOML 文本读起**。
//!
//! 该键已废弃且不做向后兼容（不折算进 key_actions），但必须**可见地**失效——加载期
//! 告警一次。告警的前提是这一段能被读进来：字段已改名为 `legacy_schema_hotkeys`，
//! 只要 `#[serde(rename)]` 写错或漏掉，用户 config.toml 里那一段就被 serde 静默丢弃，
//! 于是连告警都发不出，用户的热键失效且查不到原因。
//!
//! 单测里直接操作字段验不到这一环，故本用例从真实 TOML 文本解析。

use wind_config::Config;

#[test]
fn legacy_schema_hotkeys_parse_then_warn_and_drop() {
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
        "serde rename 丢了 ⇒ 那一段被静默丢弃，连告警都发不出来"
    );

    cfg.normalize();

    assert!(
        cfg.keys.legacy_schema_hotkeys.is_empty(),
        "告警后须清空，否则后续「有没有配过」的判断会读到假信号"
    );
    // 不做向后兼容：不折算，用户需按告警提示改写为 key_actions。
    //
    // 只断言「没有 switch_schema: 条目」而不是整表为空：normalize 里还有别的迁移
    // （trigger_keys 收编）会往同一张表里折算，断言整表空等于把无关迁移也钉死在这里。
    let migrated: Vec<&String> = cfg
        .keys
        .key_actions
        .values()
        .filter(|v| v.starts_with("switch_schema:"))
        .collect();
    assert!(
        migrated.is_empty(),
        "兼容层已删除，不该有条目被折算进来：{migrated:?}"
    );

    // 不再被写出：skip_serializing_if 保证设置页全量写回时它自然消失。
    let out = toml::to_string(&cfg).expect("序列化");
    assert!(
        !out.contains("schema_hotkeys"),
        "废弃键不该再被写回配置文件"
    );
}
