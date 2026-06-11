//! Rime Codetable 词典读取器 (.dict.yaml 格式)
//!
//! 格式：YAML 头部 + TSV 正文（code\ttext\tweight）
//! 与 Go 版 `wind_input/internal/dict/codetable/` 对齐。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use tracing::info;

/// 词典条目
#[derive(Debug, Clone)]
pub struct CodetableEntry {
    pub text: String,
    pub weight: i32,
    pub order: i32,
}

/// Rime Codetable 词典（内存模式，按 code 分组的 BTreeMap）
pub struct CodetableDict {
    /// code -> entries（按 weight 降序排列）
    entries: BTreeMap<String, Vec<CodetableEntry>>,
    /// 总条目数
    total_entries: usize,
}

impl CodetableDict {
    /// 从 .dict.yaml 文件加载
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)?;

        let mut entries: BTreeMap<String, Vec<CodetableEntry>> = BTreeMap::new();
        let mut in_body = false;
        let mut order: i32 = 0;

        for line in content.lines() {
            if !in_body {
                if line == "..." {
                    in_body = true;
                }
                continue;
            }

            // 跳过空行和注释
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // 格式: code\ttext\tweight
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }

            let code = parts[0].to_string();
            let text = parts[1].to_string();
            let weight: i32 = if parts.len() >= 3 {
                parts[2].parse().unwrap_or(0)
            } else {
                0
            };

            let entry = CodetableEntry {
                text,
                weight,
                order,
            };

            entries.entry(code).or_default().push(entry);
            order += 1;
        }

        // 每个 code 下按 weight 降序排列
        for code_entries in entries.values_mut() {
            code_entries.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.order.cmp(&b.order)));
        }

        let total: usize = entries.values().map(|v| v.len()).sum();
        info!(
            "Loaded codetable: {} ({} keys, {} entries)",
            path.display(),
            entries.len(),
            total
        );

        Ok(Self {
            entries,
            total_entries: total,
        })
    }

    /// 精确查找
    pub fn search(&self, code: &str) -> Vec<(String, i32, i32)> {
        self.entries
            .get(code)
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| (e.text.clone(), e.weight, e.order))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 前缀查找
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, String, i32, i32)> {
        let mut results = Vec::new();

        // BTreeMap 范围查询：找到所有以 prefix 开头的 key
        let range = self.entries.range(prefix.to_string()..);
        for (code, entries) in range {
            if !code.starts_with(prefix) {
                break;
            }
            for e in entries {
                results.push((code.clone(), e.text.clone(), e.weight, e.order));
            }
            if results.len() >= limit * 2 {
                break; // 收集足够多后排序截断
            }
        }

        // 按 weight 降序排序
        results.sort_by(|a, b| b.2.cmp(&a.2).then(a.3.cmp(&b.3)));
        results.truncate(limit);
        results
    }

    /// 总条目数
    pub fn len(&self) -> usize {
        self.total_entries
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.total_entries == 0
    }
}
