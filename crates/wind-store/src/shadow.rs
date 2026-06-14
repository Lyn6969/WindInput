//! Shadow 规则存储（候选词条手动调序 / 删除）
//!
//! 与 Go 版本 `wind_input/internal/store/shadow.go` 对齐（简化）。
//! 按「方案 + 输入码」分组，记录用户对候选的置顶/前后移（pinned）与删除（deleted）。
//! 规则在词频排序之后应用，优先级最高。规则的「应用」由调用方（协调器）完成，
//! 本模块只负责规则的增删查与持久化，避免对候选类型产生依赖。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// 单条置顶/移动规则：把 word 固定到 position（页内/列表内目标下标）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowPin {
    pub word: String,
    pub position: usize,
}

/// 某输入码下的 Shadow 规则集合
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShadowRecord {
    #[serde(default)]
    pub pinned: Vec<ShadowPin>,
    /// 被删除（屏蔽）的候选文本
    #[serde(default)]
    pub deleted: Vec<String>,
}

impl ShadowRecord {
    fn is_empty(&self) -> bool {
        self.pinned.is_empty() && self.deleted.is_empty()
    }
}

/// Shadow 规则存储：key = "方案id\t输入码"
pub struct ShadowStore {
    map: RwLock<HashMap<String, ShadowRecord>>,
}

impl Default for ShadowStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ShadowStore {
    pub fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
        }
    }

    fn key(schema: &str, code: &str) -> String {
        format!("{}\t{}", schema, code)
    }

    /// 置顶/移动：把 word 固定到 position（0 = 首位）。LIFO：最新规则覆盖同词旧规则。
    pub fn pin(&self, schema: &str, code: &str, word: &str, position: usize) {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        let rec = map.entry(Self::key(schema, code)).or_default();
        rec.pinned.retain(|p| p.word != word);
        rec.deleted.retain(|d| d != word); // 置顶优先于删除
        rec.pinned.insert(0, ShadowPin { word: word.to_string(), position });
    }

    /// 删除（屏蔽）：word 不再出现在该输入码的候选中。
    pub fn delete(&self, schema: &str, code: &str, word: &str) {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        let rec = map.entry(Self::key(schema, code)).or_default();
        rec.pinned.retain(|p| p.word != word);
        if !rec.deleted.iter().any(|d| d == word) {
            rec.deleted.push(word.to_string());
        }
    }

    /// 恢复默认：清除该 word 的置顶与删除规则。
    pub fn reset(&self, schema: &str, code: &str, word: &str) {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        let key = Self::key(schema, code);
        if let Some(rec) = map.get_mut(&key) {
            rec.pinned.retain(|p| p.word != word);
            rec.deleted.retain(|d| d != word);
            if rec.is_empty() {
                map.remove(&key);
            }
        }
    }

    /// 该 word 是否有 Shadow 规则（置顶或删除）
    pub fn has_rule(&self, schema: &str, code: &str, word: &str) -> bool {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        match map.get(&Self::key(schema, code)) {
            Some(rec) => {
                rec.pinned.iter().any(|p| p.word == word)
                    || rec.deleted.iter().any(|d| d == word)
            }
            None => false,
        }
    }

    /// 取某输入码的规则副本（无则 None）
    pub fn get_rules(&self, schema: &str, code: &str) -> Option<ShadowRecord> {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        map.get(&Self::key(schema, code)).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.map.read().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// 从 JSON 文件加载（不存在则静默忽略）
    pub fn load_from_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if let Ok(parsed) = serde_json::from_str::<HashMap<String, ShadowRecord>>(&content) {
            *self.map.write().unwrap_or_else(|e| e.into_inner()) = parsed;
        }
        Ok(())
    }

    /// 保存到 JSON 文件（原子写）
    pub fn save_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = {
            let map = self.map.read().unwrap_or_else(|e| e.into_inner());
            serde_json::to_string_pretty(&*map).unwrap_or_else(|_| "{}".to_string())
        };
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pin_delete_reset() {
        let s = ShadowStore::new();
        s.pin("wubi", "aaaa", "恭恭敬敬", 0);
        s.delete("wubi", "bbbb", "某词");
        assert!(s.has_rule("wubi", "aaaa", "恭恭敬敬"));
        assert!(s.has_rule("wubi", "bbbb", "某词"));

        let r = s.get_rules("wubi", "aaaa").unwrap();
        assert_eq!(r.pinned.len(), 1);
        assert_eq!(r.pinned[0].position, 0);

        // 删除优先级：pin 后再 delete 同词，pin 被移除
        s.delete("wubi", "aaaa", "恭恭敬敬");
        let r = s.get_rules("wubi", "aaaa").unwrap();
        assert!(r.pinned.is_empty());
        assert_eq!(r.deleted, vec!["恭恭敬敬".to_string()]);

        // 恢复默认清除规则
        s.reset("wubi", "aaaa", "恭恭敬敬");
        assert!(!s.has_rule("wubi", "aaaa", "恭恭敬敬"));
    }

    #[test]
    fn test_save_load_roundtrip() {
        let tmp = std::env::temp_dir().join("wind_shadow_roundtrip.json");
        let _ = std::fs::remove_file(&tmp);
        let a = ShadowStore::new();
        a.pin("py", "nihao", "你好", 0);
        a.delete("py", "ceshi", "测试");
        a.save_to_file(&tmp).unwrap();

        let b = ShadowStore::new();
        b.load_from_file(&tmp).unwrap();
        assert!(b.has_rule("py", "nihao", "你好"));
        assert!(b.has_rule("py", "ceshi", "测试"));
        let _ = std::fs::remove_file(&tmp);
    }
}
