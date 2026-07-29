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
    /// 该系统行**被用户重新添加过**（`add_phrase` / wdict 导入撞上同 `(code,text)` 的系统行）。
    ///
    /// 主键只有 `(code, text)` 一把，系统的与用户的「同款」短语在库里无法并存两行，归属规则
    /// 是先到先得。此位的作用是让「归属仍是系统」与「用户确实建过它」两件事**同时可表达**：
    /// 该行留在系统短语列表（`sync` / 恢复默认照常工作），同时也出现在用户短语列表里，
    /// 不再出现「新建了一条，两个列表都找不到」。见 [`Store::add_phrase`]。
    #[serde(default)]
    pub user_modified: bool,
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
    #[serde(default)]
    user_modified: bool,
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
                        user_modified: val.user_modified,
                    });
                }
            }
            Ok(out)
        })
    }

    /// 新增/覆盖一条用户短语。
    ///
    /// **同键行若已是系统短语，保留其 `is_system=true`**。主键只有 `(code, text)` 一把，
    /// 若在此无条件写 `is_system: false`，用户新增一条与系统短语完全同款的短语时，会把那行
    /// **原地降级**成用户行 —— 它随即从设置页「系统短语」列表消失（`list_system_phrases` 按
    /// `is_system` 过滤），且不可自愈：`sync_system_phrases` 的 `!cur.is_system → continue`
    /// 分支会永远跳过它，连「恢复默认」也救不回来。
    ///
    /// 归属规则是**先到先得**：系统行在先→保持系统（本函数），用户行在先→保持用户
    /// （`sync_system_phrases` 的跳过分支，见 `sync_does_not_overwrite_user_row`）。
    ///
    /// 撞上系统行时置 [`PhraseRecord::user_modified`]，使该行同时出现在**用户短语列表**里。
    /// 否则用户新建一条与系统短语同款的短语后，它既不在用户列表（`is_system=true` 被滤掉）、
    /// 在系统列表里又与原条目毫无区别——用户看到的就是「我建的东西不见了」。
    ///
    /// 保留归属而非降级，是因为降级不可自愈：`sync_system_phrases` 的 `!cur.is_system →
    /// continue` 会永远跳过降级行，连「恢复默认」也救不回来（那正是本函数上一版的行为，
    /// 表现为反方向的「系统短语自动隐藏」）。
    pub fn add_phrase(
        &self,
        code: &str,
        text: &str,
        position: i32,
        weight: i32,
    ) -> anyhow::Result<()> {
        let is_system = self.get_phrase(code, text)?.is_some_and(|c| c.is_system);
        self.put_phrase(
            code,
            text,
            PhraseValue {
                weight,
                position,
                enabled: true,
                is_system,
                // 归属仍是系统，但这条是用户主动建的 → 让它在用户列表里也可见。
                user_modified: is_system,
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
            user_modified: false,
        });
        let nc = new_code.unwrap_or(code);
        let nt = new_text.unwrap_or(text);
        let val = PhraseValue {
            weight: weight.unwrap_or(cur.weight),
            position: position.unwrap_or(cur.position),
            enabled: cur.enabled,
            is_system: cur.is_system,
            // 编辑既有条目**不**置位：那是在系统短语列表里正常调整，不该因此把该行搬进
            // 用户列表。本位只表达「用户新建/导入过同款」（见 `add_phrase`）。
            user_modified: cur.user_modified,
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
            user_modified: false,
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
                    // 用户行（is_system=false）优先：跳过，让用户行遮蔽同键的系统条目。
                    // 若强制改写为 is_system=true，一旦该系统条目从 TOML 移除，
                    // 删除过时系统项的路径会把这条用户短语一并静默删除。
                    if !cur.is_system {
                        continue;
                    }
                    let val = PhraseValue {
                        weight: e.weight,
                        position: e.position,
                        enabled: cur.enabled, // 保留开关
                        is_system: true,
                        // weight/position 已被刷回 TOML 定义 → 用户的那次「新建同款」已被覆盖，
                        // 标志随之清零。「恢复默认」经此路径，因而天然把这类行还原成纯系统行。
                        user_modified: false,
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
                            user_modified: false,
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

    /// 用户短语分页：纯用户行（`is_system=false`）**加上**被用户重新添加过的系统行
    /// （`user_modified`，见 [`Self::add_phrase`]）。prefix 非空时按 code/text 包含过滤后再分页。
    /// 返回 (页内行, 过滤后总数)。
    ///
    /// 后一类行同时出现在系统短语列表里——这是有意的：主键只有一把，同款短语只有一行，
    /// 它既是系统条目也确实是用户建的。调用方按 `is_system` / `user_modified` 区分标注。
    pub fn list_user_phrases_paged(
        &self,
        prefix: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<PhraseRecord>, usize)> {
        let mut all: Vec<PhraseRecord> = self
            .list_phrases()?
            .into_iter()
            .filter(|p| !p.is_system || p.user_modified)
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
    ///
    /// **不含 `user_modified` 的系统行**——它们的归属是系统，删掉就等于把系统短语删了
    /// （且 `sync` 的删除分支会认为它是过时系统项）。要还原那些行请走系统侧「恢复默认」，
    /// `sync_system_phrases` 会把 weight/position 刷回 TOML 定义并清掉标志。
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

    /// 导出全部用户短语为 wdict 文本。与用户短语列表同口径（含 `user_modified` 的系统行）——
    /// 那些行承载了用户实际写下的 weight/position，漏导等于备份丢数据。
    pub fn export_user_phrases_wdict(&self, exported_at: &str) -> anyhow::Result<String> {
        let rows: Vec<crate::wdict::PhraseIo> = self
            .list_phrases()?
            .into_iter()
            .filter(|p| !p.is_system || p.user_modified)
            .map(|p| crate::wdict::PhraseIo {
                code: p.code,
                text: p.text,
                weight: p.weight,
                position: p.position,
                enabled: p.enabled,
            })
            .collect();
        Ok(crate::wdict::export_phrases_wdict(&rows, exported_at))
    }

    /// 导入用户短语（合并 upsert）。返回 (导入条数, 跳过条数)。
    ///
    /// 与 [`Self::add_phrase`] 同样保留同键系统行的 `is_system`——导入是撞键的高发路径
    /// （用户常在导出文件里手工增删行），一次导入即可把整批系统短语静默降级。
    pub fn import_user_phrases_wdict(&self, text: &str) -> anyhow::Result<(usize, usize)> {
        let (rows, skipped) =
            crate::wdict::parse_phrases_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
        let imported = rows.len();
        for r in rows {
            let is_system = self
                .get_phrase(&r.code, &r.text)?
                .is_some_and(|c| c.is_system);
            self.put_phrase(
                &r.code,
                &r.text,
                PhraseValue {
                    weight: r.weight,
                    position: r.position,
                    enabled: r.enabled,
                    is_system,
                    // 与 add_phrase 同口径：撞系统行 → 该行在用户列表里也可见。
                    user_modified: is_system,
                },
            )?;
        }
        Ok((imported, skipped))
    }

    /// 把「(code,text) 命中系统短语表、但库里被记成用户行」的记录**认领回系统行**，
    /// 返回认领条数。供「恢复默认」显式调用——修复历史上被 `add_phrase`/导入降级的存量数据。
    ///
    /// 不放进 [`Self::sync_system_phrases`]：那条路径每次启动都跑，无法区分「被降级的系统行」
    /// 与「用户自建的同款短语」，静默认领会让后者在该条目从 TOML 移除时被连带删除。
    /// 「恢复默认」是显式用户动作，认领语义与其名称相符，且只改归属、不删文本。
    pub fn reclaim_system_phrases(&self, entries: &[SystemPhrase]) -> anyhow::Result<usize> {
        let mut n = 0;
        for e in entries {
            match self.get_phrase(&e.code, &e.text)? {
                Some(cur) if !cur.is_system => {
                    self.put_phrase(
                        &e.code,
                        &e.text,
                        PhraseValue {
                            is_system: true,
                            ..cur
                        },
                    )?;
                    n += 1;
                }
                _ => {}
            }
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

    /// 用户新增/导入一条与系统短语完全同款的记录，不得把系统行降级成用户行——
    /// 降级后它会从「系统短语」列表永久消失（sync 的 `!cur.is_system → continue` 跳过它）。
    #[test]
    fn user_write_keeps_system_ownership() {
        let path = tmp("wind_phrases_keep_sys.redb");
        let s = Store::open(&path).unwrap();
        // 系统行在先
        s.sync_system_phrases(&[SystemPhrase {
            code: "date".into(),
            text: "二〇二六年".into(),
            weight: 9,
            position: 5,
        }])
        .unwrap();

        // 用户添加同款 → 仍应留在系统短语列表
        s.add_phrase("date", "二〇二六年", 1, 100).unwrap();
        assert_eq!(
            s.list_system_phrases().unwrap().len(),
            1,
            "add_phrase 撞键不应把系统行降级"
        );
        // 用户改的 weight/position 生效
        let row = s.list_system_phrases().unwrap().pop().unwrap();
        assert_eq!((row.weight, row.position), (100, 1));

        // 导入同款 → 同样不降级
        let wd = crate::wdict::export_phrases_wdict(
            &[crate::wdict::PhraseIo {
                code: "date".into(),
                text: "二〇二六年".into(),
                weight: 7,
                position: 3,
                enabled: true,
            }],
            "2026-07-21T00:00:00+08:00",
        );
        s.import_user_phrases_wdict(&wd).unwrap();
        assert_eq!(
            s.list_system_phrases().unwrap().len(),
            1,
            "导入撞键不应把系统行降级"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 撞键可见性：用户新建一条与系统短语完全同款的短语后，**两个列表都要能看到它**。
    ///
    /// 历史上这里反复翻车过两次，方向相反：早期 `add_phrase` 无条件写 `is_system=false`，
    /// 把系统行降级 → 现象是「系统短语自动隐藏」；修掉降级后又变成「用户短语看不到」。
    /// 根因是主键只有 `(code,text)` 一把、`is_system` 是行属性，两种归属无法并存——
    /// `user_modified` 就是用来同时表达这两件事的。
    #[test]
    fn user_added_duplicate_visible_in_both_lists() {
        let path = tmp("wind_phrases_dup_visible.redb");
        let s = Store::open(&path).unwrap();
        let sys = [SystemPhrase {
            code: "date".into(),
            text: "$Y年$M月$D日".into(),
            weight: 1000,
            position: 1,
        }];
        s.sync_system_phrases(&sys).unwrap();

        // 用户建了一条一模一样的（同 code 同 text），并给了自己的权重/位置。
        s.add_phrase("date", "$Y年$M月$D日", 9, 5000).unwrap();

        // ① 仍在系统短语列表（sync / 恢复默认照常认得它），并标出被改过。
        let sys_list = s.list_system_phrases().unwrap();
        assert_eq!(sys_list.len(), 1, "不得降级出系统列表");
        assert!(sys_list[0].user_modified, "应标记为被用户重新添加过");

        // ② 同时出现在用户短语列表——这正是此前「看不到」的那一条。
        let (user_rows, total) = s.list_user_phrases_paged(None, 0, 99).unwrap();
        assert_eq!(total, 1, "用户新建的同款短语必须在用户列表可见");
        assert_eq!((user_rows[0].weight, user_rows[0].position), (5000, 9));
        assert!(user_rows[0].is_system, "归属仍是系统，UI 据此区分删除语义");

        // ③ 导出（备份）不能漏掉它，否则用户写的权重/位置备份即丢。
        let wd = s.export_user_phrases_wdict("t").unwrap();
        assert!(
            wd.contains("$Y年$M月$D日"),
            "用户改过的系统行须随用户短语导出"
        );

        // ④ 「用户清空」不碰它（归属是系统，删掉等于删系统短语）。
        assert_eq!(s.reset_user_phrases().unwrap(), 0);
        assert_eq!(s.list_system_phrases().unwrap().len(), 1);

        // ⑤ 系统「恢复默认」把 weight/position 刷回 TOML 定义并清掉标志。
        s.sync_system_phrases(&sys).unwrap();
        let after = s.list_system_phrases().unwrap();
        assert_eq!(
            (after[0].weight, after[0].position),
            (1000, 1),
            "还原成定义值"
        );
        assert!(!after[0].user_modified, "还原后不再算用户改过");
        assert_eq!(
            s.list_user_phrases_paged(None, 0, 99).unwrap().1,
            0,
            "还原后退出用户列表"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 编辑既有系统短语（改权重）**不**把它搬进用户列表——本位只表达「用户新建过同款」。
    #[test]
    fn editing_system_phrase_does_not_mark_user_modified() {
        let path = tmp("wind_phrases_edit_no_mark.redb");
        let s = Store::open(&path).unwrap();
        s.sync_system_phrases(&[SystemPhrase {
            code: "em".into(),
            text: "（＾＿＾）".into(),
            weight: 1000,
            position: 0,
        }])
        .unwrap();
        s.update_phrase("em", "（＾＿＾）", None, None, None, Some(42))
            .unwrap();
        let row = s.list_system_phrases().unwrap().pop().unwrap();
        assert_eq!(row.weight, 42, "编辑生效");
        assert!(!row.user_modified, "编辑不置位");
        assert_eq!(s.list_user_phrases_paged(None, 0, 99).unwrap().1, 0);
        let _ = std::fs::remove_file(&path);
    }

    /// 存量修复：已被降级的行，「恢复默认」应认领回系统归属。
    #[test]
    fn reclaim_restores_downgraded_system_rows() {
        let path = tmp("wind_phrases_reclaim.redb");
        let s = Store::open(&path).unwrap();
        let sys = [SystemPhrase {
            code: "date".into(),
            text: "二〇二六年".into(),
            weight: 9,
            position: 5,
        }];
        // 手工制造受损现场：用户行在先，且与系统条目同键
        s.add_phrase("date", "二〇二六年", 0, 1).unwrap();
        s.sync_system_phrases(&sys).unwrap();
        assert!(
            s.list_system_phrases().unwrap().is_empty(),
            "受损现场：系统列表应为空"
        );

        assert_eq!(s.reclaim_system_phrases(&sys).unwrap(), 1);
        assert_eq!(s.list_system_phrases().unwrap().len(), 1);
        assert!(s.list_user_phrases_paged(None, 0, 99).unwrap().0.is_empty());
        // 幂等：再认领一次不重复计数
        assert_eq!(s.reclaim_system_phrases(&sys).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sync_does_not_overwrite_user_row() {
        let path = tmp("wind_phrases_no_overwrite_user.redb");
        let s = Store::open(&path).unwrap();
        // 先建用户行 (bj, 北京)，is_system=false
        s.add_phrase("bj", "北京", 0, 1).unwrap();
        // 同步含同键的系统条目
        s.sync_system_phrases(&[SystemPhrase {
            code: "bj".into(),
            text: "北京".into(),
            weight: 9,
            position: 0,
        }])
        .unwrap();
        // 用户行应保持 is_system=false，不被系统化
        let row = s
            .list_phrases()
            .unwrap()
            .into_iter()
            .find(|p| p.code == "bj")
            .unwrap();
        assert!(!row.is_system, "用户行不应被系统短语覆写为 is_system=true");
        // 模拟系统条目移除（sync 空列表）：用户行不应被删
        s.sync_system_phrases(&[]).unwrap();
        let list = s.list_phrases().unwrap();
        assert!(
            list.iter().any(|p| p.code == "bj" && !p.is_system),
            "系统条目移除后用户行不应被静默删除"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_import_user_phrases_roundtrip() {
        let path = tmp("wind_phrases_io.redb");
        let s = Store::open(&path).unwrap();
        s.add_phrase("bj", "北京", 0, 1000).unwrap();
        s.add_phrase("ml", "多行\n内容", 2, 500).unwrap();
        let text = s
            .export_user_phrases_wdict("2026-07-02T00:00:00+08:00")
            .unwrap();
        // 清空后再导入
        s.reset_user_phrases().unwrap();
        assert_eq!(s.list_user_phrases_paged(None, 0, 99).unwrap().1, 0);
        let (imported, skipped) = s.import_user_phrases_wdict(&text).unwrap();
        assert_eq!((imported, skipped), (2, 0));
        let (rows, total) = s.list_user_phrases_paged(None, 0, 99).unwrap();
        assert_eq!(total, 2);
        assert!(
            rows.iter()
                .any(|p| p.code == "ml" && p.text == "多行\n内容"),
            "多行往返无损"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn import_upsert_merges() {
        let path = tmp("wind_phrases_import_merge.redb");
        let s = Store::open(&path).unwrap();
        s.add_phrase("bj", "北京", 0, 1).unwrap();
        // 导入含同键(权重不同)+新键
        let text = crate::wdict::export_phrases_wdict(
            &[
                crate::wdict::PhraseIo {
                    code: "bj".into(),
                    text: "北京".into(),
                    weight: 9,
                    position: 0,
                    enabled: true,
                },
                crate::wdict::PhraseIo {
                    code: "sh".into(),
                    text: "上海".into(),
                    weight: 1,
                    position: 0,
                    enabled: true,
                },
            ],
            "t",
        );
        let (imported, _) = s.import_user_phrases_wdict(&text).unwrap();
        assert_eq!(imported, 2);
        let (rows, total) = s.list_user_phrases_paged(None, 0, 99).unwrap();
        assert_eq!(total, 2, "同键合并不新增行");
        assert_eq!(
            rows.iter().find(|p| p.code == "bj").unwrap().weight,
            9,
            "同键更新权重"
        );
        let _ = std::fs::remove_file(&path);
    }
}
