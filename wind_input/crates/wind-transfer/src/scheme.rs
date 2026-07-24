//! 方案包(scheme package)导出/导入 —— v2 简化格式。
//!
//! zip 条目名 = schemas 根相对路径(无目录前缀),导入零改写:
//! ```text
//! package.toml        可选元信息(TOML;导出恒写,导入不强制——兼容手工打包)
//! my.schema.toml      方案文件(根条目,识别方案 id 的依据)
//! my/main.dict.yaml   引用资源
//! ```
//! 三分类:用户目录命中→打包;系统目录命中→记 system 引用;均无→记 missing。
//! 不再使用 bundle 层 manifest(方案文件自身已含 id/版本等大部分信息)。
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 包内元信息文件名(可选条目)。
pub const PACKAGE_META_NAME: &str = "package.toml";

/// 方案包元信息(package.toml)。全部字段可缺省——导入端拿不到就显示"未知"。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageMeta {
    #[serde(default)]
    pub package: PackageInfo,
    #[serde(default)]
    pub schema: PackageSchemaInfo,
    #[serde(default)]
    pub refs: PackageRefs,
}

/// 导出环境信息。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageInfo {
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub created_at: String,
}

/// 根方案标识(与 zip 内 .schema.toml 冗余,便于免解压显示)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageSchemaInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub version: String,
}

/// 引用清单:系统种子引用(不打包)与导出时即缺失的引用。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageRefs {
    #[serde(default)]
    pub system: Vec<String>,
    #[serde(default)]
    pub missing: Vec<String>,
}

/// 收集计划:待打包文件(schemas 相对路径, 源绝对路径)、系统引用、缺失、
/// 涉及的方案 id(根在前)、根方案版本(schema.toml 未标注则空)。
pub struct CollectPlan {
    pub pack: Vec<(String, PathBuf)>,
    pub system_refs: Vec<String>,
    pub missing: Vec<String>,
    pub schema_ids: Vec<String>,
    pub root_version: String,
}

/// 单个相对路径的三分类结果。System 携带源绝对路径,自包含导出需据此读取打包。
enum Located {
    User(PathBuf),
    System(PathBuf),
    Missing,
}

fn locate(rel: &str, user_dir: &Path, system_dir: Option<&Path>) -> Located {
    let u = user_dir.join(rel);
    if u.is_file() {
        return Located::User(u);
    }
    if let Some(s) = system_dir {
        let sp = s.join(rel);
        if sp.is_file() {
            return Located::System(sp);
        }
    }
    Located::Missing
}

/// `wubi86/x.dict.yaml` → `wubi86/x.wdat`（包内相对路径版）。
///
/// 与 `wind_dict::cached::wdat_sibling` 是同一套命名约定（剥掉整个 `.dict.yaml`），但
/// **刻意不复用它**：那个函数走 `Path`，Windows 上会把分隔符规范化成 `\`；而包内相对
/// 路径必须保持 `/`——zip 规范如此，跨平台导入也依赖它。故这里做纯字符串处理，原样
/// 保留分隔符。两处若要改约定需同步。
fn wdat_rel(rel: &str) -> Option<String> {
    let stem = rel
        .strip_suffix(".dict.yaml")
        .or_else(|| rel.strip_suffix(".yaml"))?;
    Some(format!("{stem}.wdat"))
}

/// 资源定位：先按 `rel` 本身找，未命中再按 wdat-only 约定找同名 `.wdat`。
///
/// 返回**实际命中的相对路径**——wdat-only 时它被改写成 `.wdat`。这一步不能省：包内按
/// 相对路径直存，若仍记 `.dict.yaml`，导入端就会还原出一个名为 yaml、内容却是二进制的
/// 文件，两头都读不了。
///
/// 非词库资源（字根字体、shuangpin toml 等）不受影响——`wdat_sibling` 对非 yaml 后缀
/// 返回 `None`，直接落回原结果。
fn locate_resource(rel: &str, user_dir: &Path, system_dir: Option<&Path>) -> (String, Located) {
    match locate(rel, user_dir, system_dir) {
        Located::Missing => {
            if let Some(w) = wdat_rel(rel) {
                match locate(&w, user_dir, system_dir) {
                    Located::Missing => {}
                    hit => return (w, hit),
                }
            }
            // 仍未命中：用原始 rel 上报，缺失提示才对得上用户配置里写的路径
            (rel.to_string(), Located::Missing)
        }
        hit => (rel.to_string(), hit),
    }
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
/// 包必须自含方案文件);引用的用户方案递归(visited 防环)。
///
/// `include_system` 控制系统目录命中的资源/子方案如何处理:
/// - `true`(自包含导出):一并读源打包 → 产出的包在任何机器上都完整可用,不依赖目标机内置文件。
/// - `false`(删除路径复用):系统命中只记 `system_refs` 且子方案不递归,维持"系统文件永不触碰"的语义。
pub fn collect_package_files(
    id: &str,
    user_dir: &Path,
    system_dir: Option<&Path>,
    include_system: bool,
) -> anyhow::Result<CollectPlan> {
    let mut plan = CollectPlan {
        pack: Vec::new(),
        system_refs: Vec::new(),
        missing: Vec::new(),
        schema_ids: Vec::new(),
        root_version: String::new(),
    };
    let mut visited: HashSet<String> = HashSet::new();
    collect_into(
        id,
        true,
        include_system,
        user_dir,
        system_dir,
        &mut plan,
        &mut visited,
    )?;
    Ok(plan)
}

fn collect_into(
    id: &str,
    is_root: bool,
    include_system: bool,
    user_dir: &Path,
    system_dir: Option<&Path>,
    plan: &mut CollectPlan,
    visited: &mut HashSet<String>,
) -> anyhow::Result<()> {
    if !visited.insert(id.to_string()) {
        return Ok(()); // 防环
    }
    let schema_rel = format!("{id}.schema.toml");
    // 方案文件解析:用户优先;根方案系统命中必打包(包自含);非根系统命中在自包含模式下也
    // 打包并继续递归其资源,否则只记引用不递归(删除路径:系统文件永不触碰)。
    let schema_abs = match locate(&schema_rel, user_dir, system_dir) {
        Located::User(p) => p,
        Located::System(p) => {
            if is_root || include_system {
                p
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
    plan.pack.push((schema_rel, schema_abs.clone()));

    let text = std::fs::read_to_string(&schema_abs)?;
    let schema: wind_config::schema::Schema = toml::from_str(&text)?;
    if is_root {
        plan.root_version = schema.schema.version.clone();
    }

    for rel in resource_rels(&schema) {
        // 词库可能是 wdat-only（只有编译好的 .wdat、无 .dict.yaml 源），实际打包的相对
        // 路径要以命中者为准，见 locate_resource。
        let (rel, located) = locate_resource(&rel, user_dir, system_dir);
        match located {
            Located::User(p) => plan.pack.push((rel, p)),
            // 自包含导出:系统词库/拆字库/字体也读源打包;删除路径:只记引用不打包。
            Located::System(p) => {
                if include_system {
                    plan.pack.push((rel, p));
                } else {
                    plan.system_refs.push(rel);
                }
            }
            Located::Missing => plan.missing.push(rel),
        }
    }
    // mixed 引用方案:用户命中→递归;系统命中→自包含模式递归打包,否则记引用;缺失→记缺失。
    let mixed = &schema.engine.mixed;
    for sub in [&mixed.primary_schema, &mixed.secondary_schema] {
        if !sub.is_empty() {
            collect_into(
                sub,
                false,
                include_system,
                user_dir,
                system_dir,
                plan,
                visited,
            )?;
        }
    }
    Ok(())
}

/// 导出结果(RPC 直接序列化消费)。packed 为 schemas 相对路径。
pub struct SchemeExportResult {
    pub path: PathBuf,
    pub packed: Vec<String>,
    pub system_refs: Vec<String>,
    pub missing: Vec<String>,
}

/// 导出方案包:收集 → 写 zip(package.toml 元信息 + 各文件按 schemas 相对路径直存)。
pub fn export_package(
    id: &str,
    user_dir: &Path,
    system_dir: Option<&Path>,
    out_path: &Path,
    app_version: &str,
    platform: &str,
    created_at: &str,
) -> anyhow::Result<SchemeExportResult> {
    // 自包含导出:内置(系统目录)词库/拆字/字体一并打包,产出的包脱离目标机内置文件也完整可用。
    let plan = collect_package_files(id, user_dir, system_dir, true)?;
    let meta = PackageMeta {
        package: PackageInfo {
            app_version: app_version.to_string(),
            platform: platform.to_string(),
            created_at: created_at.to_string(),
        },
        schema: PackageSchemaInfo {
            id: id.to_string(),
            version: plan.root_version.clone(),
        },
        refs: PackageRefs {
            system: plan.system_refs.clone(),
            missing: plan.missing.clone(),
        },
    };

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(out_path)?;
    let mut w = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    w.start_file(PACKAGE_META_NAME, opts)?;
    w.write_all(toml::to_string_pretty(&meta)?.as_bytes())?;
    let mut packed = Vec::new();
    for (rel, src) in &plan.pack {
        w.start_file(rel, opts)?;
        w.write_all(&std::fs::read(src)?)?;
        packed.push(rel.clone());
    }
    w.finish()?;
    Ok(SchemeExportResult {
        path: out_path.to_path_buf(),
        packed,
        system_refs: plan.system_refs,
        missing: plan.missing,
    })
}

/// 枚举包内载荷条目(排除 package.toml、目录项),逐条过穿越守卫。
/// 校验根下至少一个 `*.schema.toml`;对备份包/旧格式给出针对性错误。
fn list_payload_entries(package: &Path) -> anyhow::Result<Vec<String>> {
    let file = std::fs::File::open(package)?;
    let archive = zip::ZipArchive::new(file)?;
    let mut rels = Vec::new();
    let mut has_root_schema = false;
    for name in archive.file_names() {
        if name == PACKAGE_META_NAME || name.ends_with('/') {
            continue;
        }
        if name == "manifest.toml" || name == "manifest.json" {
            anyhow::bail!("该文件是整机备份包或旧格式归档,不是方案包");
        }
        let rel = crate::bundle::validate_entry_rel(name, "")?;
        if !rel.contains('/') && rel.ends_with(".schema.toml") {
            has_root_schema = true;
        }
        rels.push(rel.to_string());
    }
    if !has_root_schema {
        anyhow::bail!("不是有效的方案包(根目录缺少 *.schema.toml)");
    }
    rels.sort();
    Ok(rels)
}

/// 读取包内可选元信息(缺失/解析失败均回落默认值——导入不强制 package.toml)。
pub fn read_package_meta(package: &Path) -> PackageMeta {
    crate::bundle::extract_entry(package, PACKAGE_META_NAME)
        .ok()
        .and_then(|bytes| toml::from_str(&String::from_utf8_lossy(&bytes)).ok())
        .unwrap_or_default()
}

/// 从条目清单提取方案 id(根下 `*.schema.toml` 的文件名主干)。
fn schema_ids_of(rels: &[String]) -> Vec<String> {
    rels.iter()
        .filter(|r| !r.contains('/'))
        .filter_map(|r| r.strip_suffix(".schema.toml"))
        .map(String::from)
        .collect()
}

/// 导入预览(只读):按包内条目对目标目录做存在性检查。
#[derive(Debug)]
pub struct SchemeImportPreview {
    pub meta: PackageMeta,
    pub will_add: Vec<String>,
    pub conflicts: Vec<String>,
    pub system_refs: Vec<String>,
    pub missing: Vec<String>,
}

pub fn preview_import(package: &Path, user_dir: &Path) -> anyhow::Result<SchemeImportPreview> {
    let rels = list_payload_entries(package)?;
    let meta = read_package_meta(package);
    let mut will_add = Vec::new();
    let mut conflicts = Vec::new();
    for rel in &rels {
        if user_dir.join(rel).exists() {
            conflicts.push(rel.clone());
        } else {
            will_add.push(rel.clone());
        }
    }
    let system_refs = meta.refs.system.clone();
    let missing = meta.refs.missing.clone();
    Ok(SchemeImportPreview {
        meta,
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
    let rels = list_payload_entries(package)?;
    // 读取阶段:全部载荷入内存,任何缺条目/坏条目在写盘前失败。
    let mut staged: Vec<(String, Vec<u8>)> = Vec::new();
    for rel in &rels {
        let bytes = crate::bundle::extract_entry(package, rel)?;
        staged.push((rel.clone(), bytes));
    }
    // 写入阶段:tmp+rename,Merge 跳过已存在。
    let mut imported = Vec::new();
    let mut conflicts = Vec::new();
    for (rel, bytes) in &staged {
        let target = user_dir.join(rel);
        if target.exists() && strategy == crate::merge::Strategy::Merge {
            conflicts.push(rel.clone());
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
        imported.push(rel.clone());
    }
    Ok(SchemeImportResult {
        imported,
        conflicts,
        schema_ids: schema_ids_of(&rels),
    })
}

/// 删除结果。rel 均为 schemas 根相对路径。
pub struct SchemeDeleteResult {
    /// 实际删除的文件。
    pub deleted: Vec<String>,
    /// 因被其它方案共享而保留的文件。
    pub kept_shared: Vec<String>,
    /// 方案文件被实际删除的方案 id(根在前;共享保留的子方案不计)。
    pub schema_ids: Vec<String>,
}

/// 删除用户方案 —— 镜像导入的收集逻辑:方案文件 + 引用资源 + 递归引用的用户方案。
/// 被 `keep_ids` 中任一现存方案引用的文件保留(共享检查,如混输共用的子方案/码表);
/// 系统目录文件永不删;删除后自底向上清理空目录(至 user_dir 止)。
pub fn delete_package(
    id: &str,
    user_dir: &Path,
    system_dir: Option<&Path>,
    keep_ids: &[String],
) -> anyhow::Result<SchemeDeleteResult> {
    // 删除只关心用户目录文件,系统命中记引用即可(include_system=false),避免解析系统子方案。
    let plan = collect_package_files(id, user_dir, system_dir, false)?;
    // 其余现存方案引用的文件集合(单个方案收集失败不阻断删除,跳过即可)。
    let mut kept: HashSet<String> = HashSet::new();
    for kid in keep_ids {
        if kid == id {
            continue;
        }
        if let Ok(p) = collect_package_files(kid, user_dir, system_dir, false) {
            kept.extend(p.pack.into_iter().map(|(rel, _)| rel));
        }
    }
    let mut deleted = Vec::new();
    let mut kept_shared = Vec::new();
    let mut deleted_rels: HashSet<String> = HashSet::new();
    for (rel, src) in &plan.pack {
        if !src.starts_with(user_dir) {
            continue; // 系统目录文件(如内置根方案)永不删
        }
        if kept.contains(rel) {
            kept_shared.push(rel.clone());
            continue;
        }
        std::fs::remove_file(src)?;
        deleted_rels.insert(rel.clone());
        deleted.push(rel.clone());
        // 自底向上清理空目录(remove_dir 对非空目录失败即停)。
        let mut dir = src.parent();
        while let Some(d) = dir {
            if d == user_dir || std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    let schema_ids = plan
        .schema_ids
        .iter()
        .filter(|sid| deleted_rels.contains(&format!("{sid}.schema.toml")))
        .cloned()
        .collect();
    Ok(SchemeDeleteResult {
        deleted,
        kept_shared,
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
version = "1.2"
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

    /// 手工写一个 zip(测试守卫/识别用)。
    fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, data) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    /// wdat-only 词库：方案目录里只有编译好的 .wdat、没有 .dict.yaml 源。
    /// 打包必须以 .wdat 命中并按 .wdat 的相对路径入包，否则导入端会还原出一个名为 yaml、
    /// 内容却是二进制的文件。
    #[test]
    fn collect_packs_wdat_only_dict() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fs::create_dir_all(user.join("wb")).unwrap();
        fs::create_dir_all(&system).unwrap();
        fs::write(
            user.join("wb.schema.toml"),
            r#"
[schema]
id = "wb"
[engine]
type = "codetable"
[[dictionaries]]
path = "wb/main.dict.yaml"
"#,
        )
        .unwrap();
        // 只投放 wdat，不放 yaml
        fs::write(user.join("wb/main.wdat"), b"binary").unwrap();

        let plan = collect_package_files("wb", &user, Some(&system), false).unwrap();
        let names: Vec<&str> = plan.pack.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"wb/main.wdat"),
            "wdat-only 词库须以 .wdat 路径入包，实际: {names:?}"
        );
        assert!(
            !names.contains(&"wb/main.dict.yaml"),
            "不存在的 yaml 不应入包"
        );
        assert!(
            plan.missing.is_empty(),
            "wdat 在场就不算缺失，实际: {:?}",
            plan.missing
        );
        // 入包的源路径必须指向真实文件
        let src = plan
            .pack
            .iter()
            .find(|(n, _)| n == "wb/main.wdat")
            .map(|(_, p)| p.clone())
            .unwrap();
        assert!(src.is_file());
    }

    /// yaml 与 wdat 并存时走原路径，wdat 不得抢占（正常方案行为一步不变）。
    #[test]
    fn yaml_wins_over_sibling_wdat_when_packing() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fs::create_dir_all(user.join("wb")).unwrap();
        fs::create_dir_all(&system).unwrap();
        fs::write(
            user.join("wb.schema.toml"),
            "[schema]\nid = \"wb\"\n[engine]\ntype = \"codetable\"\n[[dictionaries]]\npath = \"wb/main.dict.yaml\"\n",
        )
        .unwrap();
        fs::write(user.join("wb/main.dict.yaml"), "src").unwrap();
        fs::write(user.join("wb/main.wdat"), b"binary").unwrap();

        let plan = collect_package_files("wb", &user, Some(&system), false).unwrap();
        let names: Vec<&str> = plan.pack.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"wb/main.dict.yaml"));
        assert!(!names.contains(&"wb/main.wdat"), "yaml 在场时不应改打 wdat");
    }

    /// 非词库资源不受 wdat 探测影响：缺失仍按原相对路径上报。
    #[test]
    fn non_dict_resources_keep_original_missing_path() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        let plan = collect_package_files("my", &user, Some(&system), false).unwrap();
        assert_eq!(
            plan.missing,
            vec!["my/ghost.dict.yaml"],
            "缺失路径须保持用户配置里写的原样"
        );
    }

    #[test]
    fn collect_classifies_pack_ref_missing() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        let plan = collect_package_files("my", &user, Some(&system), false).unwrap();
        let names: Vec<&str> = plan.pack.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"my.schema.toml"), "根方案文件必打包");
        assert!(names.contains(&"my/main.dict.yaml"));
        assert!(names.contains(&"my/chaizi.txt"));
        assert_eq!(plan.system_refs, vec!["sys/shared.dict.yaml"]);
        assert_eq!(plan.missing, vec!["my/ghost.dict.yaml"]);
        assert_eq!(plan.schema_ids, vec!["my"]);
        assert_eq!(plan.root_version, "1.2", "根方案版本从 schema.toml 提取");
    }

    #[test]
    fn collect_recurses_mixed_user_schema() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        // mixed 方案引用用户方案 my + 系统方案 pinyin(引用方案文件在用户目录→打包递归;
        // 在系统目录→system_ref;均无→missing)
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
        let plan = collect_package_files("mix", &user, Some(&system), false).unwrap();
        let names: Vec<&str> = plan.pack.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"mix.schema.toml"));
        assert!(names.contains(&"my.schema.toml"), "用户引用方案递归打包");
        assert!(names.contains(&"my/main.dict.yaml"), "递归方案的资源也打包");
        assert!(
            plan.system_refs.contains(&"pinyin.schema.toml".to_string()),
            "系统引用方案只记引用"
        );
        assert_eq!(plan.schema_ids, vec!["mix", "my"]);
    }

    /// 自包含模式(include_system=true):系统目录命中的资源改为读源打包,system_refs 清空。
    /// 这正是内置方案(如 wubi86)导出时词库能进包的关键。
    #[test]
    fn collect_self_contained_packs_system_resources() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        let plan = collect_package_files("my", &user, Some(&system), true).unwrap();
        let names: Vec<&str> = plan.pack.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"sys/shared.dict.yaml"),
            "系统词库自包含时须入包,实际: {names:?}"
        );
        assert!(
            plan.system_refs.is_empty(),
            "自包含时无系统引用,实际: {:?}",
            plan.system_refs
        );
        assert_eq!(plan.missing, vec!["my/ghost.dict.yaml"], "真缺失仍上报");
        // 入包的系统源路径必须指向真实文件
        let src = plan
            .pack
            .iter()
            .find(|(n, _)| n == "sys/shared.dict.yaml")
            .map(|(_, p)| p.clone())
            .unwrap();
        assert!(src.starts_with(&system) && src.is_file());
    }

    /// 自包含模式下,混输引用的系统子方案(及其资源)一并递归打包,而非只记引用。
    #[test]
    fn collect_self_contained_recurses_system_subschema() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        fs::create_dir_all(system.join("pinyin")).unwrap();
        fs::write(
            user.join("mix.schema.toml"),
            "[schema]\nid=\"mix\"\n[engine]\ntype=\"mixed\"\n[engine.mixed]\nprimary_schema=\"my\"\nsecondary_schema=\"pinyin\"\n",
        )
        .unwrap();
        fs::write(
            system.join("pinyin.schema.toml"),
            "[schema]\nid=\"pinyin\"\n[engine]\ntype=\"pinyin\"\n[[dictionaries]]\npath=\"pinyin/main.dict.yaml\"\n",
        )
        .unwrap();
        fs::write(system.join("pinyin/main.dict.yaml"), "py").unwrap();

        let plan = collect_package_files("mix", &user, Some(&system), true).unwrap();
        let names: Vec<&str> = plan.pack.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"pinyin.schema.toml"), "系统子方案文件入包");
        assert!(
            names.contains(&"pinyin/main.dict.yaml"),
            "系统子方案的词库也入包,实际: {names:?}"
        );
        assert!(plan.system_refs.is_empty(), "自包含无系统引用");
        assert_eq!(plan.schema_ids, vec!["mix", "my", "pinyin"]);
    }

    #[test]
    fn export_package_writes_flat_layout_and_meta() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        let out = t.path().join("my.zip");
        let r = export_package("my", &user, Some(&system), &out, "1.0.0", "windows", "t").unwrap();
        assert_eq!(r.path, out);
        // 自包含导出:用户资源 3 个 + 系统 sys/shared.dict.yaml 一并打包 = 4;系统引用清空。
        assert_eq!(r.packed.len(), 4);
        assert!(r.system_refs.is_empty(), "自包含导出无系统引用");
        assert_eq!(r.missing.len(), 1);
        // 零层级布局:条目名即 schemas 相对路径
        let bytes = crate::bundle::extract_entry(&out, "my/main.dict.yaml").unwrap();
        assert_eq!(bytes, b"d");
        assert!(crate::bundle::extract_entry(&out, "my.schema.toml").is_ok());
        // 系统词库内容确实进了包
        assert_eq!(
            crate::bundle::extract_entry(&out, "sys/shared.dict.yaml").unwrap(),
            b"s"
        );
        // package.toml 元信息完备
        let meta = read_package_meta(&out);
        assert_eq!(meta.package.app_version, "1.0.0");
        assert_eq!(meta.schema.id, "my");
        assert_eq!(meta.schema.version, "1.2");
        assert!(meta.refs.system.is_empty(), "自包含导出无系统引用");
        assert_eq!(meta.refs.missing, vec!["my/ghost.dict.yaml"]);
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
        // 自包含包含系统词库 → 4 个条目全部待新增,无系统引用。
        assert_eq!(prev.will_add.len(), 4);
        assert!(prev.conflicts.is_empty());
        assert!(prev.system_refs.is_empty());
        assert_eq!(prev.meta.schema.id, "my");

        let r = import_package(&out, &dest, crate::merge::Strategy::Merge).unwrap();
        assert_eq!(r.imported.len(), 4);
        assert!(r.conflicts.is_empty());
        assert_eq!(r.schema_ids, vec!["my"]);
        assert!(dest.join("my.schema.toml").is_file());
        // 系统词库随包落进目标用户目录
        assert!(dest.join("sys/shared.dict.yaml").is_file());
        assert!(
            !dest.join(PACKAGE_META_NAME).exists(),
            "package.toml 是元信息,不落盘"
        );
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
        assert_eq!(prev.conflicts, vec!["my/main.dict.yaml"]);

        // Merge:跳过已存在,内容保持 OLD(自包含包 4 条,冲突 1 条 → 导入 3 条)
        let r = import_package(&out, &dest, crate::merge::Strategy::Merge).unwrap();
        assert_eq!(r.conflicts, vec!["my/main.dict.yaml"]);
        assert_eq!(r.imported.len(), 3);
        assert_eq!(
            std::fs::read(dest.join("my/main.dict.yaml")).unwrap(),
            b"OLD"
        );

        // Replace:覆盖为包内内容(4 条全写)
        let r2 = import_package(&out, &dest, crate::merge::Strategy::Replace).unwrap();
        assert_eq!(r2.imported.len(), 4);
        assert!(r2.conflicts.is_empty());
        assert_eq!(std::fs::read(dest.join("my/main.dict.yaml")).unwrap(), b"d");
    }

    #[test]
    fn import_without_package_meta_still_works() {
        // 手工打包(无 package.toml)也能导入——方案文件本身即信息源。
        let t = tempfile::tempdir().unwrap();
        let pkg = t.path().join("hand.zip");
        write_zip(
            &pkg,
            &[
                ("hand.schema.toml", b"[schema]\nid=\"hand\"\n".as_slice()),
                ("hand/d.dict.yaml", b"d".as_slice()),
            ],
        );
        let dest = t.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let prev = preview_import(&pkg, &dest).unwrap();
        assert_eq!(prev.will_add.len(), 2);
        assert!(prev.meta.package.created_at.is_empty(), "无元信息回落默认");
        let r = import_package(&pkg, &dest, crate::merge::Strategy::Merge).unwrap();
        assert_eq!(r.schema_ids, vec!["hand"]);
        assert!(dest.join("hand.schema.toml").is_file());
    }

    #[test]
    fn delete_package_removes_exclusive_keeps_shared() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system); // my: my/main.dict.yaml + my/chaizi.txt 独享
        // other 方案与 my 共享一个码表文件
        fs::create_dir_all(user.join("shared")).unwrap();
        fs::write(user.join("shared/common.dict.yaml"), "common").unwrap();
        fs::write(
            user.join("other.schema.toml"),
            "[schema]\nid=\"other\"\n[engine]\ntype=\"codetable\"\n[[dictionaries]]\npath=\"shared/common.dict.yaml\"\n",
        )
        .unwrap();
        // my 也引用 shared/common:追加进 my 的 dictionaries
        let my_toml = fs::read_to_string(user.join("my.schema.toml")).unwrap();
        fs::write(
            user.join("my.schema.toml"),
            format!("{my_toml}[[dictionaries]]\npath = \"shared/common.dict.yaml\"\n"),
        )
        .unwrap();

        let keep = vec!["other".to_string()];
        let r = delete_package("my", &user, Some(&system), &keep).unwrap();
        assert!(r.deleted.contains(&"my.schema.toml".to_string()));
        assert!(r.deleted.contains(&"my/main.dict.yaml".to_string()));
        assert!(
            r.kept_shared
                .contains(&"shared/common.dict.yaml".to_string()),
            "共享文件保留"
        );
        assert_eq!(r.schema_ids, vec!["my"]);
        assert!(!user.join("my.schema.toml").exists());
        assert!(!user.join("my").exists(), "独享资源目录删空后应移除");
        assert!(
            user.join("shared/common.dict.yaml").is_file(),
            "共享文件仍在"
        );
        assert!(user.join("other.schema.toml").is_file(), "其它方案不受影响");
    }

    #[test]
    fn delete_package_mixed_recurses_and_never_touches_system() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        fs::write(
            user.join("mix.schema.toml"),
            "[schema]\nid=\"mix\"\n[engine]\ntype=\"mixed\"\n[engine.mixed]\nprimary_schema=\"my\"\nsecondary_schema=\"pinyin\"\n",
        )
        .unwrap();
        fs::write(
            system.join("pinyin.schema.toml"),
            "[schema]\nid=\"pinyin\"\n",
        )
        .unwrap();

        // 无其它方案共享 → mix 及其递归引用的用户方案 my 一并删除
        let r = delete_package("mix", &user, Some(&system), &[]).unwrap();
        assert!(r.deleted.contains(&"mix.schema.toml".to_string()));
        assert!(
            r.deleted.contains(&"my.schema.toml".to_string()),
            "递归引用的用户方案一并删"
        );
        assert_eq!(r.schema_ids, vec!["mix", "my"]);
        assert!(!user.join("my").exists());
        assert!(
            system.join("pinyin.schema.toml").is_file(),
            "系统目录文件永不删"
        );
        assert!(
            system.join("sys/shared.dict.yaml").is_file(),
            "系统资源永不删"
        );
    }

    #[test]
    fn delete_package_keeps_subschema_shared_by_other_mixed() {
        let t = tempfile::tempdir().unwrap();
        let (user, system) = (t.path().join("u"), t.path().join("s"));
        fixture(&user, &system);
        // 两个混输方案都引用用户方案 my:删 mix1 时 my 因被 mix2 共享而保留
        for m in ["mix1", "mix2"] {
            fs::write(
                user.join(format!("{m}.schema.toml")),
                format!(
                    "[schema]\nid=\"{m}\"\n[engine]\ntype=\"mixed\"\n[engine.mixed]\nprimary_schema=\"my\"\n"
                ),
            )
            .unwrap();
        }
        let keep = vec!["mix2".to_string()];
        let r = delete_package("mix1", &user, Some(&system), &keep).unwrap();
        assert!(r.deleted.contains(&"mix1.schema.toml".to_string()));
        assert!(
            r.kept_shared.contains(&"my.schema.toml".to_string()),
            "被 mix2 共享的子方案保留"
        );
        assert_eq!(r.schema_ids, vec!["mix1"], "只有 mix1 计入被删方案");
        assert!(user.join("my.schema.toml").is_file());
        assert!(
            user.join("my/main.dict.yaml").is_file(),
            "共享子方案的资源保留"
        );
    }

    #[test]
    fn import_rejects_path_traversal_and_wrong_kind() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();

        // 含 .. 条目应拒绝
        let bad = t.path().join("bad.zip");
        write_zip(
            &bad,
            &[
                ("my.schema.toml", b"x".as_slice()),
                ("../evil.toml", b"x".as_slice()),
            ],
        );
        assert!(preview_import(&bad, &dest).is_err(), "含 .. 条目应拒绝");
        assert!(import_package(&bad, &dest, crate::merge::Strategy::Merge).is_err());

        // Windows 盘符相对路径(C:foo):is_absolute()==false 但 join 会丢 base,必须拦。
        let bad2 = t.path().join("bad2.zip");
        write_zip(
            &bad2,
            &[
                ("my.schema.toml", b"x".as_slice()),
                ("C:evil.toml", b"x".as_slice()),
            ],
        );
        assert!(preview_import(&bad2, &dest).is_err(), "盘符相对路径应拒绝");
        assert!(import_package(&bad2, &dest, crate::merge::Strategy::Merge).is_err());

        // 根下无 *.schema.toml → 不是方案包
        let notpkg = t.path().join("not.zip");
        write_zip(&notpkg, &[("readme.txt", b"x".as_slice())]);
        assert!(preview_import(&notpkg, &dest).is_err());

        // 备份包(含 manifest.toml)→ 针对性报错
        let backup = t.path().join("backup.zip");
        write_zip(
            &backup,
            &[
                (
                    "manifest.toml",
                    b"format = \"windinput-bundle\"\n".as_slice(),
                ),
                ("config/config.toml", b"x".as_slice()),
            ],
        );
        let err = preview_import(&backup, &dest).unwrap_err().to_string();
        assert!(err.contains("备份包"), "误选备份包应有针对性提示: {err}");
    }
}
