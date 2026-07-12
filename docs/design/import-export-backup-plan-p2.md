# 导入导出/备份还原 —— P2 词库 RPC 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development(推荐)或 superpowers:executing-plans 按任务逐条实现。步骤用 `- [ ]` 复选框跟踪。

**Goal:** 交付用户词库整库导入导出的 RPC 三件套(`dict.export` / `dict.import` / `dict.previewImport`),含 Merge/Replace 策略与 dry-run 预览,供 wind-setting 对接。

**Architecture:** store 层新增"批量导入带分类"(added/updated/unchanged 三分类,单写事务)与 `clear_user_words`,P1 的 `import_user_words_wdict` 重构为委托新方法(外部签名不变);RPC 层在 `webdata.rs` 按既有 dict.* 范式接线,消费 P1 的 `wind_transfer::merge::{Strategy, ImportOutcome, ImportPreview}` 类型。

**Tech Stack:** Rust、redb、serde/serde_json、wind-transfer(P1)、chrono(coordinator 已有)。

**关联:** 设计 `docs/design/import-export-backup-design.md`;P1 计划 `import-export-backup-plan-p1.md`。

## Global Constraints

- 工作目录:所有 `cargo` 命令在 `wind_input/` 下执行。
- 格式化:**只用 `cargo fmt -p <crate>` 限定本 crate**,严禁裸 `cargo fmt`(会污染整 workspace);提交前 `git status` 核查并 `git restore` 任何非本任务文件。
- 提交信息:conventional commit,**不带** Co-Authored-By 与任何 AI trailer。
- 日志:INFO 及以下不得含用户敏感信息(词条内容、编码)。
- 测试:TDD,先写失败测试;`cargo test -p <crate>` 聚焦跑。
- **P2 约束 1(最终审查强制)**:dry-run 与实际落盘必须一致——导入权重 ≤ 现有 ⇒ `unchanged`(不落盘);仅新键 ⇒ `added`,严格更大权重 ⇒ `updated`。
- **P2 约束 2**:`Replace` = 先清空该 schema 的用户词再写入(清空后全部计 `added`)。
- 预览(`dict.previewImport`)按 Merge 语义计算,不接收 strategy 参数(与设计文档 RPC 表一致)。
- schemaId 一律先过 `self.engine_mgr.data_schema_id(schema)` 折叠拼音族(与既有 dict.* handlers 一致)。

---

### Task 1: store 批量导入带分类 + clear_user_words

**Files:**
- Modify: `wind_input/crates/wind-store/src/user_words.rs`(`impl Store` 内追加;并重构既有 `import_user_words_wdict`)
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: 既有 `wdict::{WordIo, parse_words_wdict}`、`enc_key`/`enc_val`/`dec_val`/`split_key`/`now_secs`(同文件私有)、`USER_WORDS` 表、`self.with_db`。
- Produces(Task 2 依赖):
  - `pub struct WordsImportCounts { pub added: usize, pub updated: usize, pub unchanged: usize }`
  - `pub fn clear_user_words(&self, schema: &str) -> anyhow::Result<usize>`(清空该 schema 全部用户词,返回删除数,单写事务)
  - `pub fn import_user_words(&self, schema: &str, rows: &[wdict::WordIo]) -> anyhow::Result<WordsImportCounts>`(单写事务批量;新键=added,权重>现有=updated 取导入值,否则 unchanged 不写;保留既有 count/created_at)
  - `pub fn preview_import_user_words(&self, schema: &str, rows: &[wdict::WordIo]) -> anyhow::Result<(WordsImportCounts, Vec<String>)>`(只读 dry-run,分类同上;samples 取前 5 个会落盘行的 `"code text"`)
  - `import_user_words_wdict` 外部签名不变(`(imported, skipped)`,imported=rows.len()),内部委托 `import_user_words`。

- [ ] **Step 1: 写失败测试**

在 `user_words.rs` 的 `mod tests` 内追加:

```rust
#[test]
fn import_user_words_classifies_added_updated_unchanged() {
    let path = tmp("wind_uw_batch.redb");
    let s = Store::open(&path).unwrap();
    s.add_user_word("wb", "a", "工", 100).unwrap();
    let rows = vec![
        // 已有且权重更低 → unchanged(P2 约束 1:不落盘)
        crate::wdict::WordIo { code: "a".into(), text: "工".into(), weight: 30 },
        // 新键 → added
        crate::wdict::WordIo { code: "b".into(), text: "了".into(), weight: 5 },
    ];
    let c = s.import_user_words("wb", &rows).unwrap();
    assert_eq!((c.added, c.updated, c.unchanged), (1, 0, 1));
    assert_eq!(s.get_user_words("wb", "a").unwrap()[0].weight, 100, "unchanged 不改权重");

    // 权重严格更大 → updated,取导入值
    let rows2 = vec![crate::wdict::WordIo { code: "a".into(), text: "工".into(), weight: 200 }];
    let c2 = s.import_user_words("wb", &rows2).unwrap();
    assert_eq!((c2.added, c2.updated, c2.unchanged), (0, 1, 0));
    assert_eq!(s.get_user_words("wb", "a").unwrap()[0].weight, 200);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn preview_import_is_readonly_and_matches_import() {
    let path = tmp("wind_uw_preview.redb");
    let s = Store::open(&path).unwrap();
    s.add_user_word("wb", "a", "工", 100).unwrap();
    let rows = vec![
        crate::wdict::WordIo { code: "a".into(), text: "工".into(), weight: 30 },
        crate::wdict::WordIo { code: "b".into(), text: "了".into(), weight: 5 },
        crate::wdict::WordIo { code: "a".into(), text: "工".into(), weight: 300 },
    ];
    let (c, samples) = s.preview_import_user_words("wb", &rows).unwrap();
    assert_eq!((c.added, c.updated, c.unchanged), (1, 1, 1));
    assert_eq!(samples.len(), 2, "samples 只含会落盘的行(added+updated)");
    assert!(samples.iter().any(|x| x.contains("了")));
    // 只读:预览后库里仍只有原 1 条、权重未动
    assert_eq!(s.get_user_words("wb", "a").unwrap()[0].weight, 100);
    assert!(s.get_user_words("wb", "b").unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn clear_user_words_only_target_schema() {
    let path = tmp("wind_uw_clear.redb");
    let s = Store::open(&path).unwrap();
    s.add_user_word("wb", "a", "工", 1).unwrap();
    s.add_user_word("wb", "b", "了", 1).unwrap();
    s.add_user_word("py", "ni", "你", 1).unwrap();
    let n = s.clear_user_words("wb").unwrap();
    assert_eq!(n, 2);
    assert!(s.search_user_words_prefix("wb", "", 0).unwrap().is_empty());
    assert_eq!(s.search_user_words_prefix("py", "", 0).unwrap().len(), 1, "其它 schema 不受影响");
    let _ = std::fs::remove_file(&path);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-store user_words::tests::import_user_words_classifies -- --nocapture`
Expected: 编译错误,`import_user_words` / `WordsImportCounts` 未定义。

- [ ] **Step 3: 实现三方法 + 重构 P1 委托**

在 `user_words.rs` 中 `UserWordRecord` 定义之后追加计数结构:

```rust
/// 批量导入的分类计数(P2:added=新键 / updated=权重严格更大 / unchanged=权重≤现有不落盘)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WordsImportCounts {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
}
```

在 `impl Store { ... }` 内追加:

```rust
    /// 清空某 schema 的全部用户词(单写事务),返回删除条数。
    pub fn clear_user_words(&self, schema: &str) -> anyhow::Result<usize> {
        let prefix = format!("{schema}\u{0}");
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let n;
            {
                let mut t = txn.open_table(USER_WORDS)?;
                let keys: Vec<String> = {
                    let mut ks = Vec::new();
                    for item in t.range(prefix.as_str()..)? {
                        let (k, _) = item?;
                        let key = k.value();
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        ks.push(key.to_string());
                    }
                    ks
                };
                n = keys.len();
                for k in &keys {
                    t.remove(k.as_str())?;
                }
            }
            txn.commit()?;
            Ok(n)
        })
    }

    /// 批量导入用户词(单写事务,Merge 语义与 add_user_word 一致):
    /// 新键 → added(count=0, created_at=now);导入权重 > 现有 → updated(保留 count/created_at);
    /// 否则 → unchanged(不写)。dry-run 见 preview_import_user_words,两者分类必须一致。
    pub fn import_user_words(
        &self,
        schema: &str,
        rows: &[wdict::WordIo],
    ) -> anyhow::Result<WordsImportCounts> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let mut c = WordsImportCounts::default();
            {
                let mut t = txn.open_table(USER_WORDS)?;
                for r in rows {
                    let key = enc_key(schema, &r.code, &r.text);
                    let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                    match existing {
                        None => {
                            t.insert(key.as_str(), enc_val(r.weight, 0, now_secs()).as_slice())?;
                            c.added += 1;
                        }
                        Some((w, cnt, ca)) if r.weight > w => {
                            t.insert(key.as_str(), enc_val(r.weight, cnt, ca).as_slice())?;
                            c.updated += 1;
                        }
                        Some(_) => c.unchanged += 1,
                    }
                }
            }
            txn.commit()?;
            Ok(c)
        })
    }

    /// 导入 dry-run(只读):分类规则与 import_user_words 完全一致;
    /// samples 取前 5 个会落盘行(added/updated)的 "code text"。
    pub fn preview_import_user_words(
        &self,
        schema: &str,
        rows: &[wdict::WordIo],
    ) -> anyhow::Result<(WordsImportCounts, Vec<String>)> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(USER_WORDS)?;
            let mut c = WordsImportCounts::default();
            let mut samples = Vec::new();
            for r in rows {
                let key = enc_key(schema, &r.code, &r.text);
                let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                let will_write = match existing {
                    None => {
                        c.added += 1;
                        true
                    }
                    Some((w, _, _)) if r.weight > w => {
                        c.updated += 1;
                        true
                    }
                    Some(_) => {
                        c.unchanged += 1;
                        false
                    }
                };
                if will_write && samples.len() < 5 {
                    samples.push(format!("{} {}", r.code, r.text));
                }
            }
            Ok((c, samples))
        })
    }
```

> 注:preview 对同一批内的重复键(如测试里 `a 工` 出现两次)按"每行独立对库分类"计——批内先 added 的行不会让后一行变 updated(preview 是只读,不模拟批内叠加)。import 用写事务,后一行能看到前一行的写入。测试用例已按此语义设计(preview 里 `a工30`=unchanged、`a工300`=updated,各计各的)。

重构既有 `import_user_words_wdict`(替换其函数体,签名与文档注释不变,仅把逐条 `add_user_word` 循环换为委托):

```rust
    /// 从 wdict 文本导入用户词到某方案(Merge:max-weight upsert)。
    /// 返回 (imported, skipped)。imported=解析成功的行数(含 unchanged);细分类见 import_user_words。
    pub fn import_user_words_wdict(
        &self,
        schema: &str,
        text: &str,
    ) -> anyhow::Result<(usize, usize)> {
        let (rows, skipped) = wdict::parse_words_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
        self.import_user_words(schema, &rows)?;
        Ok((rows.len(), skipped))
    }
```

- [ ] **Step 4: 跑测试确认通过(含 P1 回归)**

Run: `cd wind_input && cargo test -p wind-store user_words`
Expected: PASS——3 个新测试 + P1 的 `export_import_user_words_roundtrip` / `import_user_words_merges_max_weight` 及全部既有测试仍绿。

- [ ] **Step 5: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-store
git add wind_input/crates/wind-store/src/user_words.rs
git commit -m "feat(store): 用户词批量导入分类计数 + 只读预览 + clear_user_words"
```

---

### Task 2: RPC 三件套 dict.export / dict.import / dict.previewImport

**Files:**
- Modify: `wind_input/crates/wind-coordinator/Cargo.toml`(追加 wind-transfer 依赖)
- Modify: `wind_input/crates/wind-coordinator/src/webdata.rs`(dispatch 三分支 + 三 handlers + `web_dict_clear` 改用 `clear_user_words` + 契约测试)
- Modify: `docs/design/import-export-backup-design.md`(RPC 表 dict.import 返回值对齐 `{added, updated, skipped}`)

**Interfaces:**
- Consumes(Task 1): `Store::{clear_user_words, import_user_words, preview_import_user_words, export_user_words_wdict}`、`WordsImportCounts{added,updated,unchanged}`;`wind_store::wdict::parse_words_wdict(text) -> Result<(Vec<WordIo>, usize), String>`;P1 的 `wind_transfer::merge::{Strategy, ImportOutcome, ImportPreview}`(Strategy::from_param;ImportPreview serde camelCase)。
- Produces(wind-setting 契约):
  - `dict.export {schemaId}` → `{content}`(wdict 文本,exported_at=本地 RFC3339)
  - `dict.import {schemaId, content, strategy?}` → `{added, updated, skipped}`(strategy 缺省/未知=merge;replace=先清后写)
  - `dict.previewImport {schemaId, content}` → `{willAdd, willUpdate, willConflict, unchanged, samples}`(willConflict 恒 0,词库域无独立冲突语义,字段保留给后续域)

- [ ] **Step 1: 加依赖**

`wind_input/crates/wind-coordinator/Cargo.toml` 的 `[dependencies]` 中,`wind-store = { path = "../wind-store" }` 一行之后追加:

```toml
wind-transfer = { path = "../wind-transfer" }
```

- [ ] **Step 2: 写失败契约测试**

在 `webdata.rs` 的 `mod tests` 内追加:

```rust
#[test]
fn dict_export_import_preview_contract() {
    let c = coord("dictio");
    c.web_data_rpc(
        "dict.add",
        &json!({ "schemaId": "wb", "code": "a", "text": "工", "weight": 100 }),
    )
    .unwrap();

    // export → {content} 且是 wdict words 文本
    let exp = c
        .web_data_rpc("dict.export", &json!({ "schemaId": "wb" }))
        .unwrap();
    let content = exp.get("content").and_then(|v| v.as_str()).expect("content 字符串");
    assert!(content.contains("--- !words"));

    // preview 到空 schema:全 willAdd,camelCase 键
    let prev = c
        .web_data_rpc("dict.previewImport", &json!({ "schemaId": "wb2", "content": content }))
        .unwrap();
    assert_eq!(prev.get("willAdd").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(prev.get("willUpdate").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(prev.get("willConflict").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(prev.get("unchanged").and_then(|v| v.as_u64()), Some(0));
    assert!(prev.get("samples").and_then(|v| v.as_array()).is_some());

    // import(缺省 merge)→ {added, updated, skipped}
    let out = c
        .web_data_rpc("dict.import", &json!({ "schemaId": "wb2", "content": content }))
        .unwrap();
    assert_eq!(out.get("added").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(out.get("skipped").and_then(|v| v.as_u64()), Some(0));

    // 同内容再 import:权重相等 ⇒ 全 unchanged(P2 约束 1),added=updated=0
    let out2 = c
        .web_data_rpc("dict.import", &json!({ "schemaId": "wb2", "content": content }))
        .unwrap();
    assert_eq!(out2.get("added").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(out2.get("updated").and_then(|v| v.as_u64()), Some(0));
    // preview 同内容 ⇒ unchanged=1,与落盘一致
    let prev2 = c
        .web_data_rpc("dict.previewImport", &json!({ "schemaId": "wb2", "content": content }))
        .unwrap();
    assert_eq!(prev2.get("unchanged").and_then(|v| v.as_u64()), Some(1));

    // replace:先加一条杂词,replace 导入后只剩导入内容(P2 约束 2)
    c.web_data_rpc(
        "dict.add",
        &json!({ "schemaId": "wb2", "code": "x", "text": "另", "weight": 1 }),
    )
    .unwrap();
    let out3 = c
        .web_data_rpc(
            "dict.import",
            &json!({ "schemaId": "wb2", "content": content, "strategy": "replace" }),
        )
        .unwrap();
    assert_eq!(out3.get("added").and_then(|v| v.as_u64()), Some(1), "清空后全部计 added");
    let listed = c
        .web_data_rpc("dict.listPaged", &json!({ "schemaId": "wb2", "limit": 10 }))
        .unwrap();
    assert_eq!(listed.get("total").and_then(|v| v.as_u64()), Some(1), "replace 应清掉 x");
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-coordinator dict_export_import_preview -- --nocapture`
Expected: FAIL,`web_data_rpc` 返回 `unknown method: dict.export`(或类似错误,来自 dispatch 兜底)。

- [ ] **Step 4: 实现 dispatch 分支与 handlers**

`webdata.rs` dispatch(`web_data_rpc` 的 match)中,`"dict.genPinyin" => { ... }` 分支之后追加:

```rust
            "dict.export" => self.web_dict_export(params),
            "dict.import" => self.web_dict_import(params),
            "dict.previewImport" => self.web_dict_preview_import(params),
```

handlers 区(建议紧邻 `web_dict_clear` 之后)追加,并把 `web_dict_clear` 的手工循环替换为 `clear_user_words`:

```rust
    fn web_dict_export(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let content =
            store.export_user_words_wdict(&schema, &chrono::Local::now().to_rfc3339())?;
        Ok(json!({ "content": content }))
    }

    fn web_dict_import(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::merge::{ImportOutcome, Strategy};
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let content = str_param(params, "content")?;
        let strategy = Strategy::from_param(
            params.get("strategy").and_then(|v| v.as_str()).unwrap_or(""),
        );
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let (rows, skipped) =
            wind_store::wdict::parse_words_wdict(content).map_err(|e| anyhow::anyhow!(e))?;
        if strategy == Strategy::Replace {
            store.clear_user_words(&schema)?;
        }
        let c = store.import_user_words(&schema, &rows)?;
        Ok(serde_json::to_value(ImportOutcome {
            added: c.added,
            updated: c.updated,
            skipped,
        })?)
    }

    fn web_dict_preview_import(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::merge::ImportPreview;
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let content = str_param(params, "content")?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let (rows, _skipped) =
            wind_store::wdict::parse_words_wdict(content).map_err(|e| anyhow::anyhow!(e))?;
        let (c, samples) = store.preview_import_user_words(&schema, &rows)?;
        // 按 Merge 语义预览(与设计 RPC 表一致,不收 strategy);willConflict 词库域恒 0,字段保留。
        Ok(serde_json::to_value(ImportPreview {
            will_add: c.added,
            will_update: c.updated,
            will_conflict: 0,
            unchanged: c.unchanged,
            samples,
        })?)
    }
```

`web_dict_clear` 函数体替换为(签名与返回形状不变,仍返回删除数的裸 JSON 数字):

```rust
    fn web_dict_clear(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let n = store.clear_user_words(&schema)?;
        Ok(json!(n))
    }
```

- [ ] **Step 5: 跑测试确认通过(含既有契约测试回归)**

Run: `cd wind_input && cargo test -p wind-coordinator webdata`
Expected: PASS——新契约测试 + 全部既有 webdata 契约测试仍绿。

- [ ] **Step 6: 设计文档 RPC 表对齐**

`docs/design/import-export-backup-design.md` 的 RPC 契约表中,把

```
| | `dict.import` | `{schemaId, content, strategy}` | `{imported, updated, skipped}` |
```

改为

```
| | `dict.import` | `{schemaId, content, strategy}` | `{added, updated, skipped}` |
```

- [ ] **Step 7: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-coordinator
git add wind_input/crates/wind-coordinator/Cargo.toml wind_input/crates/wind-coordinator/src/webdata.rs docs/design/import-export-backup-design.md
git commit -m "feat(rpc): dict.export/import/previewImport 词库整库导入导出三件套"
```

---

## P2 收尾验证

- [ ] `cd wind_input && cargo test -p wind-store -p wind-coordinator -p wind-transfer` 全绿。
- [ ] `cd wind_input && cargo build` 成功。
- [ ] `cd wind_input && cargo fmt -p wind-store -p wind-coordinator -- --check` 干净。

## P2 交付物

- store:批量导入三分类(dry-run 与落盘同一套分类规则,满足"预览数=实际落盘"约束)+ 单事务 `clear_user_words`。
- RPC:`dict.export` / `dict.import`(merge|replace)/ `dict.previewImport` 契约就位,wind-setting 可直接对接。
- P1 遗留接线完成:`Strategy`/`ImportOutcome`/`ImportPreview` 类型首次投入使用。

## 后续(各自成计划)

- **P3 方案包**:`scheme.exportPackage/importPackage/previewImport` + schema 引用资源收集。
- **P4 整机备份**:temp/shadow/stats 逐表文本导出 + `backup.create/inspect/restore`;注意 `*_columns_from_header` 跨段隐患——每段保持单独文件。
- **P5 打磨**。
