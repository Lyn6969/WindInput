//! 用户词存储（redb）
//!
//! 与 Go 版本 `wind_input/internal/store/user_words.go` 对齐，但：
//! - value 用定长 16 字节（weight i32 + count u32 + created_at i64），text/code 存于 key，比 Go 的 JSON 紧凑（store.md §7.3）。
//! - created_at 统一为 i64 unix 秒（修 Go user=秒/temp=毫秒 不一致，store.md §7.2）。
//!
//! key 编码：`"{schema}\0{code}\0{text}"`（store.md §2）。

use crate::store::{Store, USER_WORDS};
use crate::wdict;
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// 用户词记录（code/text 来自 key，weight/count/created_at 来自定长 value）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserWordRecord {
    pub code: String,
    pub text: String,
    pub weight: i32,
    pub count: u32,
    /// 创建时间（unix 秒）
    pub created_at: i64,
}

/// 当前 unix 秒
pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// key: "{schema}\0{code}\0{text}"
pub(crate) fn enc_key(schema: &str, code: &str, text: &str) -> String {
    format!("{schema}\u{0}{code}\u{0}{text}")
}

/// 拆分 key → (schema, code, text)
pub(crate) fn split_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut it = key.splitn(3, '\u{0}');
    Some((it.next()?, it.next()?, it.next()?))
}

/// value: 定长 16 字节
pub(crate) fn enc_val(weight: i32, count: u32, created_at: i64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&weight.to_le_bytes());
    b[4..8].copy_from_slice(&count.to_le_bytes());
    b[8..16].copy_from_slice(&created_at.to_le_bytes());
    b
}

/// 解码 value → (weight, count, created_at)
pub(crate) fn dec_val(b: &[u8]) -> Option<(i32, u32, i64)> {
    if b.len() < 16 {
        return None;
    }
    Some((
        i32::from_le_bytes(b[0..4].try_into().ok()?),
        u32::from_le_bytes(b[4..8].try_into().ok()?),
        i64::from_le_bytes(b[8..16].try_into().ok()?),
    ))
}

impl Store {
    /// 新增/合并用户词：已存在则权重取 max、保留原 created_at；新词记 created_at=now。
    /// 用户词**无权重上限**（store.md §3）。
    pub fn add_user_word(
        &self,
        schema: &str,
        code: &str,
        text: &str,
        weight: i32,
    ) -> anyhow::Result<()> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(USER_WORDS)?;
                let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                let (w, c, ca) = match existing {
                    Some((ow, oc, oca)) => (ow.max(weight), oc, oca),
                    None => (weight, 0, now_secs()),
                };
                t.insert(key.as_str(), enc_val(w, c, ca).as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 精确取某 code 下的所有用户词
    pub fn get_user_words(&self, schema: &str, code: &str) -> anyhow::Result<Vec<UserWordRecord>> {
        let prefix = format!("{schema}\u{0}{code}\u{0}");
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(USER_WORDS)?;
            let mut out = Vec::new();
            for item in t.range(prefix.as_str()..)? {
                let (k, v) = item?;
                let key = k.value();
                if !key.starts_with(&prefix) {
                    break;
                }
                let text = &key[prefix.len()..];
                if let Some((w, c, ca)) = dec_val(v.value()) {
                    out.push(UserWordRecord {
                        code: code.to_string(),
                        text: text.to_string(),
                        weight: w,
                        count: c,
                        created_at: ca,
                    });
                }
            }
            Ok(out)
        })
    }

    /// 前缀检索（跨 code）。limit<=0 表示不限。
    pub fn search_user_words_prefix(
        &self,
        schema: &str,
        prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<UserWordRecord>> {
        let scan = format!("{schema}\u{0}{prefix}");
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(USER_WORDS)?;
            let mut out = Vec::new();
            for item in t.range(scan.as_str()..)? {
                let (k, v) = item?;
                let key = k.value();
                if !key.starts_with(&scan) {
                    break;
                }
                if let (Some((_, code, text)), Some((w, c, ca))) =
                    (split_key(key), dec_val(v.value()))
                {
                    out.push(UserWordRecord {
                        code: code.to_string(),
                        text: text.to_string(),
                        weight: w,
                        count: c,
                        created_at: ca,
                    });
                }
                if limit > 0 && out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
    }

    /// 删除用户词（不存在静默成功）
    pub fn remove_user_word(&self, schema: &str, code: &str, text: &str) -> anyhow::Result<()> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(USER_WORDS)?;
                t.remove(key.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 更新用户词权重（不存在返回 false，不创建）
    pub fn update_user_word_weight(
        &self,
        schema: &str,
        code: &str,
        text: &str,
        new_weight: i32,
    ) -> anyhow::Result<bool> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let updated;
            {
                let mut t = txn.open_table(USER_WORDS)?;
                let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                match existing {
                    Some((_, c, ca)) => {
                        t.insert(key.as_str(), enc_val(new_weight, c, ca).as_slice())?;
                        updated = true;
                    }
                    None => updated = false,
                }
            }
            txn.commit()?;
            Ok(updated)
        })
    }

    /// 选词回调：count++，每 count_threshold 次给权重 +boost_delta；不存在则创建（weight=0）。
    /// 注：用户词的"调频"为权重微调；候选"用过上浮"由独立的用户词频系统负责（frequency.md）。
    pub fn on_word_selected(
        &self,
        schema: &str,
        code: &str,
        text: &str,
        boost_delta: i32,
        count_threshold: u32,
    ) -> anyhow::Result<()> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(USER_WORDS)?;
                let (w, c, ca) = t
                    .get(key.as_str())?
                    .and_then(|g| dec_val(g.value()))
                    .unwrap_or((0, 0, now_secs()));
                let nc = c.saturating_add(1);
                let nw = if count_threshold > 0 && nc % count_threshold == 0 {
                    w.saturating_add(boost_delta)
                } else {
                    w
                };
                t.insert(key.as_str(), enc_val(nw, nc, ca).as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

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
    fn test_add_get_user_word() {
        let path = tmp("wind_uw_addget.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 100).unwrap();
        s.add_user_word("wb", "a", "戈", 50).unwrap();
        let mut got = s.get_user_words("wb", "a").unwrap();
        got.sort_by_key(|r| r.text.clone());
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|r| r.text == "工" && r.weight == 100));
        // add 同词更高权重 → 取 max
        s.add_user_word("wb", "a", "工", 200).unwrap();
        let g = s.get_user_words("wb", "a").unwrap();
        assert_eq!(g.iter().find(|r| r.text == "工").unwrap().weight, 200);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_prefix_remove_update() {
        let path = tmp("wind_uw_prefix.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "ab", "阿", 10).unwrap();
        s.add_user_word("wb", "abc", "啊", 20).unwrap();
        s.add_user_word("wb", "x", "西", 30).unwrap();
        // 前缀 "ab" 命中 ab/abc，不含 x
        let pre = s.search_user_words_prefix("wb", "ab", 0).unwrap();
        assert_eq!(pre.len(), 2);
        assert!(pre.iter().all(|r| r.code.starts_with("ab")));
        // 更新权重
        assert!(s.update_user_word_weight("wb", "ab", "阿", 99).unwrap());
        assert!(!s.update_user_word_weight("wb", "ab", "缺", 1).unwrap());
        assert_eq!(s.get_user_words("wb", "ab").unwrap()[0].weight, 99);
        // 删除
        s.remove_user_word("wb", "ab", "阿").unwrap();
        assert!(s.get_user_words("wb", "ab").unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_on_word_selected_threshold_boost() {
        let path = tmp("wind_uw_sel.redb");
        let s = Store::open(&path).unwrap();
        // 阈值 3：第 3 次选词才 +boost
        for _ in 0..2 {
            s.on_word_selected("wb", "a", "工", 500, 3).unwrap();
        }
        assert_eq!(
            s.get_user_words("wb", "a").unwrap()[0].weight,
            0,
            "未到阈值不加权"
        );
        s.on_word_selected("wb", "a", "工", 500, 3).unwrap();
        let r = s.get_user_words("wb", "a").unwrap();
        assert_eq!(r[0].count, 3);
        assert_eq!(r[0].weight, 500, "第 3 次达阈值 +500");
        let _ = std::fs::remove_file(&path);
    }

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
            &[crate::wdict::WordIo {
                code: "a".into(),
                text: "工".into(),
                weight: 30,
            }],
            "2026-07-11T00:00:00+08:00",
        );
        let (imported, _) = s.import_user_words_wdict("wb", &text).unwrap();
        assert_eq!(imported, 1);
        assert_eq!(
            s.get_user_words("wb", "a").unwrap()[0].weight,
            100,
            "Merge 取 max"
        );
        let _ = std::fs::remove_file(&path);
    }
}
