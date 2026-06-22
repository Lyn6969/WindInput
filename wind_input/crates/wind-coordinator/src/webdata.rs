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

/// 本地今天日期 "YYYY-MM-DD"（统计摘要的参照点）。
fn today_str() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

impl Coordinator {
    /// 枚举本机字体族名（去重升序）。供 system.fonts 经 CoreStatus 注入。
    /// 首次调用会扫描系统字体目录（fontdb），开销可接受（设置页打开字体选择时一次）。
    pub fn list_font_families(&self) -> Vec<String> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let mut set = std::collections::BTreeSet::new();
        for face in db.faces() {
            if let Some((family, _)) = face.families.first() {
                if !family.is_empty() {
                    set.insert(family.clone());
                }
            }
        }
        set.into_iter().collect()
    }

    /// 数据类 RPC 总分派。方法名以 `<ns>.<method>` 形式分组；未知方法返回 Err。
    pub fn web_data_rpc(&self, method: &str, params: &Value) -> anyhow::Result<Value> {
        match method {
            // ── schema.* ─────────────────────────────────────────
            "schema.list" => self.web_schema_list(),
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

            // ── phrase.*（用户短语，全局，redb 持久化）──────────
            "phrase.list" => self.web_phrase_list(),
            "phrase.add" => self.web_phrase_add(params),
            "phrase.update" => self.web_phrase_update(params),
            "phrase.remove" => self.web_phrase_remove(params),
            "phrase.setEnabled" => self.web_phrase_set_enabled(params),
            "phrase.resetDefault" => self.web_phrase_reset(),

            // ── stats.*（输入统计，redb 每日聚合）────────────────
            "stats.summary" => self.web_stats_summary(),
            "stats.daily" => self.web_stats_daily(params),
            "stats.clear" => self.web_stats_clear(),
            "stats.pruneBefore" => self.web_stats_prune(params),

            // ── theme.* ──────────────────────────────────────────
            "theme.list" => self.web_theme_list(),
            "theme.preview" => self.web_theme_preview(params),
            "theme.delete" => self.web_theme_delete(params),
            "theme.importFromText" => self.web_theme_import_text(params),
            "theme.importFromUrl" => {
                anyhow::bail!("URL 导入未启用（features.theme.import_url=false）")
            }

            other => anyhow::bail!("unknown method: {}", other),
        }
    }

    fn web_schema_list(&self) -> anyhow::Result<Value> {
        let items: Vec<Value> = self
            .engine_mgr
            .available_schemas()
            .iter()
            .map(|id| {
                // 取合并后 Schema 一次，带出方案元信息（备注/版本/图标/作者），供设置页方案列表与详情显示。
                let merged = self.engine_mgr.schema_merged(id);
                let info = merged.as_ref().map(|s| &s.schema);
                json!({
                    "id": id,
                    "name": self.engine_mgr.schema_name(id),
                    "engineType": merged.as_ref().map(resolve_engine_type),
                    "builtin": true,
                    "description": info.map(|i| i.description.clone()).unwrap_or_default(),
                    "version": info.map(|i| i.version.clone()).unwrap_or_default(),
                    "icon_label": info.map(|i| i.icon_label.clone()).unwrap_or_default(),
                    "author": info.map(|i| i.author.clone()).unwrap_or_default(),
                })
            })
            .collect();
        Ok(json!(items))
    }

    fn web_dict_list_paged(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
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
        let all = store.search_user_words_prefix(schema, prefix, 0)?;
        let total = all.len();
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
        let query = str_param(params, "query").unwrap_or("");
        let limit = usize_param(params, "limit", 50);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let items: Vec<Value> = store
            .search_user_words_prefix(schema, query, limit)?
            .into_iter()
            .map(word_item)
            .collect();
        Ok(json!(items))
    }

    fn web_dict_add(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, text) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "text")?,
        );
        let weight = i32_param(params, "weight");
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.add_user_word(schema, code, text, weight)?;
        Ok(json!({ "ok": true }))
    }

    fn web_dict_update(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, text) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "text")?,
        );
        let weight = i32_param(params, "weight");
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 存在则改权重；不存在则新增（upsert 语义）。
        if !store.update_user_word_weight(schema, code, text, weight)? {
            store.add_user_word(schema, code, text, weight)?;
        }
        Ok(json!({ "ok": true }))
    }

    fn web_dict_remove(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, text) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "text")?,
        );
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.remove_user_word(schema, code, text)?;
        Ok(json!({ "ok": true }))
    }

    fn web_dict_clear(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let all = store.search_user_words_prefix(schema, "", 0)?;
        let n = all.len();
        for r in all {
            store.remove_user_word(schema, &r.code, &r.text)?;
        }
        Ok(json!(n))
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
        self.engine_mgr.write_schema_override(id, &ov)?;
        Ok(json!({ "ok": true }))
    }

    fn web_schema_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        self.engine_mgr.delete_user_schema(id)?;
        Ok(json!({ "ok": true }))
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
        let prefix = params.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
        let offset = usize_param(params, "offset", 0);
        let limit = usize_param(params, "limit", 50);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let (page, total) = store.list_freq_paged(schema, prefix, offset, limit)?;
        let items: Vec<Value> = page
            .into_iter()
            .map(|(code, text, rec)| {
                json!({ "code": code, "text": text, "count": rec.count, "lastUsed": rec.last_used })
            })
            .collect();
        Ok(json!({ "items": items, "total": total }))
    }

    fn web_freq_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, text) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "text")?,
        );
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.delete_freq(schema, code, text)?;
        Ok(json!({ "ok": true }))
    }

    fn web_freq_clear(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        Ok(json!(store.clear_freq(schema)?))
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

    fn web_temp_list(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let items: Vec<Value> = store
            .search_temp_words_prefix(schema, "", 0)?
            .into_iter()
            .map(|r| json!({ "code": r.code, "text": r.text, "count": r.count }))
            .collect();
        Ok(json!(items))
    }

    fn web_temp_promote(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, text) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "text")?,
        );
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.promote_temp_word(schema, code, text)?;
        Ok(json!({ "ok": true }))
    }

    fn web_temp_remove(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, text) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "text")?,
        );
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.remove_temp_word(schema, code, text)?;
        Ok(json!({ "ok": true }))
    }

    fn web_temp_promote_all(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let mut n = 0u64;
        for r in store.search_temp_words_prefix(schema, "", 0)? {
            if store.promote_temp_word(schema, &r.code, &r.text)? {
                n += 1;
            }
        }
        Ok(json!(n))
    }

    fn web_temp_clear(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let all = store.search_temp_words_prefix(schema, "", 0)?;
        let n = all.len();
        for r in all {
            store.remove_temp_word(schema, &r.code, &r.text)?;
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
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_remove(&self, params: &Value) -> anyhow::Result<Value> {
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.remove_phrase(code, text)?;
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
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_reset(&self) -> anyhow::Result<Value> {
        if let Some(store) = self.store.as_ref() {
            store.reset_phrases()?;
        }
        Ok(json!({ "ok": true }))
    }

    fn web_stats_summary(&self) -> anyhow::Result<Value> {
        let store = match self.store.as_ref() {
            Some(s) => s,
            None => {
                return Ok(json!({ "today": 0, "week": 0, "month": 0, "total": 0, "streak": 0 }));
            }
        };
        let today = today_str();
        let s = store.stats_summary(&today)?;
        Ok(serde_json::to_value(s)?)
    }

    fn web_stats_daily(&self, params: &Value) -> anyhow::Result<Value> {
        let from = str_param(params, "from")?.to_string();
        let to = str_param(params, "to")?.to_string();
        let store = match self.store.as_ref() {
            Some(s) => s,
            None => return Ok(json!([])),
        };
        // 真实数据按日期建索引，再用区间内连续日期补 0（对齐 mock 的连续 DailyStat 序列，便于前端绘图）。
        let mut by_date = std::collections::HashMap::new();
        for (d, rec) in store.daily_stats(&from, &to)? {
            by_date.insert(d, rec.total());
        }
        let mut out = Vec::new();
        if let (Ok(f), Ok(t)) = (
            chrono::NaiveDate::parse_from_str(&from, "%Y-%m-%d"),
            chrono::NaiveDate::parse_from_str(&to, "%Y-%m-%d"),
        ) {
            let mut cur = f;
            while cur <= t {
                let key = cur.format("%Y-%m-%d").to_string();
                let count = by_date.get(&key).copied().unwrap_or(0);
                out.push(json!({ "date": key, "count": count }));
                cur += chrono::Duration::days(1);
            }
        }
        Ok(json!(out))
    }

    fn web_stats_clear(&self) -> anyhow::Result<Value> {
        if let Some(store) = self.store.as_ref() {
            store.clear_stats()?;
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
        Ok(json!({ "pruned": n }))
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
        // 合并 base 链后的原始主题配置（serde_yaml::Value → JSON），供前端预览渲染。
        let merged = wind_theme::load_merged_dirs(&dirs, name, 0)?;
        Ok(serde_json::to_value(&merged)?)
    }

    fn web_theme_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let name = str_param(params, "name")?;
        let user_dir = self
            .user_themes_dir()
            .ok_or_else(|| anyhow::anyhow!("无用户主题目录"))?;
        let target = user_dir.join(name);
        if !target.join("theme.yaml").exists() {
            anyhow::bail!("内置主题不可删除或主题不存在: {}", name);
        }
        std::fs::remove_dir_all(&target)?;
        Ok(json!({ "ok": true }))
    }

    fn web_theme_import_text(&self, params: &Value) -> anyhow::Result<Value> {
        let yaml = str_param(params, "yaml")?;
        let force = params
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // 校验可解析为合法主题。
        wind_theme::validate_text(yaml)?;
        let meta = wind_theme::meta_from_text(yaml)
            .ok_or_else(|| anyhow::anyhow!("主题缺少 meta.name"))?;
        if meta.name.trim().is_empty() {
            anyhow::bail!("主题 meta.name 为空");
        }
        let user_dir = self
            .user_themes_dir()
            .ok_or_else(|| anyhow::anyhow!("无用户主题目录"))?;
        let target = user_dir.join(&meta.name);
        if target.join("theme.yaml").exists() && !force {
            anyhow::bail!("主题已存在（force=false）: {}", meta.name);
        }
        std::fs::create_dir_all(&target)?;
        let file = target.join("theme.yaml");
        let tmp = file.with_extension("yaml.tmp");
        std::fs::write(&tmp, yaml.as_bytes())?;
        std::fs::rename(&tmp, &file)?;
        Ok(json!({ "ok": true }))
    }

    fn web_theme_list(&self) -> anyhow::Result<Value> {
        // 每个主题目录读取 theme.yaml 的 meta（名称/作者/版本/排序），供设置页显示友好名而非 id (#5)。
        let mut rows: Vec<(i32, String, Value)> = Vec::new();
        if let Some(dir) = &self.themes_dir {
            let dirs = [dir.clone()];
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    if !e.path().is_dir() {
                        continue;
                    }
                    let Some(name) = e.file_name().to_str().map(|s| s.to_string()) else {
                        continue;
                    };
                    let meta = wind_theme::read_meta(&dirs, &name);
                    let display = meta
                        .as_ref()
                        .map(|m| m.name.clone())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| name.clone());
                    let order = meta.as_ref().map(|m| m.order).unwrap_or(0);
                    rows.push((
                        order,
                        display.clone(),
                        json!({
                            "name": name,
                            "display_name": display,
                            "author": meta.as_ref().map(|m| m.author.clone()).unwrap_or_default(),
                            "version": meta.as_ref().map(|m| m.version.clone()).unwrap_or_default(),
                            "builtin": true,
                        }),
                    ));
                }
            }
        }
        // 按 (order, 显示名) 稳定排序。
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let out: Vec<Value> = rows.into_iter().map(|(_, _, v)| v).collect();
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
    fn stats_summary_daily_shape() {
        let c = coord("stats");
        let store = c.store.as_ref().unwrap();
        let today = today_str();
        store.record_stat(&today, 42, 0).unwrap();

        // stats.summary 形状对齐 StatsSummary{today,week,month,total,streak}
        let sum = c.web_data_rpc("stats.summary", &json!({})).unwrap();
        for k in ["today", "week", "month", "total", "streak"] {
            assert!(
                sum.get(k).and_then(|v| v.as_u64()).is_some(),
                "summary 缺 {k}"
            );
        }
        assert_eq!(sum["today"], 42);

        // stats.daily 形状对齐 DailyStat{date,count}，区间内连续补 0
        let daily = c
            .web_data_rpc("stats.daily", &json!({ "from": &today, "to": &today }))
            .unwrap();
        let arr = daily.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["date"], json!(today));
        assert_eq!(arr[0]["count"], 42);

        // pruneBefore(days) 返回 {pruned}
        let pr = c
            .web_data_rpc("stats.pruneBefore", &json!({ "days": 0 }))
            .unwrap();
        assert!(pr.get("pruned").and_then(|v| v.as_u64()).is_some());

        // clear 后 summary 归零
        c.web_data_rpc("stats.clear", &json!({})).unwrap();
        let sum2 = c.web_data_rpc("stats.summary", &json!({})).unwrap();
        assert_eq!(sum2["total"], 0);
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
    fn record_input_stats_counts_committed_text() {
        use wind_bridge::handler::KeyAction;
        let c = coord("stats_record");
        // 模拟一次上屏「你好」（中文 2 字）
        c.record_input_stats(&KeyAction::InsertText {
            text: "你好".to_string(),
            new_composition: None,
            mode_changed: false,
            chinese_mode: true,
            has_new_composition: false,
        });
        let today = today_str();
        assert_eq!(
            c.store
                .as_ref()
                .unwrap()
                .get_daily_stat(&today)
                .unwrap()
                .chinese,
            2
        );
    }
}
