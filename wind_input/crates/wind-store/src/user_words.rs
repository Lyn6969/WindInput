//! 用户词存储（redb）
//!
//! 与 Go 版本 `wind_input/internal/store/user_words.go` 对齐，但：
//! - value 用定长 16 字节（weight i32 + count u32 + created_at i64），text/code 存于 key，比 Go 的 JSON 紧凑（store.md §7.3）。
//! - created_at 统一为 i64 unix 秒（修 Go user=秒/temp=毫秒 不一致，store.md §7.2）。
//!
//! key 编码：`"{schema}\0{code}\0{text}"`（store.md §2）。

use crate::abbrev_index;
use crate::store::{Store, USER_ABBREV, USER_WORDS};
use crate::wdict;
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// 用户词记录（code/text 来自 key，weight/count/created_at/boundary 来自定长 value）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserWordRecord {
    pub code: String,
    pub text: String,
    pub weight: i32,
    pub count: u32,
    /// 创建时间（unix 秒）
    pub created_at: i64,
    /// `code` 的音节边界（见 `wind_dict::binformat::DictEntry::boundary`）；0=无信息。
    /// 造词路径（generate_word_pinyin）算得；手输码/wdict 文本导入无从得知，为 0。
    /// `serde(default)`：v1 记录与旧客户端 JSON 无此字段，按 0 处理。
    #[serde(default)]
    pub boundary: u64,
}

/// 批量导入的分类计数(P2:added=新键 / updated=权重严格更大 / unchanged=权重≤现有不落盘)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WordsImportCounts {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
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

/// value: 定长 24 字节 —— `weight i32 | count u32 | created_at i64 | boundary u64`
///
/// v1 为 16 字节（无 boundary）。**惰性升级、无需 migration**：`dec_val` 按实际长度取值，
/// 旧的 16B 记录读出 boundary=0（无边界信息，消费方降级回 DAG），下次写入时自然补齐为 24B。
pub(crate) fn enc_val(weight: i32, count: u32, created_at: i64, boundary: u64) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[0..4].copy_from_slice(&weight.to_le_bytes());
    b[4..8].copy_from_slice(&count.to_le_bytes());
    b[8..16].copy_from_slice(&created_at.to_le_bytes());
    b[16..24].copy_from_slice(&boundary.to_le_bytes());
    b
}

/// 解码 value → (weight, count, created_at, boundary)
///
/// 长度守卫刻意宽松（`< 16` 而非 `!= 24`）：旧 16B 记录仍能解出前三项，boundary 取 0。
/// 直接切 `b[16..24]` 会在旧记录上越界，故必须按长度分支。
pub(crate) fn dec_val(b: &[u8]) -> Option<(i32, u32, i64, u64)> {
    if b.len() < 16 {
        return None;
    }
    let boundary = if b.len() >= 24 {
        u64::from_le_bytes(b[16..24].try_into().ok()?)
    } else {
        0 // v1 遗留记录：无边界信息
    };
    Some((
        i32::from_le_bytes(b[0..4].try_into().ok()?),
        u32::from_le_bytes(b[4..8].try_into().ok()?),
        i64::from_le_bytes(b[8..16].try_into().ok()?),
        boundary,
    ))
}

impl Store {
    /// 新增/合并用户词：已存在则权重取 max、保留原 created_at；新词记 created_at=now。
    /// 用户词**无权重上限**（store.md §3）。
    ///
    /// `boundary`：该 code 的音节边界（见 `wind_dict::binformat::DictEntry::boundary`）。
    /// 造词路径（`generate_word_pinyin`）算得；用户手输码/wdict 导入无从得知，传 0
    /// （消费方降级回 DAG）。已存在且旧值非 0 时沿用旧值——同 (schema,code,text) 的切分是
    /// 确定的，不因再次加词而变。
    pub fn add_user_word(
        &self,
        schema: &str,
        code: &str,
        text: &str,
        weight: i32,
        boundary: u64,
    ) -> anyhow::Result<()> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(USER_WORDS)?;
                let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                let (w, c, ca, b) = match existing {
                    Some((ow, oc, oca, ob)) => {
                        (ow.max(weight), oc, oca, if ob != 0 { ob } else { boundary })
                    }
                    None => (weight, 0, now_secs(), boundary),
                };
                // 边界可能从 0 被补齐（见上），索引键随之改变 → shift 负责删旧建新。
                let old_b = existing.map(|(_, _, _, ob)| ob);
                let mut idx = txn.open_table(USER_ABBREV)?;
                abbrev_index::shift(&mut idx, schema, code, text, old_b, b)?;
                t.insert(key.as_str(), enc_val(w, c, ca, b).as_slice())?;
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
                if let (Some((_, code, text)), Some((w, c, ca, b))) =
                    (split_key(key), dec_val(v.value()))
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

    /// 删除用户词（不存在静默成功）
    pub fn remove_user_word(&self, schema: &str, code: &str, text: &str) -> anyhow::Result<()> {
        let key = enc_key(schema, code, text);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(USER_WORDS)?;
                // 先读边界才能算出索引键——删主表之后就查不到了，顺序不可调换。
                let b = t
                    .get(key.as_str())?
                    .and_then(|g| dec_val(g.value()))
                    .map(|(_, _, _, b)| b);
                t.remove(key.as_str())?;
                if let Some(b) = b {
                    abbrev_index::remove(&mut txn.open_table(USER_ABBREV)?, schema, code, text, b)?;
                }
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
                    // 仅改权重：boundary 沿用（切分与权重无关）。
                    Some((_, c, ca, b)) => {
                        t.insert(key.as_str(), enc_val(new_weight, c, ca, b).as_slice())?;
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
                // 不存在则创建 weight=0 记录（隐性造词路径）：此处只有扁平 code，无边界可算 → 0。
                let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                let is_new = existing.is_none();
                let (w, c, ca, b) = existing.unwrap_or((0, 0, now_secs(), 0));
                let nc = c.saturating_add(1);
                let nw = if count_threshold > 0 && nc % count_threshold == 0 {
                    w.saturating_add(boost_delta)
                } else {
                    w
                };
                t.insert(key.as_str(), enc_val(nw, nc, ca, b).as_slice())?;
                // ⚠️ **本路径会凭空造出用户词**（上面那句注释说的「隐性造词」），故必须建索引。
                // 改权重不用动索引（value 空），但新增必须——漏了这一处，靠选词自动产生的
                // 词就永远进不了简拼索引，且只在「用过一段时间后」才显形。
                if is_new {
                    abbrev_index::insert(&mut txn.open_table(USER_ABBREV)?, schema, code, text, b)?;
                }
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 清空某 schema 的全部用户词(单写事务),返回删除条数。
    pub fn clear_user_words(&self, schema: &str) -> anyhow::Result<usize> {
        let prefix = format!("{schema}\u{0}");
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let n;
            {
                let mut t = txn.open_table(USER_WORDS)?;
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
            abbrev_index::clear_schema(&mut txn.open_table(USER_ABBREV)?, schema)?;
            txn.commit()?;
            Ok(n)
        })
    }

    /// 批量导入用户词(单写事务,Merge 语义与 add_user_word 一致):
    /// 新键 → added(count=0, created_at=now);导入权重 > 现有 → updated(保留 count/created_at);
    /// 否则 → unchanged(不写)。dry-run 见 preview_import_user_words,两者分类必须一致。
    pub fn import_user_words(
        &self,
        schema: &str,
        rows: &[wdict::WordIo],
    ) -> anyhow::Result<WordsImportCounts> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let mut c = WordsImportCounts::default();
            {
                let mut t = txn.open_table(USER_WORDS)?;
                let mut idx = txn.open_table(USER_ABBREV)?;
                for r in rows {
                    // code 列可能是带空格的音节码（`ni hao`）→ 拆成扁平 key + 边界。
                    // 无空格（五笔码/旧版导出）→ boundary=0，与改动前等价。
                    let (code, in_b) = wdict::split_spaced_code(&r.code);
                    let key = enc_key(schema, &code, &r.text);
                    let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                    match existing {
                        None => {
                            t.insert(
                                key.as_str(),
                                enc_val(r.weight, r.count, now_secs(), in_b).as_slice(),
                            )?;
                            abbrev_index::insert(&mut idx, schema, &code, &r.text, in_b)?;
                            c.added += 1;
                        }
                        Some((w, cnt, ca, b)) => {
                            // weight/count 各取 max；boundary 旧值非 0 则沿用（同
                            // `add_user_word`：同 (schema,code,text) 的切分是确定的，
                            // 不因再次导入而变），旧值为 0 时用导入行补齐。
                            // 三者任一变化即写盘为 updated，否则 unchanged。
                            let nw = w.max(r.weight);
                            let nc = cnt.max(r.count);
                            let nb = if b != 0 { b } else { in_b };
                            if nw != w || nc != cnt || nb != b {
                                t.insert(key.as_str(), enc_val(nw, nc, ca, nb).as_slice())?;
                                // 边界被补齐时索引键随之改变 → shift 删旧建新。
                                if nb != b {
                                    abbrev_index::shift(
                                        &mut idx,
                                        schema,
                                        &code,
                                        &r.text,
                                        Some(b),
                                        nb,
                                    )?;
                                }
                                c.updated += 1;
                            } else {
                                c.unchanged += 1;
                            }
                        }
                    }
                }
            }
            txn.commit()?;
            Ok(c)
        })
    }

    /// 导入 dry-run(只读):分类规则与 import_user_words 完全一致;
    /// samples 取前 5 个会落盘行(added/updated)的 "code text"。
    pub fn preview_import_user_words(
        &self,
        schema: &str,
        rows: &[wdict::WordIo],
    ) -> anyhow::Result<(WordsImportCounts, Vec<String>)> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(USER_WORDS)?;
            let mut c = WordsImportCounts::default();
            let mut samples = Vec::new();
            for r in rows {
                // 与 import_user_words 同款拆分：key 必须用扁平码，否则带空格的行
                // 一律查不到既有记录、全部误报为 added。
                let (code, in_b) = wdict::split_spaced_code(&r.code);
                let key = enc_key(schema, &code, &r.text);
                let existing = t.get(key.as_str())?.and_then(|g| dec_val(g.value()));
                let will_write = match existing {
                    None => {
                        c.added += 1;
                        true
                    }
                    // 判据须与 import_user_words 逐项对齐，含 boundary 补齐那一项
                    // （旧值为 0 且导入行给得出边界 → 会落盘 → 算 updated）。
                    Some((w, cnt, _, b))
                        if r.weight > w || r.count > cnt || (b == 0 && in_b != 0) =>
                    {
                        c.updated += 1;
                        true
                    }
                    Some(_) => {
                        c.unchanged += 1;
                        false
                    }
                };
                if will_write && samples.len() < 5 {
                    samples.push(format!("{} {}", r.code, r.text));
                }
            }
            Ok((c, samples))
        })
    }

    /// 导出某方案的全部用户词为 wdict 文本(仅 code/text/weight,不含个人 count/created_at)。
    pub fn export_user_words_wdict(
        &self,
        schema: &str,
        exported_at: &str,
    ) -> anyhow::Result<String> {
        let rows = self.collect_user_word_rows(schema)?;
        Ok(wdict::export_words_wdict(&rows, exported_at))
    }

    /// 收集某方案全部用户词为 wdict WordIo 行(code/text/weight/count)。
    ///
    /// code 列输出**带空格的音节码**（`ni hao`），边界随之流出——此前导出的是扁平码，
    /// 边界在文本里无处安放，于是「导出→清空→导入」一轮就把 boundary 全清零
    /// （备份还原正是这条路径）。见 [`wdict::join_code_by_boundary`]。
    pub(crate) fn collect_user_word_rows(
        &self,
        schema: &str,
    ) -> anyhow::Result<Vec<wdict::WordIo>> {
        let recs = self.search_user_words_prefix(schema, "", 0)?;
        Ok(recs
            .into_iter()
            .map(|r| wdict::WordIo {
                code: wdict::join_code_by_boundary(&r.code, r.boundary),
                text: r.text,
                weight: r.weight,
                count: r.count,
            })
            .collect())
    }

    /// 从 wdict 文本导入用户词到某方案(Merge:max-weight upsert)。
    /// 返回 (imported, skipped)。imported=解析成功的行数(含 unchanged);细分类见 import_user_words。
    pub fn import_user_words_wdict(
        &self,
        schema: &str,
        text: &str,
    ) -> anyhow::Result<(usize, usize)> {
        let (rows, skipped) = wdict::parse_words_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
        self.import_user_words(schema, &rows)?;
        Ok((rows.len(), skipped))
    }

    /// 导出某方案的「用户词 + shadow 规则」为单个 wdict 文本（对齐 Go：一个文件两段）。
    pub fn export_dict_wdict(&self, schema: &str, exported_at: &str) -> anyhow::Result<String> {
        let words = self.collect_user_word_rows(schema)?;
        let shadow = self.export_shadow_actions(schema)?;
        Ok(wdict::export_dict_wdict(&words, &shadow, exported_at))
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

    /// **备份还原不得丢音节边界**（本次改动的验收标准）。
    ///
    /// `backup.rs` 的还原路径是 `clear_user_words` + `import_user_words_wdict`。清空后
    /// 全是新键，而 wdict 此前是扁平四列文本、边界无处安放 ⇒ **一轮备份还原把 boundary
    /// 全部清零**（实测：写入 `[5,21]` → 还原后 `[0,0]`）。现 code 列改写带空格的音节码。
    ///
    /// 反向验证：把 `collect_user_word_rows` 的 `join_code_by_boundary` 换回 `r.code`，
    /// 本测试即变红。
    #[test]
    fn boundary_survives_export_clear_import() {
        let p = tmp("wind_uw_boundary_backup.redb");
        let s = Store::open(&p).unwrap();
        s.add_user_word("pinyin", "nihao", "你好", 500, 0b101)
            .unwrap();
        s.add_user_word("pinyin", "xianning", "西安宁", 800, 0b10101)
            .unwrap();

        let text = s.export_user_words_wdict("pinyin", "2026-07-29").unwrap();
        assert!(
            text.contains("ni hao") && text.contains("xi an ning"),
            "导出文本的 code 列须为带空格的音节码，实际:\n{text}"
        );

        // ── 模拟备份还原 ──
        s.clear_user_words("pinyin").unwrap();
        s.import_user_words_wdict("pinyin", &text).unwrap();

        let recs = s.search_user_words_prefix("pinyin", "", 0).unwrap();
        let mut got: Vec<(String, u64)> =
            recs.iter().map(|r| (r.code.clone(), r.boundary)).collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("nihao".to_string(), 0b101),
                ("xianning".to_string(), 0b10101)
            ],
            "还原后 key 须仍是扁平码、且边界原样存活"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 旧版 wdict（code 列为扁平码，无空格）导入后 boundary=0，不报错、不误判为单音节。
    /// 用户手上已有的备份文件都是这个形态，必须平滑降级。
    #[test]
    fn legacy_flat_wdict_imports_as_unknown_boundary() {
        let p = tmp("wind_uw_legacy_wdict.redb");
        let s = Store::open(&p).unwrap();
        let legacy = "# WindInput 用户数据文件\nwind_dict:\n  version: 1\n  sections:\n    words:\n      columns: [code, text, weight, count]\n\n--- !words\nnihao\t你好\t500\t0\nabcd\t工作\t100\t0\n";
        let (imported, skipped) = s.import_user_words_wdict("pinyin", legacy).unwrap();
        assert_eq!((imported, skipped), (2, 0));
        let recs = s.search_user_words_prefix("pinyin", "", 0).unwrap();
        assert!(
            recs.iter().all(|r| r.boundary == 0),
            "无空格的码一律按「无边界信息」处理"
        );
        assert!(recs.iter().any(|r| r.code == "nihao"));
        let _ = std::fs::remove_file(&p);
    }

    /// 导入行带边界、库中旧记录 boundary=0（v1 遗留或历史扁平导入）→ 补齐而非忽略，
    /// 且该行计入 updated；`preview_import_user_words` 的分类必须给出同样答案。
    #[test]
    fn import_fills_missing_boundary_and_preview_agrees() {
        let p = tmp("wind_uw_fill_boundary.redb");
        let s = Store::open(&p).unwrap();
        s.add_user_word("pinyin", "nihao", "你好", 500, 0).unwrap(); // 旧记录无边界

        let rows = vec![wdict::WordIo {
            code: "ni hao".into(),
            text: "你好".into(),
            weight: 500, // weight/count 均不变，只有 boundary 可补
            count: 0,
        }];
        let (pc, _) = s.preview_import_user_words("pinyin", &rows).unwrap();
        let ic = s.import_user_words("pinyin", &rows).unwrap();
        assert_eq!(
            (pc.added, pc.updated, pc.unchanged),
            (0, 1, 0),
            "dry-run 须把「仅补边界」也算作会落盘的 updated"
        );
        assert_eq!(
            (ic.added, ic.updated, ic.unchanged),
            (0, 1, 0),
            "实际导入的分类须与 dry-run 完全一致"
        );
        assert_eq!(
            s.get_user_words("pinyin", "nihao").unwrap()[0].boundary,
            0b101
        );
        let _ = std::fs::remove_file(&p);
    }

    /// **向后兼容契约**（数据安全）：value 从 v1 的 16B 扩到 24B（追加 boundary u64）。
    /// 旧库里全是 16B 记录，新代码必须能读——且**不可直接切 `b[16..24]`**，那会在旧记录上
    /// 越界 panic，必须按长度分支。这是惰性升级免 migration 的前提。
    #[test]
    fn dec_val_reads_v1_16byte_records() {
        // 手工拼一条 v1（16B）记录，模拟旧库数据。
        let mut v1 = [0u8; 16];
        v1[0..4].copy_from_slice(&123i32.to_le_bytes());
        v1[4..8].copy_from_slice(&7u32.to_le_bytes());
        v1[8..16].copy_from_slice(&1_700_000_000i64.to_le_bytes());
        assert_eq!(
            dec_val(&v1),
            Some((123, 7, 1_700_000_000, 0)),
            "v1 记录须能读出，boundary 取 0（无信息 → 消费方降级回 DAG）"
        );

        // v2（24B）：boundary 原样读回。
        let v2 = enc_val(123, 7, 1_700_000_000, 0b101);
        assert_eq!(v2.len(), 24);
        assert_eq!(dec_val(&v2), Some((123, 7, 1_700_000_000, 0b101)));

        // 短于 16B 视为损坏 → None（而非 panic）。
        assert_eq!(dec_val(&[0u8; 15]), None);
        assert_eq!(dec_val(&[]), None);
    }

    /// 惰性升级：旧 16B 记录经一次写入后自然补齐为 24B，boundary 从此可用。
    #[test]
    fn v1_record_upgrades_on_write() {
        let path = tmp("wind_uw_v1_upgrade.redb");
        let s = Store::open(&path).unwrap();

        // 直接以 v1（16B）格式塞一条记录，绕过 add_user_word，模拟旧库。
        let key = enc_key("py", "nihao", "你好");
        {
            let mut v1 = [0u8; 16];
            v1[0..4].copy_from_slice(&500i32.to_le_bytes());
            v1[4..8].copy_from_slice(&3u32.to_le_bytes());
            v1[8..16].copy_from_slice(&1_700_000_000i64.to_le_bytes());
            s.with_db(|db| {
                let txn = db.begin_write()?;
                {
                    let mut t = txn.open_table(USER_WORDS)?;
                    t.insert(key.as_str(), v1.as_slice())?;
                }
                txn.commit()?;
                Ok(())
            })
            .unwrap();
        }

        // 旧记录可读，boundary=0。
        let r = s.get_user_words("py", "nihao").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].weight, 500, "旧记录的既有字段不得丢失");
        assert_eq!(r[0].boundary, 0, "v1 无 boundary");

        // 再次加词（权重取 max）：旧值 boundary=0 → 用新算出的补齐。
        s.add_user_word("py", "nihao", "你好", 100, 0b101).unwrap();
        let r2 = s.get_user_words("py", "nihao").unwrap();
        assert_eq!(r2[0].weight, 500, "权重取 max，不被低值覆盖");
        assert_eq!(r2[0].boundary, 0b101, "旧记录 boundary=0 时应被新值补齐");

        // 已有非 0 boundary 时沿用，不被后续调用抹掉（切分与 code/text 绑定，不因再加词而变）。
        s.add_user_word("py", "nihao", "你好", 100, 0).unwrap();
        let r3 = s.get_user_words("py", "nihao").unwrap();
        assert_eq!(r3[0].boundary, 0b101, "已有边界不该被 0 覆盖");
    }

    /// 无边界词（手输码/旧版扁平导入）仍须被简拼查询捞出来交给引擎现判，
    /// 且**只在首字符对得上时**才捞——这正是分组的意义。
    #[test]
    fn no_boundary_words_are_recalled_by_first_char_only() {
        let p = tmp("wind_abbrev_noboundary.redb");
        let s = Store::open(&p).unwrap();
        s.add_user_word("py", "xianning", "西安宁", 100, 0).unwrap();
        s.add_user_word("py", "nihao", "你好", 100, 0).unwrap();

        let texts = |ab: &str| -> Vec<String> {
            s.search_user_words_by_abbrev("py", ab, 0)
                .unwrap()
                .into_iter()
                .map(|r| r.text)
                .collect()
        };
        // 首字符 x → 只捞出 x 开头的那条，n 开头的那条被分组挡掉
        assert_eq!(texts("xan"), vec!["西安宁"]);
        assert_eq!(texts("nh"), vec!["你好"]);
        // 首字符对不上 → 一条不返回（改动前是整库返回）
        assert!(texts("zg").is_empty());
        let _ = std::fs::remove_file(&p);
    }

    /// **索引必须覆盖每一条写路径。**
    ///
    /// 这是本方案唯一的系统性风险：主表写了、索引没写，简拼就静默召不回那个词——
    /// 不报错、不告警。五条写路径逐一验证。
    #[test]
    fn every_write_path_maintains_the_index() {
        let p = tmp("wind_abbrev_paths.redb");
        let s = Store::open(&p).unwrap();
        let hit = |ab: &str| -> Vec<String> {
            s.search_user_words_by_abbrev("py", ab, 0)
                .unwrap()
                .into_iter()
                .map(|r| r.text)
                .collect()
        };

        // ① add_user_word
        s.add_user_word("py", "nihao", "你好", 500, 0b101).unwrap();
        assert_eq!(hit("nh"), vec!["你好"], "add_user_word 应建索引");

        // ② import_user_words
        s.import_user_words(
            "py",
            &[wdict::WordIo {
                code: "xi an ning".into(),
                text: "西安宁".into(),
                weight: 700,
                count: 0,
            }],
        )
        .unwrap();
        assert_eq!(hit("xan"), vec!["西安宁"], "import 应建索引");

        // ③ on_word_selected 的隐性造词（最容易漏的一条）
        s.on_word_selected("py", "zg", "中国", 0, 0).unwrap();
        assert_eq!(
            hit("zg"),
            vec!["中国"],
            "隐性造词的记录 boundary=0，应落在 z 的兜底组（否则它永远召不回）"
        );

        // ④ remove_user_word
        s.remove_user_word("py", "nihao", "你好").unwrap();
        assert!(hit("nh").is_empty(), "remove 应删索引");

        // ⑤ clear_user_words
        s.clear_user_words("py").unwrap();
        assert!(
            hit("xan").is_empty() && hit("zg").is_empty(),
            "clear 应清索引"
        );
        assert_eq!(s.abbrev_index_len(), 0, "索引应随主表一起清空");
        let _ = std::fs::remove_file(&p);
    }

    /// **改权重不该动索引**——这正是 value 留空的收益。
    /// 若哪天把 weight 塞进 value，这两条高频路径就都得同步更新，漏一处即静默错乱。
    #[test]
    fn weight_changes_do_not_touch_the_index() {
        let p = tmp("wind_abbrev_weight.redb");
        let s = Store::open(&p).unwrap();
        s.add_user_word("py", "nihao", "你好", 500, 0b101).unwrap();
        let before = s.abbrev_index_len();

        s.update_user_word_weight("py", "nihao", "你好", 900)
            .unwrap();
        s.on_word_selected("py", "nihao", "你好", 100, 1).unwrap();
        assert_eq!(s.abbrev_index_len(), before, "索引条数不该变");

        let got = s.search_user_words_by_abbrev("py", "nh", 0).unwrap();
        assert_eq!(got.len(), 1);
        assert!(
            got[0].weight >= 900,
            "回主表点查拿到的必须是**最新**权重，实际 {}",
            got[0].weight
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 边界从 0 被补齐时，索引键要跟着搬家——旧键残留会变成永远匹配不上的幽灵。
    #[test]
    fn filling_boundary_moves_the_index_key() {
        let p = tmp("wind_abbrev_move.redb");
        let s = Store::open(&p).unwrap();
        s.add_user_word("py", "nihao", "你好", 500, 0).unwrap(); // 无边界 → \u{1}n 兜底组
        // 探针：`nx` 的声母组必然为空，故命中只可能来自 n 的兜底组。
        assert_eq!(
            s.search_user_words_by_abbrev("py", "nx", 0).unwrap().len(),
            1,
            "补齐前它躺在 n 的兜底组里"
        );

        s.add_user_word("py", "nihao", "你好", 500, 0b101).unwrap(); // 补齐边界
        assert_eq!(
            s.abbrev_index_len(),
            1,
            "补齐后仍应只有一条索引（旧键必须被删）"
        );
        assert!(
            s.search_user_words_by_abbrev("py", "nx", 0)
                .unwrap()
                .is_empty(),
            "补齐边界后不该再留在兜底组（残留即幽灵：查什么都跟着出来）"
        );
        assert_eq!(
            s.search_user_words_by_abbrev("py", "nh", 0).unwrap().len(),
            1
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 存量数据补建索引：老库升级上来时索引是空的，不补建则简拼静默失效。
    #[test]
    fn rebuild_covers_preexisting_words() {
        let p = tmp("wind_abbrev_rebuild.redb");
        let s = Store::open(&p).unwrap();
        s.add_user_word("py", "nihao", "你好", 500, 0b101).unwrap();
        s.add_user_word("wb", "aaaa", "工工", 100, 0).unwrap();

        // 模拟老库：主表有数据、索引为空
        s.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut idx = txn.open_table(USER_ABBREV)?;
                let keys: Vec<String> = idx
                    .iter()?
                    .filter_map(|i| i.ok().map(|(k, _)| k.value().to_string()))
                    .collect();
                for k in &keys {
                    idx.remove(k.as_str())?;
                }
            }
            txn.commit()?;
            Ok(())
        })
        .unwrap();
        assert_eq!(s.abbrev_index_len(), 0, "前提：索引确实被清空了");
        assert!(
            s.search_user_words_by_abbrev("py", "nh", 0)
                .unwrap()
                .is_empty(),
            "前提校验：没有索引时确实召不回——这正是必须补建的理由"
        );

        assert_eq!(s.rebuild_abbrev_indexes().unwrap(), 2);
        assert_eq!(
            s.search_user_words_by_abbrev("py", "nh", 0).unwrap().len(),
            1,
            "补建后应能召回"
        );
        assert_eq!(
            s.search_user_words_by_abbrev("wb", "aaaa", 0)
                .unwrap()
                .len(),
            1,
            "跨 schema 的词也要补建，且各归各的 schema"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_add_get_user_word() {
        let path = tmp("wind_uw_addget.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 100, 0).unwrap();
        s.add_user_word("wb", "a", "戈", 50, 0).unwrap();
        let mut got = s.get_user_words("wb", "a").unwrap();
        got.sort_by_key(|r| r.text.clone());
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|r| r.text == "工" && r.weight == 100));
        // add 同词更高权重 → 取 max
        s.add_user_word("wb", "a", "工", 200, 0).unwrap();
        let g = s.get_user_words("wb", "a").unwrap();
        assert_eq!(g.iter().find(|r| r.text == "工").unwrap().weight, 200);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_prefix_remove_update() {
        let path = tmp("wind_uw_prefix.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "ab", "阿", 10, 0).unwrap();
        s.add_user_word("wb", "abc", "啊", 20, 0).unwrap();
        s.add_user_word("wb", "x", "西", 30, 0).unwrap();
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
        s.add_user_word("wb", "a", "工", 100, 0).unwrap();
        s.add_user_word("wb", "ml", "多行\n带\t制表", 5, 0).unwrap();
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
        s.add_user_word("wb", "a", "工", 100, 0).unwrap();
        // 导入同词更低权重 → 保持 max(100)
        let text = crate::wdict::export_words_wdict(
            &[crate::wdict::WordIo {
                code: "a".into(),
                text: "工".into(),
                weight: 30,
                count: 0,
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

    #[test]
    fn import_user_words_classifies_added_updated_unchanged() {
        let path = tmp("wind_uw_batch.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 100, 0).unwrap();
        let rows = vec![
            // 已有且权重更低 → unchanged(P2 约束 1:不落盘)
            crate::wdict::WordIo {
                code: "a".into(),
                text: "工".into(),
                weight: 30,
                count: 0,
            },
            // 新键 → added
            crate::wdict::WordIo {
                code: "b".into(),
                text: "了".into(),
                weight: 5,
                count: 0,
            },
        ];
        let c = s.import_user_words("wb", &rows).unwrap();
        assert_eq!((c.added, c.updated, c.unchanged), (1, 0, 1));
        assert_eq!(
            s.get_user_words("wb", "a").unwrap()[0].weight,
            100,
            "unchanged 不改权重"
        );

        // 权重严格更大 → updated,取导入值
        let rows2 = vec![crate::wdict::WordIo {
            code: "a".into(),
            text: "工".into(),
            weight: 200,
            count: 0,
        }];
        let c2 = s.import_user_words("wb", &rows2).unwrap();
        assert_eq!((c2.added, c2.updated, c2.unchanged), (0, 1, 0));
        assert_eq!(s.get_user_words("wb", "a").unwrap()[0].weight, 200);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preview_import_is_readonly_and_matches_import() {
        let path = tmp("wind_uw_preview.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 100, 0).unwrap();
        let rows = vec![
            crate::wdict::WordIo {
                code: "a".into(),
                text: "工".into(),
                weight: 30,
                count: 0,
            },
            crate::wdict::WordIo {
                code: "b".into(),
                text: "了".into(),
                weight: 5,
                count: 0,
            },
            crate::wdict::WordIo {
                code: "a".into(),
                text: "工".into(),
                weight: 300,
                count: 0,
            },
        ];
        let (c, samples) = s.preview_import_user_words("wb", &rows).unwrap();
        assert_eq!((c.added, c.updated, c.unchanged), (1, 1, 1));
        assert_eq!(samples.len(), 2, "samples 只含会落盘的行(added+updated)");
        assert!(samples.iter().any(|x| x.contains("了")));
        // 只读:预览后库里仍只有原 1 条、权重未动
        assert_eq!(s.get_user_words("wb", "a").unwrap()[0].weight, 100);
        assert!(s.get_user_words("wb", "b").unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_user_words_only_target_schema() {
        let path = tmp("wind_uw_clear.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 1, 0).unwrap();
        s.add_user_word("wb", "b", "了", 1, 0).unwrap();
        s.add_user_word("py", "ni", "你", 1, 0).unwrap();
        let n = s.clear_user_words("wb").unwrap();
        assert_eq!(n, 2);
        assert!(s.search_user_words_prefix("wb", "", 0).unwrap().is_empty());
        assert_eq!(
            s.search_user_words_prefix("py", "", 0).unwrap().len(),
            1,
            "其它 schema 不受影响"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_dict_wdict_roundtrips_words_count_and_shadow() {
        let path = tmp("wind_uw_dict_io.redb");
        let s = Store::open(&path).unwrap();
        // 用户词 + 调频次数
        s.add_user_word("wb", "a", "工", 100, 0).unwrap();
        s.on_word_selected("wb", "a", "工", 0, 0).unwrap(); // count -> 1
        s.on_word_selected("wb", "a", "工", 0, 0).unwrap(); // count -> 2
        // shadow：pin + del
        s.pin_shadow("wb", "aaaa", "恭", None, 0).unwrap();
        s.delete_shadow("wb", "bbbb", "见").unwrap();

        let text = s
            .export_dict_wdict("wb", "2026-07-14T00:00:00+08:00")
            .unwrap();
        assert!(text.contains("--- !words"), "含 words 段");
        assert!(text.contains("--- !shadow"), "含 shadow 段");

        // 导入到新库：words + shadow 均还原
        let path2 = tmp("wind_uw_dict_io2.redb");
        let s2 = Store::open(&path2).unwrap();
        let (imported, skipped) = s2.import_user_words_wdict("wb", &text).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(imported, 1);
        let got = s2.get_user_words("wb", "a").unwrap();
        assert_eq!(got[0].weight, 100);
        assert_eq!(got[0].count, 2, "count(调频)随导出/导入流转");

        let (actions, sk) = crate::wdict::parse_shadow_wdict(&text).unwrap();
        assert_eq!(sk, 0);
        let n = s2.import_shadow_actions("wb", &actions).unwrap();
        assert!(n >= 2, "至少重放 pin + del 两条");
        assert!(
            s2.get_shadow_rules("wb", "aaaa").unwrap().is_some(),
            "pin 规则还原"
        );
        assert_eq!(
            s2.get_shadow_rules("wb", "bbbb").unwrap().unwrap().deleted,
            vec!["见".to_string()],
            "del 规则还原"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }
}
