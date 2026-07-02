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
            .installed_schemas()
            .iter()
            .map(|id| {
                // 取合并后 Schema 一次，带出方案元信息（备注/版本/图标/作者），供设置页方案列表与详情显示。
                let merged = self.engine_mgr.schema_merged(id);
                let info = merged.as_ref().map(|s| &s.schema);
                json!({
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
        let all = store.search_user_words_prefix(&schema, prefix, 0)?;
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
        let all = store.search_user_words_prefix(&schema, "", 0)?;
        let n = all.len();
        for r in all {
            store.remove_user_word(&schema, &r.code, &r.text)?;
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
        // 持久化 override（不 invalidate），再对已加载引擎 live 翻该扩展层的 enabled 标志——
        // 扩展词库热插拔：无需重熔大词库即时生效；未加载方案下次构建按新 override 生效。
        self.engine_mgr.persist_schema_override(id, &ov)?;
        let live = self.engine_mgr.set_dict_enabled_live(id, dict_id, enabled);
        Ok(json!({ "ok": true, "live": live }))
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
        let schema = self.engine_mgr.data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let prefix = params.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
        let offset = usize_param(params, "offset", 0);
        let limit = usize_param(params, "limit", 50);
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let (page, total) = store.list_freq_paged(&schema, prefix, offset, limit)?;
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
        let (rows, total) = store.list_user_phrases_paged(prefix, offset, limit)?;
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
        // 校验可解析为合法主题。
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
        if target.join("theme.toml").exists() && !force {
            anyhow::bail!("主题已存在（force=false）: {}", meta.name);
        }
        std::fs::create_dir_all(&target)?;
        let file = target.join("theme.toml");
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &file)?;
        Ok(json!({ "ok": true }))
    }

    fn web_theme_list(&self) -> anyhow::Result<Value> {
        // 与右键菜单 list_themes 同源:扫用户+安装多目录(theme_search_dirs),含 theme.toml、
        // 过滤 `_` 前缀(_base 等)、用户优先去重;读 meta(名称/作者/版本/排序)。修列表不一致 (#5/主题)。
        let dirs = self.theme_search_dirs();
        let mut seen = std::collections::HashSet::new();
        let mut rows: Vec<(i32, String, Value)> = Vec::new();
        for (i, dir) in dirs.iter().enumerate() {
            let builtin = i > 0; // theme_search_dirs[0] 为用户目录
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in rd.flatten() {
                if !e.path().is_dir() {
                    continue;
                }
                let Some(id) = e.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                if id.starts_with('_') || !dir.join(&id).join("theme.toml").exists() {
                    continue;
                }
                if !seen.insert(id.clone()) {
                    continue;
                }
                let meta = wind_theme::read_meta(&dirs, &id);
                let display = meta
                    .as_ref()
                    .map(|m| m.name.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| id.clone());
                let order = meta.as_ref().map(|m| m.order).unwrap_or(0);
                rows.push((
                    order,
                    display.clone(),
                    json!({
                        "name": id,
                        "display_name": display,
                        "author": meta.as_ref().map(|m| m.author.clone()).unwrap_or_default(),
                        "version": meta.as_ref().map(|m| m.version.clone()).unwrap_or_default(),
                        "builtin": builtin,
                    }),
                ));
            }
        }
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
}
