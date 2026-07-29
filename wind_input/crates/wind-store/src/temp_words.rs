//! 临时词存储（redb）—— 自动学习的临时词库
//!
//! 与 Go 版本 `wind_input/internal/store/temp_words.go` 对齐。复用 user_words 的 key/value 编码。
//! 权重上限 `TEMP_WORD_MAX_WEIGHT=10000`。淘汰（evict）在**单写事务**内完成（修 Go 的 view→update
//! TOCTOU，store.md §7.4）。晋升条件（count≥promote_count）由调用方（dict StoreTempLayer）判定。
//!
//! 词频重构后 count 与权重解耦：count 只用于晋升判定（不再随复选驱动权重增长），
//! 临时词权重固定为写入时的初值；晋升进用户词库时统一取 `PROMOTED_WEIGHT`（与已有
//! 用户词取 max，不覆盖手动加词的更高权重）。

use crate::store::{Store, TEMP_WORDS, USER_WORDS};
use crate::user_words::{UserWordRecord, dec_val, enc_key, enc_val, now_secs};
use crate::wdict;
use redb::ReadableTable;

/// 临时词权重硬上限（仅约束写入时的初值，权重不再随复选累加）
pub const TEMP_WORD_MAX_WEIGHT: i32 = 10000;

/// 自动学习词晋升入用户词库时的统一权重（与已存在的用户词权重取 max，不覆盖手动加词）
pub const PROMOTED_WEIGHT: i32 = 1000;

impl Store {
    /// 学习临时词：新词 weight=min(add_weight,MAX)/count=1；已存在只 count++，权重不变
    /// （count 只用于晋升判定，不再驱动权重增长）。返回新的 count（调用方据此与
    /// promote_count 比较决定是否晋升）。
    ///
    /// `boundary`：该 code 的音节边界（见 `wind_dict::binformat::DictEntry::boundary`），
    /// 0=无信息。已存在的记录**沿用旧 boundary**——同 (schema,code,text) 的切分是确定的，
    /// 不因再次学习而变；且旧值可能来自更可靠的来源。
    pub fn learn_temp_word(
        &self,
        schema: &str,
        code: &str,
        text: &str,
        add_weight: i32,
        boundary: u64,
    ) -> anyhow::Result<u32> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let new_count;
            {
                let mut t = txn.open_table(TEMP_WORDS)?;
                let (w, c, ca, b) = match t.get(key.as_str())?.and_then(|g| dec_val(g.value())) {
                    // 旧记录 boundary 为 0（v1 遗留）时用新算出的补上，否则沿用。
                    Some((ow, oc, oca, ob)) => {
                        (ow, oc + 1, oca, if ob != 0 { ob } else { boundary })
                    }
                    None => (
                        add_weight.min(TEMP_WORD_MAX_WEIGHT),
                        1,
                        now_secs(),
                        boundary,
                    ),
                };
                new_count = c;
                t.insert(key.as_str(), enc_val(w, c, ca, b).as_slice())?;
            }
            txn.commit()?;
            Ok(new_count)
        })
    }

    /// 点查临时词当前累积 count（无记录返回 None）。供协调器选词路径判断晋升。
    pub fn get_temp_word(
        &self,
        schema: &str,
        code: &str,
        text: &str,
    ) -> anyhow::Result<Option<u32>> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(TEMP_WORDS)?;
            Ok(t.get(key.as_str())?
                .and_then(|g| dec_val(g.value()))
                .map(|(_, c, _, _)| c))
        })
    }

    /// 仅当已存在时计数 +1（不创建新条目、权重不变）。返回 (是否存在, 新count)。
    pub fn increment_temp_if_exists(
        &self,
        schema: &str,
        code: &str,
        text: &str,
    ) -> anyhow::Result<(bool, u32)> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let result;
            {
                let mut t = txn.open_table(TEMP_WORDS)?;
                let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                match existing {
                    Some((w, c, ca, b)) => {
                        let nc = c + 1;
                        t.insert(key.as_str(), enc_val(w, nc, ca, b).as_slice())?;
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
                if let Some((w, c, ca, b)) = dec_val(v.value()) {
                    out.push(UserWordRecord {
                        code: code.to_string(),
                        text: text.to_string(),
                        weight: w,
                        count: c,
                        created_at: ca,
                        boundary: b,
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
                if let (Some((_, code, text)), Some((w, c, ca, b))) =
                    (crate::user_words::split_key(key), dec_val(v.value()))
                {
                    out.push(UserWordRecord {
                        code: code.to_string(),
                        text: text.to_string(),
                        weight: w,
                        count: c,
                        created_at: ca,
                        boundary: b,
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
                    let w = dec_val(v.value()).map(|(w, _, _, _)| w).unwrap_or(0);
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

    /// 导出某方案全部临时词为 wdict 文本（code/text/weight；count 属晋升进度不流转）。
    pub fn export_temp_words_wdict(
        &self,
        schema: &str,
        exported_at: &str,
    ) -> anyhow::Result<String> {
        let recs = self.search_temp_words_prefix(schema, "", 0)?;
        // code 列输出带空格的音节码，边界随之流出（同 collect_user_word_rows）。
        let rows: Vec<wdict::WordIo> = recs
            .into_iter()
            .map(|r| wdict::WordIo {
                code: wdict::join_code_by_boundary(&r.code, r.boundary),
                text: r.text,
                weight: r.weight,
                count: 0,
            })
            .collect();
        Ok(wdict::export_words_wdict(&rows, exported_at))
    }

    /// 从 wdict 文本导入临时词（Merge：learn_temp_word，已存在 count++）。返回 (imported, skipped)。
    pub fn import_temp_words_wdict(
        &self,
        schema: &str,
        text: &str,
    ) -> anyhow::Result<(usize, usize)> {
        let (rows, skipped) = wdict::parse_words_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
        for r in &rows {
            // code 列可能是带空格的音节码 → 拆成扁平码 + 边界；无空格则 boundary=0。
            let (code, b) = wdict::split_spaced_code(&r.code);
            self.learn_temp_word(schema, &code, &r.text, r.weight, b)?;
        }
        Ok((rows.len(), skipped))
    }

    /// 从 wdict `WordIo` 行导入临时词，**保留 count 晋升进度**（Merge：weight 取 max、count 取 max；
    /// 权重仍受 `TEMP_WORD_MAX_WEIGHT` 约束）。返回导入条数。
    /// 与 `import_temp_words_wdict`（走 learn_temp_word，count 归 1）不同，本方法用于多段词库导入以保真 count。
    pub fn import_temp_word_rows(
        &self,
        schema: &str,
        rows: &[wdict::WordIo],
    ) -> anyhow::Result<usize> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(TEMP_WORDS)?;
                for r in rows {
                    // code 列可能是带空格的音节码 → 拆成扁平 key + 边界。
                    let (code, in_b) = wdict::split_spaced_code(&r.code);
                    let key = enc_key(schema, &code, &r.text);
                    let cap = r.weight.min(TEMP_WORD_MAX_WEIGHT);
                    // 已存在且旧 boundary 非 0 则沿用（切分不因导入而变），为 0 时用导入行补齐。
                    let (w, c, ca, b) = match t.get(key.as_str())?.and_then(|g| dec_val(g.value()))
                    {
                        Some((ow, oc, oca, ob)) => (
                            ow.max(cap),
                            oc.max(r.count),
                            oca,
                            if ob != 0 { ob } else { in_b },
                        ),
                        None => (cap, r.count, now_secs(), in_b),
                    };
                    t.insert(key.as_str(), enc_val(w, c, ca, b).as_slice())?;
                }
            }
            txn.commit()?;
            Ok(rows.len())
        })
    }

    /// 清空某方案全部临时词（单写事务），返回删除条数。
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

    /// 晋升：把临时词移入用户词库，并从临时库删除。返回是否发生晋升。
    /// 权重统一取 `PROMOTED_WEIGHT`（与已存在的用户词权重取 max，不覆盖手动加词的更高权重，
    /// 不再沿用临时词自身权重）；count=temp+user，created_at 优先保留 user 旧值。
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
                    Some((_tw, tc, tca, tb)) => {
                        {
                            let mut user_t = txn.open_table(USER_WORDS)?;
                            // boundary 随词一起晋升：临时词由造词算得（有边界），用户词侧若已有
                            // 非 0 值则沿用（同 code/text 的切分确定，且旧值来源未必更差）。
                            let (nw, nc, nca, nb) =
                                match user_t.get(key.as_str())?.and_then(|g| dec_val(g.value())) {
                                    Some((uw, uc, uca, ub)) => (
                                        uw.max(PROMOTED_WEIGHT),
                                        tc + uc,
                                        uca,
                                        if ub != 0 { ub } else { tb },
                                    ),
                                    None => (PROMOTED_WEIGHT, tc, tca, tb),
                                };
                            user_t.insert(key.as_str(), enc_val(nw, nc, nca, nb).as_slice())?;
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
    fn temp_words_wdict_roundtrip_and_clear() {
        let path = tmp("wind_tw_io.redb");
        let s = Store::open(&path).unwrap();
        s.learn_temp_word("wb", "ab", "临时", 50, 0).unwrap();
        s.learn_temp_word("py", "ni", "你", 10, 0).unwrap();
        let text = s.export_temp_words_wdict("wb", "t").unwrap();
        assert!(text.contains("--- !words"));

        let path2 = tmp("wind_tw_io2.redb");
        let s2 = Store::open(&path2).unwrap();
        let (imported, skipped) = s2.import_temp_words_wdict("wb", &text).unwrap();
        assert_eq!((imported, skipped), (1, 0));
        assert_eq!(s2.get_temp_word("wb", "ab", "临时").unwrap(), Some(1));

        assert_eq!(s.clear_temp_words("wb").unwrap(), 1);
        assert!(s.search_temp_words_prefix("wb", "", 0).unwrap().is_empty());
        assert_eq!(
            s.search_temp_words_prefix("py", "", 0).unwrap().len(),
            1,
            "其它 schema 不受影响"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn learn_count_reaches_threshold_then_promote() {
        let path = tmp("wind_tw_promote_thresh.redb");
        let s = Store::open(&path).unwrap();
        for i in 1..=3u32 {
            let n = s.learn_temp_word("wubi86", "abcd", "测试", 100, 0).unwrap();
            assert_eq!(n, i);
        }
        assert_eq!(
            s.get_temp_word("wubi86", "abcd", "测试").unwrap(),
            Some(3),
            "3 次学习后 count 应为 3"
        );
        assert!(s.promote_temp_word("wubi86", "abcd", "测试").unwrap());
        assert_eq!(
            s.get_temp_word("wubi86", "abcd", "测试").unwrap(),
            None,
            "晋升后临时层应删除"
        );
        // 不存在的词返回 None
        assert_eq!(s.get_temp_word("wubi86", "xxxx", "无").unwrap(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_learn_count_only_weight_unchanged() {
        let path = tmp("wind_tw_learn.redb");
        let s = Store::open(&path).unwrap();
        assert_eq!(s.learn_temp_word("wb", "a", "工", 800, 0).unwrap(), 1);
        assert_eq!(s.learn_temp_word("wb", "a", "工", 800, 0).unwrap(), 2);
        let r = s.get_temp_words("wb", "a").unwrap();
        assert_eq!(r[0].count, 2);
        assert_eq!(r[0].weight, 800, "权重不再随复选累加，保持写入初值");
        // 初值上限
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
            s.increment_temp_if_exists("wb", "a", "工").unwrap(),
            (false, 0)
        );
        s.learn_temp_word("wb", "a", "工", 100, 0).unwrap();
        assert_eq!(
            s.increment_temp_if_exists("wb", "a", "工").unwrap(),
            (true, 2)
        );
        assert_eq!(
            s.get_temp_words("wb", "a").unwrap()[0].weight,
            100,
            "计数不再驱动权重变化"
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
    fn test_promote_uses_fixed_weight() {
        let path = tmp("wind_tw_promote.redb");
        let s = Store::open(&path).unwrap();
        s.learn_temp_word("wb", "a", "工", 800, 0).unwrap();
        s.learn_temp_word("wb", "a", "工", 800, 0).unwrap(); // count=2
        assert!(s.promote_temp_word("wb", "a", "工").unwrap());
        // 临时库已删
        assert!(s.get_temp_words("wb", "a").unwrap().is_empty());
        // 晋升权重统一取 PROMOTED_WEIGHT（不沿用临时权重），count 累计保留
        let u = s.get_user_words("wb", "a").unwrap();
        assert_eq!(u[0].weight, PROMOTED_WEIGHT);
        assert_eq!(u[0].count, 2);
        // 已存在更高权重的手动加词：晋升不应下调，取 max
        s.add_user_word("wb", "b", "戈", 1200, 0).unwrap();
        s.learn_temp_word("wb", "b", "戈", 800, 0).unwrap();
        assert!(s.promote_temp_word("wb", "b", "戈").unwrap());
        assert_eq!(s.get_user_words("wb", "b").unwrap()[0].weight, 1200);
        // 不存在的临时词晋升返回 false
        assert!(!s.promote_temp_word("wb", "z", "无").unwrap());
        let _ = std::fs::remove_file(&path);
    }
}
