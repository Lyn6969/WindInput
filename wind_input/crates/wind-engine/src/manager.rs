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
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use wind_config::Config;
use wind_config::schema::{DictSpec, Schema};
use wind_dict::cached::CachedDict;

// 方案定义已统一到 wind_config::schema::Schema（取代此前的私有 SchemaFile）。
// 引擎只消费该共享类型；构建逻辑（build_engine）保持不变。

/// 码表词频应用策略（见 docs/redesign/frequency.md §3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreqStrategy {
    /// 一次到顶（MRU）：last_used 优先，最近选的置该档之首。
    Top,
    /// 逐次提升（默认）：count 优先，累积使用才爬升，抗误选。
    Step,
}

/// 活跃方案的词频排序设置（apply_freq_rerank 用）。
/// 按方案解析后缓存，避免每键读盘（frequency.md §8）。
#[derive(Debug, Clone, Copy)]
pub struct FreqSettings {
    /// 词频维度主开关（learning.freq.enabled）；关则完全不重排。
    pub enabled: bool,
    /// used-first 内的排序策略（engine.codetable.freq_strategy）。
    pub strategy: FreqStrategy,
}

impl Default for FreqSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: FreqStrategy::Step,
        }
    }
}

/// 引擎管理器（懒加载：仅在需要时构建对应方案引擎，降低启动内存）
pub struct EngineManager {
    /// schema_id -> 引擎实例（懒加载，Arc 便于无锁 convert）
    engines: Mutex<HashMap<String, Arc<dyn Engine>>>,
    /// 当前活跃方案 ID
    active: Mutex<String>,
    /// 可用方案列表（已过滤不支持的方案，用于循环切换）
    available: Vec<String>,
    /// 数据目录（懒加载时按需读取 schema）
    data_dir: Option<std::path::PathBuf>,
    /// redb 持久化存储（用户词/临时词层；None=无持久化，如纯测试/REPL）
    store: Option<Arc<wind_store::Store>>,
    /// 全码/空码上屏策略全局默认（方案级 tri-state 未设时回退至此）
    code_commit: wind_config::CodeCommitConfig,
    /// 词频排序设置缓存（schema_id -> FreqSettings；按需解析、避免每键读盘）
    freq_cache: Mutex<HashMap<String, FreqSettings>>,
}

/// 进程级缓存根目录（%LOCALAPPDATA%\WindInput\cache），EngineManager::new 设置一次。
static CACHE_DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

/// 源文件 → 缓存 .wdb 路径：放缓存根下（名字含父目录名，避免跨方案同名冲突）；
/// 未设置缓存根时回退到源旁（保持旧行为，便于测试/无 LOCALAPPDATA 场景）。
fn cache_path(source: &Path, ext: &str) -> std::path::PathBuf {
    if let Some(Some(dir)) = CACHE_DIR.get() {
        let parent = source
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = if parent.is_empty() {
            format!("{}.{}", stem, ext)
        } else {
            format!("{}_{}.{}", parent, stem, ext)
        };
        return dir.join(name);
    }
    source.with_extension(ext)
}

impl EngineManager {
    /// 从配置创建；仅构建活跃方案引擎，其余按需懒加载。
    pub fn new(config: &Config, data_dir: Option<&Path>) -> Self {
        Self::with_store(config, data_dir, None)
    }

    /// 同 [`new`]，但注入 redb 存储以注册用户词/临时词层（coordinator 用）。
    pub fn with_store(
        config: &Config,
        data_dir: Option<&Path>,
        store: Option<Arc<wind_store::Store>>,
    ) -> Self {
        // 初始化缓存根（一次）：%LOCALAPPDATA%\WindInput\cache，提前建好目录
        CACHE_DIR.get_or_init(|| {
            let dir = Config::cache_dir();
            if let Some(d) = &dir {
                let _ = std::fs::create_dir_all(d);
            }
            dir
        });

        let active_id = config.active_schema().to_string();
        let mut available = config.schema.available.clone();
        if available.is_empty() {
            available.push(active_id.clone());
        }
        // 过滤不支持的方案（如双拼），但始终保留活跃方案
        available.retain(|sid| sid == &active_id || Self::schema_supported(sid, data_dir));

        let mgr = Self {
            engines: Mutex::new(HashMap::new()),
            active: Mutex::new(active_id.clone()),
            available,
            data_dir: data_dir.map(|d| d.to_path_buf()),
            store,
            code_commit: config.input.code_commit.clone(),
            freq_cache: Mutex::new(HashMap::new()),
        };
        // 仅构建活跃方案（其余懒加载）
        mgr.ensure_loaded(&active_id);
        mgr
    }

    /// 读取 schema 判断是否受支持（不构建引擎，仅解析 TOML）
    fn schema_supported(schema_id: &str, data_dir: Option<&Path>) -> bool {
        match Self::read_schema(schema_id, data_dir) {
            Some(s) => s.is_supported(),
            None => false,
        }
    }

    /// 确保指定方案引擎已加载；返回是否可用
    fn ensure_loaded(&self, schema_id: &str) -> bool {
        if self
            .engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(schema_id)
        {
            return true;
        }
        match Self::build_engine(
            schema_id,
            self.data_dir.as_deref(),
            self.store.clone(),
            &self.code_commit,
        ) {
            Some(engine) => {
                info!(
                    "Loaded engine: {} (type={:?})",
                    schema_id,
                    engine.engine_type()
                );
                self.engines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(schema_id.to_string(), Arc::from(engine));
                true
            }
            None => {
                warn!("Failed to build engine for schema: {}", schema_id);
                false
            }
        }
    }

    /// 取当前活跃引擎（必要时懒加载）
    fn active_engine(&self) -> Option<Arc<dyn Engine>> {
        let id = self.active_schema_id();
        self.ensure_loaded(&id);
        self.engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    /// 当前活跃方案 ID
    pub fn active_schema_id(&self) -> String {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 当前活跃引擎是否为拼音类型
    pub fn is_pinyin(&self) -> bool {
        self.active_engine()
            .map(|e| e.engine_type() == EngineType::Pinyin)
            .unwrap_or(false)
    }

    /// 可用方案列表
    pub fn available_schemas(&self) -> &[String] {
        &self.available
    }

    /// 切换到指定方案；成功返回 true（必要时懒加载）
    pub fn switch_schema(&self, schema_id: &str) -> bool {
        if !self.ensure_loaded(schema_id) {
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

    /// 循环切换到下一个可加载的方案；返回新方案 ID。
    /// 懒加载：在加载前不持 active 锁，避免首次加载（拼音合并/unigram）阻塞按键路径。
    pub fn cycle_schema(&self) -> Option<String> {
        let n = self.available.len();
        if n <= 1 {
            return None;
        }
        let current = self.active_schema_id();
        let cur = self
            .available
            .iter()
            .position(|s| s == &current)
            .unwrap_or(0);
        for step in 1..n {
            let cand = self.available[(cur + step) % n].clone();
            if cand == current {
                continue;
            }
            if self.ensure_loaded(&cand) {
                let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                info!("Cycling schema: {} -> {}", *active, cand);
                *active = cand.clone();
                return Some(cand);
            }
        }
        None
    }

    /// 转换输入为候选（分发到当前引擎）
    pub fn convert(&self, input: &str, max_candidates: usize) -> ConvertResult {
        match self.active_engine() {
            Some(engine) => engine.convert(input, max_candidates).unwrap_or_else(|e| {
                warn!("convert error: {}", e);
                ConvertResult::default()
            }),
            None => ConvertResult::default(),
        }
    }

    /// 当前活跃引擎类型（必要时懒加载）
    pub fn current_engine_type(&self) -> Option<EngineType> {
        self.active_engine().map(|e| e.engine_type())
    }

    /// 确保指定方案可加载（懒加载）。用于 overlay 模式（特殊模式等）激活前的可用性校验。
    pub fn ensure_schema(&self, schema_id: &str) -> bool {
        self.ensure_loaded(schema_id)
    }

    /// 顶码上屏：超过满码长时取前 N 码首选上屏，返回 (上屏文本, 剩余编码)。
    /// 仅码表/混输引擎按 top_code_commit 实现，其余返回 None。
    pub fn handle_top_code(&self, input: &str) -> Option<(String, String)> {
        self.active_engine()?.handle_top_code(input)
    }

    /// 当前活跃方案（须为码表类型）的临时拼音目标方案 id。
    /// 启用且目标方案可加载时返回 Some(target)，否则 None。
    pub fn temp_pinyin_target(&self) -> Option<String> {
        let id = self.active_schema_id();
        let schema = Self::read_schema(&id, self.data_dir.as_deref())?;
        let tp = &schema.engine.codetable.temp_pinyin;
        if !tp.enabled {
            return None;
        }
        let target = if tp.schema.is_empty() {
            "pinyin".to_string()
        } else {
            tp.schema.clone()
        };
        if self.ensure_loaded(&target) {
            Some(target)
        } else {
            None
        }
    }

    /// 活跃方案的词频排序设置（frequency.md §3/§8）。按方案解析后缓存，避免每键读盘。
    /// 读盘失败回退默认（enabled=false → 不重排）。
    pub fn freq_settings(&self) -> FreqSettings {
        let id = self.active_schema_id();
        if let Some(s) = self
            .freq_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
        {
            return *s;
        }
        let settings = Self::read_schema(&id, self.data_dir.as_deref())
            .map(|sc| Self::parse_freq_settings(&sc))
            .unwrap_or_default();
        self.freq_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, settings);
        settings
    }

    /// 从方案解析词频排序设置（纯映射，便于单测）。
    fn parse_freq_settings(sc: &Schema) -> FreqSettings {
        let strategy = match sc.engine.codetable.freq_strategy.as_str() {
            "top" => FreqStrategy::Top,
            _ => FreqStrategy::Step,
        };
        FreqSettings {
            enabled: sc.learning.freq.enabled,
            strategy,
        }
    }

    /// 用指定方案引擎转换（不改变当前活跃方案，必要时懒加载）。
    /// 用于临时拼音：码表模式下临时借用拼音引擎反查。
    pub fn convert_with(
        &self,
        schema_id: &str,
        input: &str,
        max_candidates: usize,
    ) -> ConvertResult {
        if !self.ensure_loaded(schema_id) {
            return ConvertResult::default();
        }
        let engine = self
            .engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(schema_id)
            .cloned();
        match engine {
            Some(e) => e.convert(input, max_candidates).unwrap_or_else(|err| {
                warn!("convert_with error: {}", err);
                ConvertResult::default()
            }),
            None => ConvertResult::default(),
        }
    }

    // ───────────────────────── 词典加载 ─────────────────────────

    /// 在 [用户配置/schemas, 安装/schemas] 中解析一个 schemas 相对文件路径，用户目录优先。
    /// 用户目录存在同名文件即覆盖安装目录（schema 用户覆盖；方案/词典/字根表共用）。
    fn resolve_schema_file(rel: &str, data_dir: &Path) -> std::path::PathBuf {
        if let Some(user) = Config::user_config_dir() {
            let p = user.join("schemas").join(rel);
            if p.is_file() {
                return p;
            }
        }
        data_dir.join("schemas").join(rel)
    }

    /// 读取并解析 schema 文件（仅 TOML）。用户目录优先（见 resolve_schema_file）。
    fn read_schema(schema_id: &str, data_dir: Option<&Path>) -> Option<Schema> {
        let data_dir = data_dir?;
        let toml_path = Self::resolve_schema_file(&format!("{}.schema.toml", schema_id), data_dir);
        if !toml_path.exists() {
            warn!("Schema file not found: {}.schema.toml", schema_id);
            return None;
        }
        let content = std::fs::read_to_string(&toml_path).ok()?;
        match toml::from_str(&content) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("Parse schema TOML failed {}: {}", toml_path.display(), e);
                None
            }
        }
    }

    /// 为指定 schema 构建引擎
    fn build_engine(
        schema_id: &str,
        data_dir: Option<&Path>,
        store: Option<Arc<wind_store::Store>>,
        commit: &wind_config::CodeCommitConfig,
    ) -> Option<Box<dyn Engine>> {
        let data_dir = data_dir?;
        let schemas = data_dir.join("schemas");
        let schema = Self::read_schema(schema_id, Some(data_dir))?;

        // 混输方案：递归构建主（码表）+ 次（拼音）子引擎，包装为 MixedEngine
        if schema.engine.engine_type.to_lowercase() == "mixed" {
            let m = &schema.engine.mixed;
            if m.primary_schema.is_empty() {
                warn!("mixed schema {} 缺少 primary_schema", schema_id);
                return None;
            }
            let primary =
                Self::build_engine(&m.primary_schema, Some(data_dir), store.clone(), commit)?;
            let secondary = if m.secondary_schema.is_empty() {
                None
            } else {
                Self::build_engine(&m.secondary_schema, Some(data_dir), store.clone(), commit)
            };
            let boost = if m.codetable_weight_boost > 0 {
                m.codetable_weight_boost
            } else {
                10_000_000
            };
            let min_py = if m.min_pinyin_length > 0 {
                m.min_pinyin_length
            } else {
                2
            };
            // 拼音守护：主码表方案 tri-state > 全局 input.code_commit。
            let block_on_pinyin = Self::read_schema(&m.primary_schema, Some(data_dir))
                .and_then(|s| s.engine.codetable.auto_commit_block_on_pinyin)
                .unwrap_or(commit.auto_commit_block_on_pinyin);
            info!(
                "Built mixed engine {} (primary={}, secondary={})",
                schema_id, m.primary_schema, m.secondary_schema
            );
            return Some(Box::new(crate::mixed::MixedEngine::new(
                primary,
                secondary,
                min_py,
                boost,
                block_on_pinyin,
            )));
        }

        let dict = match Self::load_dictionary(&schema, &schemas) {
            Some(d) => d,
            None => {
                warn!("load_dictionary returned None for schema {}", schema_id);
                return None;
            }
        };

        if schema.is_pinyin() {
            // 加载 unigram 语言模型（长句 Viterbi 打分）：mmap 零拷贝，失败回退词典权重。
            let unigram: Option<Arc<dyn crate::pinyin::lm::UnigramLookup>> =
                if schema.learning.unigram_path.is_empty() {
                    None
                } else {
                    let ug_txt = schemas.join(&schema.learning.unigram_path);
                    Self::load_unigram_mmap(&ug_txt)
                };
            let mut engine = PinyinEngine::with_unigram(PinyinConfig::default(), dict, unigram);
            // 注入 redb Store 时挂用户词/临时词层（L 造词显现）：让拼音造的词进候选合并。
            // 仅含 User/Temp 层（系统词典仍由引擎自身的 CachedDict 承担 Viterbi/前缀）。
            if let Some(store) = &store {
                let dm = wind_dict::DictManager::new();
                dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
                    store.clone(),
                    schema_id,
                )));
                dm.register_layer(Box::new(wind_dict::StoreTempLayer::new(
                    store.clone(),
                    schema_id,
                )));
                engine = engine.with_store_layers(Arc::new(dm));
            }
            Some(Box::new(engine))
        } else {
            let mcl = if schema.engine.codetable.max_code_length > 0 {
                schema.engine.codetable.max_code_length
            } else {
                4
            };
            // 上屏策略解析（tri-state 继承）：方案级 Some > 全局 input.code_commit > 内置默认。
            // auto_commit_at_full 额外兼容 legacy auto_commit_unique（方案显式 true 优先于全局）。
            let ct = &schema.engine.codetable;
            let commit_opts = crate::codetable::CommitOptions {
                auto_commit_at_full: ct
                    .auto_commit_at_full
                    .or(ct.auto_commit_unique.then_some(true))
                    .unwrap_or(commit.auto_commit_at_full),
                auto_commit_min_len: if ct.auto_commit_min_len > 0 {
                    ct.auto_commit_min_len
                } else {
                    commit.auto_commit_min_len
                },
                clear_on_empty_max: ct.clear_on_empty_max.unwrap_or(commit.clear_on_empty_max),
                top_code_commit: ct.top_code_commit.unwrap_or(commit.top_code_commit),
            };
            // 码表引擎经 DictManager(CompositeDict) 查询：系统词库作 System 层。
            // 注入 redb Store 时，注册用户词/临时词层（按 schema 隔离），让用户词进候选合并。
            let dm = wind_dict::DictManager::new();
            if let Some(store) = &store {
                dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
                    store.clone(),
                    schema_id,
                )));
                dm.register_layer(Box::new(wind_dict::StoreTempLayer::new(
                    store.clone(),
                    schema_id,
                )));
            }
            dm.register_layer(Box::new(wind_dict::SystemDictLayer::new(
                dict,
                "codetable-system",
            )));
            Some(Box::new(CodeTableEngine::new(
                mcl,
                commit_opts,
                Arc::new(dm),
            )))
        }
    }

    /// 加载 unigram 语言模型（mmap）：从 unigram.txt 懒生成 unigram.wdb 后 mmap 打开。
    /// 几乎不占常驻内存（页按需载入），替代旧的全量 HashMap 方案。
    fn load_unigram_mmap(ug_txt: &Path) -> Option<Arc<dyn crate::pinyin::lm::UnigramLookup>> {
        use crate::pinyin::lm::{MmapUnigram, parse_unigram_freqs};
        use wind_dict::unigram::{UnigramReader, write_unigram_wdb};

        let ug_wdb = cache_path(ug_txt, "wdb");
        // wdb 比 txt 新则直接用；否则从 txt 重建
        let fresh = Self::combined_cache_fresh(&[ug_txt], &ug_wdb);
        if !(ug_wdb.exists() && fresh) {
            match parse_unigram_freqs(ug_txt) {
                Ok(freqs) => {
                    if let Err(e) = write_unigram_wdb(&ug_wdb, &freqs) {
                        warn!("Failed to write unigram.wdb {}: {}", ug_wdb.display(), e);
                    }
                }
                Err(e) => {
                    warn!("Failed to parse unigram {}: {}", ug_txt.display(), e);
                    return None;
                }
            }
        }
        match UnigramReader::open(&ug_wdb) {
            Ok(reader) => {
                info!(
                    "Unigram mmap: {} ({} keys)",
                    ug_wdb.display(),
                    reader.key_count()
                );
                Some(Arc::new(MmapUnigram::new(reader)))
            }
            Err(e) => {
                warn!("Failed to mmap unigram.wdb {}: {}", ug_wdb.display(), e);
                None
            }
        }
    }

    /// 加载 schema 的词典：合并所有 enabled 词库（主词库 + default_enabled 附加库）。
    ///
    /// - 拼音（rime_pinyin）：单库经 import_tables 合并（load_rime_pinyin_dict）。
    /// - 码表（rime_codetable）：主库 + 扩展库/emoji 等多库合并到 .combined.wdb。
    fn load_dictionary(schema: &Schema, schemas_dir: &Path) -> Option<CachedDict> {
        // 收集 enabled 词库（保持 schema 顺序：主库在前，扩展库在后）
        let mut enabled: Vec<&DictSpec> = schema
            .dictionaries
            .iter()
            .filter(|d| d.is_enabled() && !d.path.is_empty())
            .collect();
        if enabled.is_empty() {
            enabled = schema
                .dictionaries
                .iter()
                .filter(|d| !d.path.is_empty())
                .take(1)
                .collect();
        }
        if enabled.is_empty() {
            warn!("No usable dictionary in schema");
            return None;
        }

        // 词典文件路径解析：用户配置/schemas 优先，回退 schemas_dir（与 read_schema 同语义）。
        let resolve = |rel: &str| -> std::path::PathBuf {
            if let Some(u) = Config::user_config_dir() {
                let p = u.join("schemas").join(rel);
                if p.is_file() {
                    return p;
                }
            }
            schemas_dir.join(rel)
        };

        let dtype = |e: &DictSpec| {
            if e.dict_type.is_empty() {
                "rime_codetable".to_string()
            } else {
                e.dict_type.clone()
            }
        };

        // 单库快路径
        if enabled.len() == 1 {
            let e = enabled[0];
            let full = resolve(&e.path);
            info!("Loading dictionary: {} (type={})", full.display(), dtype(e));
            return if dtype(e) == "rime_pinyin" {
                Self::load_rime_pinyin_dict(&full)
            } else {
                // 英文词库：code 列小写化（大小写不敏感前缀匹配，text 保留原样）。
                let lowercase = dtype(e) == "english";
                match CachedDict::load_at_with(&full, &cache_path(&full, "wdb"), lowercase) {
                    Ok(d) => {
                        info!("Dictionary loaded: {} entries", d.len());
                        Some(d)
                    }
                    Err(err) => {
                        warn!("Failed to load dictionary: {}", err);
                        None
                    }
                }
            };
        }

        // 多库：合并到 combined.wdb（缓存键 = 主词库路径 + .combined.wdb）
        let sources: Vec<(std::path::PathBuf, String)> = enabled
            .iter()
            .map(|e| (resolve(&e.path), dtype(e)))
            .collect();
        let combined = cache_path(sources[0].0.as_path(), "combined.wdb");
        Self::load_merged_dicts(&sources, &combined)
    }

    /// 把多个词库合并到一个 combined.wdb（按 code 聚合），并 mmap 打开。
    /// 每个源按其 dict_type 加载：rime_pinyin 先经 import_tables 展开。
    /// 缓存有效性：combined 比所有源都新则直接复用。
    fn load_merged_dicts(
        sources: &[(std::path::PathBuf, String)],
        combined: &Path,
    ) -> Option<CachedDict> {
        let paths: Vec<&Path> = sources.iter().map(|(p, _)| p.as_path()).collect();
        if Self::combined_cache_fresh(&paths, combined) {
            if let Ok(reader) = wind_dict::binformat::DictReader::open(combined) {
                info!(
                    "Using combined cache: {} ({} keys)",
                    combined.display(),
                    reader.key_count()
                );
                return Some(CachedDict::Mmap(reader));
            }
        }

        // 按 code 聚合所有源词库条目（前面的库优先级更高，先加入；同 text 取更高权重）
        let mut agg: HashMap<String, Vec<(String, i32)>> = HashMap::new();
        let mut total = 0usize;
        for (p, dict_type) in sources {
            // rime_pinyin 需经 import_tables 展开，否则只读到主文件元数据
            let loaded = if dict_type == "rime_pinyin" {
                Self::load_rime_pinyin_dict(p)
            } else {
                CachedDict::load(p).ok()
            };
            match loaded {
                Some(d) => {
                    let n = d.len();
                    info!("  Merging {} entries from {}", n, p.display());
                    for (code, text, weight, _order) in d.search_prefix("", 5_000_000) {
                        let e = agg.entry(code).or_default();
                        if let Some(slot) = e.iter_mut().find(|(t, _)| t == &text) {
                            if weight > slot.1 {
                                slot.1 = weight; // 继承后续库中同词更高权重（对齐 Go composite）
                            }
                        } else {
                            e.push((text, weight));
                        }
                    }
                    total += n;
                }
                None => warn!("  Failed to load {}", p.display()),
            }
        }
        if total == 0 {
            return None;
        }

        let mut writer = wind_dict::binformat::DictWriter::new();
        for (code, mut entries) in agg {
            entries.sort_by(|a, b| b.1.cmp(&a.1));
            writer.add(code, entries);
        }
        match writer.write(combined) {
            Ok(_) => match wind_dict::binformat::DictReader::open(combined) {
                Ok(reader) => {
                    info!(
                        "Wrote combined cache: {} ({} keys from {} dicts)",
                        combined.display(),
                        reader.key_count(),
                        sources.len()
                    );
                    Some(CachedDict::Mmap(reader))
                }
                Err(e) => {
                    warn!("Failed to open combined cache: {}", e);
                    None
                }
            },
            Err(e) => {
                warn!("Failed to write combined cache: {}", e);
                None
            }
        }
    }

    /// combined.wdb 是否比所有源文件新（源文件缺失/不可访问视为缓存失效）
    fn combined_cache_fresh(paths: &[&Path], combined: &Path) -> bool {
        let Ok(cmb_meta) = std::fs::metadata(combined) else {
            return false;
        };
        let Ok(cmb_mtime) = cmb_meta.modified() else {
            return false;
        };
        for p in paths {
            match std::fs::metadata(p).and_then(|m| m.modified()) {
                Ok(src_mtime) if src_mtime <= cmb_mtime => {}
                _ => return false, // 源比缓存新、或源不可访问 → 强制重建
            }
        }
        true
    }

    /// 加载 rime_pinyin 词典（合并 import_tables 子词典到 .merged.wdb）
    fn load_rime_pinyin_dict(dict_path: &Path) -> Option<CachedDict> {
        // merged.wdb 写到可写缓存目录（与 unigram 一致）。安装目录（如 Program Files）
        // 通常只读，若写在源旁会失败 → 回退仅主词典(rime header 数十条) → 拼音无候选。
        let merged_wdb = cache_path(dict_path, "merged.wdb");
        // 仅当 merged 比主词库文件新时复用（主库更新后强制重建；子库通常随主库一同更新）
        let merged_fresh = Self::combined_cache_fresh(&[dict_path], &merged_wdb);
        if merged_wdb.exists() && merged_fresh {
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
        } else if merged_wdb.exists() {
            info!(
                "merged cache stale (source newer), regenerating: {}",
                merged_wdb.display()
            );
            let _ = std::fs::remove_file(&merged_wdb);
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
            match CachedDict::load_at(sub_path, &cache_path(sub_path, "wdb")) {
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
        // 写缓存目录；若仍失败（缓存目录不可写等）退到系统临时目录。绝不退化成仅主词典
        // （rime header 仅数十条），那会让拼音/混输/临时拼音全部无候选。
        let temp_fallback = std::env::temp_dir().join(
            merged_wdb
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("rime.merged.wdb")),
        );
        for target in [&merged_wdb, &temp_fallback] {
            if let Err(e) = writer.write(target) {
                warn!("Failed to write merged cache {}: {}", target.display(), e);
                continue;
            }
            match wind_dict::binformat::DictReader::open(target) {
                Ok(reader) => {
                    info!(
                        "Using merged mmap cache: {} ({} keys)",
                        target.display(),
                        reader.key_count()
                    );
                    return Some(CachedDict::Mmap(reader));
                }
                Err(e) => warn!("Failed to open merged cache {}: {}", target.display(), e),
            }
        }
        warn!("All merged cache writes failed; pinyin dictionary unavailable");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> FreqSettings {
        let sc: Schema = toml::from_str(toml).expect("schema toml");
        EngineManager::parse_freq_settings(&sc)
    }

    #[test]
    fn freq_settings_defaults_disabled_step() {
        // 空方案：主开关默认关、策略默认 step。
        let s = parse("");
        assert!(!s.enabled, "默认应关闭词频维度");
        assert_eq!(s.strategy, FreqStrategy::Step, "默认策略应为 step");
    }

    #[test]
    fn freq_settings_enabled_top() {
        let s = parse(
            "[engine.codetable]\nfreq_strategy = \"top\"\n[learning.freq]\nenabled = true\n",
        );
        assert!(s.enabled);
        assert_eq!(s.strategy, FreqStrategy::Top, "freq_strategy=top 应解析为 Top");
    }

    #[test]
    fn freq_settings_step_explicit_and_unknown_fallback() {
        let s = parse("[engine.codetable]\nfreq_strategy = \"step\"\n[learning.freq]\nenabled = true\n");
        assert_eq!(s.strategy, FreqStrategy::Step);
        // 未知策略值回退 step（稳健默认）。
        let u = parse("[engine.codetable]\nfreq_strategy = \"bogus\"\n[learning.freq]\nenabled = true\n");
        assert_eq!(u.strategy, FreqStrategy::Step, "未知策略应回退 step");
    }
}
