//! Web 设置数据 RPC：schema/dict/temp/freq/shadow/stats/theme/phrase 命名空间。
//!
//! 经 wind-rpc 的 `CoreRpc::data_rpc` 转发到此（service 的 RpcCore 适配）。
//! 方法名与前端 `contract.ts` 1:1 一致。
//!
//! 接入进度：契约全部数据域方法均接通真实 store/engine/theme：
//! - schema.*（含三层合并 getConfig/saveConfig/resetConfig/setDictEnabled/delete）
//! - dict.*（含 encode/genPinyin 反查出码）、temp.*、freq.*、shadow.*、phrase.*、stats.*、theme.*
//! - `schema.references` 暂返 `{}`（删除安全检查未用，前端宽松消费）；
//!   无 store/themes 时各方法返回合法空集（降级，不报错）。

use serde_json::{Value, json};

use crate::coordinator::Coordinator;

/// 解析方案的权威引擎类型（schema.toml 的 engine.type 可能为空，
/// 此时按 Schema::is_pinyin/is_mixed 依据默认词库类型推断）。
fn resolve_engine_type(s: &wind_config::Schema) -> &'static str {
    if s.is_mixed() {
        "mixed"
    } else if s.is_pinyin() {
        "pinyin"
    } else {
        "codetable"
    }
}

fn str_param<'a>(p: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    p.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("缺少参数 {}", key))
}

fn i32_param(p: &Value, key: &str) -> i32 {
    p.get(key).and_then(|v| v.as_i64()).unwrap_or(0) as i32
}

fn usize_param(p: &Value, key: &str, default: usize) -> usize {
    p.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(default)
}

/// 解析排序参数。sortBy 在 valid_fields 中时返回 (字段名, is_desc)，否则返回 None（保持原顺序）。
fn parse_sort<'a>(params: &'a Value, valid_fields: &[&str]) -> Option<(&'a str, bool)> {
    let by = params.get("sortBy")?.as_str()?;
    if !valid_fields.contains(&by) {
        return None;
    }
    let desc = params.get("sortOrder").and_then(|v| v.as_str()) == Some("desc");
    Some((by, desc))
}

/// 本地今天日期 "YYYY-MM-DD"（统计摘要的参照点）。
fn today_str() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

impl Coordinator {
    /// 枚举本机字体：返回 (family, display_name)。family 为匹配/渲染用名(通常英文),
    /// display_name 优先取该字体含 CJK 字符的本地化名(如"微软雅黑"),否则同 family。
    /// 首次调用扫描系统字体目录（fontdb），开销可接受（设置页打开字体选择时一次）。
    pub fn list_font_families(&self) -> Vec<(String, String)> {
        fn has_cjk(s: &str) -> bool {
            s.chars().any(|c| {
                let u = c as u32;
                (0x4E00..=0x9FFF).contains(&u) // CJK 统一表意
                    || (0x3400..=0x4DBF).contains(&u) // 扩展 A
                    || (0xF900..=0xFAFF).contains(&u) // 兼容表意
            })
        }
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        // family(英文) → 显示名;同 family 保留首个本地化名。BTreeMap 去重 + 按 family 升序。
        let mut map: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
        for face in db.faces() {
            let Some((family, _)) = face.families.first() else {
                continue;
            };
            if family.is_empty() {
                continue;
            }
            let display = face
                .families
                .iter()
                .map(|(n, _)| n)
                .find(|n| has_cjk(n))
                .cloned()
                .unwrap_or_else(|| family.clone());
            map.entry(family.clone()).or_insert(display);
        }
        map.into_iter().collect()
    }

    /// 数据类 RPC 总分派。方法名以 `<ns>.<method>` 形式分组；未知方法返回 Err。
    pub fn web_data_rpc(&self, method: &str, params: &Value) -> anyhow::Result<Value> {
        match method {
            // ── schema.* ─────────────────────────────────────────
            "schema.list" => self.web_schema_list(),
            "schema.layouts" => self.web_schema_layouts(),
            "schema.active" => Ok(json!({ "id": self.engine_mgr.active_schema_id() })),
            "schema.setActive" => {
                let ok = self.engine_mgr.switch_schema(str_param(params, "id")?);
                Ok(json!({ "ok": ok }))
            }
            // ── 方案配置编辑（三层合并：默认 ← 方案文件 ← override 层）──
            "schema.getConfig" => self.web_schema_get_config(params),
            "schema.saveConfig" => self.web_schema_save_config(params),
            "schema.resetConfig" => self.web_schema_reset_config(params),
            "schema.setDictEnabled" => self.web_schema_set_dict_enabled(params),
            "schema.delete" => self.web_schema_delete(params),
            "schema.references" => Ok(json!({})), // 引用关系（删除安全检查）：暂返空，前端宽松消费
            "scheme.exportPackage" => self.web_scheme_export_package(params),
            "scheme.importPackage" => self.web_scheme_import_package(params),
            "scheme.previewImport" => self.web_scheme_preview_import(params),

            // ── backup.*（整机备份，wind-transfer::backup）───────
            "backup.create" => self.web_backup_create(params),
            "backup.inspect" => self.web_backup_inspect(params),
            "backup.restore" => self.web_backup_restore(params),

            // ── dict.*（用户词库，redb 持久化）────────────────────
            "dict.listPaged" => self.web_dict_list_paged(params),
            "dict.search" => self.web_dict_search(params),
            "dict.add" => self.web_dict_add(params),
            "dict.update" => self.web_dict_update(params),
            "dict.remove" => self.web_dict_remove(params),
            "dict.clear" => self.web_dict_clear(params),
            "dict.stats" => self.web_dict_stats(),
            // 加词自动出码：按方案类型选拼音/五笔规则（reverse 反查表）。
            "dict.encode" => self.web_dict_encode(params),
            "dict.genPinyin" => {
                let text = str_param(params, "text")?;
                Ok(json!(self.gen_pinyin_word(text)))
            }
            "dict.export" => self.web_dict_export(params),
            "dict.import" => self.web_dict_import(params),
            "dict.previewImport" => self.web_dict_preview_import(params),

            // ── temp.*（临时词，redb）─────────────────────────────
            "temp.list" => self.web_temp_list(params),
            "temp.promote" => self.web_temp_promote(params),
            "temp.remove" => self.web_temp_remove(params),
            "temp.promoteAll" => self.web_temp_promote_all(params),
            "temp.clear" => self.web_temp_clear(params),

            // ── freq.*（用户词频，redb 持久化）───────────────────
            "freq.listPaged" => self.web_freq_list_paged(params),
            "freq.delete" => self.web_freq_delete(params),
            "freq.clear" => self.web_freq_clear(params),

            // ── shadow.*（影子规则，redb 持久化）─────────────────
            "shadow.list" => self.web_shadow_list(params),
            "shadow.pin" => self.web_shadow_pin(params),
            "shadow.delete" => self.web_shadow_delete(params),
            "shadow.removeRule" => self.web_shadow_remove_rule(params),
            "shadow.addRule" => self.web_shadow_add_rule(params),

            // ── phrase.*（用户短语，全局，redb 持久化）──────────
            "phrase.list" => self.web_phrase_list(),
            "phrase.add" => self.web_phrase_add(params),
            "phrase.update" => self.web_phrase_update(params),
            "phrase.remove" => self.web_phrase_remove(params),
            "phrase.setEnabled" => self.web_phrase_set_enabled(params),
            "phrase.resetDefault" => self.web_phrase_reset(),
            "phrase.listSystem" => self.web_phrase_list_system(),
            "phrase.listUser" => self.web_phrase_list_user(params),
            "phrase.export" => self.web_phrase_export(),
            "phrase.import" => self.web_phrase_import(params),
            "phrase.resetSystem" => self.web_phrase_reset_system(),

            // ── stats.*（输入统计，redb 每日聚合）────────────────
            "stats.summary" => self.web_stats_summary(),
            "stats.daily" => self.web_stats_daily(params),
            "stats.clear" => self.web_stats_clear(),
            "stats.pruneBefore" => self.web_stats_prune(params),

            // ── theme.* ──────────────────────────────────────────
            "theme.list" => self.web_theme_list(),
            "theme.preview" => self.web_theme_preview(params),
            "theme.getText" => self.web_theme_get_text(params),
            "theme.delete" => self.web_theme_delete(params),
            "theme.importFromText" => self.web_theme_import_text(params),
            "theme.importFromUrl" => {
                anyhow::bail!("URL 导入未启用（features.theme.import_url=false）")
            }

            other => anyhow::bail!("unknown method: {}", other),
        }
    }

    fn web_schema_list(&self) -> anyhow::Result<Value> {
        use std::collections::HashMap;

        // 设置页方案下拉的显示顺序（三段式，段间固定先后）：
        //   ① 已启用的拼音方案（数量少、最常用，置顶）
        //   ② 其余已启用方案，按 config.schema.available 配置顺序
        //   ③ 未启用方案（磁盘扫到但不在 available），按类型分组「拼音→码表→混输」，组内按 id 字典序
        // 底层 installed_schemas() 仍返回 id 字典序全集（做稳定去重锚点），排序只在此展示层重排。

        // 已启用方案 → 配置位置索引，供段①②保持配置顺序。
        let available = self.engine_mgr.available_schemas();
        let avail_pos: HashMap<&str, usize> = available
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_str(), i))
            .collect();

        // 未启用段的类型分组顺序：拼音→码表→混输。
        fn type_rank(t: &str) -> i64 {
            match t {
                "pinyin" => 0,
                "codetable" => 1,
                "mixed" => 2,
                _ => 3,
            }
        }

        // 复合排序键 (段号, 段内主键, id)：段号先分档；段内主键在启用段是配置位置、
        // 在未启用段是类型档；id 仅在未启用段做字典序 tiebreak（启用段位置唯一，不参与）。
        let mut rows: Vec<((u8, i64, String), Value)> = self
            .engine_mgr
            .installed_schemas()
            .iter()
            .map(|id| {
                // 取合并后 Schema 一次，带出方案元信息（备注/版本/图标/作者），供设置页方案列表与详情显示。
                let merged = self.engine_mgr.schema_merged(id);
                let engine_type = merged
                    .as_ref()
                    .map(resolve_engine_type)
                    .unwrap_or("codetable");
                let info = merged.as_ref().map(|s| &s.schema);
                let item = json!({
                    "id": id,
                    "name": self.engine_mgr.schema_name(id),
                    "engineType": merged.as_ref().map(resolve_engine_type),
                    "scheme": merged.as_ref().map(|s| {
                        if resolve_engine_type(s) == "pinyin" {
                            s.engine.pinyin.scheme.clone()
                        } else {
                            String::new()
                        }
                    }).unwrap_or_default(),
                    // 用户目录存在同名 schema.toml 即视为用户方案（可删除）；否则内置。
                    "builtin": !self.engine_mgr.is_user_schema(id),
                    "description": info.map(|i| i.description.clone()).unwrap_or_default(),
                    "version": info.map(|i| i.version.clone()).unwrap_or_default(),
                    "icon_label": info.map(|i| i.icon_label.clone()).unwrap_or_default(),
                    "author": info.map(|i| i.author.clone()).unwrap_or_default(),
                });

                let key = match avail_pos.get(id.as_str()) {
                    // 已启用：拼音置顶(段0)，其余(段1)，段内按配置位置。
                    Some(&pos) => {
                        let seg = if engine_type == "pinyin" { 0 } else { 1 };
                        (seg, pos as i64, String::new())
                    }
                    // 未启用(段2)：按类型档 + id 字典序。
                    None => (2, type_rank(engine_type), id.clone()),
                };
                (key, item)
            })
            .collect();

        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let items: Vec<Value> = rows.into_iter().map(|(_, item)| item).collect();
        Ok(json!(items))
    }

    /// 双拼布局清单：合并扫描安装目录与用户目录的 `schemas/shuangpin/*.toml`，
    /// 返回 `[{id, name}]`，供设置页"双拼布局"下拉动态取值（取代前端硬编码）。
    fn web_schema_layouts(&self) -> anyhow::Result<Value> {
        let items: Vec<Value> = self
            .engine_mgr
            .shuangpin_layouts()
            .into_iter()
            .map(|(id, name)| json!({ "id": id, "name": name }))
            .collect();
        Ok(json!(items))
    }

    fn web_dict_list_paged(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let prefix = params
            .get("prefix")
            .and_then(|v| v.as_str())
            .or_else(|| params.get("query").and_then(|v| v.as_str()))
            .unwrap_or("");
        let limit = usize_param(params, "limit", 50);
        let offset = usize_param(params, "offset", 0);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let mut all = store.search_user_words_prefix(&schema, prefix, 0)?;
        // 词条内容搜索：并入 text 包含搜索词的词条（编码前缀 ∪ 词条内容包含，去重）。
        // 前缀项走 redb 有序前缀扫描；内容项需全量扫描，仅在有搜索词时才付出该代价。
        if !prefix.is_empty() {
            let q = prefix.to_lowercase();
            let seen: std::collections::HashSet<(String, String)> = all
                .iter()
                .map(|w| (w.code.clone(), w.text.clone()))
                .collect();
            for w in store.search_user_words_prefix(&schema, "", 0)? {
                if w.text.to_lowercase().contains(&q)
                    && !seen.contains(&(w.code.clone(), w.text.clone()))
                {
                    all.push(w);
                }
            }
        }
        let total = all.len();
        // 有 sortBy 时在切片前排序，实现跨页全局排序
        if let Some((by, desc)) = parse_sort(params, &["code", "text", "weight"]) {
            all.sort_by(|a, b| {
                let ord = match by {
                    "weight" => a.weight.cmp(&b.weight),
                    "text" => a.text.cmp(&b.text),
                    _ => a.code.cmp(&b.code),
                };
                if desc { ord.reverse() } else { ord }
            });
        }
        let items: Vec<Value> = all
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(word_item)
            .collect();
        Ok(json!({ "items": items, "total": total }))
    }

    fn web_dict_search(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let query = str_param(params, "query").unwrap_or("");
        let limit = usize_param(params, "limit", 50);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let items: Vec<Value> = store
            .search_user_words_prefix(&schema, query, limit)?
            .into_iter()
            .map(word_item)
            .collect();
        Ok(json!(items))
    }

    fn web_dict_add(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let weight = i32_param(params, "weight");
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.add_user_word(&schema, code, text, weight)?;
        Ok(json!({ "ok": true }))
    }

    fn web_dict_update(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let weight = i32_param(params, "weight");
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 存在则改权重；不存在则新增（upsert 语义）。
        if !store.update_user_word_weight(&schema, code, text, weight)? {
            store.add_user_word(&schema, code, text, weight)?;
        }
        Ok(json!({ "ok": true }))
    }

    fn web_dict_remove(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.remove_user_word(&schema, code, text)?;
        Ok(json!({ "ok": true }))
    }

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

    fn web_dict_export(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let content = store.export_user_words_wdict(&schema, &chrono::Local::now().to_rfc3339())?;
        Ok(json!({ "content": content }))
    }

    fn web_dict_import(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::merge::{ImportOutcome, Strategy};
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let content = str_param(params, "content")?;
        let strategy = Strategy::from_param(
            params
                .get("strategy")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 格式按内容自动识别(WindDict/Rime/TSV),解析后统一走 import_user_words 管线。
        let (_fmt, rows, skipped) = wind_store::import_formats::parse_words_auto(content)
            .map_err(|e| anyhow::anyhow!(e))?;
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
        let (fmt, rows, skipped) = wind_store::import_formats::parse_words_auto(content)
            .map_err(|e| anyhow::anyhow!(e))?;
        let (c, samples) = store.preview_import_user_words(&schema, &rows)?;
        // 按 Merge 语义预览(与设计 RPC 表一致,不收 strategy);willConflict 词库域恒 0,字段保留。
        let mut v = serde_json::to_value(ImportPreview {
            will_add: c.added,
            will_update: c.updated,
            will_conflict: 0,
            unchanged: c.unchanged,
            samples,
        })?;
        // 附加识别信息:格式标识 + 解析期跳过行数(UI 预览提示用)。
        if let Some(o) = v.as_object_mut() {
            o.insert("format".into(), json!(fmt.as_str()));
            o.insert("skipped".into(), json!(skipped));
        }
        Ok(v)
    }

    fn web_dict_stats(&self) -> anyhow::Result<Value> {
        let store = match self.store.as_ref() {
            Some(s) => s,
            None => return Ok(json!([])),
        };
        let mut out = Vec::new();
        for id in self.engine_mgr.available_schemas().iter() {
            let user_words = store
                .search_user_words_prefix(id, "", 0)
                .map(|v| v.len())
                .unwrap_or(0);
            let temp_words = store
                .search_temp_words_prefix(id, "", 0)
                .map(|v| v.len())
                .unwrap_or(0);
            let shadow_rules = store
                .list_shadow_rules(id)
                .map(|v| {
                    v.iter()
                        .map(|(_, r)| r.pinned.len() + r.deleted.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            out.push(json!({
                "schemaId": id,
                "name": self.engine_mgr.schema_name(id),
                "userWords": user_words,
                "tempWords": temp_words,
                "shadowRules": shadow_rules,
            }));
        }
        Ok(json!(out))
    }

    fn web_schema_get_config(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        match self.engine_mgr.schema_merged(id) {
            Some(schema) => {
                let etype = resolve_engine_type(&schema);
                let mut v = serde_json::to_value(schema)?;
                // 确保 engine.type 为解析后的权威类型（schema.toml 可能未显式声明）
                if let Some(eng) = v.get_mut("engine").and_then(|e| e.as_object_mut()) {
                    eng.insert("type".to_string(), json!(etype));
                }
                Ok(v)
            }
            None => Ok(json!({})),
        }
    }

    fn web_schema_save_config(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        let cfg = params
            .get("cfg")
            .ok_or_else(|| anyhow::anyhow!("缺少参数 cfg"))?;
        let base = self
            .engine_mgr
            .schema_base(id)
            .ok_or_else(|| anyhow::anyhow!("方案不存在: {}", id))?;
        let base_json = serde_json::to_value(&base)?;
        // 稀疏 diff（仅变化项）写入 override 层，让方案文件后续更新仍能透传未改项。
        let diff = json_diff(&base_json, cfg).unwrap_or(json!({}));
        let mut ov = json_to_toml(&diff);
        // 保留既有 override 的 dictionaries（附加词库开关由 setDictEnabled 单独管理）。
        if let toml::Value::Table(t) = &mut ov
            && !t.contains_key("dictionaries")
            && let Some(prev) = self.engine_mgr.get_schema_override(id)
            && let Some(d) = prev.get("dictionaries")
        {
            t.insert("dictionaries".to_string(), d.clone());
        }
        self.engine_mgr.write_schema_override(id, &ov)?;
        Ok(json!({ "ok": true }))
    }

    fn web_schema_reset_config(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        self.engine_mgr.delete_schema_override(id)?;
        Ok(json!({ "ok": true }))
    }

    fn web_schema_set_dict_enabled(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        let dict_id = str_param(params, "dictId")?;
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let mut merged = self
            .engine_mgr
            .schema_merged(id)
            .ok_or_else(|| anyhow::anyhow!("方案不存在: {}", id))?;
        let mut found = false;
        for d in merged.dictionaries.iter_mut() {
            if d.id == dict_id {
                d.enabled = Some(enabled);
                found = true;
            }
        }
        if !found {
            anyhow::bail!("方案 {} 无附加词库 {}", id, dict_id);
        }
        // override 层写入完整 dictionaries 数组（合并时整体替换），保留其它 override 字段。
        let dicts_val = toml::Value::try_from(&merged.dictionaries)?;
        let mut ov = self
            .engine_mgr
            .get_schema_override(id)
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        if !ov.is_table() {
            ov = toml::Value::Table(Default::default());
        }
        if let toml::Value::Table(t) = &mut ov {
            t.insert("dictionaries".to_string(), dicts_val);
        }
        // 持久化 override（不 invalidate），再对已加载引擎 live 翻该扩展层的 enabled 标志——
        // 扩展词库热插拔：无需重熔大词库即时生效；未加载方案下次构建按新 override 生效。
        self.engine_mgr.persist_schema_override(id, &ov)?;
        let live = self.engine_mgr.set_dict_enabled_live(id, dict_id, enabled);
        Ok(json!({ "ok": true, "live": live }))
    }

    fn web_schema_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        if !self.engine_mgr.is_user_schema(id) {
            anyhow::bail!("内置方案不可删除: {id}");
        }
        let user = Self::user_schemas_dir()?;
        let system = Self::system_schemas_dir();
        // 共享检查基准 = 其余已安装方案(含内置——混输可能引用用户资源)。
        let keep: Vec<String> = self
            .engine_mgr
            .installed_schemas()
            .into_iter()
            .filter(|s| s != id)
            .collect();
        // 镜像导入的收集逻辑删文件:方案文件+引用资源+递归引用的用户方案,共享保留。
        let r = wind_transfer::scheme::delete_package(id, &user, system.as_deref(), &keep)?;
        // 级联清词库数据:仅清数据域=方案自身的(拼音族数据在共享 pinyin 域,
        // data_schema_id≠自身时跳过;文件已删读不到类型时回落自身,清空域无害)。
        if let Some(store) = self.store.as_ref() {
            for sid in &r.schema_ids {
                if self.engine_mgr.data_schema_id(sid) == *sid {
                    store.clear_user_words(sid)?;
                    store.clear_temp_words(sid)?;
                    store.clear_freq(sid)?;
                    store.clear_shadow(sid)?;
                }
            }
        }
        for sid in &r.schema_ids {
            self.engine_mgr.forget_deleted_schema(sid);
        }
        Ok(json!({
            "ok": true,
            "deleted": r.deleted,
            "keptShared": r.kept_shared,
            "schemaIds": r.schema_ids,
        }))
    }

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
            params
                .get("strategy")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let user = Self::user_schemas_dir()?;
        let r = wind_transfer::scheme::import_package(std::path::Path::new(path), &user, strategy)?;
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
            // v2:包元信息来自可选 package.toml(缺失时各字段为空串,前端显示"未知")。
            "package": serde_json::to_value(&p.meta)?,
            "willAdd": p.will_add,
            "conflicts": p.conflicts,
            "systemRefs": p.system_refs,
            "missing": p.missing,
        }))
    }

    fn web_backup_create(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::backup::{BackupOptions, BackupSources, create_backup};
        let out = str_param(params, "path")?;
        let include_stats = params
            .get("includeStats")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_state = params
            .get("includeState")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
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
            &BackupOptions {
                include_stats,
                include_state,
            },
        )?;
        let manifest = wind_transfer::bundle::read_manifest(&r.path)?;
        Ok(json!({
            "path": r.path.to_string_lossy(),
            "manifest": serde_json::to_value(&manifest)?,
        }))
    }

    fn web_backup_inspect(&self, params: &Value) -> anyhow::Result<Value> {
        let path = str_param(params, "path")?;
        let manifest = wind_transfer::bundle::read_manifest(std::path::Path::new(path))?;
        Ok(json!({ "manifest": serde_json::to_value(&manifest)? }))
    }

    fn web_backup_restore(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::backup::{RestoreTargets, restore_backup};
        use wind_transfer::merge::Strategy;
        let path = str_param(params, "path")?;
        let strategy = Strategy::from_param(
            params
                .get("strategy")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
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

    fn web_dict_encode(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let text = str_param(params, "text")?;
        // 拼音类方案出拼音码；其余（码表/五笔）出五笔词组码。
        let is_pinyin = self
            .engine_mgr
            .schema_engine_type(schema)
            .map(|t| t == "pinyin")
            .unwrap_or(false);
        let code = if is_pinyin {
            // 优先词级消歧（多音字按词典权重），引擎无果时回退逐字反查表。
            self.engine_mgr
                .generate_word_pinyin(schema, text)
                .unwrap_or_else(|| self.reverse.gen_pinyin(text))
        } else {
            self.reverse.wubi_word_code(text)
        };
        Ok(json!(code))
    }

    /// 为词语生成拼音码：优先用拼音引擎词级消歧（活跃方案→"pinyin"方案），
    /// 都无果时回退逐字反查表（pinyin_map.txt）。用于 dict.genPinyin（无方案上下文）。
    fn gen_pinyin_word(&self, text: &str) -> String {
        let active = self.engine_mgr.active_schema_id();
        self.engine_mgr
            .generate_word_pinyin(&active, text)
            .or_else(|| self.engine_mgr.generate_word_pinyin("pinyin", text))
            .unwrap_or_else(|| self.reverse.gen_pinyin(text))
    }

    fn web_freq_list_paged(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let prefix = params.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
        let offset = usize_param(params, "offset", 0);
        let limit = usize_param(params, "limit", 50);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let sort = parse_sort(params, &["code", "text", "count", "lastUsed"]);
        // 无搜索且无排序：走 store 分页快路径；否则全量拉取
        //（编码前缀 ∪ 词条内容包含）→ 排序 → 内存切片。
        let (page, total) = if prefix.is_empty() && sort.is_none() {
            store.list_freq_paged(&schema, "", offset, limit)?
        } else {
            let (mut all, _) = store.list_freq_paged(&schema, prefix, 0, 0)?;
            // 词条内容搜索：并入 text 包含搜索词的词条（去重）。
            if !prefix.is_empty() {
                let q = prefix.to_lowercase();
                let seen: std::collections::HashSet<(String, String)> =
                    all.iter().map(|(c, t, _)| (c.clone(), t.clone())).collect();
                let (rest, _) = store.list_freq_paged(&schema, "", 0, 0)?;
                for (c, t, rec) in rest {
                    if t.to_lowercase().contains(&q) && !seen.contains(&(c.clone(), t.clone())) {
                        all.push((c, t, rec));
                    }
                }
            }
            let total = all.len();
            if let Some((by, desc)) = sort {
                all.sort_by(|(ca, ta, ra), (cb, tb, rb)| {
                    let ord = match by {
                        "count" => ra.count.cmp(&rb.count),
                        "lastUsed" => ra.last_used.cmp(&rb.last_used),
                        "text" => ta.cmp(tb),
                        _ => ca.cmp(cb),
                    };
                    if desc { ord.reverse() } else { ord }
                });
            }
            let page = all.into_iter().skip(offset).take(limit).collect();
            (page, total)
        };
        let items: Vec<Value> = page
            .into_iter()
            .map(|(code, text, rec)| {
                json!({ "code": code, "text": text, "count": rec.count, "lastUsed": rec.last_used })
            })
            .collect();
        Ok(json!({ "items": items, "total": total }))
    }

    fn web_freq_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.delete_freq(&schema, code, text)?;
        Ok(json!({ "ok": true }))
    }

    fn web_freq_clear(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        Ok(json!(store.clear_freq(&schema)?))
    }

    fn web_shadow_list(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let mut out = Vec::new();
        for (code, rec) in store.list_shadow_rules(schema)? {
            for p in rec.pinned {
                out.push(json!({
                    "code": code,
                    "word": p.word,
                    "candId": p.cand_id,
                    "type": "pin",
                    "position": p.position,
                }));
            }
            for d in rec.deleted {
                out.push(json!({
                    "code": code,
                    "word": d,
                    "candId": Value::Null,
                    "type": "delete",
                }));
            }
        }
        Ok(json!(out))
    }

    fn web_shadow_pin(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, word) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "word")?,
        );
        let cand_id = params.get("candId").and_then(|v| v.as_str());
        let position = usize_param(params, "position", 0);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.pin_shadow(schema, code, word, cand_id, position)?;
        Ok(json!({ "ok": true }))
    }

    fn web_shadow_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, word) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "word")?,
        );
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.delete_shadow(schema, code, word)?;
        Ok(json!({ "ok": true }))
    }

    fn web_shadow_remove_rule(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, word) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "word")?,
        );
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.remove_shadow_rule(schema, code, word)?;
        Ok(json!({ "ok": true }))
    }

    /// 候选调整手动添加：type="hide" 转屏蔽；否则（pin）按 position 置顶。
    /// 匹配设置端候选调整对话框契约。
    fn web_shadow_add_rule(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, word) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "word")?,
        );
        let kind = params.get("type").and_then(|v| v.as_str()).unwrap_or("pin");
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        if kind == "hide" {
            store.delete_shadow(schema, code, word)?;
        } else {
            let position = usize_param(params, "position", 0);
            store.pin_shadow(schema, code, word, None, position)?;
        }
        Ok(json!({ "ok": true }))
    }

    fn web_temp_list(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let items: Vec<Value> = store
            .search_temp_words_prefix(&schema, "", 0)?
            .into_iter()
            .map(|r| json!({ "code": r.code, "text": r.text, "count": r.count }))
            .collect();
        Ok(json!(items))
    }

    fn web_temp_promote(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.promote_temp_word(&schema, code, text)?;
        Ok(json!({ "ok": true }))
    }

    fn web_temp_remove(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.remove_temp_word(&schema, code, text)?;
        Ok(json!({ "ok": true }))
    }

    fn web_temp_promote_all(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let mut n = 0u64;
        for r in store.search_temp_words_prefix(&schema, "", 0)? {
            if store.promote_temp_word(&schema, &r.code, &r.text)? {
                n += 1;
            }
        }
        Ok(json!(n))
    }

    fn web_temp_clear(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let all = store.search_temp_words_prefix(&schema, "", 0)?;
        let n = all.len();
        for r in all {
            store.remove_temp_word(&schema, &r.code, &r.text)?;
        }
        Ok(json!(n))
    }

    fn web_phrase_list(&self) -> anyhow::Result<Value> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let items: Vec<Value> = store
            .list_phrases()?
            .into_iter()
            .map(|p| {
                json!({
                    "code": p.code,
                    "text": p.text,
                    "position": p.position,
                    "weight": p.weight,
                    "enabled": p.enabled,
                    "isSystem": p.is_system,
                })
            })
            .collect();
        Ok(json!(items))
    }

    fn web_phrase_add(&self, params: &Value) -> anyhow::Result<Value> {
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let position = i32_param(params, "position");
        let weight = params.get("weight").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.add_phrase(code, text, position, weight)?;
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_update(&self, params: &Value) -> anyhow::Result<Value> {
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let new_code = params.get("newCode").and_then(|v| v.as_str());
        let new_text = params.get("newText").and_then(|v| v.as_str());
        let position = params
            .get("position")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32);
        let weight = params
            .get("weight")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.update_phrase(code, text, new_code, new_text, position, weight)?;
        // 若同时携带 enabled，应用到新键。
        if let Some(en) = params.get("enabled").and_then(|v| v.as_bool()) {
            store.set_phrase_enabled(new_code.unwrap_or(code), new_text.unwrap_or(text), en)?;
        }
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_remove(&self, params: &Value) -> anyhow::Result<Value> {
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.remove_phrase(code, text)?;
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_set_enabled(&self, params: &Value) -> anyhow::Result<Value> {
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.set_phrase_enabled(code, text, enabled)?;
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_reset(&self) -> anyhow::Result<Value> {
        if let Some(store) = self.store.as_ref() {
            store.reset_user_phrases()?;
        }
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_list_system(&self) -> anyhow::Result<Value> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let items: Vec<Value> = store
            .list_system_phrases()?
            .into_iter()
            .map(|p| {
                json!({
                    "code": p.code, "text": p.text, "weight": p.weight,
                    "position": p.position, "enabled": p.enabled, "isSystem": true,
                })
            })
            .collect();
        Ok(json!(items))
    }

    fn web_phrase_list_user(&self, params: &Value) -> anyhow::Result<Value> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let prefix = params.get("prefix").and_then(|v| v.as_str());
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
        // 带 sortBy 时全量拉取 → 排序 → 内存切片；否则走 store 分页路径
        let (rows, total) = if let Some((by, desc)) =
            parse_sort(params, &["code", "text", "weight", "position", "enabled"])
        {
            let (mut all, total) = store.list_user_phrases_paged(prefix, 0, usize::MAX)?;
            all.sort_by(|a, b| {
                let ord = match by {
                    "weight" => a.weight.cmp(&b.weight),
                    "position" => a.position.cmp(&b.position),
                    "enabled" => a.enabled.cmp(&b.enabled),
                    "text" => a.text.cmp(&b.text),
                    _ => a.code.cmp(&b.code),
                };
                if desc { ord.reverse() } else { ord }
            });
            let page = all.into_iter().skip(offset).take(limit).collect();
            (page, total)
        } else {
            store.list_user_phrases_paged(prefix, offset, limit)?
        };
        let items: Vec<Value> = rows
            .into_iter()
            .map(|p| {
                json!({
                    "code": p.code, "text": p.text, "weight": p.weight,
                    "position": p.position, "enabled": p.enabled, "isSystem": false,
                })
            })
            .collect();
        Ok(json!({ "items": items, "total": total }))
    }

    fn web_phrase_export(&self) -> anyhow::Result<Value> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let content = store.export_user_phrases_wdict("")?;
        Ok(json!({ "content": content }))
    }

    fn web_phrase_import(&self, params: &Value) -> anyhow::Result<Value> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let content = str_param(params, "content")?;
        let (imported, skipped) = store.import_user_phrases_wdict(content)?;
        self.rebuild_phrases();
        Ok(json!({ "imported": imported, "skipped": skipped }))
    }

    fn web_phrase_reset_system(&self) -> anyhow::Result<Value> {
        let n = self.restore_system_phrases();
        Ok(json!({ "ok": true, "changed": n }))
    }

    fn web_stats_summary(&self) -> anyhow::Result<Value> {
        use chrono::Datelike;
        let (collector, store) = match (self.stat_collector.as_ref(), self.store.as_ref()) {
            (Some(c), Some(s)) => (c, s),
            _ => return Ok(Self::empty_stats_summary()),
        };
        // 当日数据来自采集器内存（始终最新、完整）；历史从 store 读。
        let today_stat = collector.get_today_stat();
        let meta = collector.get_meta();
        let today = today_str();
        let today_total = today_stat.total();

        // 活跃天数（DB 天数；今天有数据但未 flush 时 +1）+ 日均。
        let all = store
            .daily_stats("0000-01-01", "9999-12-31")
            .unwrap_or_default();
        let mut active_days = all.iter().filter(|(_, r)| r.total() > 0).count();
        let today_in_db = all.iter().any(|(d, _)| d == &today);
        if today_total > 0 && !today_in_db {
            active_days += 1;
        }
        let daily_avg = if active_days > 0 {
            meta.total_chars / active_days as u64
        } else {
            0
        };

        // 周（周日起）/ 月统计（YYYY-MM-DD 字典序），今天用内存值。
        let now = chrono::Local::now().date_naive();
        let week_start = now - chrono::Duration::days(now.weekday().num_days_from_sunday() as i64);
        let month_start = now.with_day(1).unwrap_or(now);
        let week_chars: u64 = Self::daily_with_today_mem(
            store,
            &week_start.format("%Y-%m-%d").to_string(),
            &today,
            &today_stat,
        )
        .iter()
        .map(|(_, r)| r.total() as u64)
        .sum();
        let month_chars: u64 = Self::daily_with_today_mem(
            store,
            &month_start.format("%Y-%m-%d").to_string(),
            &today,
            &today_stat,
        )
        .iter()
        .map(|(_, r)| r.total() as u64)
        .sum();

        // 近 90 天：最高日 / 平均码长 / 首选率 / 平均速度。
        let recent_from = (now - chrono::Duration::days(90))
            .format("%Y-%m-%d")
            .to_string();
        let recent = Self::daily_with_today_mem(store, &recent_from, &today, &today_stat);
        let (mut max_day_chars, mut max_day_date) = (0u32, String::new());
        let (mut cl_sum, mut cl_cnt, mut first_sel, mut cand_sel) = (0u64, 0u64, 0u64, 0u64);
        let (mut sp_chars, mut sp_active) = (0u64, 0u64);
        for (d, r) in &recent {
            let t = r.total();
            if t > max_day_chars {
                max_day_chars = t;
                max_day_date = d.clone();
            }
            cl_sum += r.code_len_sum as u64;
            cl_cnt += r.code_len_count as u64;
            first_sel += r.cand_pos_dist[0] as u64;
            cand_sel += r.cand_pos_dist.iter().map(|&v| v as u64).sum::<u64>();
            sp_chars += t as u64;
            sp_active += r.active_seconds as u64;
        }
        let avg_code_len = if cl_cnt > 0 {
            cl_sum as f64 / cl_cnt as f64
        } else {
            0.0
        };
        let first_select_rate = if cand_sel > 0 {
            first_sel as f64 / cand_sel as f64
        } else {
            0.0
        };
        let today_speed =
            wind_store::stats::speed_per_minute(today_total, today_stat.active_seconds);
        let overall_speed = wind_store::stats::speed_per_minute(
            sp_chars.min(u32::MAX as u64) as u32,
            sp_active.min(u32::MAX as u64) as u32,
        );

        Ok(json!({
            "today_chars": today_total,
            "today_chinese": today_stat.chinese,
            "today_english": today_stat.english,
            "total_chars": meta.total_chars,
            "active_days": active_days,
            "daily_avg": daily_avg,
            "streak_current": meta.streak_current,
            "streak_max": meta.streak_max,
            "week_chars": week_chars,
            "month_chars": month_chars,
            "max_day_chars": max_day_chars,
            "max_day_date": max_day_date,
            "avg_code_len": avg_code_len,
            "first_select_rate": first_select_rate,
            "today_speed": today_speed,
            "overall_speed": overall_speed,
            "max_speed": meta.max_speed,
        }))
    }

    fn web_stats_daily(&self, params: &Value) -> anyhow::Result<Value> {
        let from = str_param(params, "from")?.to_string();
        let to = str_param(params, "to")?.to_string();
        let store = match self.store.as_ref() {
            Some(s) => s,
            None => return Ok(json!([])),
        };
        // 真实数据按日期索引；今天用采集器内存最新值覆盖（DB 可能未 flush）。
        let mut by_date: std::collections::HashMap<String, wind_store::stats::DailyStats> =
            store.daily_stats(&from, &to)?.into_iter().collect();
        let today = today_str();
        if let Some(c) = self.stat_collector.as_ref() {
            let ts = c.get_today_stat();
            if today.as_str() >= from.as_str() && today.as_str() <= to.as_str() && ts.total() > 0 {
                by_date.insert(today.clone(), ts);
            }
        }
        // 区间内连续日期补零值，输出完整 DailyStatItem（便于前端绘图）。
        let mut out = Vec::new();
        if let (Ok(f), Ok(t)) = (
            chrono::NaiveDate::parse_from_str(&from, "%Y-%m-%d"),
            chrono::NaiveDate::parse_from_str(&to, "%Y-%m-%d"),
        ) {
            let mut cur = f;
            while cur <= t {
                let key = cur.format("%Y-%m-%d").to_string();
                let rec = by_date.get(&key).cloned().unwrap_or_default();
                out.push(Self::daily_item_json(&key, &rec));
                cur += chrono::Duration::days(1);
            }
        }
        Ok(json!(out))
    }

    fn web_stats_clear(&self) -> anyhow::Result<Value> {
        if let Some(store) = self.store.as_ref() {
            store.clear_stats()?;
        }
        // 同步清空采集器内存（今日 + 元数据），否则 summary 仍读到旧内存值。
        if let Some(c) = self.stat_collector.as_ref() {
            c.reset();
        }
        Ok(json!({ "ok": true }))
    }

    fn web_stats_prune(&self, params: &Value) -> anyhow::Result<Value> {
        // 参数 days：删除早于 (今天 - days) 的统计。
        let days = params
            .get("days")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(0);
        let store = match self.store.as_ref() {
            Some(s) => s,
            None => return Ok(json!({ "pruned": 0 })),
        };
        let before = (chrono::Local::now().date_naive() - chrono::Duration::days(days))
            .format("%Y-%m-%d")
            .to_string();
        let n = store.prune_stats_before(&before)?;
        // 重建元数据（剔除已删历史）并让采集器重载：先 flush 今日落库，recalc 后 resume。
        if let Some(c) = self.stat_collector.as_ref() {
            c.flush();
            store.recalculate_stats_meta()?;
            c.resume();
        } else {
            store.recalculate_stats_meta()?;
        }
        Ok(json!({ "pruned": n }))
    }

    /// 取 [from, today] 的每日统计，今天用采集器内存值替换/追加（对齐 Go GetSummary 用内存今天）。
    fn daily_with_today_mem(
        store: &wind_store::Store,
        from: &str,
        today: &str,
        today_stat: &wind_store::stats::DailyStats,
    ) -> Vec<(String, wind_store::stats::DailyStats)> {
        let mut days = store.daily_stats(from, today).unwrap_or_default();
        let mut has = false;
        for (d, r) in days.iter_mut() {
            if d == today {
                *r = today_stat.clone();
                has = true;
            }
        }
        if !has && today >= from && today_stat.total() > 0 {
            days.push((today.to_string(), today_stat.clone()));
        }
        days
    }

    /// 组装前端 DailyStatItem JSON（紧凑字段名，含按方案 bs / 按来源 src）。
    fn daily_item_json(date: &str, r: &wind_store::stats::DailyStats) -> Value {
        let bs: serde_json::Map<String, Value> = r
            .by_schema
            .iter()
            .map(|(k, s)| {
                (
                    k.clone(),
                    json!({
                        "tc": s.total_chars,
                        "cn": s.commit_count,
                        "cls": s.code_len_sum,
                        "clc": s.code_len_count,
                        "cpd": s.cand_pos_dist,
                    }),
                )
            })
            .collect();
        json!({
            "d": date,
            "tc": r.total(),
            "cc": r.chinese,
            "ec": r.english,
            "pc": r.punct,
            "oc": r.other,
            "h": r.hours,
            "cn": r.commit_count,
            "cls": r.code_len_sum,
            "clc": r.code_len_count,
            "cld": r.code_len_dist,
            "cpd": r.cand_pos_dist,
            "as": r.active_seconds,
            "bs": bs,
            "src": r.by_source,
        })
    }

    /// 无采集器/存储时的空摘要（17 字段全 0，对齐前端 StatsSummary 形状）。
    fn empty_stats_summary() -> Value {
        json!({
            "today_chars": 0, "today_chinese": 0, "today_english": 0,
            "total_chars": 0, "active_days": 0, "daily_avg": 0,
            "streak_current": 0, "streak_max": 0, "week_chars": 0, "month_chars": 0,
            "max_day_chars": 0, "max_day_date": "", "avg_code_len": 0.0,
            "first_select_rate": 0.0, "today_speed": 0, "overall_speed": 0, "max_speed": 0,
        })
    }

    /// 主题查找目录：用户主题（user_config_dir/themes）优先，回退内置（data/themes）。
    fn theme_dirs(&self) -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        if let Some(u) = wind_config::Config::user_config_dir() {
            dirs.push(u.join("themes"));
        }
        if let Some(d) = &self.themes_dir {
            dirs.push(d.clone());
        }
        dirs
    }

    /// 用户主题写入目录（导入/删除）。
    fn user_themes_dir(&self) -> Option<std::path::PathBuf> {
        wind_config::Config::user_config_dir().map(|u| u.join("themes"))
    }

    fn web_theme_preview(&self, params: &Value) -> anyhow::Result<Value> {
        let name = str_param(params, "name")?;
        let dirs = self.theme_dirs();
        // 合并 base 链 + 归一化（扁平人写形态 → 嵌套内存形态）后的主题配置（toml::Value → JSON），
        // 供前端预览渲染（保持历史 views.* 嵌套契约）。
        let merged = wind_theme::load_merged_dirs(&dirs, name, 0)?;
        let normalized = wind_theme::normalize::normalize_theme(merged);
        Ok(serde_json::to_value(&normalized)?)
    }

    fn web_theme_get_text(&self, params: &Value) -> anyhow::Result<Value> {
        let slug = str_param(params, "slug")?;
        if slug.is_empty() || slug.contains('/') || slug.contains('\\') || slug.contains("..") {
            anyhow::bail!("非法主题 slug");
        }
        for dir in self.theme_dirs() {
            let path = dir.join(slug).join("theme.toml");
            if path.is_file() {
                let toml = std::fs::read_to_string(&path)
                    .map_err(|e| anyhow::anyhow!("读取主题失败：{e}"))?;
                return Ok(json!({ "slug": slug, "toml": toml }));
            }
        }
        anyhow::bail!("主题不存在")
    }

    fn web_theme_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let name = str_param(params, "name")?;
        let user_dir = self
            .user_themes_dir()
            .ok_or_else(|| anyhow::anyhow!("无用户主题目录"))?;
        let target = user_dir.join(name);
        if !target.join("theme.toml").exists() {
            anyhow::bail!("内置主题不可删除或主题不存在: {}", name);
        }
        std::fs::remove_dir_all(&target)?;
        Ok(json!({ "ok": true }))
    }

    fn web_theme_import_text(&self, params: &Value) -> anyhow::Result<Value> {
        // 参数键沿用 "yaml"（前端契约未改），内容为 TOML 文本。
        let text = str_param(params, "yaml")?;
        let force = params
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // 校验可解析为合法主题（仅自身，未校验 base 依赖链）。
        wind_theme::validate_text(text)?;
        let meta = wind_theme::meta_from_text(text)
            .ok_or_else(|| anyhow::anyhow!("主题缺少 meta.name"))?;
        if meta.name.trim().is_empty() {
            anyhow::bail!("主题 meta.name 为空");
        }
        let user_dir = self
            .user_themes_dir()
            .ok_or_else(|| anyhow::anyhow!("无用户主题目录"))?;
        let target = user_dir.join(&meta.name);
        let file = target.join("theme.toml");
        let existed_before = file.exists();
        if existed_before && !force {
            anyhow::bail!("主题已存在（force=false）: {}", meta.name);
        }
        // 覆盖已存在主题前备份原文本，供依赖链校验失败时回滚。
        let backup = if existed_before {
            std::fs::read(&file).ok()
        } else {
            None
        };
        std::fs::create_dir_all(&target)?;
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &file)?;

        // 依赖链校验：写入后按真实主题目录做完整 base 链合并求值，
        // 捕获「base 引用的基础主题不存在」「继承成环」「合并后结构非法」等 validate_text 单文件校验
        // 无法发现的问题。校验失败则回滚（新写入的删除目录；覆盖的恢复原文本）。
        let dirs = self.theme_dirs();
        if let Err(e) = wind_theme::theme::load_typed_dirs(&dirs, &meta.name) {
            match backup {
                Some(bytes) => {
                    let _ = std::fs::write(&file, bytes);
                }
                None => {
                    let _ = std::fs::remove_dir_all(&target);
                }
            }
            anyhow::bail!(
                "主题依赖校验失败：{}（请检查 base 引用的基础主题是否存在）",
                e
            );
        }
        Ok(json!({ "ok": true }))
    }

    fn web_theme_list(&self) -> anyhow::Result<Value> {
        // 复用右键菜单的 list_themes_full 顺序，保证与菜单一致 (#5/主题)。
        let dirs = self.theme_search_dirs();
        let out: Vec<Value> = self
            .list_themes_full()
            .into_iter()
            .map(|(id, display, builtin)| {
                let meta = wind_theme::read_meta(&dirs, &id);
                json!({
                    "name": id,
                    "display_name": display,
                    "author": meta.as_ref().map(|m| m.author.clone()).unwrap_or_default(),
                    "version": meta.as_ref().map(|m| m.version.clone()).unwrap_or_default(),
                    "builtin": builtin,
                })
            })
            .collect();
        Ok(json!(out))
    }
}

/// UserWordRecord → 前端 UserWordItem。
fn word_item(r: wind_store::user_words::UserWordRecord) -> Value {
    json!({ "code": r.code, "text": r.text, "weight": r.weight, "enabled": true })
}

/// 稀疏 diff：返回 `cfg` 相对 `base` 的变化项（仅含改动的叶子/键）；无变化返回 None。
/// 对象逐键递归；数组/标量按整体比较（不同则取 cfg）。用于 schema override 最小化。
fn json_diff(base: &Value, cfg: &Value) -> Option<Value> {
    match (base, cfg) {
        (Value::Object(b), Value::Object(c)) => {
            let mut out = serde_json::Map::new();
            for (k, cv) in c {
                match b.get(k) {
                    Some(bv) => {
                        if let Some(d) = json_diff(bv, cv) {
                            out.insert(k.clone(), d);
                        }
                    }
                    None => {
                        out.insert(k.clone(), cv.clone());
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Object(out))
            }
        }
        _ => {
            if base == cfg {
                None
            } else {
                Some(cfg.clone())
            }
        }
    }
}

/// JSON → toml::Value（写 override 文件）。null 在对象中跳过（TOML 无 null）。
fn json_to_toml(v: &Value) -> toml::Value {
    match v {
        Value::Null => toml::Value::String(String::new()),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else {
                toml::Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        // 跳过数组内 null（TOML 无 null），与对象分支语义一致，避免注入空串污染类型。
        Value::Array(a) => toml::Value::Array(
            a.iter()
                .filter(|x| !x.is_null())
                .map(json_to_toml)
                .collect(),
        ),
        Value::Object(o) => {
            let mut t = toml::map::Map::new();
            for (k, val) in o {
                if !val.is_null() {
                    t.insert(k.clone(), json_to_toml(val));
                }
            }
            toml::Value::Table(t)
        }
    }
}

#[cfg(test)]
mod tests {
    //! 数据域 RPC 契约测试：真实 Coordinator + 临时 redb store，断言 web_data_rpc 输出形状
    //! 与 WindInputSetting 的 mock.ts / models.ts 一致。
    use super::*;
    use crate::coordinator::Coordinator;
    use std::sync::Arc;
    use wind_config::Config;
    use wind_store::Store;

    /// 构造一个带临时 store 的无头 Coordinator。
    fn coord(tag: &str) -> Arc<Coordinator> {
        let path = std::env::temp_dir().join(format!("wind_webdata_{tag}.redb"));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(Store::open(&path).unwrap());
        Coordinator::new_headless_with_store(Config::default(), None, store)
    }

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
        let content = exp
            .get("content")
            .and_then(|v| v.as_str())
            .expect("content 字符串");
        assert!(content.contains("--- !words"));

        // preview 到空 schema:全 willAdd,camelCase 键
        let prev = c
            .web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "wb2", "content": content }),
            )
            .unwrap();
        assert_eq!(prev.get("willAdd").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(prev.get("willUpdate").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(prev.get("willConflict").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(prev.get("unchanged").and_then(|v| v.as_u64()), Some(0));
        assert!(prev.get("samples").and_then(|v| v.as_array()).is_some());

        // import(缺省 merge)→ {added, updated, skipped}
        let out = c
            .web_data_rpc(
                "dict.import",
                &json!({ "schemaId": "wb2", "content": content }),
            )
            .unwrap();
        assert_eq!(out.get("added").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(out.get("skipped").and_then(|v| v.as_u64()), Some(0));

        // 同内容再 import:权重相等 ⇒ 全 unchanged(P2 约束 1),added=updated=0
        let out2 = c
            .web_data_rpc(
                "dict.import",
                &json!({ "schemaId": "wb2", "content": content }),
            )
            .unwrap();
        assert_eq!(out2.get("added").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(out2.get("updated").and_then(|v| v.as_u64()), Some(0));
        // preview 同内容 ⇒ unchanged=1,与落盘一致
        let prev2 = c
            .web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "wb2", "content": content }),
            )
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
        assert_eq!(
            out3.get("added").and_then(|v| v.as_u64()),
            Some(1),
            "清空后全部计 added"
        );
        let listed = c
            .web_data_rpc("dict.listPaged", &json!({ "schemaId": "wb2", "limit": 10 }))
            .unwrap();
        assert_eq!(
            listed.get("total").and_then(|v| v.as_u64()),
            Some(1),
            "replace 应清掉 x"
        );
    }

    #[test]
    fn dict_import_rime_and_tsv_auto_detect() {
        let c = coord("dictio_fmt");

        // Rime:默认列 [text, code, weight],拼音码去空格;preview 回报 format
        let rime = "# Rime dictionary\n---\nname: demo\nversion: \"1.0\"\n...\n你好\tni hao\t100\n世界\tshi jie\t50\n";
        let prev = c
            .web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "pinyin", "content": rime }),
            )
            .unwrap();
        assert_eq!(prev.get("format").and_then(|v| v.as_str()), Some("rime"));
        assert_eq!(prev.get("willAdd").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(prev.get("skipped").and_then(|v| v.as_u64()), Some(0));
        let out = c
            .web_data_rpc(
                "dict.import",
                &json!({ "schemaId": "pinyin", "content": rime }),
            )
            .unwrap();
        assert_eq!(out.get("added").and_then(|v| v.as_u64()), Some(2));
        let listed = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "pinyin", "prefix": "nihao", "limit": 10 }),
            )
            .unwrap();
        assert_eq!(
            listed.get("total").and_then(|v| v.as_u64()),
            Some(1),
            "拼音码应去空格入库(ni hao→nihao)"
        );

        // TSV:code\ttext\t[weight];坏行计入 skipped
        let tsv = "a\t工\t10\nbadline\nab\t好\n";
        let prev = c
            .web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "wb", "content": tsv }),
            )
            .unwrap();
        assert_eq!(prev.get("format").and_then(|v| v.as_str()), Some("tsv"));
        assert_eq!(prev.get("willAdd").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(prev.get("skipped").and_then(|v| v.as_u64()), Some(1));
        let out = c
            .web_data_rpc("dict.import", &json!({ "schemaId": "wb", "content": tsv }))
            .unwrap();
        assert_eq!(out.get("added").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(out.get("skipped").and_then(|v| v.as_u64()), Some(1));

        // 不可识别内容 → 错误
        assert!(
            c.web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "wb", "content": "没有制表符的纯文本\n" }),
            )
            .is_err(),
            "未知格式应报错"
        );
    }

    #[test]
    fn shadow_roundtrip_shape() {
        let c = coord("shadow");
        // pin + delete 两条规则
        c.web_data_rpc(
            "shadow.pin",
            &json!({ "schemaId": "wb", "code": "aaaa", "word": "恭恭敬敬", "candId": "c1", "position": 0 }),
        )
        .unwrap();
        c.web_data_rpc(
            "shadow.delete",
            &json!({ "schemaId": "wb", "code": "bbbb", "word": "某词" }),
        )
        .unwrap();

        let list = c
            .web_data_rpc("shadow.list", &json!({ "schemaId": "wb" }))
            .unwrap();
        let arr = list.as_array().expect("shadow.list 应为数组");
        assert_eq!(arr.len(), 2, "应有 pin/delete 两条");
        // 每条形状对齐 ShadowRuleItem {code, word, candId, type, position?}
        for it in arr {
            assert!(it.get("code").is_some());
            assert!(it.get("word").is_some());
            assert!(it.get("candId").is_some());
            let ty = it["type"].as_str().unwrap();
            assert!(ty == "pin" || ty == "delete");
        }
        let pin = arr.iter().find(|i| i["type"] == "pin").unwrap();
        assert_eq!(pin["candId"], "c1");
        assert_eq!(pin["position"], 0);

        // removeRule 后清空
        c.web_data_rpc(
            "shadow.removeRule",
            &json!({ "schemaId": "wb", "code": "aaaa", "word": "恭恭敬敬" }),
        )
        .unwrap();
        c.web_data_rpc(
            "shadow.removeRule",
            &json!({ "schemaId": "wb", "code": "bbbb", "word": "某词" }),
        )
        .unwrap();
        let list2 = c
            .web_data_rpc("shadow.list", &json!({ "schemaId": "wb" }))
            .unwrap();
        assert_eq!(list2.as_array().unwrap().len(), 0, "removeRule 后应清空");
    }

    #[test]
    fn shadow_add_rule_routes_pin_and_hide() {
        let c = coord("shadow_add_rule");
        // pin：带 position
        c.web_data_rpc(
            "shadow.addRule",
            &json!({ "schemaId": "wb", "code": "aaaa", "word": "恭恭敬敬", "type": "pin", "position": 2 }),
        )
        .unwrap();
        let list = c
            .web_data_rpc("shadow.list", &json!({ "schemaId": "wb" }))
            .unwrap();
        let arr = list.as_array().unwrap();
        // aaaa 应有一条 pin，position=2
        assert!(arr.iter().any(|e| e["code"] == "aaaa"));
        // hide：转为 delete
        c.web_data_rpc(
            "shadow.addRule",
            &json!({ "schemaId": "wb", "code": "bbbb", "word": "某词", "type": "hide" }),
        )
        .unwrap();
        let list2 = c
            .web_data_rpc("shadow.list", &json!({ "schemaId": "wb" }))
            .unwrap();
        assert!(
            list2
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["code"] == "bbbb")
        );
    }

    #[test]
    fn stats_summary_daily_shape() {
        let c = coord("stats");
        let today = today_str();
        // 采集器记录今日：中文 2（码长 4，首选）。
        c.record_commit("你好", 4, 0, wind_store::stats::CommitSource::Candidate);

        // stats.summary 形状对齐富 StatsSummary（17 字段）。
        let sum = c.web_data_rpc("stats.summary", &json!({})).unwrap();
        for k in [
            "today_chars",
            "today_chinese",
            "today_english",
            "total_chars",
            "active_days",
            "daily_avg",
            "streak_current",
            "streak_max",
            "week_chars",
            "month_chars",
            "max_day_chars",
            "avg_code_len",
            "first_select_rate",
            "today_speed",
            "overall_speed",
            "max_speed",
        ] {
            assert!(sum.get(k).is_some(), "summary 缺 {k}");
        }
        assert_eq!(sum["today_chars"], 2);

        // flush 落库后 stats.daily 形状对齐 DailyStatItem。
        c.stat_collector.as_ref().unwrap().flush();
        let daily = c
            .web_data_rpc("stats.daily", &json!({ "from": &today, "to": &today }))
            .unwrap();
        let arr = daily.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["d"], json!(today));
        assert_eq!(arr[0]["tc"], 2);
        assert_eq!(arr[0]["cc"], 2);

        // pruneBefore(days) 返回 {pruned}。
        let pr = c
            .web_data_rpc("stats.pruneBefore", &json!({ "days": 0 }))
            .unwrap();
        assert!(pr.get("pruned").and_then(|v| v.as_u64()).is_some());

        // clear 后 summary 归零（含采集器内存）。
        c.web_data_rpc("stats.clear", &json!({})).unwrap();
        let sum2 = c.web_data_rpc("stats.summary", &json!({})).unwrap();
        assert_eq!(sum2["today_chars"], 0);
        assert_eq!(sum2["total_chars"], 0);
    }

    #[test]
    fn freq_list_delete_clear_shape() {
        let c = coord("freq");
        let store = c.store.as_ref().unwrap();
        store.record_freq("py", "de", "的").unwrap();
        store.record_freq("py", "shi", "是").unwrap();

        // freq.listPaged 形状对齐 PagedResult<FreqItem{code,text,count,lastUsed}>
        let r = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "limit": 50, "offset": 0 }),
            )
            .unwrap();
        assert_eq!(r["total"], 2);
        let it = &r["items"][0];
        for k in ["code", "text", "count", "lastUsed"] {
            assert!(it.get(k).is_some(), "FreqItem 缺 {k}");
        }
        // delete
        c.web_data_rpc(
            "freq.delete",
            &json!({ "schemaId": "py", "code": "de", "text": "的" }),
        )
        .unwrap();
        let r2 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "limit": 50, "offset": 0 }),
            )
            .unwrap();
        assert_eq!(r2["total"], 1);
        // clear 返回删除数（number）
        let cleared = c
            .web_data_rpc("freq.clear", &json!({ "schemaId": "py" }))
            .unwrap();
        assert_eq!(cleared, json!(1));
    }

    #[test]
    fn phrase_crud_shape() {
        let c = coord("phrase");
        // add → list 形状对齐 PhraseItem{code,text,position,weight,enabled}
        c.web_data_rpc(
            "phrase.add",
            &json!({ "code": "rq", "text": "2026-06-20", "position": 0, "weight": 1 }),
        )
        .unwrap();
        let list = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        for k in ["code", "text", "position", "weight", "enabled"] {
            assert!(arr[0].get(k).is_some(), "PhraseItem 缺 {k}");
        }
        assert_eq!(arr[0]["enabled"], json!(true));

        // setEnabled
        c.web_data_rpc(
            "phrase.setEnabled",
            &json!({ "code": "rq", "text": "2026-06-20", "enabled": false }),
        )
        .unwrap();
        let list2 = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        assert_eq!(list2[0]["enabled"], json!(false));

        // update 改 code（键迁移）
        c.web_data_rpc(
            "phrase.update",
            &json!({ "code": "rq", "text": "2026-06-20", "newCode": "date", "weight": 5 }),
        )
        .unwrap();
        let list3 = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        assert_eq!(list3[0]["code"], json!("date"));

        // remove + resetDefault
        c.web_data_rpc(
            "phrase.remove",
            &json!({ "code": "date", "text": "2026-06-20" }),
        )
        .unwrap();
        assert_eq!(
            c.web_data_rpc("phrase.list", &json!({}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0
        );
        c.web_data_rpc("phrase.resetDefault", &json!({})).unwrap();
    }

    /// 回归：phrase.resetDefault 只删用户短语，系统短语必须保留。
    #[test]
    fn phrase_reset_default_keeps_system() {
        use wind_store::phrases::SystemPhrase;

        let path = std::env::temp_dir().join("wind_webdata_phrase_reset_keeps_system.redb");
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(Store::open(&path).unwrap());

        // 先同步一条系统短语（is_system=true）
        store
            .sync_system_phrases(&[SystemPhrase {
                code: "rq".into(),
                text: "$date".into(),
                weight: 1000,
                position: 0,
            }])
            .unwrap();

        // 构造 coordinator（共享同一个 Arc<Store>）
        let c = Coordinator::new_headless_with_store(Config::default(), None, Arc::clone(&store));

        // 加一条用户短语
        c.web_data_rpc(
            "phrase.add",
            &json!({ "code": "me", "text": "自定义", "position": 0, "weight": 1 }),
        )
        .unwrap();

        // 执行用户"清空"操作
        c.web_data_rpc("phrase.resetDefault", &json!({})).unwrap();

        // 系统短语应保留
        let sys = c.web_data_rpc("phrase.listSystem", &json!({})).unwrap();
        let sys_arr = sys.as_array().expect("listSystem 应返回数组");
        assert_eq!(sys_arr.len(), 1, "系统短语应保留，不应被 resetDefault 删除");
        assert_eq!(sys_arr[0]["code"], json!("rq"));

        // 用户短语应为 0
        let user = c.web_data_rpc("phrase.listUser", &json!({})).unwrap();
        assert_eq!(user["total"], json!(0), "用户短语应被 resetDefault 清空");
    }

    #[test]
    fn json_diff_sparse() {
        let base = json!({ "a": 1, "t": { "x": 1, "y": 2 }, "same": "v" });
        let cfg = json!({ "a": 9, "t": { "x": 1, "y": 20 }, "same": "v" });
        let d = json_diff(&base, &cfg).unwrap();
        // 仅含变化项：a + t.y
        assert_eq!(d, json!({ "a": 9, "t": { "y": 20 } }));
        // 完全相同 → None
        assert!(json_diff(&base, &base).is_none());
    }

    #[test]
    fn schema_get_config_graceful_without_data_dir() {
        // data_dir=None（coord helper）→ 无方案文件 → getConfig 返回 {}，saveConfig 报错（无基础）。
        let c = coord("schema");
        let r = c
            .web_data_rpc("schema.getConfig", &json!({ "id": "pinyin" }))
            .unwrap();
        assert!(r.is_object() && r.as_object().unwrap().is_empty());
        assert!(
            c.web_data_rpc("schema.saveConfig", &json!({ "id": "pinyin", "cfg": {} }))
                .is_err()
        );
    }

    #[test]
    fn record_input_stats_fallback_records_full_classes() {
        use wind_bridge::handler::KeyAction;
        use wind_store::stats::CommitSource;
        let c = coord("stats_fallback");
        // 顶层 fallback：上屏「你好abc，」→ 4 分类，含中文推测来源为候选。
        c.record_input_stats(&KeyAction::InsertText {
            text: "你好abc，".to_string(),
            new_composition: None,
            mode_changed: false,
            chinese_mode: true,
            has_new_composition: false,
        });
        let day = c.stat_collector.as_ref().unwrap().get_today_stat();
        assert_eq!(day.chinese, 2);
        assert_eq!(day.english, 3);
        assert_eq!(day.punct, 1);
        assert_eq!(day.commit_count, 1);
        assert_eq!(day.by_source[CommitSource::Candidate.index()], 6);
    }

    #[test]
    fn record_commit_captures_code_len_and_pos() {
        use wind_store::stats::CommitSource;
        let c = coord("stats_commit");
        c.record_commit("你好", 4, 0, CommitSource::Candidate);
        let day = c.stat_collector.as_ref().unwrap().get_today_stat();
        assert_eq!(day.chinese, 2);
        assert_eq!(day.code_len_sum, 4);
        assert_eq!(day.code_len_count, 1);
        assert_eq!(day.cand_pos_dist[0], 1);
        assert!(
            c.stat_recorded.load(std::sync::atomic::Ordering::Relaxed),
            "record_commit 应置位 stat_recorded"
        );
    }

    #[test]
    fn stats_summary_rich_fields() {
        use wind_store::stats::CommitSource;
        let c = coord("stats_summary_rich");
        // 今日：中文 2(码长4,首选) + 英文 2(临英,次选)
        c.record_commit("你好", 4, 0, CommitSource::Candidate);
        c.record_commit("ab", 0, 1, CommitSource::TempEnglish);
        let r = c.web_data_rpc("stats.summary", &json!({})).unwrap();
        assert_eq!(r["today_chinese"], 2);
        assert_eq!(r["today_english"], 2);
        assert_eq!(r["today_chars"], 4);
        assert_eq!(r["total_chars"], 4);
        assert_eq!(r["active_days"], 1);
        assert!((r["avg_code_len"].as_f64().unwrap() - 4.0).abs() < 1e-9);
        assert!(
            (r["first_select_rate"].as_f64().unwrap() - 0.5).abs() < 1e-9,
            "首选率=首选1/总选2=0.5"
        );
    }

    #[test]
    fn stats_daily_rich_shape() {
        use wind_store::stats::CommitSource;
        let c = coord("stats_daily_rich");
        c.record_commit("你好", 4, 0, CommitSource::Candidate);
        c.stat_collector.as_ref().unwrap().flush(); // 落库才能被 daily 区间读到
        let today = today_str();
        let r = c
            .web_data_rpc("stats.daily", &json!({ "from": today, "to": today }))
            .unwrap();
        let arr = r.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let d = &arr[0];
        assert_eq!(d["d"], today);
        assert_eq!(d["tc"], 2);
        assert_eq!(d["cc"], 2);
        assert_eq!(d["cls"], 4);
        assert_eq!(d["h"].as_array().unwrap().len(), 24);
        assert_eq!(d["cpd"][0], 1);
    }

    #[test]
    fn record_input_stats_skips_when_already_recorded() {
        use wind_bridge::handler::KeyAction;
        use wind_store::stats::CommitSource;
        let c = coord("stats_skip");
        // 具体路径已记录 → 顶层 fallback 应跳过，不重复计数。
        c.record_commit("你好", 4, 0, CommitSource::Candidate);
        c.record_input_stats(&KeyAction::InsertText {
            text: "你好".to_string(),
            new_composition: None,
            mode_changed: false,
            chinese_mode: true,
            has_new_composition: false,
        });
        let day = c.stat_collector.as_ref().unwrap().get_today_stat();
        assert_eq!(day.commit_count, 1, "已记录则 fallback 跳过");
    }

    /// Task 2：拼音类方案（pinyin_simp / double_pinyin）写入共享 "pinyin" 存储，
    /// 跨方案 id 互读能取到同一份用户词。
    #[test]
    fn pinyin_and_shuangpin_share_userdict() {
        use std::io::Write;
        // 写两个拼音类方案 schema.toml，让 data_schema_id 折叠到 "pinyin"
        let base_dir = std::env::temp_dir().join("wind_coord_share_userdict_test");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        for name in ["pinyin_simp", "double_pinyin"] {
            let mut f = std::fs::File::create(schemas.join(format!("{name}.schema.toml"))).unwrap();
            write!(f, "[engine]\ntype = \"pinyin\"\n").unwrap();
        }

        let db_path = std::env::temp_dir().join("wind_coord_share_userdict.redb");
        let _ = std::fs::remove_file(&db_path);
        let store = Arc::new(Store::open(&db_path).unwrap());
        let c = Coordinator::new_headless_with_store(
            Config::default(),
            Some(base_dir.as_path()),
            Arc::clone(&store),
        );

        // 用拼音方案加词
        c.web_data_rpc(
            "dict.add",
            &json!({ "schemaId": "pinyin_simp", "code": "nihao", "text": "你好", "weight": 5 }),
        )
        .unwrap();

        // 用双拼方案读，应读到同一条（共享 "pinyin" 存储键）
        let list = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "double_pinyin", "offset": 0, "limit": 100 }),
            )
            .unwrap();
        let items = list["items"].as_array().unwrap();
        assert!(
            items.iter().any(|it| it["text"] == "你好"),
            "双拼应读到拼音下加的词（data_schema_id 共享）"
        );
    }

    /// Task 3：schema.list 每个拼音方案应携带 scheme 字段（full/shuangpin），非拼音方案为空串。
    #[test]
    fn schema_list_exposes_scheme() {
        use std::io::Write;
        let base_dir = std::env::temp_dir().join("wind_coord_schema_list_scheme_test");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        // 创建全拼方案
        {
            let mut f = std::fs::File::create(schemas.join("pinyin_full.schema.toml")).unwrap();
            write!(
                f,
                "[engine]\ntype = \"pinyin\"\n[engine.pinyin]\nscheme = \"full\"\n"
            )
            .unwrap();
        }
        // 创建双拼方案
        {
            let mut f =
                std::fs::File::create(schemas.join("double_pinyin_sp.schema.toml")).unwrap();
            write!(
                f,
                "[engine]\ntype = \"pinyin\"\n[engine.pinyin]\nscheme = \"shuangpin\"\n"
            )
            .unwrap();
        }
        let db_path = std::env::temp_dir().join("wind_webdata_schema_list_scheme.redb");
        let _ = std::fs::remove_file(&db_path);
        let store = Arc::new(Store::open(&db_path).unwrap());
        let c = Coordinator::new_headless_with_store(
            Config::default(),
            Some(base_dir.as_path()),
            Arc::clone(&store),
        );

        let list = c.web_data_rpc("schema.list", &json!({})).unwrap();
        let arr = list.as_array().unwrap();

        // 必须有方案
        assert!(!arr.is_empty(), "schema.list 应返回非空数组");
        // 每项都应有 scheme 键
        for item in arr.iter() {
            assert!(
                item.get("scheme").is_some(),
                "每个方案项应有 scheme 字段，缺失于: {item}"
            );
        }
        // 全拼方案 scheme="full"
        let full = arr.iter().find(|s| s["id"] == "pinyin_full");
        assert!(full.is_some(), "应有 pinyin_full 方案");
        assert_eq!(full.unwrap()["scheme"], "full", "全拼方案 scheme 应为 full");
        // 双拼方案 scheme="shuangpin"
        let sp = arr.iter().find(|s| s["id"] == "double_pinyin_sp");
        assert!(sp.is_some(), "应有 double_pinyin_sp 方案");
        assert_eq!(
            sp.unwrap()["scheme"],
            "shuangpin",
            "双拼方案 scheme 应为 shuangpin"
        );
    }

    /// P2d：构造带混输方案（primary=ct_test、secondary=py_test）的无头 Coordinator，
    /// active=mx_test；返回 (coord, store) 供直查断言。
    fn mixed_coord(tag: &str) -> (Arc<Coordinator>, Arc<Store>) {
        use std::io::Write;
        let base_dir = std::env::temp_dir().join(format!("wind_coord_p2d_{tag}"));
        let schemas = base_dir.join("schemas");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&schemas).unwrap();
        {
            let mut f = std::fs::File::create(schemas.join("py_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"py_test\"\n[engine]\ntype = \"pinyin\"\n"
            )
            .unwrap();
        }
        {
            let mut f = std::fs::File::create(schemas.join("ct_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"ct_test\"\n[engine]\ntype = \"codetable\"\n"
            )
            .unwrap();
        }
        {
            let mut f = std::fs::File::create(schemas.join("mx_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"mx_test\"\n[engine]\ntype = \"mixed\"\n[engine.mixed]\nprimary_schema = \"ct_test\"\nsecondary_schema = \"py_test\"\n"
            )
            .unwrap();
        }
        let mut cfg = Config::default();
        cfg.schema.active = "mx_test".into();
        cfg.schema.available = vec!["mx_test".into(), "ct_test".into(), "py_test".into()];
        // 开启码表词频，供 apply_freq_rerank 测试生效（混输走码表 used-first 路径）。
        cfg.schema.codetable.frequency.enabled = true;
        // 开启码表自动造词，供 learn_phrase_on_commit 测试生效（混输继承主码表 auto_phrase）。
        cfg.schema.codetable.auto_phrase.enabled = true;

        let db_path = std::env::temp_dir().join(format!("wind_coord_p2d_{tag}.redb"));
        let _ = std::fs::remove_file(&db_path);
        let store = Arc::new(Store::open(&db_path).unwrap());
        let c =
            Coordinator::new_headless_with_store(cfg, Some(base_dir.as_path()), Arc::clone(&store));
        (c, store)
    }

    /// P2d Task 2：混输 active 下 record_selection 按候选来源落子方案键空间；无法归因跳过。
    #[test]
    fn mixed_record_selection_routes_by_source() {
        use wind_candidate::CandidateSource;
        let (c, store) = mixed_coord("record_selection");

        // 码表候选 → 落 primary "ct_test"
        c.record_selection("aaaa", "工", CandidateSource::CodeTable);
        assert!(
            store.get_freq("ct_test", "aaaa", "工").unwrap().is_some(),
            "码表候选应落 primary ct_test 键空间"
        );
        assert!(
            store.get_freq("mx_test", "aaaa", "工").unwrap().is_none(),
            "不应落混输自身 id"
        );
        assert!(
            store.get_freq("pinyin", "aaaa", "工").unwrap().is_none(),
            "不应落 pinyin"
        );

        // 拼音候选 → 落 "pinyin"
        c.record_selection("nihao", "你好", CandidateSource::Pinyin);
        assert!(
            store.get_freq("pinyin", "nihao", "你好").unwrap().is_some(),
            "拼音候选应落 pinyin 共享键空间"
        );

        // 无法归因 → 三处键空间均无写入
        c.record_selection("x", "y", CandidateSource::None);
        assert!(store.get_freq("ct_test", "x", "y").unwrap().is_none());
        assert!(store.get_freq("mx_test", "x", "y").unwrap().is_none());
        assert!(store.get_freq("pinyin", "x", "y").unwrap().is_none());
    }

    /// P2d Task 2 回归：拼音方案 active 下 record_selection 忽略 source，仍折叠落 "pinyin"。
    #[test]
    fn pinyin_record_selection_ignores_source() {
        use std::io::Write;
        use wind_candidate::CandidateSource;
        let base_dir = std::env::temp_dir().join("wind_coord_p2d_pinyin_active");
        let schemas = base_dir.join("schemas");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&schemas).unwrap();
        {
            let mut f = std::fs::File::create(schemas.join("py_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"py_test\"\n[engine]\ntype = \"pinyin\"\n"
            )
            .unwrap();
        }
        let mut cfg = Config::default();
        cfg.schema.active = "py_test".into();
        cfg.schema.available = vec!["py_test".into()];
        // 开启拼音调频，供 record_selection 写入测试生效（默认关闭不落库）。
        cfg.schema.pinyin.frequency.enabled = true;
        let db_path = std::env::temp_dir().join("wind_coord_p2d_pinyin_active.redb");
        let _ = std::fs::remove_file(&db_path);
        let store = Arc::new(Store::open(&db_path).unwrap());
        let c =
            Coordinator::new_headless_with_store(cfg, Some(base_dir.as_path()), Arc::clone(&store));

        c.record_selection("nihao", "你好", CandidateSource::None);
        assert!(
            store.get_freq("pinyin", "nihao", "你好").unwrap().is_some(),
            "拼音方案忽略 source，落 pinyin"
        );
    }

    /// P2d Task 3：混输 active 下 apply_freq_rerank 按候选来源读子方案词频。
    /// 码表候选读 primary(ct_test)、拼音候选读 "pinyin"；命中记录者档内提权。
    /// （若读侧仍走 mx_test 单一归属，则两处预置的记录都读不到，无提权 → 测试失败。）
    #[test]
    fn mixed_freq_rerank_reads_sub_schema() {
        use wind_candidate::{Candidate, CandidateSource};
        let (c, store) = mixed_coord("freq_rerank");
        // 预置：ct_test 名下「工」、pinyin 名下「好」各一条词频。
        store.record_freq("ct_test", "aaaa", "工").unwrap();
        store.record_freq("pinyin", "nihao", "好").unwrap();

        let mk = |t: &str, code: &str, s: CandidateSource| Candidate {
            text: t.to_string(),
            code: code.to_string(),
            source: s,
            ..Default::default()
        };

        // 码表档（tier 0，同 source 同码）：「工」有 ct_test 记录 → 浮到「他」前。
        let mut ct_cands = vec![
            mk("他", "aaaa", CandidateSource::CodeTable),
            mk("工", "aaaa", CandidateSource::CodeTable),
        ];
        c.apply_freq_rerank(&mut ct_cands, "aaaa");
        assert_eq!(
            ct_cands[0].text, "工",
            "码表候选应按 primary(ct_test) 词频提权"
        );

        // 拼音档（tier 3，同 source）：「好」有 pinyin 记录 → 浮到「你」前。
        let mut py_cands = vec![
            mk("你", "nihao", CandidateSource::Pinyin),
            mk("好", "nihao", CandidateSource::Pinyin),
        ];
        c.apply_freq_rerank(&mut py_cands, "nihao");
        assert_eq!(py_cands[0].text, "好", "拼音候选应按 pinyin 词频提权");
    }

    /// P2d Task 4：混输自动造词按"全段同源"路由——全段同源落该源归属，混源跳过。
    #[test]
    fn mixed_learn_phrase_same_source_only() {
        use wind_candidate::CandidateSource;
        let (c, store) = mixed_coord("learn_phrase");

        // 全段拼音 → 临时词落 "pinyin"。
        {
            let mut st = c.state.lock().unwrap();
            st.committed_segs.clear();
            st.committed_segs
                .push(("ni".into(), "你".into(), CandidateSource::Pinyin));
            st.committed_segs
                .push(("hao".into(), "好".into(), CandidateSource::Pinyin));
            c.learn_phrase_on_commit(&st);
        }
        assert!(
            store
                .get_temp_words("pinyin", "nihao")
                .unwrap()
                .iter()
                .any(|w| w.text == "你好"),
            "全段拼音应落 pinyin 临时词"
        );

        // 混源（一码表一拼音）→ 三处键空间均无临时词。
        {
            let mut st = c.state.lock().unwrap();
            st.committed_segs.clear();
            st.committed_segs
                .push(("aaaa".into(), "工".into(), CandidateSource::CodeTable));
            st.committed_segs
                .push(("hao".into(), "好".into(), CandidateSource::Pinyin));
            c.learn_phrase_on_commit(&st);
        }
        for schema in ["ct_test", "pinyin", "mx_test"] {
            assert!(
                store.get_temp_words(schema, "aaaahao").unwrap().is_empty(),
                "混源不应落任何临时词（{schema}）"
            );
        }

        // 全段码表 → 临时词落 primary "ct_test"。
        {
            let mut st = c.state.lock().unwrap();
            st.committed_segs.clear();
            st.committed_segs
                .push(("aa".into(), "工".into(), CandidateSource::CodeTable));
            st.committed_segs
                .push(("bb".into(), "人".into(), CandidateSource::CodeTable));
            c.learn_phrase_on_commit(&st);
        }
        assert!(
            store
                .get_temp_words("ct_test", "aabb")
                .unwrap()
                .iter()
                .any(|w| w.text == "工人"),
            "全段码表应落 primary ct_test 临时词"
        );
    }

    /// P2d Task 4 回归：非混输（码表方案）自动造词维持现行为，不看段来源，落自身 id。
    /// （用码表而非拼音方案：无头最小 schema 无引擎数据，is_pinyin() 依赖已加载引擎会退化，
    /// 码表分支只读配置不依赖引擎，可稳定验证"非混输不看段来源"。）
    #[test]
    fn codetable_learn_phrase_ignores_source() {
        use std::io::Write;
        use wind_candidate::CandidateSource;
        let base_dir = std::env::temp_dir().join("wind_coord_p2d_ct_learn");
        let schemas = base_dir.join("schemas");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&schemas).unwrap();
        {
            let mut f = std::fs::File::create(schemas.join("ct_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"ct_test\"\n[engine]\ntype = \"codetable\"\n"
            )
            .unwrap();
        }
        let mut cfg = Config::default();
        cfg.schema.active = "ct_test".into();
        cfg.schema.available = vec!["ct_test".into()];
        cfg.schema.codetable.auto_phrase.enabled = true;
        let db_path = std::env::temp_dir().join("wind_coord_p2d_ct_learn.redb");
        let _ = std::fs::remove_file(&db_path);
        let store = Arc::new(Store::open(&db_path).unwrap());
        let c =
            Coordinator::new_headless_with_store(cfg, Some(base_dir.as_path()), Arc::clone(&store));

        // 非混输：即使段标注混源（None/Pinyin），也不影响归属，仍落自身 id "ct_test"。
        {
            let mut st = c.state.lock().unwrap();
            st.committed_segs.clear();
            st.committed_segs
                .push(("aa".into(), "工".into(), CandidateSource::None));
            st.committed_segs
                .push(("bb".into(), "人".into(), CandidateSource::Pinyin));
            c.learn_phrase_on_commit(&st);
        }
        assert!(
            store
                .get_temp_words("ct_test", "aabb")
                .unwrap()
                .iter()
                .any(|w| w.text == "工人"),
            "码表方案忽略段来源，落自身 id"
        );
    }

    /// P2d Task 5：混输 active 下手动加词（RPC dict.add）落主码表方案；primary 缺失则报错不 panic。
    #[test]
    fn mixed_manual_addword_goes_to_primary() {
        let (c, store) = mixed_coord("manual_addword");
        // 手动加词是码表语义 → 落 primary "ct_test"。
        c.cmd_dict_add("工", "aaaa").unwrap();
        assert!(
            store
                .get_user_words("ct_test", "aaaa")
                .unwrap()
                .iter()
                .any(|w| w.text == "工"),
            "混输手动加词应落 primary ct_test"
        );
        assert!(
            store.get_user_words("mx_test", "aaaa").unwrap().is_empty(),
            "不应落混输自身 id"
        );

        // primary 缺失的坏配置 → 返回 Err，不 panic，不写库。
        use std::io::Write;
        let base_dir = std::env::temp_dir().join("wind_coord_p2d_addword_bad");
        let schemas = base_dir.join("schemas");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&schemas).unwrap();
        {
            let mut f = std::fs::File::create(schemas.join("mx_bad.schema.toml")).unwrap();
            // 混输但未配 primary_schema。
            write!(f, "[schema]\nid = \"mx_bad\"\n[engine]\ntype = \"mixed\"\n").unwrap();
        }
        let mut cfg = Config::default();
        cfg.schema.active = "mx_bad".into();
        cfg.schema.available = vec!["mx_bad".into()];
        let db_path = std::env::temp_dir().join("wind_coord_p2d_addword_bad.redb");
        let _ = std::fs::remove_file(&db_path);
        let bad_store = Arc::new(Store::open(&db_path).unwrap());
        let bc = Coordinator::new_headless_with_store(
            cfg,
            Some(base_dir.as_path()),
            Arc::clone(&bad_store),
        );
        assert!(
            bc.cmd_dict_add("工", "aaaa").is_err(),
            "混输 primary 缺失应返回 Err"
        );
    }

    #[test]
    fn dict_list_paged_sort() {
        let c = coord("dict_sort");
        for (code, text, weight) in [("ab", "B词", 10i32), ("aa", "A词", 30), ("ac", "C词", 5)] {
            c.web_data_rpc(
                "dict.add",
                &json!({ "schemaId": "wb", "code": code, "text": text, "weight": weight }),
            )
            .unwrap();
        }
        // weight asc：5 → 10 → 30
        let r = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "offset": 0, "limit": 10,
                          "sortBy": "weight", "sortOrder": "asc" }),
            )
            .unwrap();
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["weight"], 5, "asc 首项应为最小权重");
        assert_eq!(items[2]["weight"], 30, "asc 末项应为最大权重");
        // weight desc：30 → 10 → 5
        let r2 = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "offset": 0, "limit": 10,
                          "sortBy": "weight", "sortOrder": "desc" }),
            )
            .unwrap();
        assert_eq!(r2["items"][0]["weight"], 30, "desc 首项应为最大权重");
        // 跨页切片：asc offset=1 limit=1 取排序后第 2 条（weight=10）
        let r3 = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "offset": 1, "limit": 1,
                          "sortBy": "weight", "sortOrder": "asc" }),
            )
            .unwrap();
        assert_eq!(r3["total"], 3, "跨页切片 total 不变");
        assert_eq!(r3["items"][0]["weight"], 10, "offset=1 asc 应取 weight=10");
        // 不传 sortBy 行为不变（total 正确）
        let r4 = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r4["total"], 3, "不传 sortBy 应保持原有行为");
    }

    #[test]
    fn freq_list_paged_sort() {
        let c = coord("freq_sort");
        let store = c.store.as_ref().unwrap();
        // de=1次, ta=2次, shi=3次
        store.record_freq("py", "de", "的").unwrap();
        store.record_freq("py", "ta", "他").unwrap();
        store.record_freq("py", "ta", "他").unwrap();
        store.record_freq("py", "shi", "是").unwrap();
        store.record_freq("py", "shi", "是").unwrap();
        store.record_freq("py", "shi", "是").unwrap();
        // count asc：1 → 2 → 3
        let r = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "offset": 0, "limit": 10,
                          "sortBy": "count", "sortOrder": "asc" }),
            )
            .unwrap();
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["count"], 1, "asc 首项应为 count=1");
        assert_eq!(items[2]["count"], 3, "asc 末项应为 count=3");
        // count desc：3 → 2 → 1
        let r2 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "offset": 0, "limit": 10,
                          "sortBy": "count", "sortOrder": "desc" }),
            )
            .unwrap();
        assert_eq!(r2["items"][0]["count"], 3, "desc 首项应为 count=3");
        // 跨页切片：asc offset=1 limit=1 取第 2 条（count=2）
        let r3 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "offset": 1, "limit": 1,
                          "sortBy": "count", "sortOrder": "asc" }),
            )
            .unwrap();
        assert_eq!(r3["total"], 3, "跨页切片 total 不变");
        assert_eq!(r3["items"][0]["count"], 2, "offset=1 asc 应取 count=2");
        // 不传 sortBy 行为不变
        let r4 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r4["total"], 3, "不传 sortBy 应保持原有行为");
    }

    #[test]
    fn dict_list_paged_text_query() {
        let c = coord("dict_text_query");
        for (code, text, weight) in [
            ("wghg", "程序", 3i32),
            ("ggkg", "王中", 5),
            ("aaaa", "工", 0),
        ] {
            c.web_data_rpc(
                "dict.add",
                &json!({ "schemaId": "wb", "code": code, "text": text, "weight": weight }),
            )
            .unwrap();
        }
        // 按词条内容搜索：编码 "wghg" 不以 "程" 开头，命中应来自 text 包含匹配。
        let r = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "prefix": "程", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r["total"], 1, "词条内容搜索应命中 1 条");
        assert_eq!(r["items"][0]["text"], "程序", "应按 text 内容命中");
        // 按编码前缀搜索仍生效。
        let r2 = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "prefix": "wg", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r2["total"], 1, "编码前缀搜索应命中 1 条");
        assert_eq!(r2["items"][0]["code"], "wghg", "应按 code 前缀命中");
    }

    #[test]
    fn freq_list_paged_text_query() {
        let c = coord("freq_text_query");
        let store = c.store.as_ref().unwrap();
        store.record_freq("py", "nihao", "你好").unwrap();
        store.record_freq("py", "women", "我们").unwrap();
        // 按词条内容搜索：编码 "nihao" 不以 "你" 开头，命中应来自 text 包含匹配。
        let r = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "prefix": "你", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r["total"], 1, "词条内容搜索应命中 1 条");
        assert_eq!(r["items"][0]["text"], "你好", "应按 text 内容命中");
        // 按编码前缀搜索仍生效。
        let r2 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "prefix": "women", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r2["total"], 1, "编码前缀搜索应命中 1 条");
        assert_eq!(r2["items"][0]["text"], "我们", "应按 code 前缀命中对应词条");
    }

    #[test]
    fn phrase_list_user_sort() {
        let c = coord("phrase_sort");
        for (code, text, weight) in [("b", "乙", 20i32), ("a", "甲", 50), ("c", "丙", 5)] {
            c.web_data_rpc(
                "phrase.add",
                &json!({ "code": code, "text": text, "position": 0, "weight": weight }),
            )
            .unwrap();
        }
        // weight asc：5 → 20 → 50
        let r = c
            .web_data_rpc(
                "phrase.listUser",
                &json!({ "offset": 0, "limit": 10,
                          "sortBy": "weight", "sortOrder": "asc" }),
            )
            .unwrap();
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["weight"], 5, "asc 首项应为 weight=5");
        assert_eq!(items[2]["weight"], 50, "asc 末项应为 weight=50");
        // weight desc：50 → 20 → 5
        let r2 = c
            .web_data_rpc(
                "phrase.listUser",
                &json!({ "offset": 0, "limit": 10,
                          "sortBy": "weight", "sortOrder": "desc" }),
            )
            .unwrap();
        assert_eq!(r2["items"][0]["weight"], 50, "desc 首项应为 weight=50");
        // 跨页切片：asc offset=1 limit=1 取第 2 条（weight=20）
        let r3 = c
            .web_data_rpc(
                "phrase.listUser",
                &json!({ "offset": 1, "limit": 1,
                          "sortBy": "weight", "sortOrder": "asc" }),
            )
            .unwrap();
        assert_eq!(r3["total"], 3, "跨页切片 total 不变");
        assert_eq!(r3["items"][0]["weight"], 20, "offset=1 asc 应取 weight=20");
        // 不传 sortBy 行为不变
        let r4 = c
            .web_data_rpc("phrase.listUser", &json!({ "offset": 0, "limit": 10 }))
            .unwrap();
        assert_eq!(r4["total"], 3, "不传 sortBy 应保持原有行为");
    }

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
            "my",
            &user,
            Some(&system),
            &pkg,
            "1.0.0",
            "windows",
            "t",
        )
        .unwrap();
        let prev = c
            .web_data_rpc(
                "scheme.previewImport",
                &json!({ "path": pkg.to_string_lossy() }),
            )
            .unwrap();
        assert_eq!(
            prev.get("package")
                .and_then(|p| p.get("schema"))
                .and_then(|s| s.get("id"))
                .and_then(|v| v.as_str()),
            Some("my"),
            "v2 预览返回 package 元信息"
        );
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
            ins.get("manifest")
                .and_then(|m| m.get("kind"))
                .and_then(|v| v.as_str()),
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
        c.web_data_rpc("dict.clear", &json!({ "schemaId": "wb" }))
            .unwrap();
        let rr = c
            .web_data_rpc(
                "backup.restore",
                &json!({ "path": out.to_string_lossy(), "sections": ["dict"] }),
            )
            .unwrap();
        assert!(
            rr.get("restored")
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        );
        let listed = c
            .web_data_rpc("dict.listPaged", &json!({ "schemaId": "wb", "limit": 10 }))
            .unwrap();
        assert_eq!(listed.get("total").and_then(|v| v.as_u64()), Some(1));
        let _ = std::fs::remove_file(&out);
    }
}
