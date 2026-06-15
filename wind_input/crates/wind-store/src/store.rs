//! Store 核心：基于 redb 的持久化存储
//!
//! 与 Go 版本 `wind_input/internal/store/store.go` 对齐。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tracing::info;

/// 存储引擎
pub struct Store {
    path: PathBuf,
    // TODO: redb database handle
    freq_deltas: Arc<RwLock<std::collections::HashMap<String, i32>>>,
}

impl Store {
    /// 打开数据库
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        info!("Opening store at {:?}", path);
        // TODO: 打开 redb 数据库，运行迁移
        Ok(Self {
            path,
            freq_deltas: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// 暂停数据库（释放文件锁，用于热替换）
    pub fn pause(&self) -> anyhow::Result<()> {
        // TODO
        Ok(())
    }

    /// 恢复数据库
    pub fn resume(&self) -> anyhow::Result<()> {
        // TODO
        Ok(())
    }

    /// 获取数据库路径
    pub fn path(&self) -> &Path {
        &self.path
    }
}
