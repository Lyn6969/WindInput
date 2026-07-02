//! Store 核心：基于 redb 的持久化存储（骨架）
//!
//! 与 Go 版本 `wind_input/internal/store/store.go`（bbolt）对齐，但用 redb。
//! 见 docs/redesign/store.md：redb 无嵌套 bucket，用扁平 table + schema 前缀复合 key。
//!
//! 本提交为**骨架**：open / 表定义 / 事务封装 / pause-resume（Windows 热替换释放文件锁）/
//! version + 迁移框架。用户词/临时词/词频/shadow 的具体 ops 在后续提交按 store.md §10.2 实现。

use redb::{Database, TableDefinition};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{info, warn};

/// 当前存储版本（迁移锚点）
pub const CURRENT_VERSION: u32 = 1;

// ── 表定义（key 编码见 store.md §2：复合 key 带 schema 前缀，redb 扁平）──
/// 用户词：key = "{schema}\0{code}\0{text}"，value = 序列化记录
pub(crate) const USER_WORDS: TableDefinition<&str, &[u8]> = TableDefinition::new("user_words");
/// 临时词：同上
pub(crate) const TEMP_WORDS: TableDefinition<&str, &[u8]> = TableDefinition::new("temp_words");
/// 用户词频：key = "{schema}\0{code}\0{text}"，value = {count,last_used}（见 frequency.md）
pub(crate) const FREQ: TableDefinition<&str, &[u8]> = TableDefinition::new("freq");
/// Shadow 规则：key = "{schema}\0{code}"
pub(crate) const SHADOW: TableDefinition<&str, &[u8]> = TableDefinition::new("shadow");
/// 全局短语：key = "{code}\0{text}"
pub(crate) const PHRASES: TableDefinition<&str, &[u8]> = TableDefinition::new("phrases");
/// 每日统计：key = "YYYY-MM-DD"
pub(crate) const STATS_DAILY: TableDefinition<&str, &[u8]> = TableDefinition::new("stats_daily");
/// 元数据：version / device_id 等
pub(crate) const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

const META_VERSION_KEY: &str = "schema_version";

/// 存储引擎（redb）。`db` 为 None 表示已暂停（pause，释放文件锁供热替换）。
pub struct Store {
    path: PathBuf,
    db: Mutex<Option<Database>>,
}

impl Store {
    /// 打开数据库：创建/打开 redb，建表，运行版本迁移。
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Database::create(&path)?;
        Self::init_tables(&db)?;
        let store = Self {
            path,
            db: Mutex::new(Some(db)),
        };
        store.run_migrations()?;
        info!(
            "Store opened: {} (v{})",
            store.path.display(),
            store.version().unwrap_or(0)
        );
        Ok(store)
    }

    /// 建表（首次打开表即创建；幂等）。
    fn init_tables(db: &Database) -> anyhow::Result<()> {
        let w = db.begin_write()?;
        {
            w.open_table(USER_WORDS)?;
            w.open_table(TEMP_WORDS)?;
            w.open_table(FREQ)?;
            w.open_table(SHADOW)?;
            w.open_table(PHRASES)?;
            w.open_table(STATS_DAILY)?;
            w.open_table(META)?;
        }
        w.commit()?;
        Ok(())
    }

    /// 在持有 db 的前提下执行闭包；暂停态返回错误。各模块 ops（user_words/temp_words…）经此访问 db。
    pub(crate) fn with_db<R>(
        &self,
        f: impl FnOnce(&Database) -> anyhow::Result<R>,
    ) -> anyhow::Result<R> {
        let guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(db) => f(db),
            None => anyhow::bail!("store is paused"),
        }
    }

    /// 读取存储版本（无 version 键视为 0=全新库）。
    pub fn version(&self) -> anyhow::Result<u32> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(META)?;
            let v = match t.get(META_VERSION_KEY)? {
                Some(g) => {
                    let b = g.value();
                    if b.len() == 4 {
                        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
                    } else {
                        0
                    }
                }
                None => 0,
            };
            Ok(v)
        })
    }

    /// 读 META 表的字符串值（UTF-8）。
    pub(crate) fn meta_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(META)?;
            Ok(t.get(key)?
                .map(|g| String::from_utf8_lossy(g.value()).into_owned()))
        })
    }

    /// 写 META 表的字符串值。
    pub(crate) fn meta_set(&self, key: &str, val: &str) -> anyhow::Result<()> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(META)?;
                t.insert(key, val.as_bytes())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    fn set_version(&self, v: u32) -> anyhow::Result<()> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(META)?;
                let vb = v.to_le_bytes();
                t.insert(META_VERSION_KEY, vb.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 版本迁移框架：全新库直接打版本号；旧库按版本链逐步迁移（当前无迁移步骤）。
    fn run_migrations(&self) -> anyhow::Result<()> {
        let mut v = self.version()?;
        if v == 0 {
            // 全新 redb 库（Go 用 bbolt，此处不存在 legacy redb 数据）→ 直接标当前版本。
            self.set_version(CURRENT_VERSION)?;
            return Ok(());
        }
        while v < CURRENT_VERSION {
            // 预留：match v { 1 => migrate_v1_to_v2()?, .. }
            v += 1;
            self.set_version(v)?;
        }
        if v > CURRENT_VERSION {
            warn!(
                "Store version {} 高于支持的 {}（程序可能被回滚）",
                v, CURRENT_VERSION
            );
        }
        Ok(())
    }

    /// 暂停：丢弃 Database，释放文件锁（Windows 下原子热替换 .redb 前调用）。
    pub fn pause(&self) -> anyhow::Result<()> {
        let mut guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
        info!("Store paused: {}", self.path.display());
        Ok(())
    }

    /// 恢复：重新打开 Database（暂停后调用）。
    pub fn resume(&self) -> anyhow::Result<()> {
        let mut guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let db = Database::create(&self.path)?;
            Self::init_tables(&db)?;
            *guard = Some(db);
            info!("Store resumed: {}", self.path.display());
        }
        Ok(())
    }

    /// 是否处于暂停态
    pub fn is_paused(&self) -> bool {
        self.db.lock().unwrap_or_else(|e| e.into_inner()).is_none()
    }

    /// 数据库路径
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_version_persist_and_reopen() {
        let path = std::env::temp_dir().join("wind_store_skeleton_test.redb");
        let _ = std::fs::remove_file(&path);

        // 首次打开：版本应为当前版本
        {
            let s = Store::open(&path).unwrap();
            assert_eq!(s.version().unwrap(), CURRENT_VERSION);
        }
        // 重开：版本持久化（证明写事务落盘）
        {
            let s = Store::open(&path).unwrap();
            assert_eq!(s.version().unwrap(), CURRENT_VERSION);
            // pause/resume 往返：暂停态报错，恢复后可用
            s.pause().unwrap();
            assert!(s.is_paused());
            assert!(s.version().is_err(), "暂停态读取应失败");
            s.resume().unwrap();
            assert!(!s.is_paused());
            assert_eq!(s.version().unwrap(), CURRENT_VERSION);
        }
        let _ = std::fs::remove_file(&path);
    }
}
