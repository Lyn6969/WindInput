//! 快捷输入格式表的**用户调整**（右键菜单调序 / 停用）。
//!
//! 与 [`crate::shadow`] 是**两套**东西，别混：
//!
//! | | 作用域 | 键 |
//! |---|---|---|
//! | shadow（候选调整） | 一次输入（这个码下的这些字） | `(方案, 输入码)` |
//! | 本模块（格式调整） | 一类输入（所有日期 / 所有数字） | `格式类别` |
//!
//! 快捷输入的「输入码」是 `2026.6.19` 这种具体值。若把格式调整存进 shadow，用户右键
//! 把农历排到最前，存下的键就是那一天；次日打 `2026.6.20` 键不匹配，调整凭空消失。
//! 症状是「当时有效、隔天失效」，间歇性发作，极难排查。故另起一张表，键只到类别。
//!
//! ## 与格式表文件的关系
//!
//! ```text
//! 出厂 data/system.quick.toml
//!   ↓ 用户整份覆盖（高级用户手写，可选）
//! 基表
//!   ↓ 本表的 moved / disabled（普通用户点右键）
//! 最终候选
//! ```
//!
//! 两类用户各写各的落点，互不干扰。**GUI 调整绝不回写 `system.quick.toml`**——那会
//! 抢走高级用户手写文件的所有权（重写丢注释），更糟的是让普通用户点两下右键就
//! 永久脱离出厂更新，而他毫不知情（整份覆盖的代价必须是知情选择）。
//!
//! ## 为什么存稀疏的 (id, position) 而不是完整顺序
//!
//! 完整 id 序列存下来后，将来出厂新增一条格式，它不在序列里，就得再定一条
//! 「新增格式排哪」的规则——怎么定都会让人意外。稀疏记录下**没被碰过的条目不出现在
//! 存储里**，于是新增格式自然落在它的基表位置。
//!
//! 规则的「应用」由调用方完成（`wind_quick_input::FormatAdjust`），本模块只管存取——
//! 与 shadow 同一条纪律：store 不依赖业务类型。

use crate::store::{QUICK_FORMAT, Store};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

/// 一条移动规则：把格式 `id` 固定到组内下标 `position`（0 = 首位）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickFormatMove {
    pub id: String,
    pub position: usize,
}

/// 某一类（`date` / `year_month` / `number` / `calc`）的用户调整。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickFormatRecord {
    /// 被移动过的格式。**LIFO，index 0 = 最新**，应用时逆序遍历（最新的最后应用、
    /// 优先级最高），与 [`crate::shadow::ShadowRecord::pinned`] 同构。
    #[serde(default)]
    pub moved: Vec<QuickFormatMove>,
    /// 被停用的格式 id。
    ///
    /// 叫 disabled 而不是 hidden 是刻意的：设置页的既有能力是 `set_enabled`（启用开关），
    /// 被停用的行**在管理界面里仍然可见**、能再打开。若语义定成「隐藏」，那一行就该从
    /// 列表消失，用户便再也点不到它——右键菜单里它本来就已经不出现了。
    #[serde(default)]
    pub disabled: Vec<String>,
}

impl QuickFormatRecord {
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.disabled.is_empty()
    }

    /// 移动：把 `id` 固定到 `position`。同 id 的旧规则被顶替（LIFO，新规则插队首）。
    pub fn apply_move(&mut self, id: &str, position: usize) {
        self.moved.retain(|m| m.id != id);
        self.moved.insert(
            0,
            QuickFormatMove {
                id: id.to_string(),
                position,
            },
        );
    }

    /// 启用 / 停用某条格式。
    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        self.disabled.retain(|d| d != id);
        if !enabled {
            self.disabled.push(id.to_string());
        }
    }

    /// 清除某条格式的全部调整（恢复它的出厂位置与启用状态）。
    pub fn apply_reset(&mut self, id: &str) {
        self.moved.retain(|m| m.id != id);
        self.disabled.retain(|d| d != id);
    }

    /// 该格式是否被调整过（供菜单项灰显判断）。
    pub fn has_rule(&self, id: &str) -> bool {
        self.moved.iter().any(|m| m.id == id) || self.disabled.iter().any(|d| d == id)
    }
}

impl Store {
    /// 读改写一类格式的调整；改完为空则删除该键（单写事务）。
    fn modify_quick_format(
        &self,
        kind: &str,
        f: impl FnOnce(&mut QuickFormatRecord),
    ) -> anyhow::Result<()> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(QUICK_FORMAT)?;
                let mut rec: QuickFormatRecord = t
                    .get(kind)?
                    .and_then(|g| serde_json::from_slice(g.value()).ok())
                    .unwrap_or_default();
                f(&mut rec);
                if rec.is_empty() {
                    t.remove(kind)?;
                } else {
                    let bytes = serde_json::to_vec(&rec)?;
                    t.insert(kind, bytes.as_slice())?;
                }
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 把某条格式移到组内下标 `position`。
    pub fn move_quick_format(&self, kind: &str, id: &str, position: usize) -> anyhow::Result<()> {
        self.modify_quick_format(kind, |rec| rec.apply_move(id, position))
    }

    /// 启用 / 停用某条格式。
    pub fn set_quick_format_enabled(
        &self,
        kind: &str,
        id: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.modify_quick_format(kind, |rec| rec.set_enabled(id, enabled))
    }

    /// 恢复单条格式的默认（位置 + 启用状态）。
    pub fn reset_quick_format_entry(&self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.modify_quick_format(kind, |rec| rec.apply_reset(id))
    }

    /// 恢复某一类的全部默认（清空该类记录）。
    ///
    /// 这是**停用之后的唯一出口**（在没有设置页时）：被停用的格式不出现在候选里，
    /// 右键点不到，只能整类重置。
    pub fn reset_quick_format_kind(&self, kind: &str) -> anyhow::Result<()> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(QUICK_FORMAT)?;
                t.remove(kind)?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 取某类的调整（无则 `None`）。
    pub fn get_quick_format(&self, kind: &str) -> anyhow::Result<Option<QuickFormatRecord>> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(QUICK_FORMAT)?;
            Ok(t.get(kind)?
                .and_then(|g| serde_json::from_slice(g.value()).ok()))
        })
    }

    /// 列出全部类别的调整（设置页与启动装载用），按 kind 升序。
    pub fn list_quick_format(&self) -> anyhow::Result<Vec<(String, QuickFormatRecord)>> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(QUICK_FORMAT)?;
            let mut out = Vec::new();
            for item in t.iter()? {
                let (k, v) = item?;
                if let Ok(rec) = serde_json::from_slice::<QuickFormatRecord>(v.value()) {
                    out.push((k.value().to_string(), rec));
                }
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        // 每个测试独立文件：redb 是单写者，共用文件会让并发测试互相阻塞
        let p = std::env::temp_dir().join(format!(
            "wind_quick_format_test_{}_{:?}.redb",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&p);
        (Store::open(&p).unwrap(), p)
    }

    #[test]
    fn move_then_read_back() {
        let (s, p) = store();
        s.move_quick_format("date", "date.lunar", 0).unwrap();
        let rec = s.get_quick_format("date").unwrap().unwrap();
        assert_eq!(rec.moved.len(), 1);
        assert_eq!(rec.moved[0].id, "date.lunar");
        assert_eq!(rec.moved[0].position, 0);
        let _ = std::fs::remove_file(&p);
    }

    /// ★ LIFO：同一条格式再次移动时顶替旧规则，不是叠加两条。
    #[test]
    fn moving_same_entry_twice_replaces_not_appends() {
        let (s, p) = store();
        s.move_quick_format("date", "date.lunar", 3).unwrap();
        s.move_quick_format("date", "date.lunar", 0).unwrap();
        let rec = s.get_quick_format("date").unwrap().unwrap();
        assert_eq!(rec.moved.len(), 1, "同 id 只留最新一条");
        assert_eq!(rec.moved[0].position, 0);
        let _ = std::fs::remove_file(&p);
    }

    /// 最新的规则排在 index 0（应用时逆序遍历，故它最后生效、优先级最高）。
    #[test]
    fn newest_move_is_first() {
        let (s, p) = store();
        s.move_quick_format("date", "a", 1).unwrap();
        s.move_quick_format("date", "b", 2).unwrap();
        let rec = s.get_quick_format("date").unwrap().unwrap();
        assert_eq!(rec.moved[0].id, "b", "最新的在队首");
        assert_eq!(rec.moved[1].id, "a");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn disable_and_reenable() {
        let (s, p) = store();
        s.set_quick_format_enabled("date", "date.basic", false)
            .unwrap();
        assert_eq!(
            s.get_quick_format("date").unwrap().unwrap().disabled,
            vec!["date.basic".to_string()]
        );
        s.set_quick_format_enabled("date", "date.basic", true)
            .unwrap();
        // 记录变空 → 整条键被删除，而不是留一条空记录
        assert!(s.get_quick_format("date").unwrap().is_none());
        let _ = std::fs::remove_file(&p);
    }

    /// 重复停用同一条不产生重复项。
    #[test]
    fn disabling_twice_is_idempotent() {
        let (s, p) = store();
        s.set_quick_format_enabled("date", "x", false).unwrap();
        s.set_quick_format_enabled("date", "x", false).unwrap();
        assert_eq!(
            s.get_quick_format("date").unwrap().unwrap().disabled.len(),
            1
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reset_entry_clears_both_move_and_disable() {
        let (s, p) = store();
        s.move_quick_format("date", "x", 0).unwrap();
        s.set_quick_format_enabled("date", "x", false).unwrap();
        s.move_quick_format("date", "y", 1).unwrap();
        s.reset_quick_format_entry("date", "x").unwrap();
        let rec = s.get_quick_format("date").unwrap().unwrap();
        assert!(!rec.has_rule("x"), "x 的调整应清干净");
        assert!(rec.has_rule("y"), "不该连累其它条目");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reset_kind_clears_everything() {
        let (s, p) = store();
        s.move_quick_format("date", "x", 0).unwrap();
        s.set_quick_format_enabled("date", "y", false).unwrap();
        s.move_quick_format("number", "z", 0).unwrap();
        s.reset_quick_format_kind("date").unwrap();
        assert!(s.get_quick_format("date").unwrap().is_none());
        assert!(
            s.get_quick_format("number").unwrap().is_some(),
            "只清本类，别的类不动"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// ★ 键只到类别，不带方案/输入码——这正是本表与 shadow 的分界。
    #[test]
    fn kinds_are_independent_and_global() {
        let (s, p) = store();
        s.move_quick_format("date", "date.lunar", 0).unwrap();
        s.move_quick_format("number", "number.thousands", 0)
            .unwrap();
        let all = s.list_quick_format().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "date", "按 kind 升序");
        assert_eq!(all[1].0, "number");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_kind_reads_none() {
        let (s, p) = store();
        assert!(s.get_quick_format("date").unwrap().is_none());
        assert!(s.list_quick_format().unwrap().is_empty());
        let _ = std::fs::remove_file(&p);
    }
}
