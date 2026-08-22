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
    /// 这条**用户短语遮蔽了同键的系统条目**（`add_phrase` / wdict 导入撞上原系统行）。
    ///
    /// 主键只有 `(code, text)` 一把，系统的与用户的「同款」短语在库里无法并存两行。归属规则
    /// 是**用户优先**：撞键时该行转为用户行（`is_system=false`），系统条目随之从系统短语列表
    /// 隐去，输入期生效的是用户那条。本位只记录「它曾是系统条目」，供 UI 标注来源。
    ///
    /// 与 `sync_system_phrases` 的既有语义一致——那里对用户行的处理正是「跳过，让用户行
    /// 遮蔽同键的系统条目」。可逆性由两条路保证：「系统恢复默认」经
    /// [`Store::reclaim_system_phrases`] 认领回系统归属；「清空用户短语」删掉该行后
    /// sync 会重新插入系统条目（调用方须补一次 sync，见 `resync_system_phrases_after_user_reset`）。
    #[serde(default)]
    pub overrides_system: bool,
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
    overrides_system: bool,
}

/// 分发导入时一条短语相对本地库的落点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhraseImportStatus {
    /// 库里没有，会新增。
    New,
    /// 已有同款**用户**短语，导入是空操作。
    ExistsUser,
    /// 撞上同款**系统**短语——导入会把它转成用户行并遮蔽系统条目
    /// （见 [`PhraseRecord::overrides_system`]）。这是接收者最需要被告知的一种。
    ShadowsSystem,
}

/// 分发导入结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhraseImportReport {
    pub added: usize,
    pub skipped_existing: usize,
    /// `added` 中遮蔽了系统条目的条数（是 `added` 的子集，不另计）。
    pub shadowed_system: usize,
}

/// 新建用户短语的默认权重（手动新增与分发导入共用，两处各写一份必然漂移）。
///
/// 短语与码表精确候选**按权重竞争**（`PHRASE_WEIGHT_BASE`(40M) 类别硬顶已删除，
/// 见 candidate-sorting-rules.md §5.1），所以这个默认值直接决定「新建的短语打不打得出」。
/// 此处曾是 1 —— 那会输给几乎每一条码表词条（五笔主库 min=120），在 40M 时代无所谓，
/// 现在是让新短语默认沉底。
///
/// 取 1800 的依据：五笔主库 median=941、p99=9000，1800 越过约 90% 的条目，
/// 又留足余量给用户手动上调（约定值域 0~10000）。与系统短语常用档 800~2000 同轴。
pub const DEFAULT_USER_PHRASE_WEIGHT: i32 = 1800;

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
                        overrides_system: val.overrides_system,
                    });
                }
            }
            Ok(out)
        })
    }

    /// 新增/覆盖一条用户短语。
    ///
    /// **归属用户优先**：撞上同 `(code, text)` 的系统行时把它转为用户行
    /// （`is_system=false`）并置 [`PhraseRecord::overrides_system`]——用户建的那条生效，
    /// 系统条目从「系统短语」列表隐去。这与 `sync_system_phrases` 对用户行的既有处理
    /// （「跳过，让用户行遮蔽同键的系统条目」）是同一条规则。
    ///
    /// ⚠️ **此处的行为被反转过两次，改前先读完这段**：
    /// - 最早也是转用户行，但当时**不可自愈**——`sync_system_phrases` 的
    ///   `!cur.is_system → continue` 永远跳过它，系统条目再也回不来，现象是
    ///   「系统短语莫名少了一条」；
    /// - 于是改成保留 `is_system=true`，结果变成反方向的
    ///   「用户新建的短语在用户列表里看不到」（`list_user_phrases_paged` 按 `!is_system` 过滤）；
    /// - 现在回到转用户行，**可逆性由两条路补齐**：「系统恢复默认」经
    ///   [`Self::reclaim_system_phrases`] 按 TOML 对照认领回系统归属；「清空用户短语」
    ///   删掉该行后由调用方补一次 sync 重新插入系统条目。缺了这两条就会退回第一种毛病。
    pub fn add_phrase(
        &self,
        code: &str,
        text: &str,
        position: i32,
        weight: i32,
    ) -> anyhow::Result<()> {
        let shadows_system = self.get_phrase(code, text)?.is_some_and(|c| c.is_system);
        self.put_phrase(
            code,
            text,
            PhraseValue {
                weight,
                position,
                enabled: true,
                // 用户优先：撞键即转为用户行，输入期与列表都以用户这条为准。
                is_system: false,
                overrides_system: shadows_system,
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
            overrides_system: false,
        });
        let nc = new_code.unwrap_or(code);
        let nt = new_text.unwrap_or(text);
        let val = PhraseValue {
            weight: weight.unwrap_or(cur.weight),
            position: position.unwrap_or(cur.position),
            enabled: cur.enabled,
            is_system: cur.is_system,
            // 编辑既有条目不改归属：在系统短语列表里调权重仍是系统条目，不因此转成用户行。
            // 本位只记录 `add_phrase` 撞键形成的遮蔽关系。
            overrides_system: cur.overrides_system,
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
            overrides_system: false,
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
                        // 本分支只处理已是系统行的条目（用户行在上面 continue 掉了），
                        // 系统行不存在遮蔽关系。
                        overrides_system: false,
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
                            overrides_system: false,
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

    /// 用户短语分页（`is_system=false`，含遮蔽了系统条目的行——它们归属用户）。
    /// prefix 非空时按 code/text 包含过滤后再分页。返回 (页内行, 过滤后总数)。
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
    ///
    /// ⚠️ 这会连**遮蔽了系统条目的行**（`overrides_system`）一起删掉——它们归属用户。
    /// 删后该 `(code,text)` 在库里彻底消失，被遮蔽的系统条目也随之不见，调用方**必须补一次**
    /// `sync_system_phrases` 把它插回来（见 `Coordinator::resync_system_phrases_after_user_reset`）。
    /// sync 平时只在 TOML 哈希变动或「系统恢复默认」时才跑，不补就要等到下次哈希变化。
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

    /// 导出全部用户短语为 wdict 文本（与用户短语列表同口径：`is_system=false`，
    /// 含遮蔽系统条目的行——那是用户写下的定义，漏导等于备份丢数据）。
    pub fn export_user_phrases_wdict(&self, exported_at: &str) -> anyhow::Result<String> {
        let rows: Vec<crate::wdict::PhraseIo> = self
            .list_phrases()?
            .into_iter()
            .filter(|p| !p.is_system)
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
    /// 与 [`Self::add_phrase`] 同口径：导入的是**用户**短语，撞上同键系统行即遮蔽它
    /// （转用户行 + 置 `overrides_system`）。导入是撞键的高发路径（用户常在导出文件里
    /// 手工增删行），可逆性同样依赖「系统恢复默认」的 reclaim 与清空后的 sync 补齐。
    pub fn import_user_phrases_wdict(&self, text: &str) -> anyhow::Result<(usize, usize)> {
        let (rows, skipped) =
            crate::wdict::parse_phrases_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
        let imported = rows.len();
        for r in rows {
            let cur = self.get_phrase(&r.code, &r.text)?;
            // 撞系统行 → 记下遮蔽关系；撞已有用户行 → 沿用它原本的遮蔽标记。
            let overrides_system = cur
                .as_ref()
                .map(|c| c.is_system || c.overrides_system)
                .unwrap_or(false);
            self.put_phrase(
                &r.code,
                &r.text,
                PhraseValue {
                    weight: r.weight,
                    position: r.position,
                    enabled: r.enabled,
                    is_system: false,
                    overrides_system,
                },
            )?;
        }
        Ok((imported, skipped))
    }

    /// 分发导入时一条短语的落点。
    ///
    /// 与 wdict 导入（备份语义、整表 upsert）不同，分发导入**只新增、不改动既有条目**：
    /// 别人的包无权改写接收者已有短语的权重与位置。
    pub fn plan_phrase_import(
        &self,
        items: &[(String, String)],
    ) -> anyhow::Result<Vec<PhraseImportStatus>> {
        items
            .iter()
            .map(|(code, text)| {
                Ok(match self.get_phrase(code, text)? {
                    None => PhraseImportStatus::New,
                    Some(cur) if cur.is_system => PhraseImportStatus::ShadowsSystem,
                    Some(_) => PhraseImportStatus::ExistsUser,
                })
            })
            .collect()
    }

    /// 分发导入：新条目**追加到末尾**，已存在的用户短语原样跳过。
    ///
    /// ★ position 由本地重新分配，不取分发文本里的值——分发格式压根不带 position，
    /// 正是因为照抄分发者的位置会打乱接收者既有短语的顺序。这与 wdict 导入
    /// （[`Self::import_user_phrases_wdict`] 原样写入 position，备份还原要求逐字还原）
    /// 是**刻意相反**的两种语义，别把其中一处「顺手统一」掉。
    ///
    /// `text` 必须已是**存储域**文本（调用方过 `unescape_text_field`），与手动新增
    /// 走同一个转换——出入口必须成对。
    pub fn import_phrases_appending(
        &self,
        items: &[(String, String)],
        weight: i32,
    ) -> anyhow::Result<PhraseImportReport> {
        let mut next_pos = self
            .list_phrases()?
            .iter()
            .map(|p| p.position)
            .max()
            .unwrap_or(-1)
            .saturating_add(1);
        let mut rep = PhraseImportReport::default();
        for (code, text) in items {
            match self.get_phrase(code, text)? {
                // 已有用户行：连 enabled 也不动——用户停用过的条目不该被一次导入悄悄启用。
                Some(cur) if !cur.is_system => {
                    rep.skipped_existing += 1;
                    continue;
                }
                Some(_) => rep.shadowed_system += 1,
                None => {}
            }
            self.add_phrase(code, text, next_pos, weight)?;
            next_pos = next_pos.saturating_add(1);
            rep.added += 1;
        }
        Ok(rep)
    }

    /// 只把**缺失**的系统条目补回库里，已存在的行一律不动。返回补回条数。
    ///
    /// **与 [`Self::sync_system_phrases`] 的区别是要点**：sync 会用 TOML 值覆盖已存在系统行的
    /// weight/position（「以文件为准」，供「恢复默认」与 TOML 变更时用），本函数一个字节都不改。
    ///
    /// 用于「用户短语被清空后，把被遮蔽的系统条目补回来」：遮蔽行归属用户
    /// （见 [`Self::add_phrase`]），`reset_user_phrases` 会连它一起删掉，该 `(code,text)` 遂
    /// 彻底消失。这条路**不能用 sync**——用户的动作只是清空用户短语，顺带把他在系统短语
    /// 列表里改过的权重重置掉是越界的副作用。
    pub fn ensure_system_phrases(&self, entries: &[SystemPhrase]) -> anyhow::Result<usize> {
        let mut n = 0;
        for e in entries {
            if self.get_phrase(&e.code, &e.text)?.is_none() {
                self.put_phrase(
                    &e.code,
                    &e.text,
                    PhraseValue {
                        weight: e.weight,
                        position: e.position,
                        enabled: true,
                        is_system: true,
                        overrides_system: false,
                    },
                )?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// 把「(code,text) 命中系统短语表、但库里是用户行」的记录**认领回系统行**，
    /// 返回认领条数。供「恢复默认」显式调用。
    ///
    /// 这是「用户短语遮蔽系统条目」（[`Self::add_phrase`]）的**撤销路径**，也修复历史上被
    /// 降级的存量数据。认领后归属回到系统、遮蔽标记清零，紧随其后的 `sync_system_phrases`
    /// 会把 weight/position 刷回 TOML 定义。
    ///
    /// 不放进 [`Self::sync_system_phrases`]：那条路径每次启动都可能跑，无法区分「遮蔽了系统
    /// 条目的用户行」与「用户自建的同款短语」，静默认领会让后者在该条目从 TOML 移除时被
    /// 连带删除。「恢复默认」是显式用户动作，认领语义与其名称相符，且只改归属、不删文本。
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
                            overrides_system: false,
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

    /// 导入撞键与 `add_phrase` 同口径：wdict 导入一条与系统短语同款的记录 → 遮蔽系统条目。
    ///
    /// 导入是撞键的高发路径（用户常在导出文件里手工增删行），归属规则必须与手工新增一致，
    /// 否则同一件事经两条入口会得到相反的归属。
    #[test]
    fn import_duplicate_shadows_system_entry() {
        let path = tmp("wind_phrases_keep_sys.redb");
        let s = Store::open(&path).unwrap();
        let sys = [SystemPhrase {
            code: "date".into(),
            text: "二〇二六年".into(),
            weight: 9,
            position: 5,
        }];
        s.sync_system_phrases(&sys).unwrap();

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

        let (rows, total) = s.list_user_phrases_paged(None, 0, 99).unwrap();
        assert_eq!(total, 1, "导入的条目归用户");
        assert!(rows[0].overrides_system, "记录遮蔽关系");
        assert_eq!((rows[0].weight, rows[0].position), (7, 3), "用导入的定义");
        assert!(s.list_system_phrases().unwrap().is_empty(), "系统条目隐去");

        // 同样可撤销：恢复默认认领回系统。
        assert_eq!(s.reclaim_system_phrases(&sys).unwrap(), 1);
        assert_eq!(s.list_system_phrases().unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// 撞键归属：用户建一条与系统短语完全同款的短语 → **用户那条生效**，系统条目隐去。
    ///
    /// ⚠️ 这个位置的行为被反转过两次，方向相反：早期转用户行但**不可自愈**（系统条目再也
    /// 回不来，现象是「系统短语莫名少一条」）；改成保留系统归属后变成反方向的
    /// 「用户新建的短语在用户列表看不到」。现在回到用户优先，本测试同时锁住**撤销路径**
    /// ——缺了它就会退回第一种毛病。
    #[test]
    fn user_phrase_shadows_system_entry() {
        let path = tmp("wind_phrases_shadow_sys.redb");
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

        // ① 出现在用户短语列表，且用的是用户写的 weight/position。
        let (user_rows, total) = s.list_user_phrases_paged(None, 0, 99).unwrap();
        assert_eq!(total, 1, "用户建的短语必须在用户列表可见");
        assert_eq!((user_rows[0].weight, user_rows[0].position), (5000, 9));
        assert!(!user_rows[0].is_system, "归属转为用户");
        assert!(user_rows[0].overrides_system, "记录它遮蔽了系统条目");

        // ② 系统短语列表里隐去（只有一行，已归用户）。
        assert!(
            s.list_system_phrases().unwrap().is_empty(),
            "被遮蔽的系统条目从系统列表隐去"
        );

        // ③ 输入期生效的是用户那条。
        let inp = s.enabled_phrases_for_input().unwrap();
        assert_eq!(inp.len(), 1);
        assert_eq!(inp[0].weight, 5000, "输入期用用户定义");

        // ④ 导出（备份）含它，否则用户写的定义备份即丢。
        assert!(
            s.export_user_phrases_wdict("t")
                .unwrap()
                .contains("$Y年$M月$D日"),
            "遮蔽行须随用户短语导出"
        );

        // ⑤ 撤销路径：「系统恢复默认」→ reclaim 认领回系统 + sync 刷回 TOML 定义。
        assert_eq!(s.reclaim_system_phrases(&sys).unwrap(), 1);
        s.sync_system_phrases(&sys).unwrap();
        let after = s.list_system_phrases().unwrap();
        assert_eq!(after.len(), 1, "系统条目回来了");
        assert_eq!(
            (after[0].weight, after[0].position),
            (1000, 1),
            "还原成定义值"
        );
        assert!(!after[0].overrides_system);
        assert_eq!(s.list_user_phrases_paged(None, 0, 99).unwrap().1, 0);
        let _ = std::fs::remove_file(&path);
    }

    /// 撤销路径二：「清空用户短语」删掉遮蔽行后，补一次 sync 能把系统条目插回来。
    ///
    /// **`reset_user_phrases` 自身不做这件事**（store 层拿不到 TOML），由调用方补——
    /// 不补就是当年那个「系统短语莫名少一条」的 bug，故在此锁死这个前后顺序。
    #[test]
    fn clearing_user_phrases_then_sync_restores_shadowed_system_entry() {
        let path = tmp("wind_phrases_clear_restore.redb");
        let s = Store::open(&path).unwrap();
        let sys = [SystemPhrase {
            code: "date".into(),
            text: "$Y年$M月$D日".into(),
            weight: 1000,
            position: 1,
        }];
        s.sync_system_phrases(&sys).unwrap();
        s.add_phrase("date", "$Y年$M月$D日", 9, 5000).unwrap();

        // 清空用户短语：遮蔽行归属用户，会被删掉 → 该 (code,text) 在库里彻底消失。
        assert_eq!(s.reset_user_phrases().unwrap(), 1);
        assert!(
            s.list_phrases().unwrap().is_empty(),
            "清空后系统条目也一并不见——这正是必须补 sync 的原因"
        );

        // 调用方补的那次 sync 把系统条目插回来，且是 TOML 定义值。
        s.sync_system_phrases(&sys).unwrap();
        let after = s.list_system_phrases().unwrap();
        assert_eq!(after.len(), 1, "系统条目应被补回");
        assert_eq!((after[0].weight, after[0].position), (1000, 1));
        let _ = std::fs::remove_file(&path);
    }

    /// `ensure_system_phrases` 只补缺失、**绝不改动已存在的行**。
    ///
    /// 这条性质是它存在的全部理由：用 sync 代替它，一次「清空用户短语」就会把用户在系统
    /// 短语列表里改过的权重/次序重置回 TOML 默认值——用户没要求这件事。
    #[test]
    fn ensure_system_phrases_only_fills_gaps() {
        let path = tmp("wind_phrases_ensure.redb");
        let s = Store::open(&path).unwrap();
        let sys = [
            SystemPhrase {
                code: "a".into(),
                text: "甲".into(),
                weight: 1000,
                position: 1,
            },
            SystemPhrase {
                code: "b".into(),
                text: "乙".into(),
                weight: 1000,
                position: 2,
            },
        ];
        s.sync_system_phrases(&sys).unwrap();

        // 用户在系统短语列表里改了「甲」的权重、并把「乙」停用。
        s.update_phrase("a", "甲", None, None, Some(9), Some(5555))
            .unwrap();
        s.set_phrase_enabled("b", "乙", false).unwrap();
        // 「乙」被删掉（模拟遮蔽行被 reset_user_phrases 连带清掉后的缺口）
        s.remove_phrase("b", "乙").unwrap();

        assert_eq!(s.ensure_system_phrases(&sys).unwrap(), 1, "只补回缺失的乙");

        let list = s.list_system_phrases().unwrap();
        let a = list.iter().find(|p| p.code == "a").unwrap();
        assert_eq!(
            (a.weight, a.position),
            (5555, 9),
            "已存在的行一字不改——这正是不能用 sync 的原因"
        );
        assert!(list.iter().any(|p| p.code == "b"), "缺失的补回");

        // 幂等：再补一次不重复插入。
        assert_eq!(s.ensure_system_phrases(&sys).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// 对照：同样的场景下 `sync_system_phrases` **会**重置用户的编辑。
    /// 锁住这个差异，避免日后有人「顺手统一成 sync」。
    #[test]
    fn sync_overwrites_user_edits_on_system_rows() {
        let path = tmp("wind_phrases_sync_overwrites.redb");
        let s = Store::open(&path).unwrap();
        let sys = [SystemPhrase {
            code: "a".into(),
            text: "甲".into(),
            weight: 1000,
            position: 1,
        }];
        s.sync_system_phrases(&sys).unwrap();
        s.update_phrase("a", "甲", None, None, Some(9), Some(5555))
            .unwrap();

        s.sync_system_phrases(&sys).unwrap();
        let a = s.list_system_phrases().unwrap().pop().unwrap();
        assert_eq!(
            (a.weight, a.position),
            (1000, 1),
            "sync 以 TOML 为准（供恢复默认/文件变更用），故补齐场景不能用它"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 编辑既有系统短语（改权重）**不**改归属——那是在系统列表里正常调整，不产生遮蔽关系。
    #[test]
    fn editing_system_phrase_keeps_system_ownership() {
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
        assert!(!row.overrides_system, "编辑不产生遮蔽关系");
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
