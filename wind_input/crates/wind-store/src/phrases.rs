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

/// 待同步的系统短语（来自 TOML，已做 platform 过滤）。
#[derive(Debug, Clone)]
pub struct SystemPhrase {
    pub code: String,
    pub text: String,
    pub weight: i32,
    pub position: i32,
}

/// 系统短语同步统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
}

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

    /// TOML 内容哈希标记（判断是否需要重新同步系统短语）。
    pub fn phrase_sys_hash(&self) -> anyhow::Result<Option<String>> {
        self.meta_get("phrase_sys_hash")
    }

    pub fn set_phrase_sys_hash(&self, h: &str) -> anyhow::Result<()> {
        self.meta_set("phrase_sys_hash", h)
    }

    /// 把系统短语同步进 PHRASES 表（is_system=true）：
    /// 已存在 (code,text) → 更新 weight/position，保留 enabled；不存在 → 插入 enabled=true；
    /// 表内 is_system=true 但不在本次列表的 → 删除。用户短语(is_system=false)不动。
    pub fn sync_system_phrases(&self, entries: &[SystemPhrase]) -> anyhow::Result<SyncStats> {
        use std::collections::HashSet;
        let mut stats = SyncStats::default();
        let wanted: HashSet<(String, String)> = entries
            .iter()
            .map(|e| (e.code.clone(), e.text.clone()))
            .collect();

        // 1. 删除过时系统短语
        let existing = self.list_phrases()?;
        for p in &existing {
            if p.is_system && !wanted.contains(&(p.code.clone(), p.text.clone())) {
                self.remove_phrase(&p.code, &p.text)?;
                stats.removed += 1;
            }
        }
        // 2. upsert
        for e in entries {
            match self.get_phrase(&e.code, &e.text)? {
                Some(cur) => {
                    let val = PhraseValue {
                        weight: e.weight,
                        position: e.position,
                        enabled: cur.enabled, // 保留开关
                        is_system: true,
                    };
                    self.put_phrase(&e.code, &e.text, val)?;
                    stats.updated += 1;
                }
                None => {
                    self.put_phrase(
                        &e.code,
                        &e.text,
                        PhraseValue {
                            weight: e.weight,
                            position: e.position,
                            enabled: true,
                            is_system: true,
                        },
                    )?;
                    stats.added += 1;
                }
            }
        }
        Ok(stats)
    }

    /// 系统短语（is_system=true），按 key 升序，不分页。
    pub fn list_system_phrases(&self) -> anyhow::Result<Vec<PhraseRecord>> {
        Ok(self
            .list_phrases()?
            .into_iter()
            .filter(|p| p.is_system)
            .collect())
    }

    /// 用户短语分页（is_system=false）。prefix 非空时按 code/text 包含过滤后再分页。
    /// 返回 (页内行, 过滤后总数)。
    pub fn list_user_phrases_paged(
        &self,
        prefix: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<PhraseRecord>, usize)> {
        let mut all: Vec<PhraseRecord> = self
            .list_phrases()?
            .into_iter()
            .filter(|p| !p.is_system)
            .collect();
        if let Some(q) = prefix {
            let q = q.trim();
            if !q.is_empty() {
                all.retain(|p| p.code.contains(q) || p.text.contains(q));
            }
        }
        let total = all.len();
        let page = all.into_iter().skip(offset).take(limit).collect();
        Ok((page, total))
    }

    /// 输入期短语集：全部 enabled 短语（系统+用户）。
    pub fn enabled_phrases_for_input(&self) -> anyhow::Result<Vec<PhraseRecord>> {
        Ok(self
            .list_phrases()?
            .into_iter()
            .filter(|p| p.enabled)
            .collect())
    }

    /// 系统"恢复默认"：is_system=true 行全部 enabled=true。返回改动条数。
    pub fn reset_system_enabled(&self) -> anyhow::Result<usize> {
        let mut n = 0;
        for p in self.list_phrases()? {
            if p.is_system && !p.enabled {
                self.set_phrase_enabled(&p.code, &p.text, true)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// 用户"清空"：删 is_system=false 行。返回删除条数。
    pub fn reset_user_phrases(&self) -> anyhow::Result<usize> {
        let users: Vec<PhraseRecord> = self
            .list_phrases()?
            .into_iter()
            .filter(|p| !p.is_system)
            .collect();
        let n = users.len();
        for p in users {
            self.remove_phrase(&p.code, &p.text)?;
        }
        Ok(n)
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
    fn sync_system_phrases_add_update_remove() {
        let path = tmp("wind_phrases_sync.redb");
        let s = Store::open(&path).unwrap();
        // 首轮：加两条系统短语
        let v1 = vec![
            SystemPhrase {
                code: "rq".into(),
                text: "$date".into(),
                weight: 1000,
                position: 0,
            },
            SystemPhrase {
                code: "em".into(),
                text: "（＾＿＾）".into(),
                weight: 1000,
                position: 0,
            },
        ];
        let st = s.sync_system_phrases(&v1).unwrap();
        assert_eq!((st.added, st.updated, st.removed), (2, 0, 0));
        // 用户关掉一条系统短语
        s.set_phrase_enabled("em", "（＾＿＾）", false).unwrap();
        // 次轮：em 改权重 + 删 rq + 加新 nn；em 的 enabled 应保留 false
        let v2 = vec![
            SystemPhrase {
                code: "em".into(),
                text: "（＾＿＾）".into(),
                weight: 500,
                position: 0,
            },
            SystemPhrase {
                code: "nn".into(),
                text: "你好".into(),
                weight: 1000,
                position: 0,
            },
        ];
        let st2 = s.sync_system_phrases(&v2).unwrap();
        assert_eq!((st2.added, st2.updated, st2.removed), (1, 1, 1));
        let list = s.list_phrases().unwrap();
        let em = list.iter().find(|p| p.code == "em").unwrap();
        assert_eq!(em.weight, 500, "内容更新");
        assert!(!em.enabled, "开关保留");
        assert!(em.is_system);
        assert!(!list.iter().any(|p| p.code == "rq"), "过时系统短语删除");
        assert!(list.iter().any(|p| p.code == "nn"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sync_keeps_user_phrases() {
        let path = tmp("wind_phrases_sync_user.redb");
        let s = Store::open(&path).unwrap();
        s.add_phrase("me", "自定义", 0, 1).unwrap(); // 用户短语 is_system=false
        s.sync_system_phrases(&[SystemPhrase {
            code: "sys".into(),
            text: "系统".into(),
            weight: 1,
            position: 0,
        }])
        .unwrap();
        // 再同步（sys 消失）应删 sys 但保留用户 me
        s.sync_system_phrases(&[]).unwrap();
        let list = s.list_phrases().unwrap();
        assert!(
            list.iter().any(|p| p.code == "me"),
            "用户短语不受系统同步影响"
        );
        assert!(!list.iter().any(|p| p.code == "sys"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn phrase_sys_hash_persist() {
        let path = tmp("wind_phrases_hash.redb");
        let s = Store::open(&path).unwrap();
        assert_eq!(s.phrase_sys_hash().unwrap(), None);
        s.set_phrase_sys_hash("abc123").unwrap();
        assert_eq!(s.phrase_sys_hash().unwrap().as_deref(), Some("abc123"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_system_and_user_split() {
        let path = tmp("wind_phrases_split.redb");
        let s = Store::open(&path).unwrap();
        s.sync_system_phrases(&[
            SystemPhrase {
                code: "a".into(),
                text: "甲".into(),
                weight: 1,
                position: 0,
            },
            SystemPhrase {
                code: "b".into(),
                text: "乙".into(),
                weight: 1,
                position: 0,
            },
        ])
        .unwrap();
        s.add_phrase("u1", "用户一", 0, 1).unwrap();
        s.add_phrase("u2", "用户二", 0, 1).unwrap();
        assert_eq!(s.list_system_phrases().unwrap().len(), 2);
        let (page, total) = s.list_user_phrases_paged(None, 0, 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(page.len(), 2);
        assert!(page.iter().all(|p| !p.is_system));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn user_paging_and_prefix() {
        let path = tmp("wind_phrases_page.redb");
        let s = Store::open(&path).unwrap();
        for i in 0..5 {
            s.add_phrase(&format!("c{i}"), &format!("词{i}"), 0, 1)
                .unwrap();
        }
        let (p0, total) = s.list_user_phrases_paged(None, 0, 2).unwrap();
        assert_eq!(total, 5);
        assert_eq!(p0.len(), 2);
        let (p2, _) = s.list_user_phrases_paged(None, 4, 2).unwrap();
        assert_eq!(p2.len(), 1, "末页不足一页");
        // prefix 过滤
        let (pf, tf) = s.list_user_phrases_paged(Some("c3"), 0, 10).unwrap();
        assert_eq!(tf, 1);
        assert_eq!(pf[0].code, "c3");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn enabled_for_input_and_resets() {
        let path = tmp("wind_phrases_enabled.redb");
        let s = Store::open(&path).unwrap();
        s.sync_system_phrases(&[SystemPhrase {
            code: "a".into(),
            text: "甲".into(),
            weight: 1,
            position: 0,
        }])
        .unwrap();
        s.add_phrase("u", "用户", 0, 1).unwrap();
        s.set_phrase_enabled("a", "甲", false).unwrap(); // 禁用系统
        let inp = s.enabled_phrases_for_input().unwrap();
        assert!(inp.iter().all(|p| p.enabled));
        assert!(!inp.iter().any(|p| p.code == "a"), "禁用项不入输入集");
        assert!(inp.iter().any(|p| p.code == "u"));
        // 系统恢复默认：全部重新启用
        let n = s.reset_system_enabled().unwrap();
        assert_eq!(n, 1);
        assert!(
            s.enabled_phrases_for_input()
                .unwrap()
                .iter()
                .any(|p| p.code == "a")
        );
        // 用户清空
        assert_eq!(s.reset_user_phrases().unwrap(), 1);
        assert!(!s.list_phrases().unwrap().iter().any(|p| !p.is_system));
        let _ = std::fs::remove_file(&path);
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
