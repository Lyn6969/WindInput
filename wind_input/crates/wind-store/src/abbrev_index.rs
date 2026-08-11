//! 简拼索引：用户词 / 临时词的「声母串 → 词条」倒排表。
//!
//! ## 为什么需要它
//!
//! 主表 key 是 `{schema}\0{code}\0{text}`，按**声母**查只能全表扫。拼音简拼族每次按键都要
//! 扫一遍（`pinyin/mod.rs` step6 / step6.2 现算声母比对），且 step6.2 是**逐切点循环**、
//! 一次按键扫十几遍。19 万词实测 172ms/次；全拼输入完全不走这条路——真机现象正是
//! 「全拼不卡、一打简拼就卡」。
//!
//! ## 索引 value 为什么留空
//!
//! weight / boundary 都在主表，查到 key 拆出 `(code, text)` 后回主表点查即可（命中数通常
//! 几十，点查是 µs 级）。**不冗余存可变字段**，写路径就只需管键的增删：改权重的那几条
//! 高频路径（`update_user_word_weight` / `on_word_selected` / `increment_temp_if_exists`）
//! 一个字节都不用碰索引。若把 weight 存进 value，它们都得跟着更新，漏一处就是静默的排序错乱。
//!
//! ## 唯一的系统性风险，与它的收敛方式
//!
//! 主表写了、索引没写 ⇒ 那个词简拼**静默召不回**（不报错、不告警）。用户词与临时词加起来
//! 有十余条写路径，其中 `promote_temp_word` 写的是用户词表却住在 `temp_words.rs` 里——
//! 按文件名去数必漏。故本模块把维护动作收敛成 [`insert`] / [`remove`] / [`shift`] 三个
//! 只接受**已打开 table 句柄**的函数：每个写路径退化成一行调用，且必须与主表在**同一个
//! 写事务**内（否则崩溃点之间会留下孤儿索引）。
//!
//! 校验方式随之变成机械的：`open_table(USER_ABBREV|TEMP_ABBREV)` 的每一处，
//! 必然紧跟对应的 `abbrev_index::*` 调用。

use crate::store::{TEMP_ABBREV, TEMP_WORDS, USER_ABBREV, USER_WORDS};
use crate::user_words::{UserWordRecord, dec_val, enc_key, split_key};
use redb::{ReadableTable, ReadableTableMetadata, Table, TableDefinition, WriteTransaction};

/// 按音节边界取各音节首字母（`nihao` + `0b101` → `nh`）。
///
/// **纯位运算，不需要拼音 trie** —— 这正是简拼索引能在存储层独立维护的前提。
///
/// ⚠️ 必须与 `wind_engine::pinyin` 的 `abbrev_of_code` 的 `boundary != 0` 分支**逐位一致**：
/// 那边是查询侧的判据，这边是索引侧的键。两者一旦漂移，索引里的键就永远匹配不上查询，
/// 表现为「简拼一条都召不回」而不是报错。故此处**不加任何额外守卫**（例如「bit0 必须
/// 置位」）——引擎不加，这边加了就是漂移。`boundary == 0` 的分组另见 [`group_of`]。
pub fn abbrev_of(code: &str, boundary: u64) -> String {
    code.char_indices()
        .filter(|(i, _)| *i < 64 && (boundary >> i) & 1 == 1)
        .map(|(_, ch)| ch)
        .collect()
}

/// 无边界词的分组前缀（SOH）。声母组的键恒为小写字母串，故两个键空间不相交。
const NO_BOUNDARY: char = '\u{1}';

/// 索引分组键：这条词该挂在哪一组下。
///
/// - `boundary != 0` → 声母串本身（[`abbrev_of`]）。查询按整串点查，这是常态。
/// - `boundary == 0` → `\u{1}` + **码首字符**。这类词（手输码、旧版扁平导入）算不出完整
///   声母，只能交给引擎侧用 DAG 现判；但**第一个声母必定是 `code[0]`**——DAG 切分从字节 0
///   起，首音节的首字母就是码的首字符。故按首字符分 26 组仍是完备的。
///
/// ## 为什么不图省事全塞进一个空串组
///
/// 那样每次简拼查询都要整组扫一遍，规模不设上限。逐键路径上留一个「通常很小」的无界扫描，
/// 正是本次要根除的那个 bug 的缩小版（原注释写的就是「规模小，现算即可」，然后它失效了）。
///
/// ## 为什么要 `\u{1}` 前缀而不是直接用首字符
///
/// 直接用首字符会让兜底组与**单音节词**的声母组撞在一起：查 `nh` 时要扫的 `n` 组会混进
/// 「你」「拟」「泥」…每一个 n 开头的单音节词，它们必然过不了判据，纯属白查。
/// 加个不可能出现在声母串里的前缀，两个键空间就彻底分开。
fn group_of(code: &str, boundary: u64) -> String {
    if boundary != 0 {
        return abbrev_of(code, boundary);
    }
    match code.chars().next() {
        Some(c) => format!("{NO_BOUNDARY}{c}"),
        None => String::new(),
    }
}

/// 查询声母串 `abbrev` 时需要扫的分组（见 [`group_of`]）：
/// 声母组本身，加上首字符对应的无边界兜底组。
fn scan_groups(abbrev: &str) -> Vec<String> {
    let mut v = vec![abbrev.to_string()];
    if let Some(c) = abbrev.chars().next() {
        v.push(format!("{NO_BOUNDARY}{c}"));
    }
    v
}

/// 索引 key：`"{schema}\0{group}\0{code}\0{text}"`
fn enc(schema: &str, group: &str, code: &str, text: &str) -> String {
    format!("{schema}\u{0}{group}\u{0}{code}\u{0}{text}")
}

/// 拆分索引 key 的尾部 → `(code, text)`（schema 与 group 由调用方按前缀已知）。
fn split_tail<'a>(key: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let mut it = key.get(prefix.len()..)?.splitn(2, '\u{0}');
    Some((it.next()?, it.next()?))
}

/// 索引表句柄（写事务内）。
type Idx<'txn> = Table<'txn, &'static str, &'static [u8]>;

/// 建索引。与主表 `insert` 成对出现。
pub(crate) fn insert(
    idx: &mut Idx<'_>,
    schema: &str,
    code: &str,
    text: &str,
    boundary: u64,
) -> anyhow::Result<()> {
    idx.insert(
        enc(schema, &group_of(code, boundary), code, text).as_str(),
        [].as_slice(),
    )?;
    Ok(())
}

/// 删索引。与主表 `remove` 成对出现。
///
/// ⚠️ `boundary` 必须是**删除前**主表里的那个值——分组键由它算出，取错了就删不掉，
/// 留下的孤儿索引会让任意查询都把这个已删的词捞回来。故调用方必须**先读后删**。
pub(crate) fn remove(
    idx: &mut Idx<'_>,
    schema: &str,
    code: &str,
    text: &str,
    boundary: u64,
) -> anyhow::Result<()> {
    idx.remove(enc(schema, &group_of(code, boundary), code, text).as_str())?;
    Ok(())
}

/// 边界变化时搬家：按旧值删、按新值建。`old == new` 时只重建（幂等）。
///
/// 边界会从 0 被**补齐**（同一个词先由手输码写入、后被带边界的导入/造词覆盖），
/// 此时索引键随之改变。只建不删的话旧键残留成幽灵：它挂在兜底组里，
/// 于是这个词会跟着任何同首字母的查询一起冒出来。
pub(crate) fn shift(
    idx: &mut Idx<'_>,
    schema: &str,
    code: &str,
    text: &str,
    old: Option<u64>,
    new: u64,
) -> anyhow::Result<()> {
    if let Some(ob) = old
        && ob != new
    {
        remove(idx, schema, code, text, ob)?;
    }
    insert(idx, schema, code, text, new)
}

/// 清掉某 schema 的全部索引。与主表的 `clear_*` 成对出现。
///
/// 不同事务清会留下孤儿索引：召回指向已删的词，回主表点查扑空后被跳过，
/// 表现为「简拼时好时坏」而不是报错。
pub(crate) fn clear_schema(idx: &mut Idx<'_>, schema: &str) -> anyhow::Result<usize> {
    let prefix = format!("{schema}\u{0}");
    let keys: Vec<String> = {
        let mut ks = Vec::new();
        for item in idx.range(prefix.as_str()..)? {
            let (k, _) = item?;
            let key = k.value();
            if !key.starts_with(&prefix) {
                break;
            }
            ks.push(key.to_string());
        }
        ks
    };
    for k in &keys {
        idx.remove(k.as_str())?;
    }
    Ok(keys.len())
}

/// **按声母串检索**（简拼召回）。这是整张索引表存在的理由。
///
/// 扫两组（见 [`group_of`]）：`abbrev` 本身，加上首字符对应的无边界兜底组。
/// 后者的词算不出声母，交给引擎侧用 DAG 现判——与建索引之前的行为一致。
///
/// 返回的是**超集**：调用方仍须逐条过自己的判据（纯简拼比对 / 混合模式校验）。
/// 索引只保证「声母投影对得上」，音节数、逐段全等这些判据都还在引擎侧。
///
/// `limit == 0` 表示不限。返回的记录来自**主表点查**，故 weight/count/boundary 都是最新的
/// ——这正是索引 value 留空所换来的：权重变化不必同步索引。
pub(crate) fn search(
    db: &redb::Database,
    idx_table: TableDefinition<&'static str, &'static [u8]>,
    main_table: TableDefinition<&'static str, &'static [u8]>,
    schema: &str,
    abbrev: &str,
    limit: usize,
) -> anyhow::Result<Vec<UserWordRecord>> {
    let txn = db.begin_read()?;
    let idx = txn.open_table(idx_table)?;
    let main = txn.open_table(main_table)?;
    let mut out = Vec::new();
    for g in scan_groups(abbrev) {
        let prefix = format!("{schema}\u{0}{g}\u{0}");
        for item in idx.range(prefix.as_str()..)? {
            let (k, _) = item?;
            let key = k.value();
            if !key.starts_with(&prefix) {
                break;
            }
            let Some((code, text)) = split_tail(key, &prefix) else {
                continue;
            };
            // 回主表取权重与边界：索引 value 刻意留空，故这里点查一次。
            let Some((w, c, ca, b)) = main
                .get(enc_key(schema, code, text).as_str())?
                .and_then(|g| dec_val(g.value()))
            else {
                continue; // 主表已无 → 孤儿索引，跳过（不该发生，防御性）
            };
            out.push(UserWordRecord {
                code: code.to_string(),
                text: text.to_string(),
                weight: w,
                count: c,
                created_at: ca,
                boundary: b,
            });
            if limit > 0 && out.len() >= limit {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// 为**存量数据**补建索引（调用方负责提交事务）。返回建了多少条。
///
/// 升级到带索引的版本时，老库里的词一条索引都没有。不补建的话简拼召回会静默地什么都
/// 查不到——比慢更糟。调用方在启动时按「索引为空而主表非空」判断是否需要。
pub(crate) fn rebuild(
    txn: &WriteTransaction,
    idx_table: TableDefinition<&'static str, &'static [u8]>,
    main_table: TableDefinition<&'static str, &'static [u8]>,
) -> anyhow::Result<usize> {
    let t = txn.open_table(main_table)?;
    // 先收集再写：读游标与写句柄不能同时持有同一事务里的两张表之外的借用。
    let entries: Vec<String> = t
        .iter()?
        .filter_map(|item| {
            let (k, v) = item.ok()?;
            let (schema, code, text) = split_key(k.value())?;
            let (_, _, _, b) = dec_val(v.value())?;
            Some(enc(schema, &group_of(code, b), code, text))
        })
        .collect();
    drop(t);
    let mut idx = txn.open_table(idx_table)?;
    for e in &entries {
        idx.insert(e.as_str(), [].as_slice())?;
    }
    Ok(entries.len())
}

impl crate::Store {
    /// 按声母串检索**用户词**。见 [`search`]（返回超集，判据仍在引擎侧）。
    pub fn search_user_words_by_abbrev(
        &self,
        schema: &str,
        abbrev: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<UserWordRecord>> {
        self.with_db(|db| search(db, USER_ABBREV, USER_WORDS, schema, abbrev, limit))
    }

    /// 按声母串检索**临时词**。见 [`search`]。
    pub fn search_temp_words_by_abbrev(
        &self,
        schema: &str,
        abbrev: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<UserWordRecord>> {
        self.with_db(|db| search(db, TEMP_ABBREV, TEMP_WORDS, schema, abbrev, limit))
    }

    /// 两张索引表的条目总数（O(1)）。为 0 而主表非空即说明索引待重建。
    pub fn abbrev_index_len(&self) -> u64 {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            Ok(txn.open_table(USER_ABBREV)?.len()? + txn.open_table(TEMP_ABBREV)?.len()?)
        })
        .unwrap_or(0)
    }

    /// 为存量数据补建两张索引（单写事务）。返回建了多少条。见 [`rebuild`]。
    pub fn rebuild_abbrev_indexes(&self) -> anyhow::Result<usize> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let n =
                rebuild(&txn, USER_ABBREV, USER_WORDS)? + rebuild(&txn, TEMP_ABBREV, TEMP_WORDS)?;
            txn.commit()?;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 声母提取必须与引擎侧 `abbrev_of_code` 的 boundary 分支逐位一致。
    ///
    /// **不得加引擎没有的守卫**。曾经这里对「bit0 未置位」特判返回空串，看似稳健，
    /// 实则是单方面漂移：引擎照样按位取字符，于是那条词的索引键与查询键对不上，
    /// 静默召不回。判据同源才是稳健，不是各自加保险。
    #[test]
    fn abbrev_of_matches_engine_semantics() {
        assert_eq!(abbrev_of("nihao", 0b101), "nh");
        assert_eq!(
            abbrev_of("xianning", 0b10101),
            "xan",
            "xi|an|ning 的真值声母是 xan（不是 maximum_match 猜的 xn）"
        );
        assert_eq!(abbrev_of("nihao", 0b100), "h", "按位取，与引擎同式，不特判");
        assert_eq!(abbrev_of("", 0b1), "");
    }

    /// 分组键：有边界走声母串，无边界走 `\u{1}+码首字符`，两个键空间不相交。
    #[test]
    fn no_boundary_words_get_their_own_key_space() {
        assert_eq!(group_of("nihao", 0b101), "nh");
        assert_eq!(group_of("abcd", 0), "\u{1}a", "无边界 → 按码首字符分组");
        assert_eq!(group_of("", 0), "");

        // 查 `nh` 要扫的两组：声母组 nh，与 n 开头的无边界兜底组。
        // 单音节词所在的 `n` 组**不在其中**——它们必然过不了判据，扫了纯属白查。
        assert_eq!(scan_groups("nh"), vec!["nh", "\u{1}n"]);
        assert_eq!(scan_groups("n"), vec!["n", "\u{1}n"]);
    }
}
