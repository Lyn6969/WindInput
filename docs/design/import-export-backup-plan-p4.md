# 导入导出/备份还原 —— P4 整机备份 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development(推荐)或 superpowers:executing-plans 按任务逐条实现。步骤用 `- [ ]` 复选框跟踪。

**Goal:** 交付整机备份/还原:config + 全部用户数据表(逐表文本)+ 用户方案 + 用户主题打成自描述 zip,可 inspect 清单、按 sections 局部还原,Merge/Replace 可选。

**Architecture:** store 层补齐 temp/freq/shadow/stats 的逐表文本导出导入与跨 schema 枚举(`list_data_schemas`);wind-transfer 把 P3 的穿越守卫下沉为共享函数,新增 `backup.rs` 组合全部积木(目录参数化,tempdir 可测);RPC 三方法薄封装,还原后调 `reload_user_config`/`rebuild_phrases`/`invalidate_schema` 刷新。**设计 RPC 表 backup 域无 preview**——`backup.inspect` 由 P1 `read_manifest` 直接覆盖。

**Tech Stack:** Rust、redb、zip、serde/serde_json(jsonl=逐行 serde_json,不建独立 codec 模块——4 行代码不值得抽层)。

**关联:** 设计 `docs/design/import-export-backup-design.md`;P1–P3 计划同目录。

## Global Constraints

- 工作目录:所有 `cargo` 命令在 `wind_input/` 下执行。
- **cwd 防护(强制)**:实现者只能在指定 worktree 内操作;每次 git 写命令前 `pwd` 验证;严禁触碰主仓 `D:/Develop/workspace/windinput/WindInput`。
- 格式化:只用 `cargo fmt -p <crate>`;提交前 `git status` 核查并 restore 非本任务文件。
- 提交:conventional commit,不带 Co-Authored-By 与任何 AI trailer。TDD 先写失败测试。
- 日志:INFO 及以下不得含用户敏感信息。
- **zip 布局(多方案子目录化,覆盖设计示意;Task 5 更新设计文档)**:
  ```
  manifest.json
  config/config.toml                     type="config"
  userdata/user_words/<schema>.wdict     type="dict"       meta={"schema"}
  userdata/temp_words/<schema>.wdict     type="temp"       meta={"schema"}
  userdata/phrases.wdict                 type="phrase"
  userdata/freq/<schema>.jsonl           type="freq"       meta={"schema"}
  userdata/shadow/<schema>.jsonl         type="shadow"     meta={"schema"}
  userdata/stats.jsonl                   type="stats"      (include_stats)
  userdata/stats_meta.json               type="stats_meta" (include_stats)
  schemas/<schemas根相对路径>             type="schema_file"(用户方案目录整树)
  themes/<themes根相对路径>              type="theme_file" (用户主题目录整树)
  state/state.toml                       type="state"      (include_state)
  ```
- **sections 名集合**:`["config","dict","temp","phrase","freq","shadow","stats","schemas","themes","state"]`;type→section:`schema_file`→schemas、`theme_file`→themes、`stats_meta`→stats,其余同名。缺省=全部。
- **Strategy 语义**:文件域(config/schemas/themes/state)Merge=已存在跳过计 conflicts、Replace=覆盖(tmp+rename,先 remove 旧文件);数据域 Merge=各表既有 upsert 合并语义、Replace=先 clear 该域再导入。
- **穿越守卫(强制)**:所有还原写盘条目经共享白名单守卫(components 全 Normal + 段内禁冒号),P3 教训见 `reference_windows_path_traversal_guard`。
- 已知限制(不做):跨机 wdat/缓存(排除 cache/logs);孤儿主题资源文件之外的 theme 附件按整目录拷贝天然覆盖;stats 的 Merge=已存在该日跳过。

---

### Task 1: store——temp 词 wdict + freq jsonl 导出/导入 + clear_temp_words

**Files:**
- Modify: `wind_input/crates/wind-store/src/temp_words.rs`
- Modify: `wind_input/crates/wind-store/src/freq.rs`

**Interfaces:**
- Consumes: `wdict::{WordIo, export_words_wdict, parse_words_wdict}`;`search_temp_words_prefix(schema,"",0)`、`learn_temp_word(schema,code,text,add_weight)->Result<u32>`;freq 的 `enc_key`/`enc_freq`/`dec_freq`/`FreqRecord{count,last_used}`(serde derive 已有)、`list_freq_paged(schema,"",0,0)`。
- Produces(Task 3/4 依赖):
  - `Store::export_temp_words_wdict(&self, schema:&str, exported_at:&str) -> Result<String>`
  - `Store::import_temp_words_wdict(&self, schema:&str, text:&str) -> Result<(usize,usize)>`(imported, skipped;逐行 `learn_temp_word`——已存在则 count++,Merge 可接受)
  - `Store::clear_temp_words(&self, schema:&str) -> Result<usize>`(镜像 `clear_user_words`,操作 TEMP_WORDS 表)
  - `Store::export_freq_jsonl(&self, schema:&str) -> Result<String>`(每行 `{"code":..,"text":..,"count":..,"last_used":..}`)
  - `Store::import_freq_jsonl(&self, schema:&str, text:&str) -> Result<(usize,usize)>`(imported, skipped;Merge=已存在取 max(count)/max(last_used);单写事务)

- [x] **Step 1: 写失败测试**

`temp_words.rs` tests 追加:

```rust
#[test]
fn temp_words_wdict_roundtrip_and_clear() {
    let path = tmp("wind_tw_io.redb");
    let s = Store::open(&path).unwrap();
    s.learn_temp_word("wb", "ab", "临时", 50).unwrap();
    s.learn_temp_word("py", "ni", "你", 10).unwrap();
    let text = s.export_temp_words_wdict("wb", "t").unwrap();
    assert!(text.contains("--- !words"));

    let path2 = tmp("wind_tw_io2.redb");
    let s2 = Store::open(&path2).unwrap();
    let (imported, skipped) = s2.import_temp_words_wdict("wb", &text).unwrap();
    assert_eq!((imported, skipped), (1, 0));
    assert_eq!(s2.get_temp_word("wb", "ab", "临时").unwrap(), Some(1));

    assert_eq!(s.clear_temp_words("wb").unwrap(), 1);
    assert!(s.search_temp_words_prefix("wb", "", 0).unwrap().is_empty());
    assert_eq!(s.search_temp_words_prefix("py", "", 0).unwrap().len(), 1, "其它 schema 不受影响");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&path2);
}
```

`freq.rs` tests 追加:

```rust
#[test]
fn freq_jsonl_roundtrip_merge_max() {
    let path = tmp("wind_freq_io.redb");
    let s = Store::open(&path).unwrap();
    s.record_freq("wb", "a", "工").unwrap();
    s.record_freq("wb", "a", "工").unwrap(); // count=2
    let text = s.export_freq_jsonl("wb").unwrap();
    assert!(text.contains("\"count\":2"));

    // 导入到已有更高 count 的库:Merge 取 max,不回退
    let path2 = tmp("wind_freq_io2.redb");
    let s2 = Store::open(&path2).unwrap();
    for _ in 0..5 {
        s2.record_freq("wb", "a", "工").unwrap(); // count=5
    }
    let (imported, skipped) = s2.import_freq_jsonl("wb", &text).unwrap();
    assert_eq!((imported, skipped), (1, 0));
    assert_eq!(s2.get_freq("wb", "a", "工").unwrap().unwrap().count, 5, "max 合并不回退");

    // 导入到空库:原值落库
    let path3 = tmp("wind_freq_io3.redb");
    let s3 = Store::open(&path3).unwrap();
    s3.import_freq_jsonl("wb", &text).unwrap();
    assert_eq!(s3.get_freq("wb", "a", "工").unwrap().unwrap().count, 2);
    // 坏行跳过
    let (_, sk) = s3.import_freq_jsonl("wb", "not json\n").unwrap();
    assert_eq!(sk, 1);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&path2);
    let _ = std::fs::remove_file(&path3);
}
```

(freq.rs 的 tests 模块若无 `tmp` 辅助,按 user_words.rs 的 `tmp(name)` 同款补一份。)

- [x] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-store temp_words::tests::temp_words_wdict freq::tests::freq_jsonl`
Expected: 编译错误,新方法未定义。

- [x] **Step 3: 实现**

`temp_words.rs` 的 `impl Store` 追加(文件顶部补 `use crate::wdict;`;若已有 USER 词的 use 则并入):

```rust
    /// 导出某方案全部临时词为 wdict 文本(code/text/weight;count 属晋升进度不流转)。
    pub fn export_temp_words_wdict(
        &self,
        schema: &str,
        exported_at: &str,
    ) -> anyhow::Result<String> {
        let recs = self.search_temp_words_prefix(schema, "", 0)?;
        let rows: Vec<wdict::WordIo> = recs
            .into_iter()
            .map(|r| wdict::WordIo { code: r.code, text: r.text, weight: r.weight })
            .collect();
        Ok(wdict::export_words_wdict(&rows, exported_at))
    }

    /// 从 wdict 文本导入临时词(Merge:learn_temp_word,已存在 count++)。返回 (imported, skipped)。
    pub fn import_temp_words_wdict(
        &self,
        schema: &str,
        text: &str,
    ) -> anyhow::Result<(usize, usize)> {
        let (rows, skipped) = wdict::parse_words_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
        for r in &rows {
            self.learn_temp_word(schema, &r.code, &r.text, r.weight)?;
        }
        Ok((rows.len(), skipped))
    }

    /// 清空某方案全部临时词(单写事务),返回删除条数。
    pub fn clear_temp_words(&self, schema: &str) -> anyhow::Result<usize> {
        let prefix = format!("{schema}\u{0}");
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let n;
            {
                let mut t = txn.open_table(TEMP_WORDS)?;
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
```

(`TEMP_WORDS` 表常量在 store.rs 已 pub(crate),temp_words.rs 顶部按既有 use 引入;以文件实况 use 为准。)

`freq.rs` 的 `impl Store` 追加:

```rust
    /// 导出某方案全部词频为 jsonl(每行 {"code","text","count","last_used"})。
    pub fn export_freq_jsonl(&self, schema: &str) -> anyhow::Result<String> {
        let (rows, _total) = self.list_freq_paged(schema, "", 0, 0)?;
        let mut out = String::new();
        for (code, text, rec) in rows {
            out.push_str(&serde_json::to_string(&serde_json::json!({
                "code": code, "text": text, "count": rec.count, "last_used": rec.last_used,
            }))?);
            out.push('\n');
        }
        Ok(out)
    }

    /// 从 jsonl 导入词频(单写事务;Merge=已存在取 max(count)/max(last_used))。
    /// 返回 (imported, skipped);非法行跳过计数。
    pub fn import_freq_jsonl(&self, schema: &str, text: &str) -> anyhow::Result<(usize, usize)> {
        let mut rows: Vec<(String, String, u32, i64)> = Vec::new();
        let mut skipped = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                skipped += 1;
                continue;
            };
            let (Some(code), Some(word)) = (
                v.get("code").and_then(|x| x.as_str()),
                v.get("text").and_then(|x| x.as_str()),
            ) else {
                skipped += 1;
                continue;
            };
            let count = v.get("count").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let last_used = v.get("last_used").and_then(|x| x.as_i64()).unwrap_or(0);
            rows.push((code.to_string(), word.to_string(), count, last_used));
        }
        let imported = rows.len();
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(FREQ)?;
                for (code, word, count, last_used) in &rows {
                    let key = enc_key(schema, code, word);
                    let merged = match t.get(key.as_str())?.and_then(|g| dec_freq(g.value())) {
                        Some(old) => (old.count.max(*count), old.last_used.max(*last_used)),
                        None => (*count, *last_used),
                    };
                    t.insert(key.as_str(), enc_freq(merged.0, merged.1).as_slice())?;
                }
            }
            txn.commit()?;
            Ok(())
        })?;
        Ok((imported, skipped))
    }
```

- [x] **Step 4: 跑测试确认通过(含既有回归)**

Run: `cd wind_input && cargo test -p wind-store temp_words freq`
Expected: PASS。

- [x] **Step 5: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-store
git add wind_input/crates/wind-store/src/temp_words.rs wind_input/crates/wind-store/src/freq.rs
git commit -m "feat(store): 临时词 wdict/词频 jsonl 导出导入 + clear_temp_words"
```

---

### Task 2: store——shadow/stats jsonl + list_data_schemas + clear_user_phrases/clear_shadow

**Files:**
- Modify: `wind_input/crates/wind-store/src/shadow.rs`
- Modify: `wind_input/crates/wind-store/src/stats.rs`
- Modify: `wind_input/crates/wind-store/src/store.rs`(`list_data_schemas` 跨表扫描)
- Modify: `wind_input/crates/wind-store/src/phrases.rs`(`clear_user_phrases`)

**Interfaces:**
- Consumes: `list_shadow_rules(schema)->Vec<(String,ShadowRecord)>`、`pin_shadow(schema,code,word,cand_id:Option<&str>,position)`、`delete_shadow(schema,code,word)`;`daily_stats(from,to)`、`put_daily_stat(date,&DailyStats)`;`list_phrases()->Vec<PhraseRecord>`、`remove_phrase(code,text)`;表常量 `USER_WORDS/TEMP_WORDS/FREQ/SHADOW`。`ShadowRecord/ShadowPin/DailyStats` 均已 serde derive。
- Produces(Task 3/4 依赖):
  - `Store::export_shadow_jsonl(&self, schema:&str) -> Result<String>`(每行 `{"code":..,"rec":ShadowRecord}`)
  - `Store::import_shadow_jsonl(&self, schema:&str, text:&str) -> Result<(usize,usize)>`(逐规则 replay pin/delete;Merge 天然 upsert)
  - `Store::clear_shadow(&self, schema:&str) -> Result<usize>`(删该 schema 全部 SHADOW 键)
  - `Store::export_stats_jsonl(&self) -> Result<String>`(每行 `{"date":..,"stat":DailyStats}`,全量日期)
  - `Store::import_stats_jsonl(&self, text:&str, overwrite:bool) -> Result<(usize,usize)>`(overwrite=false 时已存在日跳过)
  - `Store::list_data_schemas(&self) -> Result<Vec<String>>`(扫 USER_WORDS/TEMP_WORDS/FREQ/SHADOW 四表,distinct schema 前缀,排序)
  - `Store::clear_user_phrases(&self) -> Result<usize>`(删全部非 system 短语)

- [x] **Step 1: 写失败测试**

`shadow.rs` tests 追加:

```rust
#[test]
fn shadow_jsonl_roundtrip_and_clear() {
    let path = tmp("wind_sh_io.redb");
    let s = Store::open(&path).unwrap();
    s.pin_shadow("wb", "aaaa", "恭", Some("c1"), 0).unwrap();
    s.delete_shadow("wb", "bbbb", "删词").unwrap();
    let text = s.export_shadow_jsonl("wb").unwrap();
    assert_eq!(text.lines().count(), 2);

    let path2 = tmp("wind_sh_io2.redb");
    let s2 = Store::open(&path2).unwrap();
    let (imported, skipped) = s2.import_shadow_jsonl("wb", &text).unwrap();
    assert_eq!(skipped, 0);
    assert!(imported >= 2);
    let rules = s2.list_shadow_rules("wb").unwrap();
    assert_eq!(rules.len(), 2);
    let pinned = rules.iter().find(|(c, _)| c == "aaaa").unwrap();
    assert_eq!(pinned.1.pinned[0].word, "恭");
    assert_eq!(pinned.1.pinned[0].position, 0);

    assert_eq!(s.clear_shadow("wb").unwrap(), 2);
    assert!(s.list_shadow_rules("wb").unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&path2);
}
```

`stats.rs` tests 追加:

```rust
#[test]
fn stats_jsonl_roundtrip_skip_existing() {
    let path = tmp("wind_st_io.redb");
    let s = Store::open(&path).unwrap();
    let mut d = DailyStats::default();
    d.chinese = 42;
    s.put_daily_stat("2026-07-01", &d).unwrap();
    let text = s.export_stats_jsonl().unwrap();
    assert!(text.contains("2026-07-01"));

    let path2 = tmp("wind_st_io2.redb");
    let s2 = Store::open(&path2).unwrap();
    let mut local = DailyStats::default();
    local.chinese = 7;
    s2.put_daily_stat("2026-07-01", &local).unwrap();
    // overwrite=false:已存在日跳过
    let (imp, _) = s2.import_stats_jsonl(&text, false).unwrap();
    assert_eq!(imp, 0);
    assert_eq!(s2.get_daily_stat("2026-07-01").unwrap().chinese, 7);
    // overwrite=true:覆盖
    let (imp2, _) = s2.import_stats_jsonl(&text, true).unwrap();
    assert_eq!(imp2, 1);
    assert_eq!(s2.get_daily_stat("2026-07-01").unwrap().chinese, 42);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&path2);
}
```

`store.rs` tests 追加:

```rust
    #[test]
    fn list_data_schemas_across_tables() {
        let path = std::env::temp_dir().join("wind_store_schemas_test.redb");
        let _ = std::fs::remove_file(&path);
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 1).unwrap();
        s.learn_temp_word("py", "ni", "你", 1).unwrap();
        s.record_freq("sp", "x", "词").unwrap();
        s.pin_shadow("wb", "aa", "恭", None, 0).unwrap();
        let mut got = s.list_data_schemas().unwrap();
        got.sort();
        assert_eq!(got, vec!["py", "sp", "wb"]);
        let _ = std::fs::remove_file(&path);
    }
```

`phrases.rs` tests 追加:

```rust
    #[test]
    fn clear_user_phrases_keeps_system() {
        let path = tmp("wind_ph_clear.redb");
        let s = Store::open(&path).unwrap();
        s.sync_system_phrases(&[SystemPhrase {
            code: "sys".into(),
            text: "系统".into(),
            weight: 1,
            position: 0,
        }])
        .unwrap();
        s.add_phrase("u", "用户", 1, 0, true).unwrap();
        let n = s.clear_user_phrases().unwrap();
        assert_eq!(n, 1);
        let left = s.list_phrases().unwrap();
        assert!(left.iter().any(|p| p.is_system), "系统短语保留");
        assert!(!left.iter().any(|p| !p.is_system), "用户短语清空");
        let _ = std::fs::remove_file(&path);
    }
```

> 注:`SystemPhrase` 字段与 `add_phrase` 参数以 phrases.rs 实况签名为准微调(测试意图:一条系统短语 + 一条用户短语,clear 后只剩系统)。

- [x] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-store shadow_jsonl stats_jsonl list_data_schemas clear_user_phrases`
Expected: 编译错误,新方法未定义。

- [x] **Step 3: 实现**

`shadow.rs` 的 `impl Store` 追加:

```rust
    /// 导出某方案全部 shadow 规则为 jsonl(每行 {"code","rec"})。
    pub fn export_shadow_jsonl(&self, schema: &str) -> anyhow::Result<String> {
        let rules = self.list_shadow_rules(schema)?;
        let mut out = String::new();
        for (code, rec) in rules {
            out.push_str(&serde_json::to_string(
                &serde_json::json!({ "code": code, "rec": rec }),
            )?);
            out.push('\n');
        }
        Ok(out)
    }

    /// 从 jsonl 导入 shadow 规则(逐条 replay pin/delete,天然 upsert)。
    /// 返回 (imported=重放的规则条数, skipped=非法行数)。
    pub fn import_shadow_jsonl(&self, schema: &str, text: &str) -> anyhow::Result<(usize, usize)> {
        let mut imported = 0usize;
        let mut skipped = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                skipped += 1;
                continue;
            };
            let (Some(code), Some(rec)) = (
                v.get("code").and_then(|x| x.as_str()),
                v.get("rec")
                    .and_then(|x| serde_json::from_value::<ShadowRecord>(x.clone()).ok()),
            ) else {
                skipped += 1;
                continue;
            };
            for p in &rec.pinned {
                self.pin_shadow(schema, code, &p.word, p.cand_id.as_deref(), p.position)?;
                imported += 1;
            }
            for w in &rec.deleted {
                self.delete_shadow(schema, code, w)?;
                imported += 1;
            }
        }
        Ok((imported, skipped))
    }

    /// 清空某方案全部 shadow 规则(单写事务),返回删除键数。
    pub fn clear_shadow(&self, schema: &str) -> anyhow::Result<usize> {
        let prefix = format!("{schema}\u{0}");
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let n;
            {
                let mut t = txn.open_table(SHADOW)?;
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
```

`stats.rs` 的 `impl Store` 追加:

```rust
    /// 导出全部每日统计为 jsonl(每行 {"date","stat"})。
    pub fn export_stats_jsonl(&self) -> anyhow::Result<String> {
        let all = self.daily_stats("0000-01-01", "9999-12-31")?;
        let mut out = String::new();
        for (date, stat) in all {
            out.push_str(&serde_json::to_string(
                &serde_json::json!({ "date": date, "stat": stat }),
            )?);
            out.push('\n');
        }
        Ok(out)
    }

    /// 从 jsonl 导入每日统计。overwrite=false 时已存在日跳过(以本地为准)。
    /// 返回 (imported, skipped_bad_lines)。
    pub fn import_stats_jsonl(&self, text: &str, overwrite: bool) -> anyhow::Result<(usize, usize)> {
        let mut imported = 0usize;
        let mut skipped = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                skipped += 1;
                continue;
            };
            let (Some(date), Some(stat)) = (
                v.get("date").and_then(|x| x.as_str()),
                v.get("stat")
                    .and_then(|x| serde_json::from_value::<DailyStats>(x.clone()).ok()),
            ) else {
                skipped += 1;
                continue;
            };
            if !overwrite {
                let existing = self.daily_stats(date, date)?;
                if !existing.is_empty() {
                    continue;
                }
            }
            self.put_daily_stat(date, &stat)?;
            imported += 1;
        }
        Ok((imported, skipped))
    }
```

`store.rs` 的 `impl Store` 追加(与既有表常量同文件):

```rust
    /// 枚举四张按 schema 前缀编码的表(user/temp/freq/shadow)里出现过的全部 schema id。
    /// 备份用:确保有数据但未在当前配置启用的方案也被覆盖。
    pub fn list_data_schemas(&self) -> anyhow::Result<Vec<String>> {
        use redb::ReadableTable;
        let mut set = std::collections::BTreeSet::new();
        self.with_db(|db| {
            let txn = db.begin_read()?;
            for table in [USER_WORDS, TEMP_WORDS, FREQ, SHADOW] {
                let t = txn.open_table(table)?;
                for item in t.range::<&str>(..)? {
                    let (k, _) = item?;
                    if let Some((schema, _rest)) = k.value().split_once('\u{0}') {
                        set.insert(schema.to_string());
                    }
                }
            }
            Ok(())
        })?;
        Ok(set.into_iter().collect())
    }
```

`phrases.rs` 的 `impl Store` 追加:

```rust
    /// 清空全部用户短语(is_system 保留),返回删除条数。Replace 还原用。
    pub fn clear_user_phrases(&self) -> anyhow::Result<usize> {
        let rows = self.list_phrases()?;
        let mut n = 0usize;
        for p in rows.into_iter().filter(|p| !p.is_system) {
            self.remove_phrase(&p.code, &p.text)?;
            n += 1;
        }
        Ok(n)
    }
```

- [x] **Step 4: 跑测试确认通过(全 crate 回归)**

Run: `cd wind_input && cargo test -p wind-store`
Expected: PASS 全绿。

- [x] **Step 5: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-store
git add wind_input/crates/wind-store/src/shadow.rs wind_input/crates/wind-store/src/stats.rs wind_input/crates/wind-store/src/store.rs wind_input/crates/wind-store/src/phrases.rs
git commit -m "feat(store): shadow/stats jsonl 导出导入 + list_data_schemas + 清库原语"
```

---

### Task 3: wind-transfer——守卫下沉共享 + backup.rs 导出侧

**Files:**
- Modify: `wind_input/crates/wind-transfer/src/bundle.rs`(共享守卫)
- Modify: `wind_input/crates/wind-transfer/src/scheme.rs`(`entry_rel` 改薄包装)
- Create: `wind_input/crates/wind-transfer/src/backup.rs`
- Modify: `wind_input/crates/wind-transfer/src/lib.rs`(`pub mod backup;`)

**Interfaces:**
- Consumes: `BundleWriter::{new,add_bytes_with,finish}`、`Manifest::new`、`BundleKind::Backup`;Task 1/2 的全部 store 导出 API + `export_user_words_wdict`(P1)/`export_user_phrases_wdict`(既有)/`get_stats_meta`;`list_data_schemas`。
- Produces(Task 4/5 依赖):
  - bundle.rs: `pub fn validate_entry_rel<'a>(name: &'a str, required_prefix: &str) -> anyhow::Result<&'a str>`(P3 白名单守卫泛化:剥 required_prefix、非空、`\\`→`/` 归一后 components 全 `Normal` 且段内无 `:`)
  - backup.rs:
    - `pub struct BackupOptions { pub include_stats: bool, pub include_state: bool }`
    - `pub struct BackupSources<'a> { pub user_config_file: Option<&'a Path>, pub user_schemas_dir: Option<&'a Path>, pub user_themes_dir: Option<&'a Path>, pub state_file: Option<&'a Path> }`
    - `pub struct BackupResult { pub path: PathBuf, pub entries: Vec<String> }`
    - `pub fn create_backup(store: &wind_store::store::Store, src: &BackupSources, out_path: &Path, app_version: &str, platform: &str, created_at: &str, opts: &BackupOptions) -> anyhow::Result<BackupResult>`(schema 清单内部取 `store.list_data_schemas()`)

- [x] **Step 1: 守卫下沉(先改不新增行为,既有测试守护)**

bundle.rs 追加:

```rust
/// 校验归档条目名并返回剥去前缀的相对路径:必须 `required_prefix` 前缀、非空,
/// 且(`\`归一为`/`后)所有路径段均为普通段——components 白名单,
/// 拦 `..`/绝对/盘符相对(`C:foo`)/UNC/`.`,段内禁 `:`(NTFS ADS 防御)。
pub fn validate_entry_rel<'a>(
    name: &'a str,
    required_prefix: &str,
) -> anyhow::Result<&'a str> {
    let rel = name
        .strip_prefix(required_prefix)
        .ok_or_else(|| anyhow::anyhow!("非法条目(缺 {required_prefix} 前缀): {name}"))?;
    if rel.is_empty() {
        anyhow::bail!("非法条目(空路径): {name}");
    }
    let normalized = rel.replace('\\', "/");
    let ok = std::path::Path::new(&normalized).components().all(
        |c| matches!(c, std::path::Component::Normal(seg) if !seg.to_string_lossy().contains(':')),
    );
    if !ok {
        anyhow::bail!("非法条目(路径穿越): {name}");
    }
    Ok(rel)
}
```

scheme.rs 的 `entry_rel` 函数体替换为薄包装(签名与调用点不变):

```rust
fn entry_rel(name: &str) -> anyhow::Result<&str> {
    crate::bundle::validate_entry_rel(name, "schemas/")
}
```

Run: `cd wind_input && cargo test -p wind-transfer scheme bundle`
Expected: 全部既有测试(含穿越用例)仍绿——守卫语义零变化的证明。

- [x] **Step 2: 写 backup 导出失败测试**

`lib.rs` 追加 `pub mod backup;`。创建 `backup.rs`,tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn seed_store(dir: &std::path::Path) -> wind_store::store::Store {
        let s = wind_store::store::Store::open(dir.join("t.redb")).unwrap();
        s.add_user_word("wb", "a", "工", 100).unwrap();
        s.learn_temp_word("wb", "ab", "临", 5).unwrap();
        s.record_freq("wb", "a", "工").unwrap();
        s.pin_shadow("wb", "aa", "恭", None, 0).unwrap();
        s.add_phrase("bj", "北京", 10, 0, true).unwrap();
        s
    }

    #[test]
    fn create_backup_covers_all_sections() {
        let t = tempfile::tempdir().unwrap();
        let s = seed_store(t.path());
        // 文件域 fixtures
        let cfg = t.path().join("config.toml");
        fs::write(&cfg, "[ui]\n").unwrap();
        let schemas = t.path().join("schemas");
        fs::create_dir_all(schemas.join("my")).unwrap();
        fs::write(schemas.join("my.schema.toml"), "[schema]\nid=\"my\"\n").unwrap();
        fs::write(schemas.join("my/d.yaml"), "d").unwrap();
        let themes = t.path().join("themes");
        fs::create_dir_all(themes.join("dark")).unwrap();
        fs::write(themes.join("dark/theme.toml"), "[meta]\nname=\"dark\"\n").unwrap();
        let state = t.path().join("state.toml");
        fs::write(&state, "[toolbar]\n").unwrap();

        let out = t.path().join("backup.zip");
        let src = BackupSources {
            user_config_file: Some(&cfg),
            user_schemas_dir: Some(&schemas),
            user_themes_dir: Some(&themes),
            state_file: Some(&state),
        };
        let r = create_backup(
            &s, &src, &out, "1.0.0", "windows", "t",
            &BackupOptions { include_stats: true, include_state: true },
        )
        .unwrap();

        let m = crate::bundle::read_manifest(&out).unwrap();
        assert_eq!(m.kind, crate::bundle::BundleKind::Backup);
        let types: Vec<&str> = m.contents.iter().map(|e| e.r#type.as_str()).collect();
        for ty in ["config", "dict", "temp", "phrase", "freq", "shadow", "stats", "stats_meta", "schema_file", "theme_file", "state"] {
            assert!(types.contains(&ty), "缺 {ty} 条目; got {types:?}");
        }
        // dict 条目路径与 meta.schema
        let dict = m.contents.iter().find(|e| e.r#type == "dict").unwrap();
        assert_eq!(dict.path, "userdata/user_words/wb.wdict");
        assert_eq!(dict.meta.get("schema").and_then(|v| v.as_str()), Some("wb"));
        // schema_file 递归含子目录文件
        assert!(m.contents.iter().any(|e| e.path == "schemas/my/d.yaml"));
        // 载荷可取
        let bytes = crate::bundle::extract_entry(&out, "config/config.toml").unwrap();
        assert_eq!(bytes, b"[ui]\n");
        assert!(!r.entries.is_empty());
    }

    #[test]
    fn create_backup_options_exclude() {
        let t = tempfile::tempdir().unwrap();
        let s = seed_store(t.path());
        let out = t.path().join("b2.zip");
        let src = BackupSources {
            user_config_file: None,
            user_schemas_dir: None,
            user_themes_dir: None,
            state_file: None,
        };
        create_backup(
            &s, &src, &out, "1.0.0", "windows", "t",
            &BackupOptions { include_stats: false, include_state: false },
        )
        .unwrap();
        let m = crate::bundle::read_manifest(&out).unwrap();
        let types: Vec<&str> = m.contents.iter().map(|e| e.r#type.as_str()).collect();
        assert!(!types.contains(&"stats"), "include_stats=false 不含 stats");
        assert!(!types.contains(&"state"));
        assert!(!types.contains(&"config"), "无 config 源则无 config 条目");
        assert!(types.contains(&"dict"), "store 数据域始终导出");
    }
}
```

> 注:`add_phrase` 参数序以 phrases.rs 实况为准;seed 里的调用若签名不符,按实况微调(意图:各表各有一条数据)。

- [x] **Step 3: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-transfer backup`
Expected: 编译错误,`create_backup` 等未定义。

- [x] **Step 4: 实现导出侧**

`backup.rs` 顶部(tests 之上):

```rust
//! 整机备份:config + 逐表用户数据(文本)+ 用户方案/主题目录 + 可选 state,
//! 组合 bundle/merge/store 导出原语打成 kind=backup 的自描述 zip。
use crate::bundle::{BundleKind, BundleWriter, Manifest};
use std::path::{Path, PathBuf};
use wind_store::store::Store;

pub struct BackupOptions {
    pub include_stats: bool,
    pub include_state: bool,
}

pub struct BackupSources<'a> {
    pub user_config_file: Option<&'a Path>,
    pub user_schemas_dir: Option<&'a Path>,
    pub user_themes_dir: Option<&'a Path>,
    pub state_file: Option<&'a Path>,
}

pub struct BackupResult {
    pub path: PathBuf,
    pub entries: Vec<String>,
}

/// 递归收集目录下全部文件的 (zip条目名, 绝对路径);条目名 = prefix + 目录相对路径(`/`分隔)。
fn walk_dir(dir: &Path, prefix: &str) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                let rel = p
                    .strip_prefix(dir)
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((format!("{prefix}{rel}"), p));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// 创建整机备份。schema 清单取 `store.list_data_schemas()`(覆盖有数据但未启用的方案)。
pub fn create_backup(
    store: &Store,
    src: &BackupSources,
    out_path: &Path,
    app_version: &str,
    platform: &str,
    created_at: &str,
    opts: &BackupOptions,
) -> anyhow::Result<BackupResult> {
    let manifest = Manifest::new(BundleKind::Backup, app_version, platform, created_at);
    let mut w = BundleWriter::new(out_path, manifest)?;
    let mut entries = Vec::new();
    let mut add = |w: &mut BundleWriter,
                   name: String,
                   data: &[u8],
                   ty: &str,
                   meta: serde_json::Value|
     -> anyhow::Result<()> {
        w.add_bytes_with(&name, data, ty, meta)?;
        entries.push(name);
        Ok(())
    };

    // 文件域:config / state
    if let Some(cfg) = src.user_config_file {
        if cfg.is_file() {
            add(&mut w, "config/config.toml".into(), &std::fs::read(cfg)?, "config", serde_json::Value::Null)?;
        }
    }
    if opts.include_state {
        if let Some(st) = src.state_file {
            if st.is_file() {
                add(&mut w, "state/state.toml".into(), &std::fs::read(st)?, "state", serde_json::Value::Null)?;
            }
        }
    }

    // 数据域:逐 schema 四表 + 全局 phrases
    let schemas = store.list_data_schemas()?;
    for sc in &schemas {
        let meta = serde_json::json!({ "schema": sc });
        let words = store.export_user_words_wdict(sc, created_at)?;
        add(&mut w, format!("userdata/user_words/{sc}.wdict"), words.as_bytes(), "dict", meta.clone())?;
        let temp = store.export_temp_words_wdict(sc, created_at)?;
        add(&mut w, format!("userdata/temp_words/{sc}.wdict"), temp.as_bytes(), "temp", meta.clone())?;
        let freq = store.export_freq_jsonl(sc)?;
        add(&mut w, format!("userdata/freq/{sc}.jsonl"), freq.as_bytes(), "freq", meta.clone())?;
        let shadow = store.export_shadow_jsonl(sc)?;
        add(&mut w, format!("userdata/shadow/{sc}.jsonl"), shadow.as_bytes(), "shadow", meta)?;
    }
    let phrases = store.export_user_phrases_wdict(created_at)?;
    add(&mut w, "userdata/phrases.wdict".into(), phrases.as_bytes(), "phrase", serde_json::Value::Null)?;

    if opts.include_stats {
        let stats = store.export_stats_jsonl()?;
        add(&mut w, "userdata/stats.jsonl".into(), stats.as_bytes(), "stats", serde_json::Value::Null)?;
        let meta = store.get_stats_meta()?;
        add(&mut w, "userdata/stats_meta.json".into(), serde_json::to_vec(&meta)?.as_slice(), "stats_meta", serde_json::Value::Null)?;
    }

    // 文件域:用户方案 / 主题整目录
    if let Some(dir) = src.user_schemas_dir {
        if dir.is_dir() {
            for (name, path) in walk_dir(dir, "schemas/")? {
                let data = std::fs::read(&path)?;
                add(&mut w, name, &data, "schema_file", serde_json::Value::Null)?;
            }
        }
    }
    if let Some(dir) = src.user_themes_dir {
        if dir.is_dir() {
            for (name, path) in walk_dir(dir, "themes/")? {
                let data = std::fs::read(&path)?;
                add(&mut w, name, &data, "theme_file", serde_json::Value::Null)?;
            }
        }
    }

    w.finish()?;
    Ok(BackupResult { path: out_path.to_path_buf(), entries })
}
```

(闭包借用 `entries` 与 `w` 若打架,改为普通函数或直接内联三行——以编译通过的最小调整为准,语义不变。)

- [x] **Step 5: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-transfer`
Expected: PASS(backup 2 个 + 既有全部)。

- [x] **Step 6: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-transfer
git add wind_input/crates/wind-transfer/src/bundle.rs wind_input/crates/wind-transfer/src/scheme.rs wind_input/crates/wind-transfer/src/lib.rs wind_input/crates/wind-transfer/src/backup.rs
git commit -m "feat(transfer): 穿越守卫下沉共享 + 整机备份导出"
```

---

### Task 4: wind-transfer——backup 还原侧(sections 过滤 + Merge/Replace)

**Files:**
- Modify: `wind_input/crates/wind-transfer/src/backup.rs`

**Interfaces:**
- Consumes: Task 3 全部;`crate::bundle::{read_manifest, extract_entry, validate_entry_rel}`;`crate::merge::Strategy`;store 导入原语:`import_user_words_wdict`/`clear_user_words`、`import_temp_words_wdict`/`clear_temp_words`、`import_freq_jsonl`/`clear_freq`、`import_shadow_jsonl`/`clear_shadow`、`import_user_phrases_wdict`/`clear_user_phrases`、`import_stats_jsonl`/`clear_stats`、`put_stats_meta`。
- Produces(Task 5 依赖):
  - `pub struct RestoreTargets<'a> { pub user_config_file: Option<&'a Path>, pub user_schemas_dir: Option<&'a Path>, pub user_themes_dir: Option<&'a Path>, pub state_file: Option<&'a Path> }`
  - `pub struct RestoreResult { pub restored: Vec<String>, pub conflicts: Vec<String>, pub schemas_touched: Vec<String> }`
  - `pub fn restore_backup(package: &Path, store: &Store, targets: &RestoreTargets, strategy: crate::merge::Strategy, sections: Option<&[String]>) -> anyhow::Result<RestoreResult>`
  - type→section 映射函数 `fn section_of(ty: &str) -> &str`(schema_file→schemas、theme_file→themes、stats_meta→stats,其余原名)

- [x] **Step 1: 写失败测试**

`backup.rs` tests 追加:

```rust
    #[test]
    fn restore_roundtrip_full() {
        let t = tempfile::tempdir().unwrap();
        let s = seed_store(t.path());
        let cfg = t.path().join("config.toml");
        std::fs::write(&cfg, "[ui]\n").unwrap();
        let out = t.path().join("b.zip");
        let src = BackupSources {
            user_config_file: Some(&cfg),
            user_schemas_dir: None,
            user_themes_dir: None,
            state_file: None,
        };
        create_backup(&s, &src, &out, "1.0.0", "windows", "t",
            &BackupOptions { include_stats: true, include_state: false }).unwrap();

        // 全新目标环境
        let t2 = tempfile::tempdir().unwrap();
        let s2 = wind_store::store::Store::open(t2.path().join("t2.redb")).unwrap();
        let cfg2 = t2.path().join("config.toml");
        let targets = RestoreTargets {
            user_config_file: Some(&cfg2),
            user_schemas_dir: None,
            user_themes_dir: None,
            state_file: None,
        };
        let r = restore_backup(&out, &s2, &targets, crate::merge::Strategy::Merge, None).unwrap();
        assert!(r.conflicts.is_empty());
        assert!(r.schemas_touched.contains(&"wb".to_string()));
        assert_eq!(std::fs::read(&cfg2).unwrap(), b"[ui]\n");
        assert_eq!(s2.get_user_words("wb", "a").unwrap()[0].weight, 100);
        assert_eq!(s2.get_temp_word("wb", "ab", "临").unwrap(), Some(1));
        assert_eq!(s2.get_freq("wb", "a", "工").unwrap().unwrap().count, 1);
        assert_eq!(s2.list_shadow_rules("wb").unwrap().len(), 1);
        assert!(s2.list_phrases().unwrap().iter().any(|p| p.code == "bj"));
    }

    #[test]
    fn restore_sections_filter_and_merge_conflict() {
        let t = tempfile::tempdir().unwrap();
        let s = seed_store(t.path());
        let cfg = t.path().join("config.toml");
        std::fs::write(&cfg, "[ui]\n").unwrap();
        let out = t.path().join("b.zip");
        let src = BackupSources {
            user_config_file: Some(&cfg),
            user_schemas_dir: None, user_themes_dir: None, state_file: None,
        };
        create_backup(&s, &src, &out, "1.0.0", "windows", "t",
            &BackupOptions { include_stats: false, include_state: false }).unwrap();

        let t2 = tempfile::tempdir().unwrap();
        let s2 = wind_store::store::Store::open(t2.path().join("t2.redb")).unwrap();
        let cfg2 = t2.path().join("config.toml");
        std::fs::write(&cfg2, "LOCAL").unwrap();
        let targets = RestoreTargets {
            user_config_file: Some(&cfg2),
            user_schemas_dir: None, user_themes_dir: None, state_file: None,
        };
        // 只还原 config;Merge 下本地已存在 → conflict,内容不变
        let sections = vec!["config".to_string()];
        let r = restore_backup(&out, &s2, &targets, crate::merge::Strategy::Merge, Some(&sections)).unwrap();
        assert_eq!(r.conflicts, vec!["config/config.toml"]);
        assert_eq!(std::fs::read(&cfg2).unwrap(), b"LOCAL");
        assert!(s2.get_user_words("wb", "a").unwrap().is_empty(), "dict 未在 sections,不还原");
        // Replace 覆盖
        let r2 = restore_backup(&out, &s2, &targets, crate::merge::Strategy::Replace, Some(&sections)).unwrap();
        assert!(r2.conflicts.is_empty());
        assert_eq!(std::fs::read(&cfg2).unwrap(), b"[ui]\n");
    }

    #[test]
    fn restore_replace_clears_data_domain() {
        let t = tempfile::tempdir().unwrap();
        let s = seed_store(t.path());
        let out = t.path().join("b.zip");
        let src = BackupSources {
            user_config_file: None, user_schemas_dir: None,
            user_themes_dir: None, state_file: None,
        };
        create_backup(&s, &src, &out, "1.0.0", "windows", "t",
            &BackupOptions { include_stats: false, include_state: false }).unwrap();

        let t2 = tempfile::tempdir().unwrap();
        let s2 = wind_store::store::Store::open(t2.path().join("t2.redb")).unwrap();
        s2.add_user_word("wb", "zz", "杂", 1).unwrap(); // 本地杂词
        let targets = RestoreTargets {
            user_config_file: None, user_schemas_dir: None,
            user_themes_dir: None, state_file: None,
        };
        let sections = vec!["dict".to_string()];
        restore_backup(&out, &s2, &targets, crate::merge::Strategy::Replace, Some(&sections)).unwrap();
        let all = s2.search_user_words_prefix("wb", "", 0).unwrap();
        assert_eq!(all.len(), 1, "Replace 清掉杂词只剩备份内容");
        assert_eq!(all[0].code, "a");
    }
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-transfer backup::tests::restore`
Expected: 编译错误,`restore_backup` 未定义。

- [x] **Step 3: 实现还原侧**

`backup.rs` 追加(create_backup 之后、tests 之前):

```rust
pub struct RestoreTargets<'a> {
    pub user_config_file: Option<&'a Path>,
    pub user_schemas_dir: Option<&'a Path>,
    pub user_themes_dir: Option<&'a Path>,
    pub state_file: Option<&'a Path>,
}

pub struct RestoreResult {
    pub restored: Vec<String>,
    pub conflicts: Vec<String>,
    pub schemas_touched: Vec<String>,
}

/// 条目 type → section 名(sections 过滤用)。
fn section_of(ty: &str) -> &str {
    match ty {
        "schema_file" => "schemas",
        "theme_file" => "themes",
        "stats_meta" => "stats",
        other => other,
    }
}

/// 写单个文件(tmp+rename;Merge 已存在→false 表示冲突跳过;Replace 先删旧)。
fn write_file(
    target: &Path,
    bytes: &[u8],
    strategy: crate::merge::Strategy,
) -> anyhow::Result<bool> {
    if target.exists() && strategy == crate::merge::Strategy::Merge {
        return Ok(false);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("windinput.tmp");
    std::fs::write(&tmp, bytes)?;
    if target.exists() {
        std::fs::remove_file(target)?;
    }
    std::fs::rename(&tmp, target)?;
    Ok(true)
}

/// 还原整机备份。sections=None 还原全部;数据域 Replace 先清对应表,文件域 Merge 跳过已存在。
pub fn restore_backup(
    package: &Path,
    store: &Store,
    targets: &RestoreTargets,
    strategy: crate::merge::Strategy,
    sections: Option<&[String]>,
) -> anyhow::Result<RestoreResult> {
    let manifest = crate::bundle::read_manifest(package)?;
    if manifest.kind != BundleKind::Backup {
        anyhow::bail!("不是整机备份(kind={:?})", manifest.kind);
    }
    let wanted = |ty: &str| -> bool {
        match sections {
            None => true,
            Some(ss) => ss.iter().any(|s| s == section_of(ty)),
        }
    };
    let replace = strategy == crate::merge::Strategy::Replace;
    let mut restored = Vec::new();
    let mut conflicts = Vec::new();
    let mut schemas_touched: std::collections::BTreeSet<String> = Default::default();
    // Replace 的数据域清库只做一次(phrases/stats 全局;四表按 schema 首次遇到时清)。
    let mut cleared: std::collections::HashSet<String> = Default::default();

    for e in &manifest.contents {
        if !wanted(&e.r#type) {
            continue;
        }
        let bytes = crate::bundle::extract_entry(package, &e.path)?;
        let text = || String::from_utf8_lossy(&bytes).into_owned();
        let schema = e.meta.get("schema").and_then(|v| v.as_str()).unwrap_or("");
        match e.r#type.as_str() {
            "config" => {
                if let Some(target) = targets.user_config_file {
                    if write_file(target, &bytes, strategy)? {
                        restored.push(e.path.clone());
                    } else {
                        conflicts.push(e.path.clone());
                    }
                }
            }
            "state" => {
                if let Some(target) = targets.state_file {
                    if write_file(target, &bytes, strategy)? {
                        restored.push(e.path.clone());
                    } else {
                        conflicts.push(e.path.clone());
                    }
                }
            }
            "dict" if !schema.is_empty() => {
                if replace && cleared.insert(format!("dict:{schema}")) {
                    store.clear_user_words(schema)?;
                }
                store.import_user_words_wdict(schema, &text())?;
                schemas_touched.insert(schema.to_string());
                restored.push(e.path.clone());
            }
            "temp" if !schema.is_empty() => {
                if replace && cleared.insert(format!("temp:{schema}")) {
                    store.clear_temp_words(schema)?;
                }
                store.import_temp_words_wdict(schema, &text())?;
                schemas_touched.insert(schema.to_string());
                restored.push(e.path.clone());
            }
            "freq" if !schema.is_empty() => {
                if replace && cleared.insert(format!("freq:{schema}")) {
                    store.clear_freq(schema)?;
                }
                store.import_freq_jsonl(schema, &text())?;
                schemas_touched.insert(schema.to_string());
                restored.push(e.path.clone());
            }
            "shadow" if !schema.is_empty() => {
                if replace && cleared.insert(format!("shadow:{schema}")) {
                    store.clear_shadow(schema)?;
                }
                store.import_shadow_jsonl(schema, &text())?;
                schemas_touched.insert(schema.to_string());
                restored.push(e.path.clone());
            }
            "phrase" => {
                if replace && cleared.insert("phrase".into()) {
                    store.clear_user_phrases()?;
                }
                store.import_user_phrases_wdict(&text())?;
                restored.push(e.path.clone());
            }
            "stats" => {
                if replace && cleared.insert("stats".into()) {
                    store.clear_stats()?;
                }
                store.import_stats_jsonl(&text(), replace)?;
                restored.push(e.path.clone());
            }
            "stats_meta" => {
                if replace {
                    let meta: wind_store::stats::StatsMeta = serde_json::from_slice(&bytes)?;
                    store.put_stats_meta(&meta)?;
                    restored.push(e.path.clone());
                }
                // Merge:保留本地 meta(streak 等本机累积),跳过不计冲突。
            }
            "schema_file" => {
                if let Some(dir) = targets.user_schemas_dir {
                    let rel = crate::bundle::validate_entry_rel(&e.path, "schemas/")?;
                    if write_file(&dir.join(rel), &bytes, strategy)? {
                        restored.push(e.path.clone());
                    } else {
                        conflicts.push(e.path.clone());
                    }
                }
            }
            "theme_file" => {
                if let Some(dir) = targets.user_themes_dir {
                    let rel = crate::bundle::validate_entry_rel(&e.path, "themes/")?;
                    if write_file(&dir.join(rel), &bytes, strategy)? {
                        restored.push(e.path.clone());
                    } else {
                        conflicts.push(e.path.clone());
                    }
                }
            }
            _ => {} // 未知/空 schema 条目:静默忽略(向前兼容)
        }
    }
    Ok(RestoreResult {
        restored,
        conflicts,
        schemas_touched: schemas_touched.into_iter().collect(),
    })
}
```

(`wind_store::stats::StatsMeta` 的模块路径以实况为准——若 stats.rs 类型从 crate 根 re-export 则用相应路径。)

- [x] **Step 4: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-transfer`
Expected: PASS(backup 5 个 + 既有全部)。

- [x] **Step 5: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-transfer
git add wind_input/crates/wind-transfer/src/backup.rs
git commit -m "feat(transfer): 整机备份还原(sections 过滤 + Merge/Replace + 域级清库)"
```

---

### Task 5: RPC backup.create / inspect / restore + 刷新钩子 + 设计文档对齐

**Files:**
- Modify: `wind_input/crates/wind-coordinator/src/webdata.rs`(dispatch 三分支 + 三 handlers + 契约测试)
- Modify: `docs/design/import-export-backup-design.md`(整机备份布局按多方案子目录化实况更新)

**Interfaces:**
- Consumes: Task 3/4 的 `wind_transfer::backup::{create_backup, restore_backup, BackupOptions, BackupSources, RestoreTargets}`;`wind_transfer::bundle::read_manifest`;`wind_transfer::merge::Strategy`;`Config::{user_config_dir, local_dir, data_dir}`;`self.store`、`self.engine_mgr.invalidate_schema`、`self.reload_user_config()`(coordinator.rs:1357)、`self.rebuild_phrases()`(coordinator.rs:2915)。
- Produces(wind-setting 契约):
  - `backup.create {path, includeStats?, includeState?}` → `{path, manifest}`
  - `backup.inspect {path}` → `{manifest}`
  - `backup.restore {path, strategy?, sections?}` → `{restored, conflicts, schemasTouched}`

- [x] **Step 1: 写失败契约测试**

`webdata.rs` tests 追加(happy path 的 create/restore 会读写真实用户目录,契约测试只走 store 数据域为主的临时环境 + 错误路径;文件域 happy path 由 wind-transfer 单测覆盖):

```rust
#[test]
fn backup_rpc_contract() {
    let c = coord("backuprpc");
    // 种一条数据,create 到临时路径(coord 的 store 是临时 redb;文件域目录真实但只读不写:
    // create 只读取 config/schemas/themes,不写入它们)
    c.web_data_rpc(
        "dict.add",
        &json!({ "schemaId": "wb", "code": "a", "text": "工", "weight": 100 }),
    )
    .unwrap();
    let out = std::env::temp_dir().join("wind_backup_rpc_test.zip");
    let _ = std::fs::remove_file(&out);
    let r = c
        .web_data_rpc(
            "backup.create",
            &json!({ "path": out.to_string_lossy(), "includeStats": false }),
        )
        .unwrap();
    assert!(r.get("manifest").is_some());
    // inspect
    let ins = c
        .web_data_rpc("backup.inspect", &json!({ "path": out.to_string_lossy() }))
        .unwrap();
    assert_eq!(
        ins.get("manifest").and_then(|m| m.get("kind")).and_then(|v| v.as_str()),
        Some("backup")
    );
    // inspect 不存在的包 → 错误
    assert!(
        c.web_data_rpc(
            "backup.inspect",
            &json!({ "path": std::env::temp_dir().join("zz_no.zip").to_string_lossy() }),
        )
        .is_err()
    );
    // restore 仅数据域 sections(dict):写临时 store,不碰真实用户文件
    c.web_data_rpc("dict.clear", &json!({ "schemaId": "wb" })).unwrap();
    let rr = c
        .web_data_rpc(
            "backup.restore",
            &json!({ "path": out.to_string_lossy(), "sections": ["dict"] }),
        )
        .unwrap();
    assert!(rr.get("restored").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false));
    let listed = c
        .web_data_rpc("dict.listPaged", &json!({ "schemaId": "wb", "limit": 10 }))
        .unwrap();
    assert_eq!(listed.get("total").and_then(|v| v.as_u64()), Some(1));
    let _ = std::fs::remove_file(&out);
}
```

> 注:`backup.create` 会读取真实 `Config::user_config_dir()` 下的 config/schemas/themes(只读),并把 store 数据域(临时 redb)入包——安全。`backup.restore` 限定 `sections:["dict"]` 只写临时 store。

- [x] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-coordinator backup_rpc -- --nocapture`
Expected: FAIL,`unknown method: backup.create`。

- [x] **Step 3: 实现 dispatch 与 handlers**

dispatch 中 `"scheme.previewImport"` 分支之后追加:

```rust
            "backup.create" => self.web_backup_create(params),
            "backup.inspect" => self.web_backup_inspect(params),
            "backup.restore" => self.web_backup_restore(params),
```

handlers 区追加(紧邻 scheme handlers 之后;复用 P3 的 `Self::user_schemas_dir()`,新增主题/文件路径辅助):

```rust
    fn web_backup_create(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::backup::{create_backup, BackupOptions, BackupSources};
        let out = str_param(params, "path")?;
        let include_stats = params.get("includeStats").and_then(|v| v.as_bool()).unwrap_or(false);
        let include_state = params.get("includeState").and_then(|v| v.as_bool()).unwrap_or(false);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let user_dir = wind_config::Config::user_config_dir();
        let cfg_file = user_dir.as_ref().map(|d| d.join("config.toml"));
        let schemas_dir = user_dir.as_ref().map(|d| d.join("schemas"));
        let themes_dir = user_dir.as_ref().map(|d| d.join("themes"));
        let state_file = wind_config::Config::local_dir().map(|d| d.join("state.toml"));
        let src = BackupSources {
            user_config_file: cfg_file.as_deref(),
            user_schemas_dir: schemas_dir.as_deref(),
            user_themes_dir: themes_dir.as_deref(),
            state_file: state_file.as_deref(),
        };
        let r = create_backup(
            store,
            &src,
            std::path::Path::new(out),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            &chrono::Local::now().to_rfc3339(),
            &BackupOptions { include_stats, include_state },
        )?;
        let manifest = wind_transfer::bundle::read_manifest(&r.path)?;
        Ok(json!({ "path": r.path.to_string_lossy(), "manifest": serde_json::to_value(&manifest)? }))
    }

    fn web_backup_inspect(&self, params: &Value) -> anyhow::Result<Value> {
        let path = str_param(params, "path")?;
        let manifest = wind_transfer::bundle::read_manifest(std::path::Path::new(path))?;
        Ok(json!({ "manifest": serde_json::to_value(&manifest)? }))
    }

    fn web_backup_restore(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::backup::{restore_backup, RestoreTargets};
        use wind_transfer::merge::Strategy;
        let path = str_param(params, "path")?;
        let strategy = Strategy::from_param(
            params.get("strategy").and_then(|v| v.as_str()).unwrap_or(""),
        );
        let sections: Option<Vec<String>> = params.get("sections").and_then(|v| {
            v.as_array().map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
        });
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let user_dir = wind_config::Config::user_config_dir();
        let cfg_file = user_dir.as_ref().map(|d| d.join("config.toml"));
        let schemas_dir = user_dir.as_ref().map(|d| d.join("schemas"));
        let themes_dir = user_dir.as_ref().map(|d| d.join("themes"));
        let state_file = wind_config::Config::local_dir().map(|d| d.join("state.toml"));
        let targets = RestoreTargets {
            user_config_file: cfg_file.as_deref(),
            user_schemas_dir: schemas_dir.as_deref(),
            user_themes_dir: themes_dir.as_deref(),
            state_file: state_file.as_deref(),
        };
        let r = restore_backup(
            std::path::Path::new(path),
            store,
            &targets,
            strategy,
            sections.as_deref(),
        )?;
        // 刷新:config 域生效、短语重建、涉及方案失效缓存(未加载时安全 no-op)。
        let touched_config = r.restored.iter().any(|p| p.starts_with("config/"));
        let touched_phrase = r.restored.iter().any(|p| p == "userdata/phrases.wdict");
        for id in &r.schemas_touched {
            self.engine_mgr.invalidate_schema(id);
        }
        for p in &r.restored {
            if let Some(rel) = p.strip_prefix("schemas/") {
                if let Some(id) = rel.strip_suffix(".schema.toml") {
                    if !id.contains('/') {
                        self.engine_mgr.invalidate_schema(id);
                    }
                }
            }
        }
        if touched_phrase {
            self.rebuild_phrases();
        }
        if touched_config {
            self.reload_user_config();
        }
        Ok(json!({
            "restored": r.restored,
            "conflicts": r.conflicts,
            "schemasTouched": r.schemas_touched,
        }))
    }
```

(`reload_user_config`/`rebuild_phrases` 为同 crate `impl Coordinator` 方法可直接调;若签名带返回值,忽略即可。)

- [x] **Step 4: 跑测试确认通过(含既有 webdata 回归)**

Run: `cd wind_input && cargo test -p wind-coordinator webdata`
Expected: PASS。

- [x] **Step 5: 设计文档布局更新**

`docs/design/import-export-backup-design.md` 的「整机备份 `.zip`(kind=backup)」布局代码块替换为 Global Constraints 中的实际布局(多方案子目录化),并在该小节末尾追加:`用户数据按方案拆分子目录(userdata/user_words/<schema>.wdict 等),manifest 条目以 type+meta.schema 标注归属;stats_meta 随 include_stats 一并导出。backup 域无独立 preview,由 backup.inspect(manifest 清单)承担还原前概览。`

- [x] **Step 6: 格式化并提交**

```bash
cd wind_input && cargo fmt -p wind-coordinator
git add wind_input/crates/wind-coordinator/src/webdata.rs docs/design/import-export-backup-design.md
git commit -m "feat(rpc): backup.create/inspect/restore 整机备份三件套"
```

---

## P4 收尾验证

- [x] `cd wind_input && cargo test -p wind-store -p wind-transfer -p wind-coordinator` 全绿。
- [x] `cd wind_input && cargo build` 成功。
- [x] `cd wind_input && cargo fmt -p wind-store -p wind-transfer -p wind-coordinator -- --check` 干净(不含既有漂移文件)。

## P4 交付物

- store:temp/freq/shadow/stats 逐表文本导出导入 + `list_data_schemas`(覆盖孤儿数据方案)+ 三个清库原语。
- wind-transfer:穿越守卫下沉共享(`validate_entry_rel`);`backup.rs` 整机导出/还原(sections 过滤、Merge/Replace、域级一次性清库)。
- RPC:`backup.create/inspect/restore` 契约就位,还原后自动刷新(config 热重载/短语重建/方案缓存失效)。

## 遗留(P5 打磨)

- 方案导出 override 布局已知限制(P3 遗留,用户已决策)。
- `web_schema_list` builtin 恒 true。
- 还原文件域的多文件覆盖回滚。
- **数据域还原非原子**(P4 审查产出,与上一条文件域回滚是不同的面):restore 逐条目独立事务,多 schema Replace 还原中途失败(如坏 jsonl)会留下"部分域已清已导、当前域已清未导"的中间态且无回滚;若要原子性需把数据域整体包进单个写事务(改造量大)。当前定性为已知限制。
- restore 循环对确定 no-op 的条目(Merge 下的 stats_meta、目标为 None 的 config/state)仍先解压载荷,属无用 I/O,可把 extract_entry 下推到分支内。
- 真机验证:备份→还原→重启全链路;跨机还原(平台标注提示)。
