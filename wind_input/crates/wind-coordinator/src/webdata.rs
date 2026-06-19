//! Web 设置数据 RPC：schema/dict/temp/freq/shadow/stats/theme/phrase 命名空间。
//!
//! 经 wind-webapi 的 `CoreStatus::data_rpc` 转发到此（service 的 WebStatus 适配）。
//! 方法名与前端 `contract.ts` 1:1 一致。
//!
//! 接入进度：
//! - ✅ schema.list/active/setActive、dict.*（listPaged/search/add/update/remove/clear/stats）、theme.list
//! - 🚧 其余命名空间先返回合法空壳/默认值（保证前端页面加载、不报 unknown method），逐步深化。

use serde_json::{Value, json};

use crate::coordinator::Coordinator;

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

impl Coordinator {
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
            // 🚧 方案 YAML 配置编辑：待深化
            "schema.getConfig" | "schema.references" => Ok(json!({})),
            "schema.saveConfig"
            | "schema.resetConfig"
            | "schema.setDictEnabled"
            | "schema.delete" => Ok(json!({ "ok": true })),

            // ── dict.*（用户词库，redb 持久化）────────────────────
            "dict.listPaged" => self.web_dict_list_paged(params),
            "dict.search" => self.web_dict_search(params),
            "dict.add" => self.web_dict_add(params),
            "dict.update" => self.web_dict_update(params),
            "dict.remove" => self.web_dict_remove(params),
            "dict.clear" => self.web_dict_clear(params),
            "dict.stats" => self.web_dict_stats(),
            // 🚧 引擎编码/拼音生成：待深化
            "dict.encode" | "dict.genPinyin" => Ok(json!("")),

            // ── temp.*（临时词，redb）─────────────────────────────
            "temp.list" => self.web_temp_list(params),
            "temp.promote" => self.web_temp_promote(params),
            "temp.remove" => self.web_temp_remove(params),
            "temp.promoteAll" => self.web_temp_promote_all(params),
            "temp.clear" => self.web_temp_clear(params),

            // ── freq.*（词频）🚧 待深化 ──────────────────────────
            "freq.listPaged" => Ok(json!({ "items": [], "total": 0 })),
            "freq.delete" => Ok(json!({ "ok": true })),
            "freq.clear" => Ok(json!(0)),

            // ── shadow.*（影子规则）🚧 待深化 ────────────────────
            "shadow.list" => Ok(json!([])),
            "shadow.pin" | "shadow.delete" | "shadow.removeRule" => Ok(json!({ "ok": true })),

            // ── phrase.*（短语）🚧 待深化 ────────────────────────
            "phrase.list" => Ok(json!([])),
            "phrase.add" | "phrase.update" | "phrase.remove" | "phrase.setEnabled"
            | "phrase.resetDefault" => Ok(json!({ "ok": true })),

            // ── stats.*（统计）🚧 待深化 ─────────────────────────
            "stats.summary" => {
                Ok(json!({ "today": 0, "week": 0, "month": 0, "total": 0, "streak": 0 }))
            }
            "stats.daily" => Ok(json!([])),
            "stats.clear" => Ok(json!({ "ok": true })),
            "stats.pruneBefore" => Ok(json!({ "pruned": 0 })),

            // ── theme.* ──────────────────────────────────────────
            "theme.list" => self.web_theme_list(),
            "theme.preview" => Ok(json!({})), // 🚧 待深化
            "theme.delete" | "theme.importFromText" | "theme.importFromUrl" => {
                Ok(json!({ "ok": true }))
            }

            other => anyhow::bail!("unknown method: {}", other),
        }
    }

    fn web_schema_list(&self) -> anyhow::Result<Value> {
        let items: Vec<Value> = self
            .engine_mgr
            .available_schemas()
            .iter()
            .map(|id| json!({ "id": id, "name": id, "builtin": true }))
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
        for id in self.engine_mgr.available_schemas() {
            let user_words = store.search_user_words_prefix(id, "", 0).map(|v| v.len()).unwrap_or(0);
            out.push(json!({
                "schemaId": id,
                "name": id,
                "userWords": user_words,
                "tempWords": 0,   // 🚧 待深化
                "shadowRules": 0, // 🚧 待深化
            }));
        }
        Ok(json!(out))
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

    fn web_theme_list(&self) -> anyhow::Result<Value> {
        let mut out = Vec::new();
        if let Some(dir) = &self.themes_dir {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        if let Some(name) = e.file_name().to_str() {
                            out.push(json!({ "name": name, "builtin": true }));
                        }
                    }
                }
            }
        }
        Ok(json!(out))
    }
}

/// UserWordRecord → 前端 UserWordItem。
fn word_item(r: wind_store::user_words::UserWordRecord) -> Value {
    json!({ "code": r.code, "text": r.text, "weight": r.weight, "enabled": true })
}
