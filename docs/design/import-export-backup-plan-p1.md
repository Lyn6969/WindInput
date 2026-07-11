# 导入导出/备份还原 —— P1 底座 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development(推荐)或 superpowers:executing-plans 按任务逐条实现。步骤用 `- [ ]` 复选框跟踪。

**Goal:** 搭好导入导出/备份还原的复用底座——通用 wdict 编解码(含用户词段)、`wind-transfer` 新 crate 的 Bundle 归档(manifest + zip)与合并引擎骨架——为后续 P2 词库、P3 方案包、P4 整机备份提供地基。

**Architecture:** 编解码层就地留在 `wind-store`(与其序列化的 redb 表同处,`wind-transfer` 依赖 `wind-store`,规避循环依赖)。新增 `wind-transfer` crate 承载 Bundle(zip+manifest)与 Merge(策略/dry-run)骨架。本计划不涉及 RPC 与 UI(那在 P2+)。

**Tech Stack:** Rust(edition 2021)、redb、`zip` crate、serde/serde_json、anyhow。

**关联设计:** `docs/design/import-export-backup-design.md`。

## Global Constraints

- 工作目录:所有 `cargo` 命令在 `wind_input/` 下执行(workspace 根)。
- 代码风格:提交前 `cargo fmt`;文档注释用简体中文,与既有 crate 风格一致。
- 提交信息:conventional commit,**不带** Co-Authored-By 与 Constraint/Confidence/Tested 等 AI trailer。
- 日志:INFO 及以下级别不得含用户敏感信息(词条内容、编码)。
- 测试:每个 crate 用 `cargo test -p <crate>` 跑;新代码 TDD,先写失败测试。
- 依赖新增:`zip = "2"`(P1 引入时确认为当时最新的 2.x)。

---

### Task 1: 通用化 wdict codec —— 新增 user_words 段

**Files:**
- Modify: `wind_input/crates/wind-store/src/wdict.rs`(在 phrases 段之后追加 words 段)

**Interfaces:**
- Consumes: 复用现有 `escape_field` / `unescape_field` / `find_section_body`(同文件私有函数)。
- Produces:
  - `pub struct WordIo { pub code: String, pub text: String, pub weight: i32 }`
  - `pub fn export_words_wdict(rows: &[WordIo], exported_at: &str) -> String`
  - `pub fn parse_words_wdict(text: &str) -> Result<(Vec<WordIo>, usize), String>`(返回 (行, 跳过的非法行数);version 非 1 → Err;无 words 段 → 空)

- [ ] **Step 1: 写失败测试**

在 `wdict.rs` 的 `mod tests` 内追加:

```rust
#[test]
fn words_wdict_roundtrip() {
    let rows = vec![
        WordIo { code: "a".into(), text: "工".into(), weight: 100 },
        WordIo { code: "ml".into(), text: "多行\n带\t制表".into(), weight: 0 },
    ];
    let s = export_words_wdict(&rows, "2026-07-11T00:00:00+08:00");
    assert!(s.contains("wind_dict:"));
    assert!(s.contains("--- !words"));
    let (parsed, skipped) = parse_words_wdict(&s).unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(parsed, rows, "导出→解析应无损往返(含换行/制表)");
}

#[test]
fn words_parse_rejects_bad_version() {
    let bad = "wind_dict:\n  version: 2\n\n--- !words\na\t工\t1\n";
    assert!(parse_words_wdict(bad).is_err(), "version!=1 应拒绝");
}

#[test]
fn words_parse_tolerates_bad_lines() {
    let s = "wind_dict:\n  version: 1\n  sections:\n    words:\n      columns: [code, text, weight]\n\n--- !words\nok\t好\t10\nbadline_no_tabs\nkw\t坏权重\tNaN\n";
    let (rows, skipped) = parse_words_wdict(s).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(skipped, 1, "列数不足的行跳过");
    assert_eq!(rows[1].weight, 0, "非法数字回退 0");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-store wdict::tests::words_ -- --nocapture`
Expected: 编译错误 / FAIL,`WordIo` / `export_words_wdict` 未定义。

- [ ] **Step 3: 实现 words 段(镜像 phrases)**

在 `wdict.rs` 中 `PhraseIo` 之后追加(需要 `#[derive(PartialEq, Eq)]` 以支持测试断言):

```rust
/// wdict words 段的一行(用户词导入导出)。count/created_at 属个人数据,不随导出流转。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordIo {
    pub code: String,
    pub text: String,
    pub weight: i32,
}

const WORD_COLUMNS: &[&str] = &["code", "text", "weight"];

/// 导出 words 为 wdict 文本(YAML 头 + `--- !words` TSV 段)。
pub fn export_words_wdict(rows: &[WordIo], exported_at: &str) -> String {
    let mut s = String::new();
    s.push_str("# WindInput 用户数据文件\n");
    s.push_str("wind_dict:\n");
    s.push_str("  version: 1\n");
    s.push_str("  generator: WindInput\n");
    s.push_str(&format!("  exported_at: {exported_at}\n"));
    s.push_str("  sections:\n");
    s.push_str("    words:\n");
    s.push_str(&format!("      columns: [{}]\n", WORD_COLUMNS.join(", ")));
    s.push_str("\n--- !words\n");
    for r in rows {
        s.push_str(&format!(
            "{}\t{}\t{}\n",
            escape_field(&r.code),
            escape_field(&r.text),
            r.weight,
        ));
    }
    s
}

/// 解析 wdict 文本的 words 段。返回 (行, 跳过的非法行数)。只认 version==1。
pub fn parse_words_wdict(text: &str) -> Result<(Vec<WordIo>, usize), String> {
    let header = text.split("\n---").next().unwrap_or("");
    if !header.contains("wind_dict:") {
        return Err("不是 WindDict 文件(缺 wind_dict 头)".into());
    }
    let version_ok = header.lines().any(|l| {
        let t = l.trim();
        t.starts_with("version:") && t.trim_start_matches("version:").trim() == "1"
    });
    if !version_ok {
        return Err("不支持的 WindDict 版本(需 version: 1)".into());
    }
    let Some(after_tag) = find_section_body(text, "words") else {
        return Ok((Vec::new(), 0));
    };
    let cols = words_columns_from_header(header);
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in after_tag.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < cols.len() {
            skipped += 1;
            continue;
        }
        let get = |name: &str| -> &str {
            cols.iter()
                .position(|c| c == name)
                .map(|i| fields[i])
                .unwrap_or("")
        };
        rows.push(WordIo {
            code: unescape_field(get("code")),
            text: unescape_field(get("text")),
            weight: get("weight").trim().parse().unwrap_or(0),
        });
    }
    Ok((rows, skipped))
}

/// 从头部读 words 段列定义;缺则默认列。
fn words_columns_from_header(header: &str) -> Vec<String> {
    for l in header.lines() {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("columns:") {
            let inner = rest.trim().trim_start_matches('[').trim_end_matches(']');
            let cols: Vec<String> = inner
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect();
            if !cols.is_empty() {
                return cols;
            }
        }
    }
    WORD_COLUMNS.iter().map(|s| s.to_string()).collect()
}
```

> 注:`words_columns_from_header` 与既有 `phrase_columns_from_header` 结构相同但各自持有默认列常量;P1 暂不合并抽象(YAGNI),待第三个段出现时再泛型化。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-store wdict`
Expected: PASS(含既有 phrases 测试仍绿)。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-store/src/wdict.rs
git commit -m "feat(store): wdict 新增 words 段编解码"
```

---

### Task 2: store 侧用户词整库导出/导入(wdict)

**Files:**
- Modify: `wind_input/crates/wind-store/src/user_words.rs`(在 `impl Store` 内追加两个方法 + 一个全量迭代辅助)
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `wdict::{WordIo, export_words_wdict, parse_words_wdict}`(Task 1);既有 `search_user_words_prefix`、`add_user_word`。
- Produces:
  - `pub fn export_user_words_wdict(&self, schema: &str, exported_at: &str) -> anyhow::Result<String>`
  - `pub fn import_user_words_wdict(&self, schema: &str, text: &str) -> anyhow::Result<(usize, usize)>`(返回 (imported, skipped);采用 **Merge** 语义,即 `add_user_word` 的 max-weight upsert)

- [ ] **Step 1: 写失败测试**

在 `user_words.rs` 的 `mod tests` 内追加:

```rust
#[test]
fn export_import_user_words_roundtrip() {
    let path = tmp("wind_uw_io.redb");
    let s = Store::open(&path).unwrap();
    s.add_user_word("wb", "a", "工", 100).unwrap();
    s.add_user_word("wb", "ml", "多行\n带\t制表", 5).unwrap();
    let text = s
        .export_user_words_wdict("wb", "2026-07-11T00:00:00+08:00")
        .unwrap();
    assert!(text.contains("--- !words"));

    // 导入到新库应还原
    let path2 = tmp("wind_uw_io2.redb");
    let s2 = Store::open(&path2).unwrap();
    let (imported, skipped) = s2.import_user_words_wdict("wb", &text).unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(imported, 2);
    let got = s2.get_user_words("wb", "a").unwrap();
    assert_eq!(got[0].text, "工");
    assert_eq!(got[0].weight, 100);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&path2);
}

#[test]
fn import_user_words_merges_max_weight() {
    let path = tmp("wind_uw_merge.redb");
    let s = Store::open(&path).unwrap();
    s.add_user_word("wb", "a", "工", 100).unwrap();
    // 导入同词更低权重 → 保持 max(100)
    let text = crate::wdict::export_words_wdict(
        &[crate::wdict::WordIo { code: "a".into(), text: "工".into(), weight: 30 }],
        "2026-07-11T00:00:00+08:00",
    );
    let (imported, _) = s.import_user_words_wdict("wb", &text).unwrap();
    assert_eq!(imported, 1);
    assert_eq!(s.get_user_words("wb", "a").unwrap()[0].weight, 100, "Merge 取 max");
    let _ = std::fs::remove_file(&path);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-store user_words::tests::export_import -- --nocapture`
Expected: FAIL,`export_user_words_wdict` 未定义。

- [ ] **Step 3: 实现两个方法**

在 `user_words.rs` 的 `impl Store { ... }` 内追加(文件顶部 `use crate::wdict;`):

```rust
    /// 导出某方案的全部用户词为 wdict 文本(仅 code/text/weight,不含个人 count/created_at)。
    pub fn export_user_words_wdict(
        &self,
        schema: &str,
        exported_at: &str,
    ) -> anyhow::Result<String> {
        let recs = self.search_user_words_prefix(schema, "", 0)?;
        let rows: Vec<wdict::WordIo> = recs
            .into_iter()
            .map(|r| wdict::WordIo {
                code: r.code,
                text: r.text,
                weight: r.weight,
            })
            .collect();
        Ok(wdict::export_words_wdict(&rows, exported_at))
    }

    /// 从 wdict 文本导入用户词到某方案(Merge:add_user_word 的 max-weight upsert)。
    /// 返回 (imported, skipped)。
    pub fn import_user_words_wdict(
        &self,
        schema: &str,
        text: &str,
    ) -> anyhow::Result<(usize, usize)> {
        let (rows, skipped) = wdict::parse_words_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
        let mut imported = 0usize;
        for r in &rows {
            self.add_user_word(schema, &r.code, &r.text, r.weight)?;
            imported += 1;
        }
        Ok((imported, skipped))
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-store user_words`
Expected: PASS(既有 user_words 测试仍绿)。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-store/src/user_words.rs
git commit -m "feat(store): 用户词整库 wdict 导出/导入(Merge)"
```

---

### Task 3: 新建 `wind-transfer` crate + manifest 类型与校验

**Files:**
- Create: `wind_input/crates/wind-transfer/Cargo.toml`
- Create: `wind_input/crates/wind-transfer/src/lib.rs`
- Create: `wind_input/crates/wind-transfer/src/bundle.rs`
- Modify: `wind_input/Cargo.toml`(workspace `members` 追加 `crates/wind-transfer`)

**Interfaces:**
- Produces:
  - `pub enum BundleKind { Scheme, Backup }`(serde 序列化为 `"scheme"` / `"backup"`)
  - `pub struct ContentEntry { pub r#type: String, pub path: String, pub meta: serde_json::Value }`
  - `pub struct Manifest { pub format: String, pub kind: BundleKind, pub spec_version: u32, pub app_version: String, pub platform: String, pub created_at: String, pub contents: Vec<ContentEntry> }`
  - `pub const SPEC_VERSION: u32 = 1;`
  - `pub const FORMAT_TAG: &str = "windinput-bundle";`
  - `impl Manifest { pub fn new(kind: BundleKind, app_version: &str, platform: &str, created_at: &str) -> Self; pub fn validate(&self) -> anyhow::Result<()> }`(validate:format 匹配、spec_version<=SPEC_VERSION,否则 Err)

- [ ] **Step 1: 建 crate 骨架与依赖**

`wind_input/crates/wind-transfer/Cargo.toml`:

```toml
[package]
name = "wind-transfer"
version = "0.1.0"
edition = "2021"

[dependencies]
wind-store = { path = "../wind-store" }
wind-config = { path = "../wind-config" }
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
zip = "2"

[dev-dependencies]
tempfile = "3"
```

> 若 workspace 用统一版本表(`[workspace.dependencies]`),改为 `anyhow.workspace = true` 等,与相邻 crate 的 Cargo.toml 保持一致——实现时先看 `crates/wind-store/Cargo.toml` 的写法照抄。

`wind_input/crates/wind-transfer/src/lib.rs`:

```rust
//! 导入导出/备份还原的复用底座:Bundle(manifest + zip)与 Merge 骨架。
//! 编解码在 wind-store(与 redb 表同处);本 crate 负责聚合打包与合并策略。
pub mod bundle;
// pub mod merge;  // Task 5 创建 merge.rs 时取消注释
```

> 注:`merge` 模块在 Task 5 才创建,此处先不声明,以免 Task 3 的 `cargo build` 因缺文件失败。

在 `wind_input/Cargo.toml` 的 `[workspace] members` 列表内追加一行 `"crates/wind-transfer"`(位置按字母序,紧邻 `crates/wind-theme` 之后)。

- [ ] **Step 2: 写失败测试**

`wind_input/crates/wind-transfer/src/bundle.rs`(先只写 tests,类型下一步补):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip_and_validate() {
        let m = Manifest::new(BundleKind::Backup, "1.2.3", "windows", "2026-07-11T00:00:00+08:00");
        assert_eq!(m.format, FORMAT_TAG);
        assert_eq!(m.spec_version, SPEC_VERSION);
        m.validate().unwrap();

        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"kind\":\"backup\""), "kind 序列化为小写字符串");
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.app_version, "1.2.3");
    }

    #[test]
    fn validate_rejects_future_spec_and_bad_format() {
        let mut m = Manifest::new(BundleKind::Scheme, "1.0.0", "darwin", "t");
        m.spec_version = SPEC_VERSION + 1;
        assert!(m.validate().is_err(), "更高 spec_version 应拒绝");

        let mut m2 = Manifest::new(BundleKind::Scheme, "1.0.0", "darwin", "t");
        m2.format = "wrong".into();
        assert!(m2.validate().is_err(), "format 不匹配应拒绝");
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-transfer bundle`
Expected: 编译错误,`Manifest` 等未定义。

- [ ] **Step 4: 实现 manifest 类型与校验**

在 `bundle.rs` 顶部(tests 之上)写:

```rust
use serde::{Deserialize, Serialize};

pub const SPEC_VERSION: u32 = 1;
pub const FORMAT_TAG: &str = "windinput-bundle";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleKind {
    Scheme,
    Backup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentEntry {
    pub r#type: String,
    pub path: String,
    #[serde(default)]
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub kind: BundleKind,
    pub spec_version: u32,
    pub app_version: String,
    pub platform: String,
    pub created_at: String,
    #[serde(default)]
    pub contents: Vec<ContentEntry>,
}

impl Manifest {
    pub fn new(kind: BundleKind, app_version: &str, platform: &str, created_at: &str) -> Self {
        Self {
            format: FORMAT_TAG.to_string(),
            kind,
            spec_version: SPEC_VERSION,
            app_version: app_version.to_string(),
            platform: platform.to_string(),
            created_at: created_at.to_string(),
            contents: Vec::new(),
        }
    }

    /// 校验:format 与 FORMAT_TAG 一致,且 spec_version 不高于当前支持版本。
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.format != FORMAT_TAG {
            anyhow::bail!("非 WindInput 归档(format={})", self.format);
        }
        if self.spec_version > SPEC_VERSION {
            anyhow::bail!(
                "归档版本过高(spec_version={},当前支持 {}),请升级 WindInput",
                self.spec_version,
                SPEC_VERSION
            );
        }
        Ok(())
    }
}
```

- [ ] **Step 5: 跑测试确认通过 + 构建 workspace**

Run: `cd wind_input && cargo test -p wind-transfer bundle && cargo build`
Expected: PASS;workspace 构建成功(新成员已登记)。

- [ ] **Step 6: 提交**

```bash
git add wind_input/Cargo.toml wind_input/crates/wind-transfer/
git commit -m "feat(transfer): 新增 wind-transfer crate 与 Bundle manifest"
```

---

### Task 4: Bundle zip 打包/解包 + 免解压读 manifest

**Files:**
- Modify: `wind_input/crates/wind-transfer/src/bundle.rs`

**Interfaces:**
- Consumes: `Manifest`(Task 3);`zip` crate。
- Produces:
  - `pub struct BundleWriter`,方法 `new(path: &Path, manifest: Manifest) -> anyhow::Result<Self>`、`add_bytes(&mut self, name: &str, data: &[u8]) -> anyhow::Result<()>`、`finish(self) -> anyhow::Result<()>`(finish 时把 `manifest.json` 写入并更新其 `contents` 由调用方在 add 时登记;P1 简化:`add_bytes` 只写文件,`contents` 由调用方在构造 manifest 时给定或留空)
  - `pub fn read_manifest(path: &Path) -> anyhow::Result<Manifest>`(打开 zip,读 `manifest.json`,`validate()` 后返回)
  - `pub fn extract_entry(path: &Path, name: &str) -> anyhow::Result<Vec<u8>>`(读单个条目字节)

- [ ] **Step 1: 写失败测试**

在 `bundle.rs` 的 `mod tests` 追加:

```rust
#[test]
fn bundle_write_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let zip_path = dir.path().join("t.zip");

    let manifest = Manifest::new(BundleKind::Backup, "1.0.0", "windows", "t");
    let mut w = BundleWriter::new(&zip_path, manifest).unwrap();
    w.add_bytes("userdata/user_words.wdict", b"hello-words").unwrap();
    w.finish().unwrap();

    // 免解压读 manifest
    let m = read_manifest(&zip_path).unwrap();
    assert_eq!(m.kind, BundleKind::Backup);
    // 取单个条目
    let data = extract_entry(&zip_path, "userdata/user_words.wdict").unwrap();
    assert_eq!(data, b"hello-words");
}

#[test]
fn read_manifest_rejects_bad_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.zip");
    // 手写一个不含 manifest.json 的 zip
    let mut w = zip::ZipWriter::new(std::fs::File::create(&bad).unwrap());
    w.start_file("foo.txt", zip::write::SimpleFileOptions::default()).unwrap();
    use std::io::Write;
    w.write_all(b"x").unwrap();
    w.finish().unwrap();
    assert!(read_manifest(&bad).is_err(), "缺 manifest.json 应报错");
}
```

在 `bundle.rs` 顶部 tests 模块加 `use tempfile;`(dev-dep 已在 Cargo.toml)。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-transfer bundle::tests::bundle_write -- --nocapture`
Expected: FAIL,`BundleWriter` 未定义。

- [ ] **Step 3: 实现 zip 读写**

在 `bundle.rs` 追加(顶部补 `use std::io::{Read, Write}; use std::path::Path;`):

```rust
const MANIFEST_NAME: &str = "manifest.json";

pub struct BundleWriter {
    writer: zip::ZipWriter<std::fs::File>,
    manifest: Manifest,
}

impl BundleWriter {
    pub fn new(path: &Path, manifest: Manifest) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        Ok(Self {
            writer: zip::ZipWriter::new(file),
            manifest,
        })
    }

    /// 写入一个条目,并在 manifest.contents 里登记(type 由调用方后续细化;P1 记 path)。
    pub fn add_bytes(&mut self, name: &str, data: &[u8]) -> anyhow::Result<()> {
        self.writer
            .start_file(name, zip::write::SimpleFileOptions::default())?;
        self.writer.write_all(data)?;
        self.manifest.contents.push(ContentEntry {
            r#type: String::new(),
            path: name.to_string(),
            meta: serde_json::Value::Null,
        });
        Ok(())
    }

    /// 收尾:写入 manifest.json 并关闭。
    pub fn finish(mut self) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(&self.manifest)?;
        self.writer
            .start_file(MANIFEST_NAME, zip::write::SimpleFileOptions::default())?;
        self.writer.write_all(&json)?;
        self.writer.finish()?;
        Ok(())
    }
}

/// 免全解压读取并校验 manifest.json。
pub fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut entry = archive
        .by_name(MANIFEST_NAME)
        .map_err(|_| anyhow::anyhow!("归档缺少 {}", MANIFEST_NAME))?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf)?;
    let manifest: Manifest = serde_json::from_str(&buf)?;
    manifest.validate()?;
    Ok(manifest)
}

/// 读取单个条目字节。
pub fn extract_entry(path: &Path, name: &str) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut entry = archive
        .by_name(name)
        .map_err(|_| anyhow::anyhow!("归档缺少条目 {}", name))?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf)?;
    Ok(buf)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-transfer bundle`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-transfer/src/bundle.rs
git commit -m "feat(transfer): Bundle zip 打包/解包与免解压读 manifest"
```

---

### Task 5: 合并引擎骨架(Strategy + 导入结果 + dry-run 类型)

**Files:**
- Create: `wind_input/crates/wind-transfer/src/merge.rs`

**Interfaces:**
- Produces:
  - `pub enum Strategy { Merge, Replace }`(serde `rename_all = "lowercase"`;`Default = Merge`;`from_param(s: &str) -> Strategy`,未知值回退 Merge)
  - `pub struct ImportOutcome { pub added: usize, pub updated: usize, pub skipped: usize }`(serde)
  - `pub struct ImportPreview { pub will_add: usize, pub will_update: usize, pub will_conflict: usize, pub unchanged: usize, pub samples: Vec<String> }`(serde;字段 camelCase 供前端:`#[serde(rename_all = "camelCase")]`)

> 说明:P1 只固化类型与 `Strategy` 解析(供 P2+ 的 RPC 与 store 批量 upsert 复用),真正的逐表 dry-run 计算在 P2 词库任务里接线到 store。

- [ ] **Step 1: 声明模块 + 写失败测试**

先在 `wind_input/crates/wind-transfer/src/lib.rs` 取消注释(或追加)`pub mod merge;`(否则测试文件不会被编译,Step 2 无法看到"Strategy 未定义"的失败)。再创建 `wind_input/crates/wind-transfer/src/merge.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_from_param() {
        assert_eq!(Strategy::from_param("replace"), Strategy::Replace);
        assert_eq!(Strategy::from_param("merge"), Strategy::Merge);
        assert_eq!(Strategy::from_param("garbage"), Strategy::Merge, "未知回退 Merge");
        assert_eq!(Strategy::default(), Strategy::Merge);
    }

    #[test]
    fn preview_serializes_camel_case() {
        let p = ImportPreview {
            will_add: 3,
            will_update: 1,
            will_conflict: 0,
            unchanged: 2,
            samples: vec!["工".into()],
        };
        let j = serde_json::to_string(&p).unwrap();
        assert!(j.contains("willAdd"), "字段应为 camelCase 供前端");
        assert!(j.contains("\"willAdd\":3"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-transfer merge`
Expected: FAIL,`Strategy` 未定义。

- [ ] **Step 3: 实现类型**

在 `merge.rs` 顶部(tests 之上)写:

```rust
//! 合并引擎:导入/还原的策略与结果类型。逐表 dry-run 计算在各功能任务里接线到 store。
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    Merge,
    Replace,
}

impl Default for Strategy {
    fn default() -> Self {
        Strategy::Merge
    }
}

impl Strategy {
    /// 从 RPC 参数字符串解析;未知值回退 Merge(默认合并)。
    pub fn from_param(s: &str) -> Strategy {
        match s {
            "replace" => Strategy::Replace,
            _ => Strategy::Merge,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportOutcome {
    pub added: usize,
    pub updated: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    pub will_add: usize,
    pub will_update: usize,
    pub will_conflict: usize,
    pub unchanged: usize,
    pub samples: Vec<String>,
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-transfer`
Expected: PASS(bundle + merge 全绿)。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-transfer/src/merge.rs
git commit -m "feat(transfer): 合并策略与导入结果/预览类型"
```

---

## P1 收尾验证

- [ ] `cd wind_input && cargo test -p wind-store -p wind-transfer` 全绿。
- [ ] `cd wind_input && cargo build` 成功(workspace 含新成员)。
- [ ] `cd wind_input && cargo fmt --check`(或 `cargo fmt` 后无 diff)。

## P1 完成后交付物

- `wind-store` 具备用户词整库 wdict 导出/导入(Merge)。
- `wind-transfer` crate 就位:Bundle(manifest+zip 打包/解包/免解压读)、Merge 策略与结果类型。
- 为 P2(词库 RPC)、P3(方案包)、P4(整机备份)提供全部底层积木。

## 后续计划(待 P1 落地后各自成计划)

- **P2 词库**:`dict.export/import/previewImport` RPC(webdata.rs)+ store 批量 upsert 带 Strategy + dry-run 计算接线。
- **P3 方案包**:`scheme.rs` 资源收集 + `scheme.exportPackage/importPackage/previewImport`。
- **P4 整机备份**:`backup.rs` 组合 codec+merge+bundle + `backup.create/inspect/restore`;temp/shadow/stats 逐表文本导出。
- **P5 打磨**:state/跨平台项过滤、stats 可选、错误提示。
