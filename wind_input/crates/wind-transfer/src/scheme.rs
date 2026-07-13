//! 方案包(scheme package)导出/导入:方案 .schema.toml + 引用资源打包为自描述 zip。
//! 路径规则:zip 条目名 = "schemas/" + schemas 根相对路径,导入零改写。
//! 三分类:用户目录命中→打包;系统目录命中→system_ref(只记引用);均无→missing。
use crate::bundle::{BundleKind, BundleWriter, Manifest};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 收集计划:待打包文件(zip 名, 源绝对路径)、系统引用、缺失、涉及的方案 id(根在前)。
pub struct CollectPlan {
    pub pack: Vec<(String, PathBuf)>,
    pub system_refs: Vec<String>,
    pub missing: Vec<String>,
    pub schema_ids: Vec<String>,
}

/// 单个相对路径的三分类结果。
enum Located {
    User(PathBuf),
    System,
    Missing,
}

fn locate(rel: &str, user_dir: &Path, system_dir: Option<&Path>) -> Located {
    let u = user_dir.join(rel);
    if u.is_file() {
        return Located::User(u);
    }
    if let Some(s) = system_dir {
        if s.join(rel).is_file() {
            return Located::System;
        }
    }
    Located::Missing
}

/// 从一个已解析 Schema 提取其资源相对路径(不含方案文件本身、不含引用方案)。
fn resource_rels(schema: &wind_config::schema::Schema) -> Vec<String> {
    let mut rels = Vec::new();
    for d in &schema.dictionaries {
        if !d.path.is_empty() {
            rels.push(d.path.clone());
        }
    }
    let cz = &schema.engine.chaizi;
    if !cz.db_path.is_empty() {
        rels.push(cz.db_path.clone());
    }
    if !cz.font_path.is_empty() {
        rels.push(cz.font_path.clone());
    }
    let py = &schema.engine.pinyin;
    if !py.unigram_path.is_empty() {
        rels.push(py.unigram_path.clone());
    }
    if !py.shuangpin.layout.is_empty() {
        rels.push(format!("shuangpin/{}.toml", py.shuangpin.layout));
    }
    rels
}

/// 收集方案 `id` 的打包计划。根方案文件必打包(用户目录优先解析,系统命中也打包——
/// 包必须自含方案文件);资源与 mixed 引用方案按三分类;引用的用户方案递归(visited 防环)。
pub fn collect_package_files(
    id: &str,
    user_dir: &Path,
    system_dir: Option<&Path>,
) -> anyhow::Result<CollectPlan> {
    let mut plan = CollectPlan {
        pack: Vec::new(),
        system_refs: Vec::new(),
        missing: Vec::new(),
        schema_ids: Vec::new(),
    };
    let mut visited: HashSet<String> = HashSet::new();
    collect_into(id, true, user_dir, system_dir, &mut plan, &mut visited)?;
    Ok(plan)
}

fn collect_into(
    id: &str,
    is_root: bool,
    user_dir: &Path,
    system_dir: Option<&Path>,
    plan: &mut CollectPlan,
    visited: &mut HashSet<String>,
) -> anyhow::Result<()> {
    if !visited.insert(id.to_string()) {
        return Ok(()); // 防环
    }
    let schema_rel = format!("{id}.schema.toml");
    // 方案文件解析:用户优先;根方案系统命中也打包(包自含);非根系统命中→只记引用。
    let schema_abs = match locate(&schema_rel, user_dir, system_dir) {
        Located::User(p) => p,
        Located::System => {
            if is_root {
                system_dir.unwrap().join(&schema_rel)
            } else {
                plan.system_refs.push(schema_rel);
                return Ok(());
            }
        }
        Located::Missing => {
            if is_root {
                anyhow::bail!("方案文件不存在: {}", schema_rel);
            }
            plan.missing.push(schema_rel);
            return Ok(());
        }
    };
    plan.schema_ids.push(id.to_string());
    plan.pack
        .push((format!("schemas/{schema_rel}"), schema_abs.clone()));

    let text = std::fs::read_to_string(&schema_abs)?;
    let schema: wind_config::schema::Schema = toml::from_str(&text)?;

    for rel in resource_rels(&schema) {
        match locate(&rel, user_dir, system_dir) {
            Located::User(p) => plan.pack.push((format!("schemas/{rel}"), p)),
            Located::System => plan.system_refs.push(rel),
            Located::Missing => plan.missing.push(rel),
        }
    }
    // mixed 引用方案:用户命中→递归;系统命中→记引用;缺失→记缺失(在递归调用内处理)。
    let mixed = &schema.engine.mixed;
    for sub in [&mixed.primary_schema, &mixed.secondary_schema] {
        if !sub.is_empty() {
            collect_into(sub, false, user_dir, system_dir, plan, visited)?;
        }
    }
    Ok(())
}

/// 导出结果(RPC 直接序列化消费)。
pub struct SchemeExportResult {
    pub path: PathBuf,
    pub packed: Vec<String>,
    pub system_refs: Vec<String>,
    pub missing: Vec<String>,
}

/// 导出方案包:收集 → 写 zip(schema/resource 载荷条目 + system_ref/missing 引用条目)。
pub fn export_package(
    id: &str,
    user_dir: &Path,
    system_dir: Option<&Path>,
    out_path: &Path,
    app_version: &str,
    platform: &str,
    created_at: &str,
) -> anyhow::Result<SchemeExportResult> {
    let plan = collect_package_files(id, user_dir, system_dir)?;
    let manifest = Manifest::new(BundleKind::Scheme, app_version, platform, created_at);
    let mut w = BundleWriter::new(out_path, manifest)?;
    let mut packed = Vec::new();
    for (name, src) in &plan.pack {
        let data = std::fs::read(src)?;
        // .schema.toml 条目带 id 元数据,其余为 resource。
        let (ty, meta) = if name.ends_with(".schema.toml") {
            let id = name
                .trim_start_matches("schemas/")
                .trim_end_matches(".schema.toml");
            ("schema", serde_json::json!({ "id": id }))
        } else {
            ("resource", serde_json::Value::Null)
        };
        w.add_bytes_with(name, &data, ty, meta)?;
        packed.push(name.clone());
    }
    for r in &plan.system_refs {
        w.add_ref("system_ref", r, serde_json::Value::Null);
    }
    for m in &plan.missing {
        w.add_ref("missing", m, serde_json::Value::Null);
    }
    w.finish()?;
    Ok(SchemeExportResult {
        path: out_path.to_path_buf(),
        packed,
        system_refs: plan.system_refs,
        missing: plan.missing,
    })
}

/// 校验 zip 条目名并转为 schemas 根相对路径:必须 `schemas/` 前缀、无 `..`、非绝对。
fn entry_rel(name: &str) -> anyhow::Result<&str> {
    let rel = name
        .strip_prefix("schemas/")
        .ok_or_else(|| anyhow::anyhow!("非法条目(缺 schemas/ 前缀): {name}"))?;
    if rel.is_empty()
        || Path::new(rel).is_absolute()
        || rel.split(['/', '\\']).any(|seg| seg == "..")
    {
        anyhow::bail!("非法条目(路径穿越): {name}");
    }
    Ok(rel)
}

/// 从 manifest 取有载荷条目(schema/resource),校验条目名合法性。
fn payload_entries(manifest: &Manifest) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for e in &manifest.contents {
        if e.r#type == "schema" || e.r#type == "resource" {
            let rel = entry_rel(&e.path)?;
            out.push((e.path.clone(), rel.to_string()));
        }
    }
    Ok(out)
}

/// 导入预览(只读):按 manifest 载荷条目对目标目录做存在性检查。
pub struct SchemeImportPreview {
    pub manifest: Manifest,
    pub will_add: Vec<String>,
    pub conflicts: Vec<String>,
    pub system_refs: Vec<String>,
    pub missing: Vec<String>,
}

pub fn preview_import(package: &Path, user_dir: &Path) -> anyhow::Result<SchemeImportPreview> {
    let manifest = crate::bundle::read_manifest(package)?;
    if manifest.kind != BundleKind::Scheme {
        anyhow::bail!("不是方案包(kind={:?})", manifest.kind);
    }
    let entries = payload_entries(&manifest)?;
    let mut will_add = Vec::new();
    let mut conflicts = Vec::new();
    for (name, rel) in &entries {
        if user_dir.join(rel).exists() {
            conflicts.push(name.clone());
        } else {
            will_add.push(name.clone());
        }
    }
    let system_refs = manifest
        .contents
        .iter()
        .filter(|e| e.r#type == "system_ref")
        .map(|e| e.path.clone())
        .collect();
    let missing = manifest
        .contents
        .iter()
        .filter(|e| e.r#type == "missing")
        .map(|e| e.path.clone())
        .collect();
    Ok(SchemeImportPreview {
        manifest,
        will_add,
        conflicts,
        system_refs,
        missing,
    })
}

/// 导入结果。
pub struct SchemeImportResult {
    pub imported: Vec<String>,
    pub conflicts: Vec<String>,
    pub schema_ids: Vec<String>,
}

/// 导入方案包到用户 schemas 目录。先全部读入内存(校验期零落盘),再逐文件 tmp+rename。
/// Merge=已存在跳过(计 conflicts);Replace=覆盖。
pub fn import_package(
    package: &Path,
    user_dir: &Path,
    strategy: crate::merge::Strategy,
) -> anyhow::Result<SchemeImportResult> {
    let manifest = crate::bundle::read_manifest(package)?;
    if manifest.kind != BundleKind::Scheme {
        anyhow::bail!("不是方案包(kind={:?})", manifest.kind);
    }
    let entries = payload_entries(&manifest)?;
    // 读取阶段:全部载荷入内存,任何缺条目/坏条目在写盘前失败。
    let mut staged: Vec<(String, String, Vec<u8>)> = Vec::new(); // (zip名, rel, bytes)
    for (name, rel) in &entries {
        let bytes = crate::bundle::extract_entry(package, name)?;
        staged.push((name.clone(), rel.clone(), bytes));
    }
    // 写入阶段:tmp+rename,Merge 跳过已存在。
    let mut imported = Vec::new();
    let mut conflicts = Vec::new();
    for (name, rel, bytes) in &staged {
        let target = user_dir.join(rel);
        if target.exists() && strategy == crate::merge::Strategy::Merge {
            conflicts.push(name.clone());
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = target.with_extension("windinput.tmp");
        std::fs::write(&tmp, bytes)?;
        // Windows: rename 到已存在目标会失败,Replace 先移除旧文件。
        if target.exists() {
            std::fs::remove_file(&target)?;
        }
        std::fs::rename(&tmp, &target)?;
        imported.push(name.clone());
    }
    let schema_ids = manifest
        .contents
        .iter()
        .filter(|e| e.r#type == "schema")
        .filter_map(|e| e.meta.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect();
    Ok(SchemeImportResult {
        imported,
        conflicts,
        schema_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 造一个最小用户 schemas 目录:my 方案引用 1 个用户码表 + 1 个系统码表 + 1 个缺失文件。
    fn fixture(user: &std::path::Path, system: &std::path::Path) {
        fs::create_dir_all(user.join("my")).unwrap();
        fs::create_dir_all(system.join("sys")).unwrap();
        fs::write(
            user.join("my.schema.toml"),
            r#"
[schema]
id = "my"
[engine]
type = "codetable"
[engine.chaizi]
db_path = "my/chaizi.txt"
[[dictionaries]]
path = "my/main.dict.yaml"
[[dictionaries]]
path = "sys/shared.dict.yaml"
[[dictionaries]]
path = "my/ghost.dict.yaml"
"#,
        )
        .unwrap();
        fs::write(user.join("my/main.dict.yaml"), "d").unwrap();
        fs::write(user.join("my/chaizi.txt"), "c").unwrap();
        fs::write(system.join("sys/shared.dict.yaml"), "s").unwrap();
        // my/ghost.dict.yaml 故意不创建 → missing
    }

    #[test]
    fn collect_classifies_pack_ref_missing() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        let plan = collect_package_files("my", &user, Some(&system)).unwrap();
        let names: Vec<&str> = plan.pack.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"schemas/my.schema.toml"),
            "根方案文件必打包"
        );
        assert!(names.contains(&"schemas/my/main.dict.yaml"));
        assert!(names.contains(&"schemas/my/chaizi.txt"));
        assert_eq!(plan.system_refs, vec!["sys/shared.dict.yaml"]);
        assert_eq!(plan.missing, vec!["my/ghost.dict.yaml"]);
        assert_eq!(plan.schema_ids, vec!["my"]);
    }

    #[test]
    fn collect_recurses_mixed_user_schema() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        // mixed 方案引用用户方案 my + 系统方案 pinyin(不存在于两目录 → 系统引用按方案文件算 missing?
        // 规则:引用方案文件在用户目录→打包递归;在系统目录→system_ref;均无→missing)
        fs::write(
            user.join("mix.schema.toml"),
            r#"
[schema]
id = "mix"
[engine]
type = "mixed"
[engine.mixed]
primary_schema = "my"
secondary_schema = "pinyin"
"#,
        )
        .unwrap();
        fs::write(
            system.join("pinyin.schema.toml"),
            "[schema]\nid=\"pinyin\"\n",
        )
        .unwrap();
        let plan = collect_package_files("mix", &user, Some(&system)).unwrap();
        let names: Vec<&str> = plan.pack.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"schemas/mix.schema.toml"));
        assert!(
            names.contains(&"schemas/my.schema.toml"),
            "用户引用方案递归打包"
        );
        assert!(
            names.contains(&"schemas/my/main.dict.yaml"),
            "递归方案的资源也打包"
        );
        assert!(
            plan.system_refs.contains(&"pinyin.schema.toml".to_string()),
            "系统引用方案只记引用"
        );
        assert_eq!(plan.schema_ids, vec!["mix", "my"]);
    }

    #[test]
    fn export_package_roundtrip_manifest() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        let out = t.path().join("my.zip");
        let r = export_package("my", &user, Some(&system), &out, "1.0.0", "windows", "t").unwrap();
        assert_eq!(r.path, out);
        assert_eq!(r.packed.len(), 3);
        assert_eq!(r.system_refs.len(), 1);
        assert_eq!(r.missing.len(), 1);
        let m = crate::bundle::read_manifest(&out).unwrap();
        assert_eq!(m.kind, crate::bundle::BundleKind::Scheme);
        assert_eq!(
            m.contents.iter().filter(|e| e.r#type == "schema").count(),
            1
        );
        assert_eq!(
            m.contents
                .iter()
                .filter(|e| e.r#type == "system_ref")
                .count(),
            1
        );
        assert_eq!(
            m.contents.iter().filter(|e| e.r#type == "missing").count(),
            1
        );
        let bytes = crate::bundle::extract_entry(&out, "schemas/my/main.dict.yaml").unwrap();
        assert_eq!(bytes, b"d");
    }

    #[test]
    fn import_roundtrip_into_fresh_dir() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        let out = t.path().join("my.zip");
        export_package("my", &user, Some(&system), &out, "1.0.0", "windows", "t").unwrap();

        let dest = t.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let prev = preview_import(&out, &dest).unwrap();
        assert_eq!(prev.will_add.len(), 3);
        assert!(prev.conflicts.is_empty());
        assert_eq!(prev.system_refs.len(), 1);

        let r = import_package(&out, &dest, crate::merge::Strategy::Merge).unwrap();
        assert_eq!(r.imported.len(), 3);
        assert!(r.conflicts.is_empty());
        assert_eq!(r.schema_ids, vec!["my"]);
        assert!(dest.join("my.schema.toml").is_file());
        assert_eq!(std::fs::read(dest.join("my/main.dict.yaml")).unwrap(), b"d");
    }

    #[test]
    fn import_merge_skips_existing_replace_overwrites() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        let out = t.path().join("my.zip");
        export_package("my", &user, Some(&system), &out, "1.0.0", "windows", "t").unwrap();

        let dest = t.path().join("dest");
        std::fs::create_dir_all(dest.join("my")).unwrap();
        std::fs::write(dest.join("my/main.dict.yaml"), b"OLD").unwrap();

        // preview 报冲突
        let prev = preview_import(&out, &dest).unwrap();
        assert_eq!(prev.conflicts, vec!["schemas/my/main.dict.yaml"]);

        // Merge:跳过已存在,内容保持 OLD
        let r = import_package(&out, &dest, crate::merge::Strategy::Merge).unwrap();
        assert_eq!(r.conflicts, vec!["schemas/my/main.dict.yaml"]);
        assert_eq!(r.imported.len(), 2);
        assert_eq!(
            std::fs::read(dest.join("my/main.dict.yaml")).unwrap(),
            b"OLD"
        );

        // Replace:覆盖为包内内容
        let r2 = import_package(&out, &dest, crate::merge::Strategy::Replace).unwrap();
        assert_eq!(r2.imported.len(), 3);
        assert!(r2.conflicts.is_empty());
        assert_eq!(std::fs::read(dest.join("my/main.dict.yaml")).unwrap(), b"d");
    }

    #[test]
    fn import_rejects_path_traversal() {
        // 手工构造带非法条目名的包
        let t = tempfile::tempdir().unwrap();
        let bad = t.path().join("bad.zip");
        let manifest = crate::bundle::Manifest::new(
            crate::bundle::BundleKind::Scheme,
            "1.0.0",
            "windows",
            "t",
        );
        let mut w = crate::bundle::BundleWriter::new(&bad, manifest).unwrap();
        w.add_bytes_with(
            "schemas/../evil.toml",
            b"x",
            "resource",
            serde_json::Value::Null,
        )
        .unwrap();
        w.finish().unwrap();
        let dest = t.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        assert!(preview_import(&bad, &dest).is_err(), "含 .. 条目应拒绝");
        assert!(
            import_package(&bad, &dest, crate::merge::Strategy::Merge).is_err(),
            "导入同样拒绝"
        );
    }
}
