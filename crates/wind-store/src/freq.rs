//! 频率存储（含异步批处理）
//!
//! 与 Go 版本 `wind_input/internal/store/freq.go` 对齐。
//! 运行时词频跟踪 + 异步持久化。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// 频率记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreqRecord {
    pub count: u32,
    pub last_used: String,
    pub streak: u32,
}

/// 频率配置
#[derive(Debug, Clone)]
pub struct FreqProfile {
    pub base_scale: f64,
    pub max_recency: f64,
    pub lambda: f64,
    pub streak_scale: f64,
    pub streak_cap: f64,
    pub boost_max: f64,
}

impl Default for FreqProfile {
    fn default() -> Self {
        Self {
            base_scale: 100.0,
            max_recency: 50.0,
            lambda: 0.1,
            streak_scale: 10.0,
            streak_cap: 200.0,
            boost_max: 500.0,
        }
    }
}

impl FreqProfile {
    /// 计算频率提升值
    pub fn calc_boost(&self, count: u32, age_hours: f64, streak: u32) -> f64 {
        let base = ((count + 1) as f64).log2() * self.base_scale;
        let recency = self.max_recency * (-self.lambda * age_hours).exp();
        let streak_val = (streak as f64 * self.streak_scale).min(self.streak_cap);
        (base + recency + streak_val).min(self.boost_max)
    }
}

/// 运行时词频跟踪器
///
/// 跟踪用户选择的词频，用于实时调整候选排序。
/// 异步批量写入持久化存储。
pub struct FreqTracker {
    /// word -> 选择次数（运行时）
    freq_map: RwLock<HashMap<String, u32>>,
    /// 配置
    profile: FreqProfile,
}

impl FreqTracker {
    pub fn new() -> Self {
        Self {
            freq_map: RwLock::new(HashMap::new()),
            profile: FreqProfile::default(),
        }
    }

    /// 记录一次词选择
    pub fn record_selection(&self, word: &str) {
        let mut map = self.freq_map.write().unwrap();
        let entry = map.entry(word.to_string()).or_insert(0);
        *entry += 1;
    }

    /// 获取词的频率 boost 值
    pub fn get_boost(&self, word: &str) -> f64 {
        let map = self.freq_map.read().unwrap();
        let count = *map.get(word).unwrap_or(&0);
        if count == 0 {
            return 0.0;
        }
        // 简化计算：只用 count，不考虑时间和 streak
        ((count + 1) as f64).log2() * self.profile.base_scale * 0.1
    }

    /// 获取词的选择次数
    pub fn get_count(&self, word: &str) -> u32 {
        let map = self.freq_map.read().unwrap();
        *map.get(word).unwrap_or(&0)
    }

    /// 是否包含某词
    pub fn contains(&self, word: &str) -> bool {
        let map = self.freq_map.read().unwrap();
        map.contains_key(word)
    }

    /// 获取所有频率记录（用于持久化）
    pub fn export_records(&self) -> Vec<(String, u32)> {
        let map = self.freq_map.read().unwrap();
        map.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// 从持久化数据加载
    pub fn import_records(&self, records: &[(String, u32)]) {
        let mut map = self.freq_map.write().unwrap();
        for (word, count) in records {
            map.insert(word.clone(), *count);
        }
    }

    /// 清空运行时频率（用于测试）
    pub fn clear(&self) {
        let mut map = self.freq_map.write().unwrap();
        map.clear();
    }

    /// 从文件加载词频（格式：`word\tcount`，每行一条）。文件不存在时静默忽略。
    pub fn load_from_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut records = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split('\t');
            if let (Some(w), Some(c)) = (it.next(), it.next()) {
                if let Ok(count) = c.trim().parse::<u32>() {
                    if !w.is_empty() && count > 0 {
                        records.push((w.to_string(), count));
                    }
                }
            }
        }
        self.import_records(&records);
        Ok(())
    }

    /// 将词频保存到文件（原子写：先写临时文件再 rename）。
    pub fn save_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        {
            let map = self.freq_map.read().unwrap_or_else(|e| e.into_inner());
            for (word, count) in map.iter() {
                if *count > 0 {
                    out.push_str(word);
                    out.push('\t');
                    out.push_str(&count.to_string());
                    out.push('\n');
                }
            }
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, out.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 当前记录条数
    pub fn len(&self) -> usize {
        self.freq_map.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_freq_save_load_roundtrip() {
        let tmp = std::env::temp_dir().join("wind_freq_roundtrip.tsv");
        let _ = std::fs::remove_file(&tmp);

        let a = FreqTracker::new();
        a.record_selection("你好");
        a.record_selection("你好");
        a.record_selection("中国");
        a.save_to_file(&tmp).unwrap();

        // 新 tracker 从文件加载，计数应保留（重启不丢）
        let b = FreqTracker::new();
        b.load_from_file(&tmp).unwrap();
        assert_eq!(b.get_count("你好"), 2);
        assert_eq!(b.get_count("中国"), 1);
        assert!(b.get_boost("你好") > 0.0);

        // 不存在的文件静默成功
        let c = FreqTracker::new();
        assert!(c.load_from_file(std::path::Path::new("/nonexistent/x.tsv")).is_ok());

        let _ = std::fs::remove_file(&tmp);
    }
}
