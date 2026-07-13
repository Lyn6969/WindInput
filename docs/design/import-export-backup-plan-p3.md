# 导入导出/备份还原 —— P3 方案包 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development(推荐)或 superpowers:executing-plans 按任务逐条实现。步骤用 `- [ ]` 复选框跟踪。

**Goal:** 交付方案包导出/导入:把一个输入方案(.schema.toml)连同其引用资源(码表/拆字表/字体/双拼布局/unigram)打成自描述 zip,可在另一台机器导入到用户方案目录并即刻可用。

**Architecture:** 核心逻辑全部在 `wind-transfer/scheme.rs`(纯函数,目录作参数,tempdir 可测);资源收集按"用户目录命中→打包 / 系统目录命中→只记引用 / 均无→记缺失"三分类,mixed 方案递归收集引用的用户方案;zip 内**保留 schemas 根相对路径**(`schemas/<rel>`),导入零路径改写。RPC 层三个薄 handlers;导入后 `invalidate_schema`,列表可见性靠 `installed_schemas()` 实时扫盘天然生效。

**Tech Stack:** Rust、zip(已有)、toml、serde、wind-config 的 `Schema` 类型、tempfile(dev)。

**关联:** 设计 `docs/design/import-export-backup-design.md`;P1/P2 计划同目录。

## Global Constraints

- 工作目录:所有 `cargo` 命令在 `wind_input/` 下执行。
- **cwd 防护(强制)**:实现者只能在指定 worktree 内操作;每次 git 写命令前 `pwd` 验证;严禁触碰 `D:/Develop/workspace/windinput/WindInput` 主仓。
- 格式化:只用 `cargo fmt -p <crate>`,严禁裸 `cargo fmt`;提交前 `git status` 核查并 restore 非本任务文件。
- 提交:conventional commit,不带 Co-Authored-By 与任何 AI trailer。TDD 先写失败测试。
- **zip 布局决策(覆盖设计文档示意)**:包内条目名 = `schemas/` + schemas 根相对路径(如 `schemas/wubi86.schema.toml`、`schemas/wubi86/xx.dict.yaml`、`schemas/shuangpin/xiaohe.toml`)。设计文档原示意(`schema/dicts/...` 重排)会破坏 schema.toml 内的相对引用,弃用;Task 4 更新设计文档。
- **三分类规则(设计既定)**:资源相对路径先在用户 schemas 目录找→命中打包(`ContentEntry.type="resource"`);否则系统 schemas 目录找→命中只记引用(`type="system_ref"`,不打包);均无→`type="missing"`。根方案与递归到的用户方案 `.schema.toml` 总是打包(`type="schema"`,meta={"id"})。mixed 引用的系统方案记 `system_ref` 不递归。
- **路径穿越防护(强制)**:导入时条目名必须以 `schemas/` 开头、不含 `..` 段、非绝对路径,违规条目直接报错。
- **导入原子性(v1 尺度)**:先把全部待写条目读入内存(校验期零落盘),再逐文件 tmp+rename 写入;写入阶段失败可接受部分完成(记录于返回值),不做已覆盖文件回滚(theme 单文件 backup 模式不扩展到多文件,记为 P5 打磨项)。
- Strategy 语义:`Merge`(默认)=目标文件已存在则跳过并计入 conflicts;`Replace`=覆盖写。

---

### Task 1: bundle API 扩展——带类型条目与无载荷引用条目

**Files:**
- Modify: `wind_input/crates/wind-transfer/src/bundle.rs`

**Interfaces:**
- Consumes: 既有 `BundleWriter{writer, manifest}`、`ContentEntry`、`MANIFEST_NAME`。
- Produces(Task 2/3 依赖):
  - `pub fn add_bytes_with(&mut self, name: &str, data: &[u8], r#type: &str, meta: serde_json::Value) -> anyhow::Result<()>`(写 zip 条目并按给定 type/meta 登记)
  - `pub fn add_ref(&mut self, r#type: &str, path: &str, meta: serde_json::Value)`(只登记 manifest,不写 zip 载荷——用于 system_ref/missing)
  - 既有 `add_bytes` 重构为 `add_bytes_with(name, data, "", Value::Null)` 的委托,外部行为不变。

- [ ] **Step 1: 写失败测试**

在 `bundle.rs` 的 `mod tests` 追加:

```rust
#[test]
fn typed_entries_and_refs_in_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let zip_path = dir.path().join("typed.zip");
    let manifest = Manifest::new(BundleKind::Scheme, "1.0.0", "windows", "t");
    let mut w = BundleWriter::new(&zip_path, manifest).unwrap();
    w.add_bytes_with(
        "schemas/my.schema.toml",
        b"[schema]\nid=\"my\"\n",
        "schema",
        serde_json::json!({ "id": "my" }),
    )
    .unwrap();
    w.add_ref("system_ref", "wubi86/wubi86_jidian.dict.yaml", serde_json::Value::Null);
    w.finish().unwrap();

    let m = read_manifest(&zip_path).unwrap();
    assert_eq!(m.contents.len(), 2);
    let schema_entry = &m.contents[0];
    assert_eq!(schema_entry.r#type, "schema");
    assert_eq!(schema_entry.meta.get("id").and_then(|v| v.as_str()), Some("my"));
    let ref_entry = &m.contents[1];
    assert_eq!(ref_entry.r#type, "system_ref");
    // ref 条目无 zip 载荷
    assert!(extract_entry(&zip_path, "wubi86/wubi86_jidian.dict.yaml").is_err());
    // 带载荷条目可取
    assert!(extract_entry(&zip_path, "schemas/my.schema.toml").is_ok());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-transfer bundle::tests::typed_entries`
Expected: 编译错误,`add_bytes_with` / `add_ref` 未定义。

- [ ] **Step 3: 实现**

在 `bundle.rs` 的 `impl BundleWriter` 中,把既有 `add_bytes` 函数体改为委托,并新增两方法:

```rust
    /// 写入一个条目,并在 manifest.contents 里登记(type/meta 由调用方细化;空 type 为 P1 兼容)。
    pub fn add_bytes(&mut self, name: &str, data: &[u8]) -> anyhow::Result<()> {
        self.add_bytes_with(name, data, "", serde_json::Value::Null)
    }

    /// 写入条目并按给定 type/meta 登记(P3:schema/resource 等类型化条目)。
    pub fn add_bytes_with(
        &mut self,
        name: &str,
        data: &[u8],
        r#type: &str,
        meta: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.writer
            .start_file(name, zip::write::SimpleFileOptions::default())?;
        self.writer.write_all(data)?;
        self.manifest.contents.push(ContentEntry {
            r#type: r#type.to_string(),
            path: name.to_string(),
            meta,
        });
        Ok(())
    }

    /// 只登记 manifest 的引用条目,不写 zip 载荷(system_ref/missing 等)。
    pub fn add_ref(&mut self, r#type: &str, path: &str, meta: serde_json::Value) {
        self.manifest.contents.push(ContentEntry {
            r#type: r#type.to_string(),
            path: path.to_string(),
            meta,
        });
    }
```

- [ ] **Step 4: 跑测试确认通过(含既有 bundle 测试回归)**

Run: `cd wind_input && cargo test -p wind-transfer bundle`
Expected: PASS(新测试 + 既有 4 个 bundle 测试全绿)。

- [ ] **Step 5: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-transfer
git add wind_input/crates/wind-transfer/src/bundle.rs
git commit -m "feat(transfer): bundle 支持类型化条目与无载荷引用条目"
```

---

### Task 2: scheme.rs——资源收集与方案包导出

**Files:**
- Create: `wind_input/crates/wind-transfer/src/scheme.rs`
- Modify: `wind_input/crates/wind-transfer/src/lib.rs`(追加 `pub mod scheme;`)

**Interfaces:**
- Consumes: Task 1 的 `BundleWriter::{new, add_bytes_with, add_ref, finish}`、`Manifest::new`、`BundleKind::Scheme`;`wind_config::schema::Schema`(serde 反序列化 TOML;字段:`schema.id`、`engine.engine_type`、`engine.chaizi.{db_path,font_path}`、`engine.pinyin.{unigram_path, shuangpin.layout}`、`engine.mixed.{primary_schema,secondary_schema}`、`dictionaries[].path`)。wind-transfer 需追加依赖 `toml = "0.8"`(Cargo.toml `[dependencies]`;若 workspace 表有 toml 则用 `{ workspace = true }`,以根 Cargo.toml 实况为准)。
- Produces(Task 3/4 依赖):
  - `pub struct CollectPlan { pub pack: Vec<(String, std::path::PathBuf)>, pub system_refs: Vec<String>, pub missing: Vec<String>, pub schema_ids: Vec<String> }`(pack 元素 = (zip 条目名, 绝对源路径))
  - `pub fn collect_package_files(id: &str, user_dir: &Path, system_dir: Option<&Path>) -> anyhow::Result<CollectPlan>`
  - `pub struct SchemeExportResult { pub path: std::path::PathBuf, pub packed: Vec<String>, pub system_refs: Vec<String>, pub missing: Vec<String> }`
  - `pub fn export_package(id: &str, user_dir: &Path, system_dir: Option<&Path>, out_path: &Path, app_version: &str, platform: &str, created_at: &str) -> anyhow::Result<SchemeExportResult>`

- [ ] **Step 1: 声明模块 + 写失败测试**

`lib.rs` 追加 `pub mod scheme;`。创建 `scheme.rs`,先写测试(fixture 辅助置于 tests 模块):

```rust
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
        assert!(names.contains(&"schemas/my.schema.toml"), "根方案文件必打包");
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
        fs::write(system.join("pinyin.schema.toml"), "[schema]\nid=\"pinyin\"\n").unwrap();
        let plan = collect_package_files("mix", &user, Some(&system)).unwrap();
        let names: Vec<&str> = plan.pack.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"schemas/mix.schema.toml"));
        assert!(names.contains(&"schemas/my.schema.toml"), "用户引用方案递归打包");
        assert!(names.contains(&"schemas/my/main.dict.yaml"), "递归方案的资源也打包");
        assert!(plan.system_refs.contains(&"pinyin.schema.toml".to_string()), "系统引用方案只记引用");
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
            m.contents.iter().filter(|e| e.r#type == "system_ref").count(),
            1
        );
        assert_eq!(m.contents.iter().filter(|e| e.r#type == "missing").count(), 1);
        let bytes =
            crate::bundle::extract_entry(&out, "schemas/my/main.dict.yaml").unwrap();
        assert_eq!(bytes, b"d");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-transfer scheme`
Expected: 编译错误,`collect_package_files` 等未定义。

- [ ] **Step 3: 实现收集与导出**

`scheme.rs` 顶部(tests 之上)实现:

```rust
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
```

`Cargo.toml` 的 `[dependencies]` 追加 `toml = "0.8"`(若根 `[workspace.dependencies]` 已有 toml 项则改用 `toml = { workspace = true }`——以实况为准,与相邻 crate 写法一致)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-transfer scheme`
Expected: PASS(3 个测试)。再跑 `cargo test -p wind-transfer` 确认 bundle/merge 无回归。

- [ ] **Step 5: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-transfer
git add wind_input/crates/wind-transfer/Cargo.toml wind_input/crates/wind-transfer/src/lib.rs wind_input/crates/wind-transfer/src/scheme.rs
git commit -m "feat(transfer): 方案包资源收集与导出(三分类+mixed递归)"
```

---

### Task 3: scheme.rs——导入预览与导入落盘

**Files:**
- Modify: `wind_input/crates/wind-transfer/src/scheme.rs`(追加导入侧)

**Interfaces:**
- Consumes: Task 2 全部;`crate::bundle::{read_manifest, extract_entry}`;`crate::merge::Strategy`。
- Produces(Task 4 依赖):
  - `pub struct SchemeImportPreview { pub manifest: crate::bundle::Manifest, pub will_add: Vec<String>, pub conflicts: Vec<String>, pub system_refs: Vec<String>, pub missing: Vec<String> }`
  - `pub fn preview_import(package: &Path, user_dir: &Path) -> anyhow::Result<SchemeImportPreview>`(只读)
  - `pub struct SchemeImportResult { pub imported: Vec<String>, pub conflicts: Vec<String>, pub schema_ids: Vec<String> }`
  - `pub fn import_package(package: &Path, user_dir: &Path, strategy: crate::merge::Strategy) -> anyhow::Result<SchemeImportResult>`

- [ ] **Step 1: 写失败测试**

`scheme.rs` tests 模块追加:

```rust
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
        assert_eq!(
            std::fs::read(dest.join("my/main.dict.yaml")).unwrap(),
            b"d"
        );
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
        assert_eq!(std::fs::read(dest.join("my/main.dict.yaml")).unwrap(), b"OLD");

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
        w.add_bytes_with("schemas/../evil.toml", b"x", "resource", serde_json::Value::Null)
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-transfer scheme::tests::import_`
Expected: 编译错误,`preview_import` / `import_package` 未定义。

- [ ] **Step 3: 实现导入侧**

`scheme.rs` 追加(export_package 之后、tests 之前):

```rust
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
fn payload_entries(
    manifest: &Manifest,
) -> anyhow::Result<Vec<(String, String)>> {
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-transfer scheme`
Expected: PASS(6 个测试:Task 2 的 3 个 + 本任务 3 个)。

- [ ] **Step 5: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-transfer
git add wind_input/crates/wind-transfer/src/scheme.rs
git commit -m "feat(transfer): 方案包导入预览与落盘(Merge跳过/Replace覆盖+穿越防护)"
```

---

### Task 4: RPC scheme.exportPackage / importPackage / previewImport

**Files:**
- Modify: `wind_input/crates/wind-coordinator/src/webdata.rs`(dispatch 三分支 + 三 handlers + 契约测试)
- Modify(条件): `wind_input/crates/wind-engine/src/manager.rs`(若 `invalidate_schema` 非 pub 则改为 pub,带 doc 注释;先查再改)
- Modify: `docs/design/import-export-backup-design.md`(方案包 zip 布局章节按"schemas 根相对路径"实况更新)

**Interfaces:**
- Consumes: Task 2/3 的 `wind_transfer::scheme::{export_package, preview_import, import_package}`;`wind_transfer::merge::Strategy`;`Config::user_config_dir()`(用户 schemas 根 = `user_config_dir()/schemas`,不存在则 `create_dir_all`)、`Config::data_dir()`(系统 schemas 根 = `data_dir()/schemas`);`self.engine_mgr.invalidate_schema(id)`。
- Produces(wind-setting 契约):
  - `scheme.exportPackage {id, path}` → `{path, packed, systemRefs, missing}`
  - `scheme.importPackage {path, strategy?}` → `{imported, conflicts, schemaIds}`
  - `scheme.previewImport {path}` → `{manifest, willAdd, conflicts, systemRefs, missing}`

- [ ] **Step 1: 确认 invalidate_schema 可见性**

Run: `cd wind_input && grep -n "fn invalidate_schema" crates/wind-engine/src/manager.rs`
若非 `pub fn`,改为 `pub fn` 并在其 doc 注释追加一行:`/// pub:方案包导入后由 RPC 层调用,失效已加载缓存(未加载时安全 no-op)。`

- [ ] **Step 2: 写失败契约测试**

`webdata.rs` 的 `mod tests` 追加(只测只读/错误路径——importPackage 的 happy path 会写真实用户目录,由 wind-transfer 单测 + 真机覆盖):

```rust
#[test]
fn scheme_package_rpc_contract() {
    let c = coord("schemepkg");
    // exportPackage:不存在的方案 id → 错误
    assert!(
        c.web_data_rpc(
            "scheme.exportPackage",
            &json!({ "id": "zz_no_such_schema", "path": std::env::temp_dir().join("zz_no.zip").to_string_lossy() }),
        )
        .is_err(),
        "不存在的方案应报错"
    );
    // previewImport:不存在的包路径 → 错误
    assert!(
        c.web_data_rpc(
            "scheme.previewImport",
            &json!({ "path": std::env::temp_dir().join("zz_no_such_pkg.zip").to_string_lossy() }),
        )
        .is_err()
    );
    // previewImport:真实构造的包 → 只读预览成功,形状正确
    let t = std::env::temp_dir().join("wind_schemepkg_test");
    let _ = std::fs::remove_dir_all(&t);
    let (user, system) = (t.join("u"), t.join("s"));
    std::fs::create_dir_all(user.join("my")).unwrap();
    std::fs::create_dir_all(&system).unwrap();
    std::fs::write(
        user.join("my.schema.toml"),
        "[schema]\nid=\"my\"\n[[dictionaries]]\npath=\"my/d.yaml\"\n",
    )
    .unwrap();
    std::fs::write(user.join("my/d.yaml"), "d").unwrap();
    let pkg = t.join("my.zip");
    wind_transfer::scheme::export_package(
        "my", &user, Some(&system), &pkg, "1.0.0", "windows", "t",
    )
    .unwrap();
    let prev = c
        .web_data_rpc("scheme.previewImport", &json!({ "path": pkg.to_string_lossy() }))
        .unwrap();
    assert!(prev.get("manifest").is_some());
    assert!(prev.get("willAdd").and_then(|v| v.as_array()).is_some());
    assert!(prev.get("conflicts").and_then(|v| v.as_array()).is_some());
    let _ = std::fs::remove_dir_all(&t);
    // importPackage:不存在的包路径 → 错误
    assert!(
        c.web_data_rpc(
            "scheme.importPackage",
            &json!({ "path": std::env::temp_dir().join("zz_no_such_pkg.zip").to_string_lossy() }),
        )
        .is_err()
    );
}
```

> 注:previewImport 对真实用户目录只做**存在性读取**,测试方案 id 取 `my`+临时包,不写用户目录,安全。

- [ ] **Step 3: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-coordinator scheme_package_rpc -- --nocapture`
Expected: FAIL,`unknown method: scheme.exportPackage`。

- [ ] **Step 4: 实现 dispatch 与 handlers**

dispatch(`web_data_rpc` 的 match)中 `"schema.references"` 分支之后追加:

```rust
            "scheme.exportPackage" => self.web_scheme_export_package(params),
            "scheme.importPackage" => self.web_scheme_import_package(params),
            "scheme.previewImport" => self.web_scheme_preview_import(params),
```

handlers 区追加(建议紧邻 `web_schema_delete` 之后):

```rust
    /// 用户 schemas 根目录(%APPDATA%/WindInput/schemas),不存在则创建。
    fn user_schemas_dir() -> anyhow::Result<std::path::PathBuf> {
        let dir = wind_config::Config::user_config_dir()
            .ok_or_else(|| anyhow::anyhow!("无用户配置目录"))?
            .join("schemas");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// 系统 schemas 根目录(<exe>/data/schemas),可能不存在(如测试环境)。
    fn system_schemas_dir() -> Option<std::path::PathBuf> {
        wind_config::Config::data_dir()
            .map(|d| d.join("schemas"))
            .filter(|d| d.is_dir())
    }

    fn web_scheme_export_package(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        let out = str_param(params, "path")?;
        let user = Self::user_schemas_dir()?;
        let system = Self::system_schemas_dir();
        let r = wind_transfer::scheme::export_package(
            id,
            &user,
            system.as_deref(),
            std::path::Path::new(out),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            &chrono::Local::now().to_rfc3339(),
        )?;
        Ok(json!({
            "path": r.path.to_string_lossy(),
            "packed": r.packed,
            "systemRefs": r.system_refs,
            "missing": r.missing,
        }))
    }

    fn web_scheme_import_package(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::merge::Strategy;
        let path = str_param(params, "path")?;
        let strategy = Strategy::from_param(
            params.get("strategy").and_then(|v| v.as_str()).unwrap_or(""),
        );
        let user = Self::user_schemas_dir()?;
        let r = wind_transfer::scheme::import_package(
            std::path::Path::new(path),
            &user,
            strategy,
        )?;
        // 覆盖已加载方案时失效缓存(新方案为安全 no-op);列表可见性由 installed_schemas 实时扫盘天然生效。
        for id in &r.schema_ids {
            self.engine_mgr.invalidate_schema(id);
        }
        Ok(json!({
            "imported": r.imported,
            "conflicts": r.conflicts,
            "schemaIds": r.schema_ids,
        }))
    }

    fn web_scheme_preview_import(&self, params: &Value) -> anyhow::Result<Value> {
        let path = str_param(params, "path")?;
        let user = Self::user_schemas_dir()?;
        let p = wind_transfer::scheme::preview_import(std::path::Path::new(path), &user)?;
        Ok(json!({
            "manifest": serde_json::to_value(&p.manifest)?,
            "willAdd": p.will_add,
            "conflicts": p.conflicts,
            "systemRefs": p.system_refs,
            "missing": p.missing,
        }))
    }
```

> 注:wind-coordinator 已依赖 wind-transfer(P2 加入),无需改 Cargo.toml。若 `invalidate_schema` 签名为 `&self` 以外形式,以实况为准适配。

- [ ] **Step 5: 跑测试确认通过(含既有 webdata 回归)**

Run: `cd wind_input && cargo test -p wind-coordinator webdata`
Expected: PASS。

- [ ] **Step 6: 设计文档布局更新**

`docs/design/import-export-backup-design.md` 的「方案包 `.zip`(kind=scheme)」小节,把布局代码块替换为:

```
manifest.json
schemas/<id>.schema.toml
schemas/<引用资源,保留 schemas 根相对路径,如 wubi86/xx.dict.yaml>
schemas/shuangpin/<布局>.toml     (若引用自定义双拼布局)
```

并在该小节资源收集段落末尾追加一句:`zip 内条目保留 schemas 根相对路径(而非按类型重排目录),使 schema.toml 的相对引用免改写、导入即用;系统种子引用与缺失文件以 manifest 的 system_ref/missing 条目记录。`

- [ ] **Step 7: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-coordinator
# 若 Step 1 改了 manager.rs: cargo fmt -p wind-engine
git add wind_input/crates/wind-coordinator/src/webdata.rs docs/design/import-export-backup-design.md
# 若改了 manager.rs 一并 add
git commit -m "feat(rpc): scheme.exportPackage/importPackage/previewImport 方案包三件套"
```

---

## P3 收尾验证

- [ ] `cd wind_input && cargo test -p wind-transfer -p wind-coordinator` 全绿。
- [ ] `cd wind_input && cargo build` 成功。
- [ ] `cd wind_input && cargo fmt -p wind-transfer -p wind-coordinator -- --check` 干净(不含既有漂移文件)。

## P3 交付物

- `wind-transfer/scheme.rs`:三分类资源收集(mixed 递归防环)、导出打包、只读预览、Merge/Replace 导入(穿越防护 + 读齐再写)。
- RPC:`scheme.exportPackage/importPackage/previewImport` 契约就位。
- 遗留(P5):多文件覆盖回滚(backup 已覆盖文件);`web_schema_list` 的 `builtin` 恒 true(用户方案无法在 UI 区分,建议 P5 一并修);真机验证导入后方案切换可用。

## 后续

- **P4 整机备份**:temp/shadow/stats 逐表文本 + `backup.create/inspect/restore`(复用 scheme 的用户方案打包 + P2 的 clear/import 原语)。
