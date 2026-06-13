//! 引擎管理器
//!
//! 与 Go 版本 `wind_input/internal/engine/manager.go` 对齐。
//!
//! 职责：
//! - 预加载所有可用方案的词典与引擎（Pinyin / CodeTable）
//! - 持有当前活跃方案，支持运行时切换 / 循环切换
//! - 将 `convert` 请求分发到当前引擎
//!
//! 词典加载逻辑从原 `wind_service::bridge_impl` 下沉至此，使引擎层自洽。

use crate::codetable::CodeTableEngine;
use crate::engine::{ConvertResult, Engine, EngineType};
use crate::pinyin::{Config as PinyinConfig, PinyinEngine};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use tracing::{info, warn};
use wind_config::Config;
use wind_dict::cached::CachedDict;
use wind_dict::codetable::CodetableDict;

/// schema 文件结构（兼容 Go `.schema.toml` 与遗留 `.schema.yaml`）
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct SchemaFile {
    #[serde(default)]
    engine: EngineSection,
    #[serde(default)]
    dictionaries: Vec<DictEntry>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct EngineSection {
    #[serde(rename = "type", default)]
    engine_type: String,
    #[serde(default)]
    codetable: CodetableSection,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct CodetableSection {
    #[serde(default)]
    max_code_length: usize,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct DictEntry {
    #[serde(default)]
    path: String,
    #[serde(rename = "type", default)]
    dict_type: String,
    #[serde(default)]
    default: bool,
}

impl SchemaFile {
    /// 是否为拼音类型引擎
    fn is_pinyin(&self) -> bool {
        let t = self.engine.engine_type.to_lowercase();
        if t == "pinyin" {
            return true;
        }
        if t == "codetable" || t == "mixed" {
            return false;
        }
        // engine.type 缺省时，依据默认词典类型判定
        let default = self
            .dictionaries
            .iter()
            .find(|d| d.default)
            .or_else(|| self.dictionaries.first());
        matches!(default, Some(d) if d.dict_type == "rime_pinyin")
    }
}

/// 引擎管理器
pub struct EngineManager {
    /// schema_id -> 引擎实例（构造后只读）
    engines: HashMap<String, Box<dyn Engine>>,
    /// 当前活跃方案 ID（Mutex 支持运行时切换）
    active: Mutex<String>,
    /// 可用方案列表（用于循环切换）
    available: Vec<String>,
}

impl EngineManager {
    /// 从配置预加载所有可用方案的引擎
    pub fn new(config: &Config, data_dir: Option<&Path>) -> Self {
        let active_id = config.active_schema().to_string();
        let mut available = config.schema.available.clone();
        if available.is_empty() {
            available.push(active_id.clone());
        }

        let mut engines: HashMap<String, Box<dyn Engine>> = HashMap::new();
        for sid in &available {
            match Self::build_engine(sid, data_dir) {
                Some(engine) => {
                    info!(
                        "Pre-loaded engine: {} (type={:?})",
                        sid,
                        engine.engine_type()
                    );
                    engines.insert(sid.clone(), engine);
                }
                None => warn!("Failed to build engine for schema: {}", sid),
            }
        }

        // 确保活跃方案已加载
        if !engines.contains_key(&active_id) {
            if let Some(engine) = Self::build_engine(&active_id, data_dir) {
                engines.insert(active_id.clone(), engine);
            }
        }

        Self {
            engines,
            active: Mutex::new(active_id),
            available,
        }
    }

    /// 当前活跃方案 ID
    pub fn active_schema_id(&self) -> String {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 当前活跃引擎是否为拼音类型
    pub fn is_pinyin(&self) -> bool {
        let id = self.active_schema_id();
        self.engines
            .get(&id)
            .map(|e| e.engine_type() == EngineType::Pinyin)
            .unwrap_or(false)
    }

    /// 可用方案列表
    pub fn available_schemas(&self) -> &[String] {
        &self.available
    }

    /// 切换到指定方案；成功返回 true
    pub fn switch_schema(&self, schema_id: &str) -> bool {
        if !self.engines.contains_key(schema_id) {
            warn!("Schema not loaded: {}", schema_id);
            return false;
        }
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if *active == schema_id {
            return false;
        }
        info!("Switching schema: {} -> {}", *active, schema_id);
        *active = schema_id.to_string();
        true
    }

    /// 循环切换到下一个【已成功加载】的方案；返回新方案 ID。
    /// 单次加锁完成 read-modify-write，避免 TOCTOU 竞争；跳过构建失败/未加载的方案，
    /// 否则当 available 中夹杂未加载方案（如 mixed/shuangpin）时按键会"无反应"。
    pub fn cycle_schema(&self) -> Option<String> {
        let n = self.available.len();
        if n <= 1 {
            return None;
        }
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let cur = self
            .available
            .iter()
            .position(|s| s == active.as_str())
            .unwrap_or(0);
        // 从下一个开始环形查找首个已加载方案
        for step in 1..n {
            let cand = &self.available[(cur + step) % n];
            if cand != active.as_str() && self.engines.contains_key(cand) {
                let next = cand.clone();
                info!("Cycling schema: {} -> {}", *active, next);
                *active = next.clone();
                return Some(next);
            }
        }
        None
    }

    /// 转换输入为候选（分发到当前引擎）
    pub fn convert(&self, input: &str, max_candidates: usize) -> ConvertResult {
        let id = self.active_schema_id();
        match self.engines.get(&id) {
            Some(engine) => engine
                .convert(input, max_candidates)
                .unwrap_or_else(|e| {
                    warn!("convert error: {}", e);
                    ConvertResult::default()
                }),
            None => ConvertResult::default(),
        }
    }

    // ───────────────────────── 词典加载 ─────────────────────────

    /// 为指定 schema 构建引擎
    ///
    /// schema 文件优先读取 Go 规范格式 `{id}.schema.toml`，回退到遗留 `{id}.schema.yaml`。
    fn build_engine(schema_id: &str, data_dir: Option<&Path>) -> Option<Box<dyn Engine>> {
        let data_dir = data_dir?;
        let schemas = data_dir.join("schemas");
        let toml_path = schemas.join(format!("{}.schema.toml", schema_id));
        let yaml_path = schemas.join(format!("{}.schema.yaml", schema_id));

        let schema: SchemaFile = if toml_path.exists() {
            let content = std::fs::read_to_string(&toml_path).ok()?;
            match toml::from_str(&content) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Parse schema TOML failed {}: {}", toml_path.display(), e);
                    return None;
                }
            }
        } else if yaml_path.exists() {
            let content = std::fs::read_to_string(&yaml_path).ok()?;
            match serde_yaml::from_str(&content) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Parse schema YAML failed {}: {}", yaml_path.display(), e);
                    return None;
                }
            }
        } else {
            warn!("Schema file not found: {}.schema.toml/.yaml", schema_id);
            return None;
        };

        let dict = match Self::load_dictionary(&schema, &schemas) {
            Some(d) => d,
            None => {
                warn!("load_dictionary returned None for schema {}", schema_id);
                return None;
            }
        };

        if schema.is_pinyin() {
            Some(Box::new(PinyinEngine::new(PinyinConfig::default(), dict)))
        } else {
            let mcl = if schema.engine.codetable.max_code_length > 0 {
                schema.engine.codetable.max_code_length
            } else {
                4
            };
            Some(Box::new(CodeTableEngine::new(mcl, dict)))
        }
    }

    /// 加载 schema 的默认词典
    fn load_dictionary(schema: &SchemaFile, schemas_dir: &Path) -> Option<CachedDict> {
        let entry = schema
            .dictionaries
            .iter()
            .find(|d| d.default)
            .or_else(|| schema.dictionaries.first())?;
        if entry.path.is_empty() {
            warn!("Default dictionary has empty path");
            return None;
        }
        let dict_type = if entry.dict_type.is_empty() {
            "rime_codetable"
        } else {
            entry.dict_type.as_str()
        };
        let full_path = schemas_dir.join(&entry.path);
        info!("Loading dictionary: {} (type={})", full_path.display(), dict_type);

        match dict_type {
            "rime_pinyin" => Self::load_rime_pinyin_dict(&full_path),
            _ => match CachedDict::load(&full_path) {
                Ok(dict) => {
                    info!("Dictionary loaded: {} entries", dict.len());
                    Some(dict)
                }
                Err(e) => {
                    warn!("Failed to load dictionary: {}", e);
                    None
                }
            },
        }
    }

    /// 加载 rime_pinyin 词典（合并 import_tables 子词典到 .merged.wdb）
    fn load_rime_pinyin_dict(dict_path: &Path) -> Option<CachedDict> {
        let merged_wdb = dict_path.with_extension("merged.wdb");
        if merged_wdb.exists() {
            match wind_dict::binformat::DictReader::open(&merged_wdb) {
                Ok(reader) => {
                    info!(
                        "Using merged mmap cache: {} ({} keys)",
                        merged_wdb.display(),
                        reader.key_count()
                    );
                    return Some(CachedDict::Mmap(reader));
                }
                Err(e) => {
                    warn!("Stale merged cache ({}), regenerating", e);
                    let _ = std::fs::remove_file(&merged_wdb);
                }
            }
        }

        let content = std::fs::read_to_string(dict_path).ok()?;
        let yaml_section = if let Some(start) = content.find("---") {
            let after = &content[start + 3..];
            after.find("...").map(|end| &after[..end]).unwrap_or(after)
        } else {
            &content
        };
        let yaml: serde_yaml::Value = serde_yaml::from_str(yaml_section).ok()?;
        let dict_dir = dict_path.parent()?;

        let mut writer = wind_dict::binformat::DictWriter::new();
        let mut total_entries = 0usize;

        let mut sub_paths = vec![dict_path.to_path_buf()];
        if let Some(import_tables) = yaml.get("import_tables").and_then(|v| v.as_sequence()) {
            for table_ref in import_tables {
                if let Some(name) = table_ref.as_str() {
                    let sub = dict_dir.join(format!("{}.dict.yaml", name));
                    if sub.exists() {
                        sub_paths.push(sub);
                    }
                }
            }
        }

        // 按 code 聚合所有子词典条目。DictWriter::add 不会合并同 code 的多次调用，
        // 若每条目单独 add，会在 wdb 中写出重复 KeyIndex，DictReader 的二分查找只能命中
        // 其中之一，导致同 code 的其余候选系统性丢失（拼音词典尤甚）。
        let mut agg: HashMap<String, Vec<(String, i32)>> = HashMap::new();
        for sub_path in &sub_paths {
            match CachedDict::load(sub_path) {
                Ok(sub_dict) => {
                    let count = sub_dict.len();
                    info!("  Loading {} entries from {}", count, sub_path.display());
                    for (code, text, weight, _order) in sub_dict.search_prefix("", 5_000_000) {
                        agg.entry(code).or_default().push((text, weight));
                    }
                    total_entries += count;
                }
                Err(e) => warn!("  Failed to load {}: {}", sub_path.display(), e),
            }
        }

        if total_entries == 0 {
            warn!("No entries loaded from pinyin dictionary");
            return None;
        }

        for (code, mut entries) in agg {
            // 同 code 下按权重降序，保证 KeyIndex 内候选顺序稳定
            entries.sort_by(|a, b| b.1.cmp(&a.1));
            writer.add(code, entries);
        }

        info!("Writing merged .wdb cache ({} entries)...", total_entries);
        match writer.write(&merged_wdb) {
            Ok(_) => match wind_dict::binformat::DictReader::open(&merged_wdb) {
                Ok(reader) => {
                    info!("Using merged mmap cache ({} keys)", reader.key_count());
                    Some(CachedDict::Mmap(reader))
                }
                Err(e) => {
                    warn!("Failed to open merged cache: {}", e);
                    CodetableDict::load(dict_path).ok().map(CachedDict::Memory)
                }
            },
            Err(e) => {
                warn!("Failed to write merged cache: {}", e);
                CodetableDict::load(dict_path).ok().map(CachedDict::Memory)
            }
        }
    }
}
