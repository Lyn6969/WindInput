//! 热缓存：单字母前缀的预聚合 top-K
//!
//! 与 Go 版本 `wind_input/internal/dict/hotcache/` 对齐。
//! 单字母前缀查询（如 LookupPrefix("s", 200)）非常昂贵——
//! 'z' 子树有 ~47k 候选。热缓存提供进程级缓存。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// 缓存条目：每个首字母一个
struct CacheEntry {
    slots: Vec<OnceLock<Vec<CachedCandidate>>>,
}

/// 缓存的候选词
#[derive(Debug, Clone)]
pub struct CachedCandidate {
    pub text: String,
    pub code: String,
    pub weight: i32,
    pub order: i32,
}

/// 热缓存管理器
pub struct HotCache {
    /// 文件键 -> 缓存条目
    entries: RwLock<HashMap<String, CacheEntry>>,
    /// 每个前缀缓存的最大候选数（hotcache 接线时启用，见 dict.md §5.4）
    #[allow(dead_code)]
    max_per_prefix: usize,
}

impl Default for HotCache {
    fn default() -> Self {
        Self::new()
    }
}

impl HotCache {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            max_per_prefix: 500,
        }
    }

    /// 获取或构建缓存
    ///
    /// - `file_key`: 文件标识（路径+大小+mtime）
    /// - `first_byte`: 首字母（如 's'）
    /// - `build`: 构建函数，返回该前缀的 top-K 候选
    pub fn get_or_build<F>(&self, file_key: &str, first_byte: u8, build: F) -> Vec<CachedCandidate>
    where
        F: FnOnce() -> Vec<CachedCandidate>,
    {
        let mut entries = self.entries.write().unwrap();
        let entry = entries
            .entry(file_key.to_string())
            .or_insert_with(|| CacheEntry {
                slots: (0..256).map(|_| OnceLock::new()).collect(),
            });

        let slot = &entry.slots[first_byte as usize];
        let result = slot.get_or_init(build);
        result.clone()
    }

    /// 清除指定文件的缓存
    pub fn invalidate(&self, file_key: &str) {
        let mut entries = self.entries.write().unwrap();
        entries.remove(file_key);
    }

    /// 清除所有缓存
    pub fn clear(&self) {
        let mut entries = self.entries.write().unwrap();
        entries.clear();
    }
}

/// 全局热缓存实例
static GLOBAL_HOT_CACHE: OnceLock<HotCache> = OnceLock::new();

/// 获取全局热缓存
pub fn global_hot_cache() -> &'static HotCache {
    GLOBAL_HOT_CACHE.get_or_init(HotCache::new)
}
