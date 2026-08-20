//! 文本信封(`kind = "schema_text"`):一段 TOML 文本即一个完整分发包。
//!
//! 服务小方案(快符类:方案 + 小词库共 KB 级)的纯文本分发——剪贴板 / 文档站代码块
//! 即贴即装。**分发格式,不是存储格式**:拆解落盘后的形态与 zip 导入完全一致
//! (方案文件 + 词库文件),引擎与缓存管线零感知。
//!
//! ```toml
//! [package]
//! format_version = 2          # 必填。信封无 legacy——缺失即错
//! kind = "schema_text"        # 必填。显式声明,侦测不做猜测
//! title = "快符方案"           # 可选,说明元信息(语法与片段/zip 包同名同义)
//! description = """
//! 分号引导的符号表。
//! """
//!
//! [schema]                    # 可选冗余,免解析 files 即可显示 id/版本
//! id = "kf"
//! version = "1.00.0"
//!
//! [[files]]
//! path = "kf.schema.toml"     # schemas 根相对路径,逐条过 validate_entry_rel
//! content = '''…方案原文…'''
//! ```
//!
//! **显式声明才走此路**:`[schema]` 也是合法的配置片段键前缀(config.toml 有 `schema.` 段),
//! 裸方案文本不得被猜成信封。故没有 `kind = "schema_text"` 的文本一律返回
//! [`NOT_SCHEMA_TEXT`] 前缀的错误,由调用方回落配置片段管线,让片段管线如实报「未知配置键」。

use serde::Deserialize;

use crate::scheme::{
    CONFIG_PATCH_NAME, PACKAGE_FORMAT_VERSION, PackageInfo, PackageMeta, PackageSchemaInfo,
    SchemeImportPreview,
};

/// 信封 kind 的唯一合法值。
pub const ENVELOPE_KIND: &str = "schema_text";

/// 「这段文本不是信封」错误的前缀。**跨仓契约**:设置端按此前缀判定要回落到配置片段
/// 管线(而不是把错误直接展示给用户)。改动须同步 wind-setting 的导入分派。
pub const NOT_SCHEMA_TEXT: &str = "not_schema_text:";

/// 信封文本体积上限(2 MB)。大方案走 zip/.wpkg,信封只服务小方案。
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// 信封文件条目数上限。
pub const MAX_FILES: usize = 64;

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    package: EnvelopePackage,
    #[serde(default)]
    schema: PackageSchemaInfo,
    #[serde(default)]
    files: Vec<EnvelopeFile>,
}

/// 信封的 `[package]` 段。`format_version` 用 `Option` 承载:「缺失」与「写了但过高」
/// 要报不同的话,回落默认值就分不清了。
///
/// `kind` 不在此列——它必须在**反序列化之前**判定(不是信封就得回落片段管线,
/// 而反序列化失败本身也可能只是因为它不是信封),故由 [`parse`] 直接从 `toml::Value` 取。
#[derive(Debug, Default, Deserialize)]
struct EnvelopePackage {
    format_version: Option<u32>,
    #[serde(default)]
    app_version: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    created_at: String,
    /// 说明元信息,语法与限额同片段与 zip 包(同名同义,一套语法学一次)。
    ///
    /// ⚠️ 与信封内 `config_patch.toml` 自带的 `[package]` 说明**不合并**:这里说的是
    /// "这个信封是什么",片段级说的是"这段配置改了什么"。导入确认对话框各显各的。
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
struct EnvelopeFile {
    path: String,
    content: String,
}

/// 拆解后的信封:落盘载荷 + 元信息 + 配置片段原文。
struct ParsedEnvelope {
    meta: PackageMeta,
    /// (schemas 根相对路径, 文件内容字节),顺序 = 信封中的书写顺序。
    staged: Vec<(String, Vec<u8>)>,
    config_patch: Option<String>,
}

/// 解析并全量校验信封文本。
///
/// 校验顺序即报错优先级:先确认「是不是信封」(不是就让调用方回落片段管线),
/// 再查版本门禁,最后才是限额与布局——对一段根本不是信封的文本报「files 超限」
/// 会把人带偏。
fn parse(text: &str) -> anyhow::Result<ParsedEnvelope> {
    // ★ 体积限额先于解析,且是**硬错误、不带回落前缀**。两条理由:
    // 一、超 2 MB 的文本几乎必是超大信封而非配置片段(片段是手写配置,KB 量级),
    //    报「信封超上限」比回落片段管线刷一屏「未知配置键」有用得多;
    // 二、放在解析之后,就留下了「最大 16 MB(IPC 帧上限)的文本先被完整解析、再被拒绝」
    //    的窗口——限额要挡的正是这份开销,挡在解析后等于没挡。
    if text.len() > MAX_TEXT_BYTES {
        anyhow::bail!(
            "方案文本信封过大({} 字节,上限 {MAX_TEXT_BYTES}),请改用 .wpkg 分发",
            text.len()
        );
    }
    // 非 TOML 文本同样不是信封:片段管线会给出更准确的解析错误。
    let value: toml::Value = toml::from_str(text)
        .map_err(|e| anyhow::anyhow!("{NOT_SCHEMA_TEXT} 不是合法 TOML 文本: {e}"))?;
    let kind = value
        .get("package")
        .and_then(|p| p.get("kind"))
        .and_then(|k| k.as_str());
    if kind != Some(ENVELOPE_KIND) {
        anyhow::bail!("{NOT_SCHEMA_TEXT} 缺少 package.kind = \"{ENVELOPE_KIND}\"");
    }
    // kind 已确认 → 后续一律是硬错误,不再回落片段管线。
    let env: Envelope = value
        .try_into()
        .map_err(|e| anyhow::anyhow!("方案文本信封无法解析: {e}"))?;

    let Some(format_version) = env.package.format_version else {
        anyhow::bail!("方案文本信封缺少 package.format_version(信封无 legacy,必须显式声明)");
    };
    if format_version > PACKAGE_FORMAT_VERSION {
        anyhow::bail!(
            "方案文本信封版本过高(format_version={format_version},当前支持 {PACKAGE_FORMAT_VERSION}),请升级 WindInput"
        );
    }
    // 说明元信息与版本同层(都是 `[package]` 段的声明)：走 wind-config 的同一份净化，
    // 不另写一份。非法即硬错误——说明会直接渲染进 UI。
    let mut package = PackageInfo {
        format_version,
        app_version: env.package.app_version,
        platform: env.package.platform,
        created_at: env.package.created_at,
        title: env.package.title,
        description: env.package.description,
    };
    crate::scheme::sanitize_package_info(&mut package)?;
    if env.files.len() > MAX_FILES {
        anyhow::bail!(
            "方案文本信封文件过多({} 个,上限 {MAX_FILES}),请改用 .wpkg 分发",
            env.files.len()
        );
    }

    let mut staged: Vec<(String, Vec<u8>)> = Vec::with_capacity(env.files.len());
    let mut config_patch: Option<String> = None;
    let mut has_root_schema = false;
    for f in &env.files {
        // 穿越守卫只此一处,与 zip 导入同一实现(components 白名单)。返回值已把 `\`
        // 归一为 `/`:判根与落盘都用它,否则 `sub\x.schema.toml` 会被当成根方案文件。
        let rel = crate::bundle::validate_entry_rel(&f.path, "")?;
        if staged.iter().any(|(r, _)| *r == rel)
            || (rel == CONFIG_PATCH_NAME && config_patch.is_some())
        {
            anyhow::bail!("方案文本信封含重复路径: {rel}");
        }
        // 配置片段按名识别、不落盘,语义与 zip 包内的 config_patch.toml 完全一致。
        if rel == CONFIG_PATCH_NAME {
            config_patch = Some(f.content.clone());
            continue;
        }
        if !rel.contains('/') && rel.ends_with(".schema.toml") {
            has_root_schema = true;
        }
        staged.push((rel, f.content.clone().into_bytes()));
    }
    if !has_root_schema {
        anyhow::bail!("不是有效的方案文本信封(根目录缺少 *.schema.toml)");
    }

    Ok(ParsedEnvelope {
        meta: PackageMeta {
            package,
            schema: env.schema,
            refs: Default::default(),
        },
        staged,
        config_patch,
    })
}

/// 信封导入预览(只读):will_add/conflicts 对 `user_dir` 计算,语义与 zip 版一致。
pub fn preview_import_text(
    text: &str,
    user_dir: &std::path::Path,
) -> anyhow::Result<SchemeImportPreview> {
    let p = parse(text)?;
    let rels: Vec<String> = p.staged.iter().map(|(r, _)| r.clone()).collect();
    Ok(crate::scheme::build_preview(
        p.meta,
        &rels,
        user_dir,
        p.config_patch,
    ))
}

/// 信封导入:校验期零落盘(全量解析通过才写),随后走与 zip 导入同一个写盘辅助
/// (逐文件 tmp+rename,Merge 跳过已存在)。配置片段不落盘。
pub fn import_text(
    text: &str,
    user_dir: &std::path::Path,
    strategy: crate::merge::Strategy,
) -> anyhow::Result<crate::scheme::SchemeImportResult> {
    let p = parse(text)?;
    let (imported, conflicts) = crate::scheme::write_staged(&p.staged, user_dir, strategy)?;
    let schema_ids = p
        .staged
        .iter()
        .map(|(r, _)| r.as_str())
        .filter(|r| !r.contains('/'))
        .filter_map(|r| r.strip_suffix(".schema.toml"))
        .map(String::from)
        .collect();
    Ok(crate::scheme::SchemeImportResult {
        imported,
        conflicts,
        schema_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAPPY: &str = r#"
[package]
format_version = 2
kind = "schema_text"

[schema]
id = "kf"
version = "1.00.0"

[[files]]
path = "kf.schema.toml"
content = """
[schema]
id = "kf"
"""

[[files]]
path = "flypy/12_kf.dict.yaml"
content = "a\t啊\n"
"#;

    fn dest() -> (tempfile::TempDir, std::path::PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("schemas");
        std::fs::create_dir_all(&d).unwrap();
        (t, d)
    }

    #[test]
    fn happy_path_previews_and_lands_two_files() {
        let (_t, d) = dest();
        let prev = preview_import_text(HAPPY, &d).unwrap();
        assert_eq!(prev.meta.package.format_version, 2);
        assert_eq!(prev.meta.schema.id, "kf");
        assert_eq!(prev.meta.schema.version, "1.00.0");
        assert_eq!(
            prev.will_add,
            vec!["kf.schema.toml", "flypy/12_kf.dict.yaml"]
        );
        assert!(prev.conflicts.is_empty());
        assert!(prev.config_patch.is_none());

        let r = import_text(HAPPY, &d, crate::merge::Strategy::Merge).unwrap();
        assert_eq!(r.imported.len(), 2);
        assert_eq!(r.schema_ids, vec!["kf"], "根方案 id 从文件名主干取");
        assert_eq!(
            std::fs::read_to_string(d.join("kf.schema.toml")).unwrap(),
            "[schema]\nid = \"kf\"\n",
            "内容逐字落盘"
        );
        assert!(d.join("flypy/12_kf.dict.yaml").is_file(), "子目录自动建出");

        // 二次导入(Merge):全部计冲突,不覆盖——语义与 zip 版一致。
        let r2 = import_text(HAPPY, &d, crate::merge::Strategy::Merge).unwrap();
        assert!(r2.imported.is_empty());
        assert_eq!(r2.conflicts.len(), 2);
    }

    /// 缺 format_version → 硬错误(信封无 legacy),且**不带** not_schema_text 前缀:
    /// 它确实是信封,只是写坏了,回落片段管线只会给出更莫名其妙的错误。
    #[test]
    fn missing_format_version_is_hard_error() {
        let (_t, d) = dest();
        let text = "[package]\nkind = \"schema_text\"\n[[files]]\npath = \"a.schema.toml\"\ncontent = \"x\"\n";
        let err = preview_import_text(text, &d).unwrap_err().to_string();
        assert!(err.contains("format_version"), "{err}");
        assert!(
            !err.starts_with(NOT_SCHEMA_TEXT),
            "已声明 kind 就不该回落片段管线: {err}"
        );
    }

    /// 高于当前支持的版本 → 提示升级(宽容只给过去,不给未来)。
    #[test]
    fn future_format_version_is_rejected() {
        let (_t, d) = dest();
        let text = "[package]\nformat_version = 3\nkind = \"schema_text\"\n[[files]]\npath = \"a.schema.toml\"\ncontent = \"x\"\n";
        let err = preview_import_text(text, &d).unwrap_err().to_string();
        assert!(err.contains("升级"), "{err}");
    }

    /// kind 不对 / 缺失 / 根本不是 TOML → 一律带 not_schema_text: 前缀,
    /// 设置端据此回落配置片段管线。裸方案文本(有 [schema] 段)绝不能被猜成信封。
    #[test]
    fn non_envelope_text_is_flagged_for_fragment_fallback() {
        let (_t, d) = dest();
        for text in [
            "[package]\nkind = \"backup\"\n",
            "[package]\nformat_version = 2\n",
            "[schema]\nid = \"kf\"\n",
            "ui.candidate.per_page = 9\n",
            "= not toml =",
        ] {
            let err = preview_import_text(text, &d).unwrap_err().to_string();
            assert!(
                err.starts_with(NOT_SCHEMA_TEXT),
                "非信封文本须带回落前缀({text:?}): {err}"
            );
        }
    }

    /// 穿越路径逐条过 validate_entry_rel(与 zip 导入同一守卫),且拒绝后零落盘。
    #[test]
    fn traversal_paths_are_rejected_without_writing() {
        let (_t, d) = dest();
        for bad in [
            "../evil.toml",
            "C:evil.toml",
            "/etc/passwd",
            "a/../../b.toml",
        ] {
            let text = format!(
                "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
                 [[files]]\npath = \"ok.schema.toml\"\ncontent = \"x\"\n\
                 [[files]]\npath = \"{bad}\"\ncontent = \"x\"\n"
            );
            assert!(
                preview_import_text(&text, &d).is_err(),
                "穿越路径应拒绝: {bad}"
            );
            assert!(import_text(&text, &d, crate::merge::Strategy::Merge).is_err());
        }
        assert!(!d.join("ok.schema.toml").exists(), "拒绝的信封不得落盘");
    }

    #[test]
    fn duplicate_paths_are_rejected() {
        let (_t, d) = dest();
        let text = "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
                    [[files]]\npath = \"a.schema.toml\"\ncontent = \"1\"\n\
                    [[files]]\npath = \"a.schema.toml\"\ncontent = \"2\"\n";
        let err = preview_import_text(text, &d).unwrap_err().to_string();
        assert!(err.contains("重复路径"), "{err}");
    }

    /// 根下无 *.schema.toml → 不是有效信封(与 zip 侦测规则同构)。
    /// 注意 config_patch 不算方案文件:纯配置包不走信封这条路。
    #[test]
    fn envelope_without_root_schema_is_rejected() {
        let (_t, d) = dest();
        let nested = "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
                      [[files]]\npath = \"sub/a.schema.toml\"\ncontent = \"x\"\n";
        assert!(
            preview_import_text(nested, &d).is_err(),
            "子目录方案文件不算根"
        );
        let only_patch = format!(
            "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
             [[files]]\npath = \"{CONFIG_PATCH_NAME}\"\ncontent = \"ui.candidate.per_page = 9\"\n"
        );
        assert!(preview_import_text(&only_patch, &d).is_err());
        let empty = "[package]\nformat_version = 2\nkind = \"schema_text\"\n";
        assert!(preview_import_text(empty, &d).is_err(), "无 files 也应拒绝");
    }

    /// 反斜杠路径归一化后不算根条目 → 该信封缺根方案文件被拒。
    /// 拿未归一化的原始切片判根会让 `sub\x.schema.toml` 冒充根方案文件。
    #[test]
    fn backslash_path_does_not_count_as_root_schema() {
        let (_t, d) = dest();
        let text = "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
                    [[files]]\npath = \"sub\\\\x.schema.toml\"\ncontent = \"x\"\n";
        let err = preview_import_text(text, &d).unwrap_err().to_string();
        assert!(err.contains("缺少 *.schema.toml"), "{err}");

        // 对照:同一个信封补一个真正的根方案文件 → 通过,且反斜杠条目落盘用 `/` 形态。
        let ok = "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
                  [[files]]\npath = \"r.schema.toml\"\ncontent = \"x\"\n\
                  [[files]]\npath = \"sub\\\\x.dict.yaml\"\ncontent = \"y\"\n";
        let prev = preview_import_text(ok, &d).unwrap();
        assert_eq!(prev.will_add, vec!["r.schema.toml", "sub/x.dict.yaml"]);
        import_text(ok, &d, crate::merge::Strategy::Merge).unwrap();
        assert!(d.join("sub").join("x.dict.yaml").is_file());
    }

    /// 超 2 MB → 硬错误(不带回落前缀),且在**解析之前**就拒掉。
    /// 片段是手写配置、KB 量级,超限文本几乎必是超大信封;回落片段管线只会刷一屏
    /// 「未知配置键」噪音。
    #[test]
    fn oversized_text_is_rejected_before_parsing() {
        let (_t, d) = dest();
        // 一形:合法信封但内容超限
        let mut big = String::from(
            "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
             [[files]]\npath = \"b.schema.toml\"\ncontent = \"",
        );
        big.push_str(&"x".repeat(MAX_TEXT_BYTES + 1));
        big.push_str("\"\n");
        let err = preview_import_text(&big, &d).unwrap_err().to_string();
        assert!(err.contains("过大"), "{err}");
        assert!(
            !err.starts_with(NOT_SCHEMA_TEXT),
            "超限是硬错误,不回落: {err}"
        );

        // 二形:纯垃圾(连 TOML 都不是)——同样按超限拒绝,证明限额先于解析。
        let garbage = "\u{4e00}".repeat(MAX_TEXT_BYTES); // 每字 3 字节,必定超限
        let err2 = preview_import_text(&garbage, &d).unwrap_err().to_string();
        assert!(err2.contains("过大"), "限额须先于 TOML 解析: {err2}");
        assert!(!err2.starts_with(NOT_SCHEMA_TEXT), "{err2}");

        // 零落盘
        assert!(import_text(&big, &d, crate::merge::Strategy::Merge).is_err());
        assert!(!d.join("b.schema.toml").exists());
    }

    #[test]
    fn too_many_files_is_rejected() {
        let (_t, d) = dest();
        let mut text = String::from("[package]\nformat_version = 2\nkind = \"schema_text\"\n");
        text.push_str("[[files]]\npath = \"a.schema.toml\"\ncontent = \"x\"\n");
        for i in 0..MAX_FILES {
            text.push_str(&format!(
                "[[files]]\npath = \"d/{i}.txt\"\ncontent = \"x\"\n"
            ));
        }
        let err = preview_import_text(&text, &d).unwrap_err().to_string();
        assert!(err.contains("文件过多"), "{err}");
    }

    /// 信封的说明元信息:与 zip 包同名同义、同一份校验。缺省即空串(不报错)。
    #[test]
    fn envelope_carries_package_info() {
        let (_t, d) = dest();
        let text = "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
                    title = \"快符方案\"\n\
                    description = \"\"\"\n分号引导的符号表。\n含常用标点。\n\"\"\"\n\
                    [[files]]\npath = \"kf.schema.toml\"\ncontent = \"x\"\n";
        let prev = preview_import_text(text, &d).unwrap();
        assert_eq!(prev.meta.package.title, "快符方案");
        assert_eq!(
            prev.meta.package.description,
            "分号引导的符号表。\n含常用标点。"
        );

        // 缺省 → 空串,不报错(HAPPY 没写这两个字段)。
        let prev2 = preview_import_text(HAPPY, &d).unwrap();
        assert!(prev2.meta.package.title.is_empty());
        assert!(prev2.meta.package.description.is_empty());
    }

    /// 说明非法 → 硬错误(与 format_version 门禁同层),且**不带**回落前缀:
    /// 它确实是信封,只是说明写坏了。零落盘。
    #[test]
    fn invalid_envelope_info_is_rejected() {
        let (_t, d) = dest();
        let long_title = "字".repeat(wind_config::patch::INFO_TITLE_MAX_CHARS + 1);
        for info in [
            "title = \"第一行\\n第二行\"".to_string(),
            format!("title = \"{long_title}\""),
            "description = \"说明\\u0007\"".to_string(),
        ] {
            let text = format!(
                "[package]\nformat_version = 2\nkind = \"schema_text\"\n{info}\n\
                 [[files]]\npath = \"bad.schema.toml\"\ncontent = \"x\"\n"
            );
            let err = preview_import_text(&text, &d).unwrap_err().to_string();
            assert!(err.contains("package."), "错误须点名字段: {err}");
            assert!(
                !err.starts_with(NOT_SCHEMA_TEXT),
                "已声明 kind 就不回落: {err}"
            );
            assert!(import_text(&text, &d, crate::merge::Strategy::Merge).is_err());
        }
        assert!(!d.join("bad.schema.toml").exists(), "拒绝的信封不得落盘");
    }

    /// config_patch.toml 作 files 条目:不落盘,原文随预览返回(语义同 zip 包)。
    #[test]
    fn config_patch_entry_is_returned_not_written() {
        let (_t, d) = dest();
        let text = format!(
            "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
             [[files]]\npath = \"kf.schema.toml\"\ncontent = \"[schema]\\nid = 'kf'\\n\"\n\
             [[files]]\npath = \"{CONFIG_PATCH_NAME}\"\ncontent = \"ui.candidate.per_page = 9\\n\"\n"
        );
        let prev = preview_import_text(&text, &d).unwrap();
        assert_eq!(prev.will_add, vec!["kf.schema.toml"], "片段不进文件清单");
        assert_eq!(
            prev.config_patch.as_deref(),
            Some("ui.candidate.per_page = 9\n")
        );
        let r = import_text(&text, &d, crate::merge::Strategy::Merge).unwrap();
        assert_eq!(r.imported, vec!["kf.schema.toml"]);
        assert!(
            !d.join(CONFIG_PATCH_NAME).exists(),
            "配置片段不是方案资源,不得落盘"
        );
    }
}
