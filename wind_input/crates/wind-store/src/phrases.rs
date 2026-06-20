//! 用户短语存储（redb）
//!
//! 与 Go 版本 `wind_input/internal/store/phrases.go` 对齐。短语是**全局**的（不分方案）：
//! code（触发码）→ text（上屏内容，可为字面量或 cmdbar 模板如 `$date`）。
//!
//! PHRASES 表，key=`"{code}\0{text}"`（store.md §2），value = PhraseValue 的 JSON
//! （短语数量少、写入低频，JSON 足够）。系统短语来自 data/system.phrases.toml（wind-phrase
//! 层），此处只存**用户**短语；resetDefault = 清空用户短语。

use crate::store::{PHRASES, Store};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

/// 短语记录（code/text 来自 key，其余来自 value）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhraseRecord {
    pub code: String,
    pub text: String,
    pub weight: i32,
    pub position: i32,
    pub enabled: bool,
    pub is_system: bool,
}

/// value 部分（text/code 存于 key）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PhraseValue {
    #[serde(default)]
    weight: i32,
    #[serde(default)]
    position: i32,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    is_system: bool,
}

fn default_true() -> bool {
    true
}

/// key: "{code}\0{text}"
fn phrase_key(code: &str, text: &str) -> String {
    format!("{code}\u{0}{text}")
}

/// 拆分 key → (code, text)
fn split_phrase_key(key: &str) -> Option<(&str, &str)> {
    let mut it = key.splitn(2, '\u{0}');
    Some((it.next()?, it.next()?))
}

impl Store {
    /// 列举全部用户短语（按 code\0text 升序）。
    pub fn list_phrases(&self) -> anyhow::Result<Vec<PhraseRecord>> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(PHRASES)?;
            let mut out = Vec::new();
            for item in t.range::<&str>(..)? {
                let (k, v) = item?;
                if let (Some((code, text)), Ok(val)) = (
                    split_phrase_key(k.value()),
                    serde_json::from_slice::<PhraseValue>(v.value()),
                ) {
                    out.push(PhraseRecord {
                        code: code.to_string(),
                        text: text.to_string(),
                        weight: val.weight,
                        position: val.position,
                        enabled: val.enabled,
                        is_system: val.is_system,
                    });
                }
            }
            Ok(out)
        })
    }

    /// 新增/覆盖一条用户短语。
    pub fn add_phrase(
        &self,
        code: &str,
        text: &str,
        position: i32,
        weight: i32,
    ) -> anyhow::Result<()> {
        self.put_phrase(
            code,
            text,
            PhraseValue {
                weight,
                position,
                enabled: true,
                is_system: false,
            },
        )
    }

    fn put_phrase(&self, code: &str, text: &str, val: PhraseValue) -> anyhow::Result<()> {
        let key = phrase_key(code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(PHRASES)?;
                let bytes = serde_json::to_vec(&val)?;
                t.insert(key.as_str(), bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 取一条短语（无则 None）。
    fn get_phrase(&self, code: &str, text: &str) -> anyhow::Result<Option<PhraseValue>> {
        let key = phrase_key(code, text);
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(PHRASES)?;
            Ok(t.get(key.as_str())?
                .and_then(|g| serde_json::from_slice::<PhraseValue>(g.value()).ok()))
        })
    }

    /// 删除一条短语（不存在静默成功）。
    pub fn remove_phrase(&self, code: &str, text: &str) -> anyhow::Result<()> {
        let key = phrase_key(code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(PHRASES)?;
                t.remove(key.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 编辑短语：可改 code/text（键变化时 remove+add）/position/weight。保留 enabled/is_system。
    pub fn update_phrase(
        &self,
        code: &str,
        text: &str,
        new_code: Option<&str>,
        new_text: Option<&str>,
        position: Option<i32>,
        weight: Option<i32>,
    ) -> anyhow::Result<()> {
        let cur = self.get_phrase(code, text)?.unwrap_or(PhraseValue {
            weight: 0,
            position: 0,
            enabled: true,
            is_system: false,
        });
        let nc = new_code.unwrap_or(code);
        let nt = new_text.unwrap_or(text);
        let val = PhraseValue {
            weight: weight.unwrap_or(cur.weight),
            position: position.unwrap_or(cur.position),
            enabled: cur.enabled,
            is_system: cur.is_system,
        };
        // 键改变 → 先删旧键
        if nc != code || nt != text {
            self.remove_phrase(code, text)?;
        }
        self.put_phrase(nc, nt, val)
    }

    /// 设置启停。
    pub fn set_phrase_enabled(&self, code: &str, text: &str, enabled: bool) -> anyhow::Result<()> {
        let mut cur = self.get_phrase(code, text)?.unwrap_or(PhraseValue {
            weight: 0,
            position: 0,
            enabled: true,
            is_system: false,
        });
        cur.enabled = enabled;
        self.put_phrase(code, text, cur)
    }

    /// 重置为默认：清空全部用户短语，返回删除条数。
    pub fn reset_phrases(&self) -> anyhow::Result<usize> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let n;
            {
                let mut t = txn.open_table(PHRASES)?;
                let keys: Vec<String> = {
                    let mut ks = Vec::new();
                    for item in t.range::<&str>(..)? {
                        ks.push(item?.0.value().to_string());
                    }
                    ks
                };
                n = keys.len();
                for k in keys {
                    t.remove(k.as_str())?;
                }
            }
            txn.commit()?;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn test_phrase_crud() {
        let path = tmp("wind_phrases_crud.redb");
        let s = Store::open(&path).unwrap();
        s.add_phrase("rq", "2026-06-20", 0, 1).unwrap();
        s.add_phrase("yx", "user@example.com", 0, 1).unwrap();
        assert_eq!(s.list_phrases().unwrap().len(), 2);

        // 启停
        s.set_phrase_enabled("rq", "2026-06-20", false).unwrap();
        let rq = s
            .list_phrases()
            .unwrap()
            .into_iter()
            .find(|p| p.code == "rq")
            .unwrap();
        assert!(!rq.enabled);

        // 改 code（键迁移）
        s.update_phrase("yx", "user@example.com", Some("mail"), None, None, Some(5))
            .unwrap();
        let list = s.list_phrases().unwrap();
        assert!(list.iter().any(|p| p.code == "mail" && p.weight == 5));
        assert!(!list.iter().any(|p| p.code == "yx"));

        // 删除
        s.remove_phrase("rq", "2026-06-20").unwrap();
        assert_eq!(s.list_phrases().unwrap().len(), 1);

        // 重置清空
        assert_eq!(s.reset_phrases().unwrap(), 1);
        assert_eq!(s.list_phrases().unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }
}
