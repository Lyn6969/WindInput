//! 临时词存储（redb）—— 自动学习的临时词库
//!
//! 与 Go 版本 `wind_input/internal/store/temp_words.go` 对齐。复用 user_words 的 key/value 编码。
//! 权重上限 `TEMP_WORD_MAX_WEIGHT=10000`。淘汰（evict）在**单写事务**内完成（修 Go 的 view→update
//! TOCTOU，store.md §7.4）。晋升条件（count≥promote_count）由调用方（dict StoreTempLayer）判定。

use crate::store::{Store, TEMP_WORDS, USER_WORDS};
use crate::user_words::{UserWordRecord, dec_val, enc_key, enc_val, now_secs};
use redb::ReadableTable;

/// 临时词动态权重硬上限
pub const TEMP_WORD_MAX_WEIGHT: i32 = 10000;

impl Store {
    /// 学习临时词：新词 weight=min(add_weight,MAX)/count=1；已存在 weight=min(old+delta,MAX)/count++。
    /// 返回新的 count（调用方据此与 promote_count 比较决定是否晋升）。
    pub fn learn_temp_word(
        &self,
        schema: &str,
        code: &str,
        text: &str,
        add_weight: i32,
        weight_delta: i32,
    ) -> anyhow::Result<u32> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let new_count;
            {
                let mut t = txn.open_table(TEMP_WORDS)?;
                let (w, c, ca) = match t.get(key.as_str())?.and_then(|g| dec_val(g.value())) {
                    Some((ow, oc, oca)) => (
                        ow.saturating_add(weight_delta).min(TEMP_WORD_MAX_WEIGHT),
                        oc + 1,
                        oca,
                    ),
                    None => (add_weight.min(TEMP_WORD_MAX_WEIGHT), 1, now_secs()),
                };
                new_count = c;
                t.insert(key.as_str(), enc_val(w, c, ca).as_slice())?;
            }
            txn.commit()?;
            Ok(new_count)
        })
    }

    /// 仅当已存在时计数 +1、权重 +delta（不创建新条目）。返回 (是否存在, 新count)。
    pub fn increment_temp_if_exists(
        &self,
        schema: &str,
        code: &str,
        text: &str,
        weight_delta: i32,
    ) -> anyhow::Result<(bool, u32)> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let result;
            {
                let mut t = txn.open_table(TEMP_WORDS)?;
                let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                match existing {
                    Some((w, c, ca)) => {
                        let nc = c + 1;
                        let nw = w.saturating_add(weight_delta).min(TEMP_WORD_MAX_WEIGHT);
                        t.insert(key.as_str(), enc_val(nw, nc, ca).as_slice())?;
                        result = (true, nc);
                    }
                    None => result = (false, 0),
                }
            }
            txn.commit()?;
            Ok(result)
        })
    }

    /// 精确取某 code 下的所有临时词
    pub fn get_temp_words(&self, schema: &str, code: &str) -> anyhow::Result<Vec<UserWordRecord>> {
        let prefix = format!("{schema}\u{0}{code}\u{0}");
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(TEMP_WORDS)?;
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

    /// 前缀检索临时词（跨 code）。limit<=0 不限。
    pub fn search_temp_words_prefix(
        &self,
        schema: &str,
        prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<UserWordRecord>> {
        let scan = format!("{schema}\u{0}{prefix}");
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(TEMP_WORDS)?;
            let mut out = Vec::new();
            for item in t.range(scan.as_str()..)? {
                let (k, v) = item?;
                let key = k.value();
                if !key.starts_with(&scan) {
                    break;
                }
                if let (Some((_, code, text)), Some((w, c, ca))) =
                    (crate::user_words::split_key(key), dec_val(v.value()))
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

    /// 淘汰：保留权重最高的 max_keep 条，删除其余（按 weight 升序淘汰最低的）。
    /// 单写事务完成（先在事务内收集快照再删除，无 TOCTOU）。返回淘汰条数。
    pub fn evict_temp_words(&self, schema: &str, max_keep: usize) -> anyhow::Result<usize> {
        let scan = format!("{schema}\u{0}");
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let mut deleted = 0usize;
            {
                let mut t = txn.open_table(TEMP_WORDS)?;
                // 1) 事务内收集本方案全部 (key, weight)
                let mut all: Vec<(String, i32)> = Vec::new();
                for item in t.range(scan.as_str()..)? {
                    let (k, v) = item?;
                    let key = k.value();
                    if !key.starts_with(&scan) {
                        break;
                    }
                    let w = dec_val(v.value()).map(|(w, _, _)| w).unwrap_or(0);
                    all.push((key.to_string(), w));
                }
                // 2) 超出 max_keep 则删除权重最低的若干条
                if all.len() > max_keep {
                    all.sort_by_key(|(_, w)| *w); // 升序：最低在前
                    let to_delete = all.len() - max_keep;
                    for (key, _) in all.iter().take(to_delete) {
                        t.remove(key.as_str())?;
                        deleted += 1;
                    }
                }
            }
            txn.commit()?;
            Ok(deleted)
        })
    }

    /// 删除临时词（不存在静默成功）。
    pub fn remove_temp_word(&self, schema: &str, code: &str, text: &str) -> anyhow::Result<()> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(TEMP_WORDS)?;
                t.remove(key.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 晋升：把临时词移入用户词库（合并权重/计数），并从临时库删除。返回是否发生晋升。
    /// 合并：weight=min(temp+user,MAX)，count=temp+user，created_at 优先保留 user 旧值。
    pub fn promote_temp_word(&self, schema: &str, code: &str, text: &str) -> anyhow::Result<bool> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let promoted;
            {
                let mut temp_t = txn.open_table(TEMP_WORDS)?;
                let temp = temp_t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                match temp {
                    None => promoted = false,
                    Some((tw, tc, tca)) => {
                        {
                            let mut user_t = txn.open_table(USER_WORDS)?;
                            let (nw, nc, nca) =
                                match user_t.get(key.as_str())?.and_then(|g| dec_val(g.value())) {
                                    Some((uw, uc, uca)) => (
                                        tw.saturating_add(uw).min(TEMP_WORD_MAX_WEIGHT),
                                        tc + uc,
                                        uca,
                                    ),
                                    None => (tw.min(TEMP_WORD_MAX_WEIGHT), tc, tca),
                                };
                            user_t.insert(key.as_str(), enc_val(nw, nc, nca).as_slice())?;
                        }
                        temp_t.remove(key.as_str())?;
                        promoted = true;
                    }
                }
            }
            txn.commit()?;
            Ok(promoted)
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
    fn test_learn_and_cap() {
        let path = tmp("wind_tw_learn.redb");
        let s = Store::open(&path).unwrap();
        assert_eq!(s.learn_temp_word("wb", "a", "工", 800, 40).unwrap(), 1);
        assert_eq!(s.learn_temp_word("wb", "a", "工", 800, 40).unwrap(), 2);
        let r = s.get_temp_words("wb", "a").unwrap();
        assert_eq!(r[0].count, 2);
        assert_eq!(r[0].weight, 840, "800 + 40");
        // 权重上限
        let _ = s.learn_temp_word("wb", "b", "戈", 99999, 0).unwrap();
        assert_eq!(
            s.get_temp_words("wb", "b").unwrap()[0].weight,
            TEMP_WORD_MAX_WEIGHT
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_increment_if_exists() {
        let path = tmp("wind_tw_inc.redb");
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.increment_temp_if_exists("wb", "a", "工", 10).unwrap(),
            (false, 0)
        );
        s.learn_temp_word("wb", "a", "工", 100, 10).unwrap();
        assert_eq!(
            s.increment_temp_if_exists("wb", "a", "工", 10).unwrap(),
            (true, 2)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_evict_lowest_weight() {
        let path = tmp("wind_tw_evict.redb");
        let s = Store::open(&path).unwrap();
        s.learn_temp_word("wb", "a", "低", 10, 0).unwrap();
        s.learn_temp_word("wb", "b", "中", 50, 0).unwrap();
        s.learn_temp_word("wb", "c", "高", 90, 0).unwrap();
        // 保留 2 → 删除权重最低的 1 条（"低"）
        assert_eq!(s.evict_temp_words("wb", 2).unwrap(), 1);
        assert!(s.get_temp_words("wb", "a").unwrap().is_empty());
        assert!(!s.get_temp_words("wb", "c").unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_promote_merges_into_user() {
        let path = tmp("wind_tw_promote.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 100).unwrap();
        s.learn_temp_word("wb", "a", "工", 800, 0).unwrap();
        s.learn_temp_word("wb", "a", "工", 800, 50).unwrap(); // count=2, weight=850
        assert!(s.promote_temp_word("wb", "a", "工").unwrap());
        // 临时库已删
        assert!(s.get_temp_words("wb", "a").unwrap().is_empty());
        // 用户库合并：weight=min(850+100,1e4)=950, count=2+0=2
        let u = s.get_user_words("wb", "a").unwrap();
        assert_eq!(u[0].weight, 950);
        assert_eq!(u[0].count, 2);
        // 不存在的临时词晋升返回 false
        assert!(!s.promote_temp_word("wb", "z", "无").unwrap());
        let _ = std::fs::remove_file(&path);
    }
}
