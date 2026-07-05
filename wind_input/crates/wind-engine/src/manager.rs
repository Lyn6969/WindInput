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
use wind_candidate::CandidateSource;
use wind_config::Config;
use wind_config::schema::{DictSpec, Schema};
use wind_dict::cached::CachedDict;

// 方案定义已统一到 wind_config::schema::Schema（取代此前的私有 SchemaFile）。
// 引擎只消费该共享类型；构建逻辑（build_engine）保持不变。

/// 拼音族共享数据归属命名空间：所有拼音引擎方案（全拼/双拼）的用户词/临时词/词频
/// 统一落此键空间（P2c）。区别于恰好同名的真实方案 id "pinyin"（如临时拼音默认目标）。
pub const PINYIN_DATA_SCHEMA: &str = "pinyin";

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
    /// 词频维度主开关（全局 schema.{codetable,pinyin}.frequency.enabled）；关则完全不重排。
    pub enabled: bool,
    /// used-first 内的排序策略（全局 schema.codetable.frequency.strategy；仅码表用）。
    pub strategy: FreqStrategy,
    /// 呈现层前 N 位保护（全局 schema.codetable.frequency.protect_top_n；仅码表用）。
    /// 重排前记录基础序前 N 个候选，重排后原序回填——优先级高于词频，默认 0 = 空保护集。
    pub protect_top_n: usize,
}

impl Default for FreqSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: FreqStrategy::Step,
            protect_top_n: 0,
        }
    }
}

/// 引擎管理器（懒加载：仅在需要时构建对应方案引擎，降低启动内存）
pub struct EngineManager {
    /// schema_id -> 引擎实例（懒加载，Arc 便于无锁 convert）
    engines: Mutex<HashMap<String, Arc<dyn Engine>>>,
    /// 当前活跃方案 ID
    active: Mutex<String>,
    /// 可用方案列表（已过滤不支持的方案，用于循环切换）。
    /// Mutex 以支持配置热重载时原地更新（无需重建 EngineManager）。
    available: Mutex<Vec<String>>,
    /// 数据目录（懒加载时按需读取 schema）
    data_dir: Option<std::path::PathBuf>,
    /// redb 持久化存储（用户词/临时词层；None=无持久化，如纯测试/REPL）
    store: Option<Arc<wind_store::Store>>,
    /// 全局码表配置（公共基线；方案经 schema_overrides 的 [codetable] 段逐字段覆盖）。
    /// Mutex 以支持热重载（变更后清空引擎缓存按新策略重建）。
    codetable: Mutex<wind_config::CodetableGlobal>,
    /// 全局混输配置（融合策略；全局唯一，无方案级 override）。Mutex 以支持热重载。
    mix: Mutex<wind_config::MixGlobal>,
    /// 全局临时拼音配置（码表方案下临时切拼音反查；全局唯一）。Mutex 以支持热重载。
    temp_pinyin: Mutex<wind_config::config::TempPinyinConfig>,
    /// 词频排序设置缓存（schema_id -> FreqSettings；按需解析、避免每键读盘）
    freq_cache: Mutex<HashMap<String, FreqSettings>>,
    /// 方案显示名缓存（schema_id -> schema.name；缺则回退 id）。按需读盘一次。
    name_cache: Mutex<HashMap<String, String>>,
    /// 方案 override 层目录（schema_overrides/{id}.toml）；读 schema 时深合并到基础方案之上。
    /// None=不读 override（如纯测试）。设置页 saveConfig 写此目录。
    override_dir: Option<std::path::PathBuf>,
    /// 主码表方案 id(拼音反查码源):config.schema.primary_codetable 解析后(可空)。构造/重载时更新。
    primary_codetable: Mutex<String>,
    /// 主码表反查索引缓存:(主码表 id, 汉字/词 → 实际编码)。供拼音方案编码提示按词查实际码。
    /// 懒建(首次需要时按主码表全量构建),主码表 id 变化时重建,invalidate/reload 时清空。
    reverse_index: Mutex<Option<(String, Arc<HashMap<String, String>>)>>,
    /// 全局拼音配置（fuzzy/show_code_hint/...）。Mutex 以支持热重载。
    pinyin: Mutex<wind_config::config::PinyinGlobalConfig>,
    /// 双拼韵母键集缓存：(已缓存的活跃方案 id, Option<HashSet<u8>>)。
    /// None = 当前活跃方案不是双拼；Some = 双拼布局的 finals 键集合。
    /// 活跃方案 id 变化时按需重建（惰性），避免每键读盘。
    shuangpin_finals_cache: Mutex<(String, Option<std::collections::HashSet<u8>>)>,
    /// 每方案构建锁（single-flight）：同一方案的引擎/缓存构建串行，避免后台预热与首次
    /// 切换并发时重复构建同一份大缓存；不同方案可并行构建（缓存在各自子目录，互不冲突）。
    build_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

/// 进程级缓存根目录（%LOCALAPPDATA%\WindInput\cache），EngineManager::new 设置一次。
static CACHE_DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

/// 深合并 TOML：`over` 覆盖到 `base` 之上。两侧皆为 table 时逐键递归；否则 over 整体替换。
/// 数组按整体替换（如 dictionaries 覆盖即替换全表）。
fn merge_toml(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, ov) in o {
                match b.get_mut(&k) {
                    Some(bv) => merge_toml(bv, ov),
                    None => {
                        b.insert(k, ov);
                    }
                }
            }
        }
        (b, ov) => *b = ov,
    }
}

/// 源文件 → 缓存路径：`<cache>/<方案>/<文件名干>.<ext>`。
///
/// 用**每方案子目录**(父目录名=schemas/<方案>/ 即方案名)做命名空间，避免跨方案同名冲突，
/// 并把一个方案的全部缓存(主库/扩展/unigram/merged)归拢一处，便于整方案失效=删一目录。
/// 文件名干剥掉 `.dict.yaml` 的 `.dict` 冗余中缀(`rime_frost.dict` → `rime_frost`)。
/// 未设置缓存根时回退到源旁(保持旧行为，便于测试/无 LOCALAPPDATA 场景)。
fn cache_path(source: &Path, ext: &str) -> std::path::PathBuf {
    if let Some(Some(dir)) = CACHE_DIR.get() {
        let scheme = source
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut stem = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(s) = stem.strip_suffix(".dict") {
            stem = s.to_string();
        }
        let base = if scheme.is_empty() {
            dir.clone()
        } else {
            dir.join(&scheme)
        };
        return base.join(format!("{stem}.{ext}"));
    }
    source.with_extension(ext)
}

impl EngineManager {
    /// 从配置创建；仅构建活跃方案引擎，其余按需懒加载。
    pub fn new(config: &Config, data_dir: Option<&Path>) -> Self {
        Self::with_store(config, data_dir, None)
    }

    /// 同 [`new`]，但注入 redb 存储以注册用户词/临时词层（coordinator 用）。
    /// override 目录默认取 `Config::user_config_dir()/schema_overrides`（与用户 schema 覆盖同根）。
    pub fn with_store(
        config: &Config,
        data_dir: Option<&Path>,
        store: Option<Arc<wind_store::Store>>,
    ) -> Self {
        let override_dir = Config::user_config_dir().map(|d| d.join("schema_overrides"));
        Self::with_store_override(config, data_dir, store, override_dir)
    }

    /// 同 [`with_store`]，但显式指定 override 目录（测试用，避免污染真实用户目录）。
    pub fn with_store_override(
        config: &Config,
        data_dir: Option<&Path>,
        store: Option<Arc<wind_store::Store>>,
        override_dir: Option<std::path::PathBuf>,
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
        let ov = override_dir.as_deref();
        available.retain(|sid| sid == &active_id || Self::schema_supported(sid, data_dir, ov));
        // 主码表方案(拼音反查码源):config 显式 > available 首个 codetable 类型方案。
        let primary_codetable = Self::resolve_primary_codetable(
            &config.schema.primary_codetable,
            &available,
            data_dir,
            ov,
        );

        let mgr = Self {
            engines: Mutex::new(HashMap::new()),
            active: Mutex::new(active_id.clone()),
            available: Mutex::new(available),
            data_dir: data_dir.map(|d| d.to_path_buf()),
            store,
            codetable: Mutex::new(config.schema.codetable.clone()),
            mix: Mutex::new(config.schema.mix.clone()),
            temp_pinyin: Mutex::new(config.input.temp_pinyin.clone()),
            freq_cache: Mutex::new(HashMap::new()),
            name_cache: Mutex::new(HashMap::new()),
            override_dir,
            primary_codetable: Mutex::new(primary_codetable),
            reverse_index: Mutex::new(None),
            pinyin: Mutex::new(config.schema.pinyin.clone()),
            shuangpin_finals_cache: Mutex::new((String::new(), None)),
            build_locks: Mutex::new(HashMap::new()),
        };
        // 仅同步构建活跃方案；其余方案由 Coordinator 启动后台预热（prewarm_schema）提前构建，
        // 避免首次切换时同步重熔大词库卡顿。单飞构建锁保证预热与切换不重复构建。
        mgr.ensure_loaded(&active_id);
        mgr
    }

    /// 当前拼音方案是否显示编码提示(反查)。
    /// Task 1.5：改为直接读全局 [pinyin] 配置，不再读 schema 级 show_code_hint。
    /// (码表类方案的「剩余编码」由码表引擎在 convert 内处理，不走此路径。)
    pub fn pinyin_show_code_hint(&self) -> bool {
        self.pinyin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .show_code_hint
    }

    /// 拼音分隔符模式（auto/quote/backtick/none）的原始配置值。
    /// 分隔符键的最终判定（含 auto 动态避让候选选择键）在协调器侧完成——
    /// 因「`'` 是否为选择键」需读 `select_key_groups`（协调器配置），引擎无该信息。
    pub fn pinyin_separator_mode(&self) -> String {
        self.pinyin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .separator
            .clone()
    }

    /// 当前活跃拼音方案是否为双拼（`engine.pinyin.scheme == "shuangpin"`）。
    /// 双拼不支持手动音节分隔符（`'` 会进 buffer 但引擎 convert 前剥除，致 buffer 与 preedit
    /// 发散、Backspace 删不可见字符），供协调器 gate。复用韵母键集缓存（Some 即双拼），
    /// 与 `shuangpin_final_key` 同源、方案切换/reload 自动失效。
    pub fn pinyin_is_shuangpin(&self) -> bool {
        let active_id = self.active_schema_id();
        {
            let cache = self
                .shuangpin_finals_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cache.0 == active_id {
                return cache.1.is_some();
            }
        }
        let finals_set = self.build_shuangpin_finals(&active_id);
        let is_sp = finals_set.is_some();
        *self
            .shuangpin_finals_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (active_id, finals_set);
        is_sp
    }

    /// 活跃方案为双拼且 `key`（ASCII 字节）是其布局的韵母键时返回 true，否则 false。
    /// 供选词热键避让：正在输入双拼时，韵母键优先作编码输入而非触发选词（对齐 Go IsShuangpinFinalKey）。
    /// 内部按活跃方案 id 缓存韵母键集合，方案切换/reload/invalidate 时自动失效。
    pub fn shuangpin_final_key(&self, key: u8) -> bool {
        let active_id = self.active_schema_id();
        // 检查缓存是否命中
        {
            let cache = self
                .shuangpin_finals_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cache.0 == active_id {
                return cache.1.as_ref().map(|s| s.contains(&key)).unwrap_or(false);
            }
        }
        // 缓存未命中：读取活跃方案，判断是否双拼并构建韵母键集合
        let finals_set = self.build_shuangpin_finals(&active_id);
        let result = finals_set
            .as_ref()
            .map(|s| s.contains(&key))
            .unwrap_or(false);
        *self
            .shuangpin_finals_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (active_id, finals_set);
        result
    }

    /// 内部辅助：为指定方案 id 构建双拼韵母键集（非双拼返回 None）。
    fn build_shuangpin_finals(&self, schema_id: &str) -> Option<std::collections::HashSet<u8>> {
        let data_dir = self.data_dir.as_deref()?;
        let schema = Self::read_schema(schema_id, Some(data_dir), self.override_dir.as_deref())?;
        if !schema
            .engine
            .pinyin
            .scheme
            .eq_ignore_ascii_case("shuangpin")
        {
            return None;
        }
        let layout_id = if schema.engine.pinyin.shuangpin.layout.is_empty() {
            "xiaohe".to_string()
        } else {
            schema.engine.pinyin.shuangpin.layout.clone()
        };
        // 用户目录优先（resolve_schema_file）：%APPDATA%/…/schemas/shuangpin/<id>.toml
        // 存在即覆盖安装目录，使用户自带/覆盖的双拼布局生效。
        let lp = Self::resolve_schema_file(&format!("shuangpin/{layout_id}.toml"), data_dir);
        crate::pinyin::shuangpin::Layout::from_toml(&lp)
            .map(|lay| lay.final_key_set())
            .ok()
    }

    /// 拼音方案编码提示:返回主码表中 `text` 实际对应的编码(多码取权重/码长/字典序首位),
    /// 不存在返回空。对齐 Go `manager_convert.go` 的 ApplyCodeHintsToCandidates——用主码表
    /// **反向索引**取实际码,而非按字生成码再校验(后者生成码常与码表实际码不一致,导致全被拒)。
    /// 索引按主码表 id 懒建并缓存,主码表 id 变化时重建,reload/invalidate 时清空。
    pub fn codetable_reverse_hint(&self, text: &str) -> String {
        let primary = self
            .primary_codetable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if primary.is_empty() {
            return String::new();
        }
        let idx = {
            let mut guard = self.reverse_index.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some((id, m)) if *id == primary => m.clone(),
                _ => {
                    let m = Arc::new(self.build_primary_reverse_index(&primary));
                    *guard = Some((primary.clone(), m.clone()));
                    m
                }
            }
        };
        idx.get(text).cloned().unwrap_or_default()
    }

    /// 按主码表方案全量构建反查索引(汉字/词 → 实际编码)。失败返回空表。
    fn build_primary_reverse_index(&self, primary: &str) -> HashMap<String, String> {
        let Some(data_dir) = self.data_dir.as_deref() else {
            return HashMap::new();
        };
        let schemas = data_dir.join("schemas");
        let Some(schema) = Self::read_schema(primary, Some(data_dir), self.override_dir.as_deref())
        else {
            return HashMap::new();
        };
        match Self::load_dictionary(&schema, &schemas) {
            Some(dict) => {
                let idx = dict.build_reverse_index();
                info!(
                    "Built code-hint reverse index: {} ({} texts)",
                    primary,
                    idx.len()
                );
                idx
            }
            None => HashMap::new(),
        }
    }

    /// 解析主码表方案 id:config 显式指定优先;否则取 available 中首个 codetable 类型方案;都无返回空。
    fn resolve_primary_codetable(
        cfg_primary: &str,
        available: &[String],
        data_dir: Option<&Path>,
        override_dir: Option<&Path>,
    ) -> String {
        if !cfg_primary.is_empty() {
            return cfg_primary.to_string();
        }
        for id in available {
            if Self::read_schema(id, data_dir, override_dir)
                .map(|s| s.engine.engine_type.eq_ignore_ascii_case("codetable"))
                .unwrap_or(false)
            {
                return id.clone();
            }
        }
        String::new()
    }

    /// 读取 schema 判断是否受支持（不构建引擎，仅解析 TOML）
    fn schema_supported(
        schema_id: &str,
        data_dir: Option<&Path>,
        override_dir: Option<&Path>,
    ) -> bool {
        match Self::read_schema(schema_id, data_dir, override_dir) {
            Some(s) => s.is_supported(),
            None => false,
        }
    }

    /// 读取 schema 的隐藏标志（[schema].hidden）：隐藏方案不在设置页「方案管理」列出。
    fn schema_hidden(
        schema_id: &str,
        data_dir: Option<&Path>,
        override_dir: Option<&Path>,
    ) -> bool {
        Self::read_schema(schema_id, data_dir, override_dir)
            .map(|s| s.schema.hidden)
            .unwrap_or(false)
    }

    /// 确保指定方案引擎已加载；返回是否可用
    /// 某方案引擎是否已加载（已就绪，切换即时无构建）。
    pub fn is_loaded(&self, schema_id: &str) -> bool {
        self.engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(schema_id)
    }

    /// 某方案是否正在后台构建（未加载且构建锁被占）。供 UI 显示「准备中」用。
    pub fn is_building(&self, schema_id: &str) -> bool {
        if self.is_loaded(schema_id) {
            return false;
        }
        let lock = self
            .build_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(schema_id)
            .cloned();
        // 锁存在且被占 = 有线程正在构建该方案。
        matches!(lock, Some(l) if l.try_lock().is_err())
    }

    /// 后台预热：构建某方案的引擎与缓存（阻塞，供后台线程调用）。返回是否成功。
    /// 与首次切换共享 single-flight 构建锁，竞争时只构建一次。
    pub fn prewarm_schema(&self, schema_id: &str) -> bool {
        self.ensure_loaded(schema_id)
    }

    fn ensure_loaded(&self, schema_id: &str) -> bool {
        if self.is_loaded(schema_id) {
            return true;
        }
        // single-flight：取该方案的专用构建锁（不同方案各自一把，可并行构建）。
        let build_lock = {
            let mut locks = self.build_locks.lock().unwrap_or_else(|e| e.into_inner());
            locks.entry(schema_id.to_string()).or_default().clone()
        };
        let _build_guard = build_lock.lock().unwrap_or_else(|e| e.into_inner());
        // 抢到锁后复查：等待期间可能已被另一线程（预热/切换）构建完成。
        if self.is_loaded(schema_id) {
            return true;
        }
        let codetable_cfg = self
            .codetable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mix_cfg = self.mix.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let pinyin_cfg = self
            .pinyin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match Self::build_engine(
            schema_id,
            self.data_dir.as_deref(),
            self.store.clone(),
            &codetable_cfg,
            &mix_cfg,
            self.override_dir.as_deref(),
            &pinyin_cfg,
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

    /// 可用方案列表（快照拷贝）。
    pub fn available_schemas(&self) -> Vec<String> {
        self.available
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 所有已安装且受支持的方案列表（目录扫描）。
    ///
    /// 扫描 `data_dir/schemas/*.schema.toml`，对每个文件取去掉 `.schema.toml` 后缀的 id，
    /// 按 `is_supported()` 过滤掉不支持的方案，再并入当前 `available`（保证已启用方案即使
    /// 文件异常也在列），去重后按 id 字典序稳定排序返回。
    ///
    /// `data_dir` 为 None 时（纯测试）回退到 `available_schemas()`。
    ///
    /// 供设置页"方案管理"的候选全集使用；**不影响** `available_schemas()`（循环切换用）。
    pub fn installed_schemas(&self) -> Vec<String> {
        let Some(data_dir) = self.data_dir.as_deref() else {
            return self.available_schemas();
        };
        let mut ids: Vec<String> = self.available_schemas();
        let ov = self.override_dir.as_deref();

        // 合并扫描：安装目录 data/schemas 与用户目录 %APPDATA%/…/schemas，
        // 两处的 *.schema.toml 都算"已安装"——用户目录可新增第三方方案（read_schema
        // 走 resolve_schema_file，本就用户目录优先，故用户方案能被读出并通过过滤）。
        // 注：此处扫描顺序无关紧要（靠 !ids.contains 去重，两目录都贡献 id）；与
        // shuangpin_layouts 的"用户优先覆盖"语义不同——那里靠前目录同名 stem 胜出。
        let mut scan_dirs: Vec<std::path::PathBuf> = vec![data_dir.join("schemas")];
        if let Some(user) = Config::user_config_dir() {
            let ud = user.join("schemas");
            if ud != scan_dirs[0] {
                scan_dirs.push(ud);
            }
        }

        for dir in &scan_dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let fname_str = fname.to_string_lossy();
                if let Some(id) = fname_str.strip_suffix(".schema.toml") {
                    let id = id.to_string();
                    if !ids.contains(&id)
                        // 只加入受支持且非隐藏的方案（隐藏方案如 english 仅供懒加载引用）
                        && Self::schema_supported(&id, Some(data_dir), ov)
                        && !Self::schema_hidden(&id, Some(data_dir), ov)
                    {
                        ids.push(id);
                    }
                }
            }
        }

        ids.sort();
        ids.dedup();
        ids
    }

    /// 枚举可选的双拼布局：合并扫描 [用户目录, 安装目录] 的
    /// `schemas/shuangpin/*.toml`，用户目录同名（按文件名 stem）覆盖安装目录。
    ///
    /// 返回 `(id, 显示名)`：**id 取文件名 stem**（与加载路径 `{layout}.toml` 一致，
    /// 保证"能选=能加载"），显示名取布局 `[meta].name`；解析失败（如缺 `[finals]`）
    /// 的布局跳过并告警。供设置页"双拼布局"下拉动态取值，取代前端硬编码清单。
    ///
    /// `data_dir` 为 None（纯测试）时返回空。
    pub fn shuangpin_layouts(&self) -> Vec<(String, String)> {
        let Some(data_dir) = self.data_dir.as_deref() else {
            return Vec::new();
        };
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        if let Some(user) = Config::user_config_dir() {
            dirs.push(user.join("schemas").join("shuangpin"));
        }
        dirs.push(data_dir.join("schemas").join("shuangpin"));
        Self::scan_shuangpin_layouts(&dirs)
    }

    /// 纯扫描逻辑（可测）：按 `dirs` 顺序扫描 `*.toml`，靠前目录优先，
    /// 以文件名 stem 去重；输出按 id 字典序稳定排序。
    fn scan_shuangpin_layouts(dirs: &[std::path::PathBuf]) -> Vec<(String, String)> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<(String, String)> = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                // 只收普通文件：忽略名字恰好以 .toml 结尾的子目录等。
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let fname = entry.file_name();
                let fname_str = fname.to_string_lossy();
                let Some(stem) = fname_str.strip_suffix(".toml") else {
                    continue;
                };
                let id = stem.to_string();
                if !seen.insert(id.clone()) {
                    continue; // 靠前目录已收录，跳过后续同名
                }
                match crate::pinyin::shuangpin::Layout::from_toml(&entry.path()) {
                    Ok(lay) => {
                        // id 以文件名 stem 为准（加载路径 {layout}.toml）；[meta].id 仅作校验。
                        if !lay.id.is_empty() && lay.id != id {
                            warn!(
                                "双拼布局文件名 {} 与 [meta].id=\"{}\" 不符，以文件名为准",
                                id, lay.id
                            );
                        }
                        let name = if lay.name.is_empty() { id.clone() } else { lay.name };
                        out.push((id, name));
                    }
                    Err(e) => {
                        warn!("双拼布局枚举跳过 {}: {}", entry.path().display(), e);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 方案显示名（schema.name 优先，缺/读不到回退 id）。带缓存避免重复读盘。
    pub fn schema_name(&self, schema_id: &str) -> String {
        if let Some(n) = self
            .name_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(schema_id)
        {
            return n.clone();
        }
        let name = Self::read_schema(
            schema_id,
            self.data_dir.as_deref(),
            self.override_dir.as_deref(),
        )
        .map(|s| s.schema.name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| schema_id.to_string());
        self.name_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(schema_id.to_string(), name.clone());
        name
    }

    /// 指定方案的图标短称（schema.icon_label）；未配置返回空串。
    /// 用于状态气泡 short 模式（对齐 Go GetSchemaDisplayInfo 的 iconLabel）。
    pub fn schema_icon_label(&self, schema_id: &str) -> String {
        Self::read_schema(
            schema_id,
            self.data_dir.as_deref(),
            self.override_dir.as_deref(),
        )
        .map(|s| s.schema.icon_label)
        .unwrap_or_default()
    }

    /// 指定方案的引擎类型字符串（小写，如 "pinyin"|"codetable"|"mixed"）；读不到返回 None。
    /// 不切换活跃方案（设置页 dict.encode 据此选拼音/五笔出码规则）。
    pub fn schema_engine_type(&self, schema_id: &str) -> Option<String> {
        Self::read_schema(
            schema_id,
            self.data_dir.as_deref(),
            self.override_dir.as_deref(),
        )
        .map(|s| s.engine.engine_type.to_lowercase())
    }

    /// 存储归属 id：拼音引擎方案统一为 "pinyin"（拼音/双拼共享一份用户词/临时/词频）；
    /// 其余方案（码表/混输/未知）用自身 id。仅影响存储键，不影响引擎行为。
    pub fn data_schema_id(&self, schema_id: &str) -> String {
        if self.schema_engine_type(schema_id).as_deref() == Some("pinyin") {
            PINYIN_DATA_SCHEMA.to_string()
        } else {
            schema_id.to_string()
        }
    }

    /// 混输方案的主码表方案 id（`[engine.mixed].primary_schema`）；非混输/未知/未配置返回 None。
    pub fn mixed_primary_schema(&self, schema_id: &str) -> Option<String> {
        if self.schema_engine_type(schema_id).as_deref() != Some("mixed") {
            return None;
        }
        Self::read_schema(
            schema_id,
            self.data_dir.as_deref(),
            self.override_dir.as_deref(),
        )
        .map(|s| s.engine.mixed.primary_schema)
        .filter(|p| !p.is_empty())
    }

    /// 写入/词频归属 id：非混输 = `data_schema_id(自身)`（source 无关，零回归）；
    /// 混输按候选来源分流（码表→主方案自身 id、拼音→"pinyin"）；
    /// 无法归因（None/English/Phrase 或 primary 缺失）返回 None，调用方跳过本次读写。
    pub fn write_data_schema_id(&self, schema_id: &str, source: CandidateSource) -> Option<String> {
        if self.schema_engine_type(schema_id).as_deref() != Some("mixed") {
            return Some(self.data_schema_id(schema_id));
        }
        match source {
            CandidateSource::CodeTable => self
                .mixed_primary_schema(schema_id)
                .map(|p| self.data_schema_id(&p)),
            CandidateSource::Pinyin => Some(PINYIN_DATA_SCHEMA.to_string()),
            _ => None,
        }
    }

    /// 方案基础定义（不含 override 层）——设置页计算 saveConfig 稀疏 diff 的基准。
    pub fn schema_base(&self, schema_id: &str) -> Option<Schema> {
        Self::read_schema(schema_id, self.data_dir.as_deref(), None)
    }

    /// 方案合并定义（基础 + override 层）——设置页 getConfig 返回。
    pub fn schema_merged(&self, schema_id: &str) -> Option<Schema> {
        Self::read_schema(
            schema_id,
            self.data_dir.as_deref(),
            self.override_dir.as_deref(),
        )
    }

    /// 读取某方案 override 层（TOML 值，无则 None）。
    pub fn get_schema_override(&self, schema_id: &str) -> Option<toml::Value> {
        let dir = self.override_dir.as_deref()?;
        Self::read_override_value(schema_id, dir)
    }

    /// 仅写入某方案 override 层（原子 tmp+rename），**不**使引擎缓存失效。
    /// 供「持久化 + 对已加载引擎 live 生效」的场景（如扩展词库热插拔）使用。
    pub fn persist_schema_override(
        &self,
        schema_id: &str,
        value: &toml::Value,
    ) -> anyhow::Result<()> {
        let dir = self
            .override_dir
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("无 override 目录"))?;
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{schema_id}.toml"));
        let out = toml::to_string_pretty(value)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 写入某方案 override 层并使其引擎缓存失效（下次使用按新配置重建）。
    pub fn write_schema_override(
        &self,
        schema_id: &str,
        value: &toml::Value,
    ) -> anyhow::Result<()> {
        self.persist_schema_override(schema_id, value)?;
        self.invalidate_schema(schema_id);
        Ok(())
    }

    /// 运行时启停某方案的扩展词库：对**已加载引擎**即时翻对应系统层的 enabled 标志
    /// （无需重建/重熔大词库）；未加载的方案此处不做事（下次构建按已持久化的 override 生效）。
    /// 启用集变化会影响反查索引/编码提示（基于启用词库合并），故一并失效之使下次重算。
    /// 返回是否对已加载引擎即时生效。**注意**：调用方须先 [`persist_schema_override`] 持久化，
    /// 否则重启/重建后状态丢失。
    pub fn set_dict_enabled_live(&self, schema_id: &str, dict_id: &str, enabled: bool) -> bool {
        let engine = self
            .engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(schema_id)
            .cloned();
        let hit = engine.is_some_and(|e| e.set_dict_enabled(dict_id, enabled));
        // 反查索引依赖「启用词库合并」，启用集变了须失效（懒重建）。
        // 注：编码提示开关已改读全局 config.pinyin.show_code_hint，无方案级缓存需失效。
        *self.reverse_index.lock().unwrap_or_else(|e| e.into_inner()) = None;
        hit
    }

    /// 删除某方案 override 层并使其引擎缓存失效。返回是否删除了文件。
    pub fn delete_schema_override(&self, schema_id: &str) -> anyhow::Result<bool> {
        let removed = if let Some(dir) = self.override_dir.as_deref() {
            let path = dir.join(format!("{schema_id}.toml"));
            if path.exists() {
                std::fs::remove_file(&path)?;
                true
            } else {
                false
            }
        } else {
            false
        };
        self.invalidate_schema(schema_id);
        Ok(removed)
    }

    /// 删除用户自定义方案：仅当方案文件存在于用户目录（非内置 data 目录）时允许。
    /// 同时清除其 override 并从可用列表移除。返回是否删除。内置方案返回 Err。
    pub fn delete_user_schema(&self, schema_id: &str) -> anyhow::Result<bool> {
        let user_file = Config::user_config_dir()
            .map(|d| d.join("schemas").join(format!("{schema_id}.schema.toml")));
        match &user_file {
            Some(p) if p.is_file() => {
                std::fs::remove_file(p)?;
            }
            _ => anyhow::bail!("内置方案不可删除: {}", schema_id),
        }
        let _ = self.delete_schema_override(schema_id);
        self.available
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|s| s != schema_id);
        self.invalidate_schema(schema_id);
        Ok(true)
    }

    /// 使某方案的引擎与解析缓存失效（override/词典变更后，下次构建按新定义重建）。
    pub fn invalidate_schema(&self, schema_id: &str) {
        self.engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(schema_id);
        self.freq_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(schema_id);
        self.name_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(schema_id);
        // 主码表(及其词库/override)可能变更:失效反查索引,下次按新内容重建。
        *self.reverse_index.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // 双拼布局可能变更：失效韵母键缓存，下次按新布局重建。
        *self
            .shuangpin_finals_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (String::new(), None);
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

    /// 按新配置热重载方案集（无需重建 EngineManager）：重算可用方案、更新上屏策略、
    /// 清空引擎/词频/名称缓存使其按新配置/词典重建，并切到新的活跃方案。
    /// 返回活跃方案是否发生变化（供上层决定是否清输入缓冲、刷新 UI）。
    pub fn reload_from_config(&self, config: &Config) -> bool {
        let new_active = config.active_schema().to_string();
        let mut available = config.schema.available.clone();
        if available.is_empty() {
            available.push(new_active.clone());
        }
        // 过滤不支持的方案，但始终保留活跃方案（与构造逻辑一致）。
        available.retain(|sid| {
            sid == &new_active
                || Self::schema_supported(
                    sid,
                    self.data_dir.as_deref(),
                    self.override_dir.as_deref(),
                )
        });

        // 更新可变状态。
        // 重算主码表(拼音反查码源)。在 available 移入锁前用其引用解析。
        let primary = Self::resolve_primary_codetable(
            &config.schema.primary_codetable,
            &available,
            self.data_dir.as_deref(),
            self.override_dir.as_deref(),
        );
        *self
            .primary_codetable
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = primary;
        // 主码表可能变更:失效反查索引,下次按新主码表重建。
        *self.reverse_index.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.available.lock().unwrap_or_else(|e| e.into_inner()) = available;
        *self.codetable.lock().unwrap_or_else(|e| e.into_inner()) = config.schema.codetable.clone();
        *self.mix.lock().unwrap_or_else(|e| e.into_inner()) = config.schema.mix.clone();
        *self.temp_pinyin.lock().unwrap_or_else(|e| e.into_inner()) =
            config.input.temp_pinyin.clone();
        // 全局拼音配置变更：更新缓存，引擎缓存随下方 clear() 一起失效，下次按新配置重建。
        *self.pinyin.lock().unwrap_or_else(|e| e.into_inner()) = config.schema.pinyin.clone();
        // 丢弃缓存：引擎按新上屏策略/词典重建，名称/词频按新方案重读。
        self.engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.freq_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.name_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        // 双拼布局可能变更：失效韵母键缓存，下次按新布局重建。
        *self
            .shuangpin_finals_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (String::new(), None);

        // 切换活跃方案（即便 id 未变，引擎已被清空，这里立即重建避免首键延迟）。
        let changed = {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            let changed = *active != new_active;
            *active = new_active.clone();
            changed
        };
        self.ensure_loaded(&new_active);
        info!(
            "EngineManager reloaded from config (active={}, changed={})",
            new_active, changed
        );
        changed
    }

    /// 循环切换到下一个可加载的方案；返回新方案 ID。
    /// 懒加载：在加载前不持 active 锁，避免首次加载（拼音合并/unigram）阻塞按键路径。
    pub fn cycle_schema(&self) -> Option<String> {
        let available = self.available_schemas();
        let n = available.len();
        if n <= 1 {
            return None;
        }
        let current = self.active_schema_id();
        let cur = available.iter().position(|s| s == &current).unwrap_or(0);
        for step in 1..n {
            let cand = available[(cur + step) % n].clone();
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

    /// 满码自动上屏「显示态」复评（透传到活跃引擎）：据已过滤/重排/shadow 的显示候选复评，
    /// 引擎按未过滤候选因生僻同码字判不唯一而否决时，智能过滤后剩唯一精确全码则放行上屏。
    pub fn recheck_auto_commit(
        &self,
        input: &str,
        candidates: &[wind_candidate::Candidate],
    ) -> Option<String> {
        self.active_engine()?.recheck_auto_commit(input, candidates)
    }

    /// 临时拼音目标方案 id（读全局 input.temp_pinyin；不再读方案级配置）。
    /// 启用且目标方案可加载时返回 Some(target)，否则 None。
    pub fn temp_pinyin_target(&self) -> Option<String> {
        let tp = self
            .temp_pinyin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
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

    /// 活跃方案的**有效**码表行为配置：全局 `schema.codetable` 经该方案
    /// `schema_overrides/{id}.toml` 的 `[codetable]` 段（带开关）解析。供 coordinator 读
    /// punct_commit / z_key_repeat 等行为字段（取代旧的直接读 schema 字段）。
    pub fn codetable_settings(&self) -> wind_config::CodetableGlobal {
        let id = self.active_schema_id();
        let global = self
            .codetable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // 混输方案自身无独立 codetable 配置，override 从其 primary_schema（主码表方案）读取
        let resolve_id = if matches!(self.current_engine_type(), Some(EngineType::Mixed)) {
            Self::read_schema(&id, self.data_dir.as_deref(), self.override_dir.as_deref())
                .map(|s| s.engine.mixed.primary_schema)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| id.clone())
        } else {
            id
        };
        Self::resolve_codetable(&resolve_id, &global, self.override_dir.as_deref())
    }

    /// 拼音自动造词配置（[schema.pinyin.auto_learn]）。
    pub fn auto_learn_settings(&self) -> wind_config::config::AutoLearnConfig {
        self.pinyin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .auto_learn
            .clone()
    }

    /// 拼音词频衰减参数（用户配置；0 表示使用 store 默认值）。
    pub fn pinyin_freq_profile(&self) -> wind_store::freq::FreqProfile {
        let pf = self.pinyin.lock().unwrap_or_else(|e| e.into_inner());
        let def = wind_store::freq::FreqProfile::default();
        wind_store::freq::FreqProfile {
            base_scale: if pf.frequency.base_scale > 0.0 {
                pf.frequency.base_scale
            } else {
                def.base_scale
            },
            half_life_hours: if pf.frequency.half_life > 0.0 {
                pf.frequency.half_life
            } else {
                def.half_life_hours
            },
            recency_peak: pf.frequency.recency_peak.max(0.0),
        }
    }

    /// 解析某方案的有效码表配置：全局基线 + `[codetable]` override（开关开启时逐字段覆盖）。
    fn resolve_codetable(
        schema_id: &str,
        global: &wind_config::CodetableGlobal,
        override_dir: Option<&Path>,
    ) -> wind_config::CodetableGlobal {
        let ov = override_dir
            .and_then(|d| Self::read_override_value(schema_id, d))
            .map(|v| wind_config::schema::SchemeOverride::from_toml(&v))
            .and_then(|so| so.codetable);
        global.resolved(ov.as_ref())
    }

    /// 主码表方案的拆字配置（`[engine.chaizi]`：db/font 路径 + DWrite 家族名）。
    /// 拆字反查表与字根字体随主码表方案下发；无配置返回 None。路径相对 `data/schemas/`。
    pub fn chaizi_spec(&self) -> Option<wind_config::schema::ChaiziSpec> {
        let primary = self
            .primary_codetable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let id = if primary.is_empty() {
            self.active_schema_id()
        } else {
            primary
        };
        let schema =
            Self::read_schema(&id, self.data_dir.as_deref(), self.override_dir.as_deref())?;
        let c = schema.engine.chaizi;
        c.is_configured().then_some(c)
    }

    /// 活跃方案的词频排序设置（frequency.md §3/§8）。**全局唯一、按引擎类型分**：
    /// 码表/混输取 `schema.codetable.frequency`，拼音取 `schema.pinyin.frequency`。
    /// 按 id 缓存（reload 时清空），避免每键重算。
    pub fn freq_settings(&self) -> FreqSettings {
        let id = self.active_schema_id();
        {
            let cache = self.freq_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = cache.get(&id) {
                return *s;
            }
        }
        let is_pinyin = matches!(self.schema_engine_type(&id).as_deref(), Some("pinyin"));
        let settings = if is_pinyin {
            let pf = self.pinyin.lock().unwrap_or_else(|e| e.into_inner());
            // 拼音 strategy/protect_top_n 字段不参与（仅码表 used-first 排序用），取默认。
            FreqSettings {
                enabled: pf.frequency.enabled,
                strategy: FreqStrategy::Step,
                protect_top_n: 0,
            }
        } else {
            let ct = self.codetable.lock().unwrap_or_else(|e| e.into_inner());
            FreqSettings {
                enabled: ct.frequency.enabled,
                strategy: Self::parse_freq_strategy(&ct.frequency.strategy),
                protect_top_n: ct.frequency.protect_top_n,
            }
        };
        self.freq_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, settings);
        settings
    }

    /// 词频策略字符串 → 枚举（纯映射，便于单测）。
    fn parse_freq_strategy(s: &str) -> FreqStrategy {
        match s {
            "top" => FreqStrategy::Top,
            _ => FreqStrategy::Step,
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

    /// 用指定方案的引擎为词语生成全拼编码（造词反推、多音字消歧）。
    /// 方案非拼音类、未能加载或无法生成时返回 None（调用方可回退逐字反查表）。
    pub fn generate_word_pinyin(&self, schema_id: &str, text: &str) -> Option<String> {
        if !self.ensure_loaded(schema_id) {
            return None;
        }
        let engine = self
            .engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(schema_id)
            .cloned()?;
        engine.generate_word_pinyin(text)
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

    /// 读取并解析 schema 文件（仅 TOML）。用户目录优先（见 resolve_schema_file）；
    /// 若 `override_dir/{id}.toml` 存在则深合并到基础方案之上（设置页 override 层 L3）。
    fn read_schema(
        schema_id: &str,
        data_dir: Option<&Path>,
        override_dir: Option<&Path>,
    ) -> Option<Schema> {
        let data_dir = data_dir?;
        let toml_path = Self::resolve_schema_file(&format!("{}.schema.toml", schema_id), data_dir);
        if !toml_path.exists() {
            warn!("Schema file not found: {}.schema.toml", schema_id);
            return None;
        }
        let content = std::fs::read_to_string(&toml_path).ok()?;
        let mut base: toml::Value = match toml::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                warn!("Parse schema TOML failed {}: {}", toml_path.display(), e);
                return None;
            }
        };
        // 合并 override 层（存在才读；不存在则零影响）。
        if let Some(ov) = override_dir.and_then(|d| Self::read_override_value(schema_id, d)) {
            merge_toml(&mut base, ov);
        }
        match base.try_into() {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("Schema {} override 合并后解析失败: {}", schema_id, e);
                None
            }
        }
    }

    /// 读取某方案的 override TOML 值（无则 None）。
    fn read_override_value(schema_id: &str, override_dir: &Path) -> Option<toml::Value> {
        let path = override_dir.join(format!("{schema_id}.toml"));
        let content = std::fs::read_to_string(&path).ok()?;
        toml::from_str(&content).ok()
    }

    /// 为指定 schema 构建引擎
    fn build_engine(
        schema_id: &str,
        data_dir: Option<&Path>,
        store: Option<Arc<wind_store::Store>>,
        codetable_cfg: &wind_config::CodetableGlobal,
        mix_cfg: &wind_config::MixGlobal,
        override_dir: Option<&Path>,
        pinyin_cfg: &wind_config::config::PinyinGlobalConfig,
    ) -> Option<Box<dyn Engine>> {
        let data_dir = data_dir?;
        let schemas = data_dir.join("schemas");
        let schema = Self::read_schema(schema_id, Some(data_dir), override_dir)?;

        // 混输方案：递归构建主（码表）+ 次（拼音）子引擎，包装为 MixedEngine
        if schema.engine.engine_type.to_lowercase() == "mixed" {
            let m = &schema.engine.mixed;
            if m.primary_schema.is_empty() {
                warn!("mixed schema {} 缺少 primary_schema", schema_id);
                return None;
            }
            let primary = Self::build_engine(
                &m.primary_schema,
                Some(data_dir),
                store.clone(),
                codetable_cfg,
                mix_cfg,
                override_dir,
                pinyin_cfg,
            )?;
            let secondary = if m.secondary_schema.is_empty() {
                None
            } else {
                Self::build_engine(
                    &m.secondary_schema,
                    Some(data_dir),
                    store.clone(),
                    codetable_cfg,
                    mix_cfg,
                    override_dir,
                    pinyin_cfg,
                )
            };
            let boost = if m.codetable_weight_boost > 0 {
                m.codetable_weight_boost
            } else {
                10_000_000
            };
            // 融合策略走全局 schema.mix（无方案级 override）。
            let min_py = if mix_cfg.min_pinyin_length > 0 {
                mix_cfg.min_pinyin_length
            } else {
                2
            };
            let block_on_pinyin = mix_cfg.auto_commit_block_on_pinyin;
            // 英文候选（schema.mix.enable_english）：开启时懒加载 english 词库引擎混入混输候选。
            // 走 build_engine("english") → EnglishEngine（词库缺失则 None，静默退化为无英文）。
            // 开关热切换经 reload_from_config 的 engines.clear() 重建混输引擎自然生效。
            let english = if mix_cfg.enable_english {
                Self::build_engine(
                    "english",
                    Some(data_dir),
                    store.clone(),
                    codetable_cfg,
                    mix_cfg,
                    override_dir,
                    pinyin_cfg,
                )
            } else {
                None
            };
            // 英文最小触发长度（0=回退 3，即 2 字符以内不查英文）。
            let min_en = if mix_cfg.min_english_length > 0 {
                mix_cfg.min_english_length
            } else {
                3
            };
            info!(
                "Built mixed engine {} (primary={}, secondary={}, english={})",
                schema_id,
                m.primary_schema,
                m.secondary_schema,
                english.is_some()
            );
            let cfg = crate::mixed::MixConfig {
                min_pinyin_length: min_py,
                codetable_weight_boost: boost,
                auto_commit_block_on_pinyin: block_on_pinyin,
                pinyin_only_overflow: mix_cfg.pinyin_only_overflow,
                top_code_override_pinyin: mix_cfg.top_code_override_pinyin,
                show_source_hint: mix_cfg.show_source_hint,
                min_english_length: min_en,
                auto_commit_block_on_english: mix_cfg.auto_commit_block_on_english,
                block_commit_on_pinyin_word: mix_cfg.block_commit_on_pinyin_word,
                pinyin_word_min_weight: mix_cfg.pinyin_word_min_weight,
            };
            return Some(Box::new(crate::mixed::MixedEngine::new(
                primary, secondary, english, cfg,
            )));
        }

        // 英文方案：复用码表词典加载 + 前缀查询，但包成独立 EnglishEngine（EngineType::English）。
        // 关闭自动上屏 / 顶码 / 编码提示（英文词变长，无满码顶字语义）；词库以 type="english"
        // 声明（code 列小写化，大小写不敏感前缀匹配）。供临时英文 / 融合英文候选懒加载。
        if schema.engine.engine_type.eq_ignore_ascii_case("english") {
            let layers = Self::load_codetable_layers(&schema, &schemas);
            if layers.is_empty() {
                warn!("No usable english dictionary for schema {}", schema_id);
                return None;
            }
            // 英文暂不挂用户词 / 临时词层（无造词学习），仅系统词库层。
            let dm = wind_dict::DictManager::new();
            for (name, dict, enabled) in layers {
                dm.register_layer(Box::new(wind_dict::SystemDictLayer::with_enabled(
                    dict, name, enabled,
                )));
            }
            // 英文最大码长取词库最长词的安全上界（前缀匹配用，不触发顶码/自动上屏）。
            let mcl = if schema.engine.codetable.max_code_length > 0 {
                schema.engine.codetable.max_code_length
            } else {
                32
            };
            // 全 false：无自动上屏 / 顶码 / 编码提示，纯前缀查词。
            let commit_opts = crate::codetable::CommitOptions::default();
            info!("Built english engine {}", schema_id);
            return Some(Box::new(crate::english::EnglishEngine::new(
                CodeTableEngine::new(mcl, commit_opts, Arc::new(dm)),
            )));
        }

        if schema.is_pinyin() {
            let dict = match Self::load_dictionary(&schema, &schemas) {
                Some(d) => d,
                None => {
                    warn!("load_dictionary returned None for schema {}", schema_id);
                    return None;
                }
            };
            // 加载 unigram 语言模型（长句 Viterbi 打分）：mmap 零拷贝，失败回退词典权重。
            let unigram: Option<Arc<dyn crate::pinyin::lm::UnigramLookup>> =
                if schema.engine.pinyin.unigram_path.is_empty() {
                    None
                } else {
                    let ug_txt = schemas.join(&schema.engine.pinyin.unigram_path);
                    Self::load_unigram_mmap(&ug_txt)
                };
            // 从全局拼音配置构建引擎配置和模糊音（Task 1.4：修 fuzzy 从未生效 bug）。
            // enabled 作总开关：未启用时所有模糊标志归零（与 Go 行为一致）。
            let pg = pinyin_cfg;
            let fuzzy = crate::pinyin::fuzzy::FuzzyConfig {
                zh_z: pg.fuzzy.enabled && pg.fuzzy.zh_z,
                ch_c: pg.fuzzy.enabled && pg.fuzzy.ch_c,
                sh_s: pg.fuzzy.enabled && pg.fuzzy.sh_s,
                n_l: pg.fuzzy.enabled && pg.fuzzy.n_l,
                f_h: pg.fuzzy.enabled && pg.fuzzy.f_h,
                r_l: pg.fuzzy.enabled && pg.fuzzy.r_l,
                an_ang: pg.fuzzy.enabled && pg.fuzzy.an_ang,
                en_eng: pg.fuzzy.enabled && pg.fuzzy.en_eng,
                in_ing: pg.fuzzy.enabled && pg.fuzzy.in_ing,
                ian_iang: pg.fuzzy.enabled && pg.fuzzy.ian_iang,
                uan_uang: pg.fuzzy.enabled && pg.fuzzy.uan_uang,
            };
            let pcfg = PinyinConfig {
                show_code_hint: pg.show_code_hint,
                use_smart_compose: pg.use_smart_compose,
            };
            let mut engine =
                PinyinEngine::with_unigram(pcfg, dict, unigram).with_fuzzy(fuzzy.clone());
            // 双拼方案：按 layout 加载布局并注入 ShuangpinConverter
            if schema
                .engine
                .pinyin
                .scheme
                .eq_ignore_ascii_case("shuangpin")
            {
                let layout_id = if schema.engine.pinyin.shuangpin.layout.is_empty() {
                    "xiaohe".to_string()
                } else {
                    schema.engine.pinyin.shuangpin.layout.clone()
                };
                // 用户目录优先（见 resolve_schema_file）：用户自带/覆盖布局生效。
                let lp = Self::resolve_schema_file(&format!("shuangpin/{layout_id}.toml"), data_dir);
                match crate::pinyin::shuangpin::Layout::from_toml(&lp) {
                    Ok(layout) => {
                        let mut conv = crate::pinyin::shuangpin::ShuangpinConverter::new(layout);
                        conv.set_fuzzy(fuzzy.zh_z, fuzzy.ch_c, fuzzy.sh_s);
                        engine = engine.with_shuangpin(conv);
                    }
                    Err(e) => {
                        warn!("双拼布局 {} 加载失败，回退全拼: {}", layout_id, e);
                    }
                }
            }
            // 注入 redb Store 时挂用户词/临时词层（L 造词显现）：让拼音造的词进候选合并。
            // 仅含 User/Temp 层（系统词典仍由引擎自身的 CachedDict 承担 Viterbi/前缀）。
            // 存储归属统一为 "pinyin"，使全拼/双拼方案共享同一份用户词与临时词。
            if let Some(store) = &store {
                let dm = wind_dict::DictManager::new();
                dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
                    store.clone(),
                    PINYIN_DATA_SCHEMA,
                )));
                dm.register_layer(Box::new(wind_dict::StoreTempLayer::new(
                    store.clone(),
                    PINYIN_DATA_SCHEMA,
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
            // 上屏策略：全局 schema.codetable 基线 + 该方案 [codetable] override（带开关）解析。
            let eff = Self::resolve_codetable(schema_id, codetable_cfg, override_dir);
            let commit_opts = crate::codetable::CommitOptions {
                auto_commit_at_full: eff.auto_commit_at_full,
                auto_commit_min_len: eff.auto_commit_min_len,
                clear_on_empty_max: eff.clear_on_empty_max,
                top_code_commit: eff.top_code_commit,
                show_code_hint: eff.show_code_hint,
                single_code_input: eff.single_code_input,
                single_code_complete: eff.single_code_complete,
            };
            // 码表引擎经 DictManager(CompositeDict) 查询。系统词库不再合并成单个 combined，
            // 而是主库 + 每个扩展（含禁用）各自一个 System 层，查询期由 composite 合并去重。
            // 开关扩展只需翻该层 enabled 标志即时生效，无需重熔大词库。
            let layers = Self::load_codetable_layers(&schema, &schemas);
            if layers.is_empty() {
                warn!("No usable codetable dictionary for schema {}", schema_id);
                return None;
            }
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
            // 主库优先注册（在 load_codetable_layers 中已置首），扩展库其后。
            for (name, dict, enabled) in layers {
                dm.register_layer(Box::new(wind_dict::SystemDictLayer::with_enabled(
                    dict, name, enabled,
                )));
            }
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
                Ok(freqs) => match write_unigram_wdb(&ug_wdb, &freqs) {
                    Ok(()) => wind_dict::cache_fp::write_cache_fp(&ug_wdb, &[ug_txt]),
                    Err(e) => warn!("Failed to write unigram.wdb {}: {}", ug_wdb.display(), e),
                },
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

    /// 码表方案：把主词库 + 每个扩展词库（含**当前禁用**的）各自加载为独立 system 层。
    /// 返回 `(层名, CachedDict, 初始enabled)`：主库 → `codetable-system`(恒启用)；扩展 →
    /// `codetable-extra-<id>`(enabled=is_enabled())。**不再合并 combined.wdb**——查询期由
    /// CompositeDict 合并去重，开关扩展只需翻该层 enabled 标志，无需重熔大词库（对齐 Go 的
    /// 每库独立缓存 + 查询期合并）。主库优先返回（层序最靠前 → 等权重时排前）。
    fn load_codetable_layers(
        schema: &Schema,
        schemas_dir: &Path,
    ) -> Vec<(String, CachedDict, bool)> {
        let resolve = |rel: &str| -> std::path::PathBuf {
            if let Some(u) = Config::user_config_dir() {
                let p = u.join("schemas").join(rel);
                if p.is_file() {
                    return p;
                }
            }
            schemas_dir.join(rel)
        };
        let is_english =
            |e: &DictSpec| -> bool { !e.dict_type.is_empty() && e.dict_type == "english" };

        let usable: Vec<&DictSpec> = schema
            .dictionaries
            .iter()
            .filter(|d| !d.path.is_empty())
            .collect();
        if usable.is_empty() {
            return Vec::new();
        }
        // 主库 = 首个 default；无 default 则取首个可用库。
        let main_idx = usable.iter().position(|d| d.default).unwrap_or(0);

        let load_one = |e: &DictSpec| -> Option<CachedDict> {
            let full = resolve(&e.path);
            match CachedDict::load_at_with(&full, &cache_path(&full, "wdat"), is_english(e)) {
                Ok(d) => Some(d),
                Err(err) => {
                    warn!("Failed to load codetable dict {}: {}", full.display(), err);
                    None
                }
            }
        };

        let mut out: Vec<(String, CachedDict, bool)> = Vec::new();
        // 主库优先注册。加载失败 → 无系统层可用，放弃整方案（避免无候选）。
        match load_one(usable[main_idx]) {
            Some(d) => {
                info!(
                    "  codetable main: {} ({} entries)",
                    usable[main_idx].path,
                    d.len()
                );
                out.push(("codetable-system".to_string(), d, true));
            }
            None => return Vec::new(),
        }
        // 扩展库（含禁用的，全部加载常驻，供运行时热插拔）。
        for (i, e) in usable.iter().enumerate() {
            if i == main_idx {
                continue;
            }
            let enabled = e.is_enabled();
            if let Some(d) = load_one(e) {
                info!(
                    "  codetable extra: {} (id={}, enabled={}, {} entries)",
                    e.path,
                    e.id,
                    enabled,
                    d.len()
                );
                out.push((format!("codetable-extra-{}", e.id), d, enabled));
            }
        }
        out
    }

    /// 加载 schema 的词典：合并所有 enabled 词库（主词库 + default_enabled 附加库）。
    ///
    /// - 拼音（rime_pinyin）：单库经 import_tables 合并（load_rime_pinyin_dict）。
    /// - 码表反查索引用（build_primary_reverse_index）：主库 + 扩展库合并到 .combined.wdb。
    ///   注意 live 查询层已改为 load_codetable_layers 的每库独立层，此处仅供反查索引复用。
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
                match CachedDict::load_at_with(&full, &cache_path(&full, "wdat"), lowercase) {
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
        let combined = cache_path(sources[0].0.as_path(), "combined.wdat");
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
            if let Ok(reader) = wind_dict::datformat::WdatReader::open(combined) {
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

        let mut writer = wind_dict::datformat::WdatWriter::new();
        for (code, mut entries) in agg {
            entries.sort_by(|a, b| b.1.cmp(&a.1));
            writer.add(code, entries);
        }
        match writer.write(combined) {
            Ok(_) => {
                // 写内容指纹(覆盖全部源，与上面 fresh 校验的 paths 一致)
                wind_dict::cache_fp::write_cache_fp(combined, &paths);
                match wind_dict::datformat::WdatReader::open(combined) {
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
                }
            }
            Err(e) => {
                warn!("Failed to write combined cache: {}", e);
                None
            }
        }
    }

    /// combined.wdb 是否比所有源文件新（源文件缺失/不可访问视为缓存失效）
    /// 缓存是否可复用：按源文件**内容指纹**判定（非 mtime）。
    /// scp/部署/版本控制会刷新源 mtime，旧的 mtime 校验会因此恒失效 → 每次重建(300MB)；
    /// 改为内容指纹后，源内容未变即复用，构建后由 write_cache_fp 写指纹 sidecar。
    fn combined_cache_fresh(paths: &[&Path], combined: &Path) -> bool {
        wind_dict::cache_fp::cache_is_fresh(combined, paths)
    }

    /// 加载 rime_pinyin 词典（合并 import_tables 子词典到 .merged.wdb）
    fn load_rime_pinyin_dict(dict_path: &Path) -> Option<CachedDict> {
        // merged.wdb 写到可写缓存目录（与 unigram 一致）。安装目录（如 Program Files）
        // 通常只读，若写在源旁会失败 → 回退仅主词典(rime header 数十条) → 拼音无候选。
        let merged_wdb = cache_path(dict_path, "merged.wdat");
        // 先解析主表头部，收集全部源（主表 + import_tables 子表）。指纹/缓存校验需覆盖
        // 全部源，故须先于 fresh 判定算出 sub_paths（仅解析头部 yaml，开销极低）。
        let content = std::fs::read_to_string(dict_path).ok()?;
        let yaml_section = if let Some(start) = content.find("---") {
            let after = &content[start + 3..];
            after.find("...").map(|end| &after[..end]).unwrap_or(after)
        } else {
            &content
        };
        let yaml: serde_yaml::Value = serde_yaml::from_str(yaml_section).ok()?;
        let dict_dir = dict_path.parent()?;

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
        let src_refs: Vec<&Path> = sub_paths.iter().map(|p| p.as_path()).collect();

        // merged 缓存对**全部源**做内容指纹校验：主表或任一子表内容变化、或源清单增删都
        // 判定失效并重建（避免「子表改了却仍用旧 merged」的静默陈旧）。
        if merged_wdb.exists() && Self::combined_cache_fresh(&src_refs, &merged_wdb) {
            match wind_dict::datformat::WdatReader::open(&merged_wdb) {
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
                "merged cache stale (sources changed), regenerating: {}",
                merged_wdb.display()
            );
            let _ = std::fs::remove_file(&merged_wdb);
        }

        // 并行解析每个源正文（纯 CPU 多线程），直接产出 (code,text,weight)：不再为每个子表
        // 生成中间 .wdb，也绕过 CodetableDict 的 BTreeMap 构建与逐 code 排序（merged 稍后会
        // 统一按权重重排）。DictWriter::add 不合并同 code 多次调用，故先用 HashMap 聚合，否则
        // wdb 出现重复 KeyIndex，DictReader 二分只命中其一 → 同 code 候选系统性丢失。
        // 全拼按 code 聚合；简拼（声母缩写，如 nh→你好）按简拼码聚合，存进 wdat 独立 AbbrevSection。
        let mut agg: HashMap<String, Vec<(String, i32)>> = HashMap::new();
        let mut agg_ab: HashMap<String, Vec<(String, i32)>> = HashMap::new();
        let mut total_entries = 0usize;
        for sub_path in &sub_paths {
            // lowercase_code=false：import_tables 子表均为拼音表(非 english)，与改前
            // CachedDict::load_at(默认不小写 code)行为一致。
            match wind_dict::codetable::parse_rime_entries_parallel(sub_path, false) {
                Ok((fulls, abbrevs)) => {
                    let count = fulls.len();
                    info!(
                        "  Loading {} entries ({} abbrev) from {}",
                        count,
                        abbrevs.len(),
                        sub_path.display()
                    );
                    for (code, text, weight) in fulls {
                        agg.entry(code).or_default().push((text, weight));
                    }
                    for (ab, text, weight) in abbrevs {
                        agg_ab.entry(ab).or_default().push((text, weight));
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

        let mut writer = wind_dict::datformat::WdatWriter::new();

        for (code, mut entries) in agg {
            // 同 code 下按权重降序，保证候选顺序稳定
            entries.sort_by(|a, b| b.1.cmp(&a.1));
            writer.add(code, entries);
        }
        // 简拼表 → 独立 AbbrevSection（与全拼查询互不污染）；同简拼下按权重降序。
        let abbrev_count = agg_ab.len();
        for (ab, mut entries) in agg_ab {
            entries.sort_by(|a, b| b.1.cmp(&a.1));
            writer.add_abbrev(ab, entries);
        }
        info!(
            "  merged pinyin: {} codes + {} abbrevs",
            writer.key_count(),
            abbrev_count
        );

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
            // 写内容指纹(覆盖全部源；仅对正式缓存路径，fresh 校验也只看 merged_wdb)
            if target.as_path() == merged_wdb.as_path() {
                wind_dict::cache_fp::write_cache_fp(&merged_wdb, &src_refs);
            }
            match wind_dict::datformat::WdatReader::open(target) {
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

    #[test]
    fn freq_strategy_top_parsed() {
        assert_eq!(EngineManager::parse_freq_strategy("top"), FreqStrategy::Top);
    }

    #[test]
    fn freq_strategy_step_and_unknown_fallback() {
        assert_eq!(
            EngineManager::parse_freq_strategy("step"),
            FreqStrategy::Step
        );
        // 未知策略值回退 step（稳健默认）。
        assert_eq!(
            EngineManager::parse_freq_strategy("bogus"),
            FreqStrategy::Step,
            "未知策略应回退 step"
        );
    }

    #[test]
    fn codetable_override_resolves_over_global() {
        // 全局基线 + override（开关开启）逐字段覆盖。
        let global = wind_config::CodetableGlobal {
            top_code_commit: false,
            z_key_repeat: false,
            ..Default::default()
        };
        let ov = wind_config::schema::CodetableOverride {
            enabled: true,
            top_code_commit: Some(true),
            ..Default::default()
        };
        let eff = global.resolved(Some(&ov));
        assert!(eff.top_code_commit, "override 开启时 Some 字段应覆盖");
        assert!(!eff.z_key_repeat, "override 未给的字段应回落全局");
        // 开关关闭：整段忽略。
        let ov_off = wind_config::schema::CodetableOverride {
            enabled: false,
            top_code_commit: Some(true),
            ..Default::default()
        };
        assert!(
            !global.resolved(Some(&ov_off)).top_code_commit,
            "开关关闭时应回落全局"
        );
    }

    #[test]
    fn merge_toml_table_recurse_and_scalar_replace() {
        let mut base: toml::Value = toml::from_str("a = 1\n[t]\nx = 1\ny = 2\n").unwrap();
        let over: toml::Value = toml::from_str("a = 9\n[t]\ny = 20\nz = 30\n").unwrap();
        merge_toml(&mut base, over);
        assert_eq!(base.get("a").unwrap().as_integer(), Some(9));
        let t = base.get("t").unwrap();
        assert_eq!(t.get("x").unwrap().as_integer(), Some(1), "未覆盖键保留");
        assert_eq!(t.get("y").unwrap().as_integer(), Some(20), "覆盖键替换");
        assert_eq!(t.get("z").unwrap().as_integer(), Some(30), "新增键加入");
    }

    #[test]
    fn schema_override_merge_and_delete() {
        use std::io::Write;
        let base_dir = std::env::temp_dir().join("wind_eng_ov_data");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        let mut f = std::fs::File::create(schemas.join("tcfg.schema.toml")).unwrap();
        write!(
            f,
            "[schema]\nid = \"tcfg\"\nname = \"基础名\"\n[engine]\ntype = \"codetable\"\n[engine.codetable]\nmax_code_length = 4\n"
        )
        .unwrap();
        drop(f);

        let ov_dir = std::env::temp_dir().join("wind_eng_ov_overrides");
        let _ = std::fs::remove_dir_all(&ov_dir);

        let cfg = Config::default();
        let mgr =
            EngineManager::with_store_override(&cfg, Some(&base_dir), None, Some(ov_dir.clone()));

        // 基础层
        let base = mgr.schema_base("tcfg").unwrap();
        assert_eq!(base.schema.name, "基础名");
        assert_eq!(base.engine.codetable.max_code_length, 4);

        // 写 override：覆盖 name + max_code_length
        let ov: toml::Value = toml::from_str(
            "[schema]\nname = \"覆盖名\"\n[engine.codetable]\nmax_code_length = 5\n",
        )
        .unwrap();
        mgr.write_schema_override("tcfg", &ov).unwrap();

        let merged = mgr.schema_merged("tcfg").unwrap();
        assert_eq!(merged.schema.name, "覆盖名", "override 覆盖 name");
        assert_eq!(merged.engine.codetable.max_code_length, 5);
        assert_eq!(merged.schema.id, "tcfg", "未覆盖字段保留基础值");
        // base 不受 override 影响
        assert_eq!(mgr.schema_base("tcfg").unwrap().schema.name, "基础名");

        // 删除 override → 回到基础层
        assert!(mgr.delete_schema_override("tcfg").unwrap());
        assert_eq!(mgr.schema_merged("tcfg").unwrap().schema.name, "基础名");

        let _ = std::fs::remove_dir_all(&base_dir);
        let _ = std::fs::remove_dir_all(&ov_dir);
    }

    /// Task 4.3：验证 shuangpin 方案在 EngineManager 层 available 过滤中真正被放行。
    ///
    /// 测试设计：
    /// - 用 temp 数据目录，写三个最小 schema TOML：
    ///   * "dummy_ct"：codetable 类型，作为 active（ensure_loaded 无词库会 warn 但不 panic）
    ///   * "sp_test"：pinyin + scheme="shuangpin" → is_supported()=true，应留在 available
    ///   * "sp_unsupported"：pinyin + scheme="ziranma_xxx" → is_supported()=false，应被过滤
    /// - 不触发词库加载（shuangpin 不是 active，schema_supported 只做 TOML 解析）
    /// - Linux 无词库环境可跑。
    #[test]
    fn shuangpin_available_not_filtered_out() {
        use std::io::Write;

        // 建 temp 数据目录
        let base_dir = std::env::temp_dir().join("wind_eng_sp_available_test");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();

        // active schema：最小 codetable，无词库（ensure_loaded 失败 = warn，manager 仍构造成功）
        {
            let mut f = std::fs::File::create(schemas.join("dummy_ct.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"dummy_ct\"\n[engine]\ntype = \"codetable\"\n"
            )
            .unwrap();
        }

        // shuangpin schema：engine.type="pinyin" + scheme="shuangpin" → is_supported()=true
        {
            let mut f = std::fs::File::create(schemas.join("sp_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"sp_test\"\n[engine]\ntype = \"pinyin\"\n[engine.pinyin]\nscheme = \"shuangpin\"\n"
            )
            .unwrap();
        }

        // 不支持的双拼变体：engine.type="pinyin" + scheme="ziranma_xxx" → is_supported()=false
        {
            let mut f = std::fs::File::create(schemas.join("sp_unsupported.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"sp_unsupported\"\n[engine]\ntype = \"pinyin\"\n[engine.pinyin]\nscheme = \"ziranma_xxx\"\n"
            )
            .unwrap();
        }

        // 构造 config：active = dummy_ct（首个 available 即为 active）
        let mut cfg = Config::default();
        cfg.schema.active = "dummy_ct".into();
        cfg.schema.available = vec!["dummy_ct".into(), "sp_test".into(), "sp_unsupported".into()];

        let ov_dir = std::env::temp_dir().join("wind_eng_sp_available_ov");
        let _ = std::fs::remove_dir_all(&ov_dir);

        let mgr =
            EngineManager::with_store_override(&cfg, Some(&base_dir), None, Some(ov_dir.clone()));

        let available = mgr.available_schemas();

        // shuangpin 方案应通过过滤，进入 available 列表
        assert!(
            available.contains(&"sp_test".to_string()),
            "shuangpin schema 应在 available 中，实际 available={available:?}"
        );

        // 不支持的 scheme 应被过滤掉（过滤仍有效）
        assert!(
            !available.contains(&"sp_unsupported".to_string()),
            "ziranma_xxx schema 应被过滤，实际 available={available:?}"
        );

        // active 方案始终保留（不论 schema_supported 结果如何）
        assert!(
            available.contains(&"dummy_ct".to_string()),
            "active schema 应始终保留，实际 available={available:?}"
        );

        // 清理
        let _ = std::fs::remove_dir_all(&base_dir);
        let _ = std::fs::remove_dir_all(&ov_dir);
    }

    /// Task 5.1：双拼活跃时，韵母键 shuangpin_final_key 返回 true；非韵母键返回 false。
    /// 用真实 data 目录 + mspy 布局（含 `;` = ing）验证。
    #[test]
    fn shuangpin_final_key_true_for_shuangpin() {
        use std::io::Write;

        // 真实 data 目录（含 shuangpin/ 布局文件 + 可读的 schema TOML）
        let data_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data");
        // 把测试用 schema 写到 data/schemas/ 目录下（与真实布局同 data_dir）
        // 注意：测试结束后删除，防止污染。
        let sp_schema_path = data_dir.join("schemas").join("sp_mspy_test.schema.toml");
        {
            let mut f = std::fs::File::create(&sp_schema_path).unwrap();
            write!(
                f,
                "[schema]\nid = \"sp_mspy_test\"\n[engine]\ntype = \"pinyin\"\n[engine.pinyin]\nscheme = \"shuangpin\"\n[engine.pinyin.shuangpin]\nlayout = \"mspy\"\n"
            )
            .unwrap();
        }

        let mut cfg = Config::default();
        cfg.schema.active = "sp_mspy_test".into();
        cfg.schema.available = vec!["sp_mspy_test".into()];

        let ov_dir = std::env::temp_dir().join("wind_eng_sp_finalkey_ov");
        let _ = std::fs::remove_dir_all(&ov_dir);

        let mgr =
            EngineManager::with_store_override(&cfg, Some(&data_dir), None, Some(ov_dir.clone()));

        // mspy `;` = ing → 是韵母键
        assert!(mgr.shuangpin_final_key(b';'), "mspy `;` 应是韵母键");
        // `k` 在 mspy 是韵母键（ao）
        assert!(mgr.shuangpin_final_key(b'k'), "mspy `k` 应是韵母键");
        // `[` 不是 mspy 的韵母键（mspy finals 仅含字母和 `;`）
        assert!(!mgr.shuangpin_final_key(b'['), "mspy `[` 不应是韵母键");

        let _ = std::fs::remove_file(&sp_schema_path);
        let _ = std::fs::remove_dir_all(&ov_dir);
    }

    /// Task 5.1：非双拼方案（codetable）时，shuangpin_final_key 对任何键返回 false。
    #[test]
    fn shuangpin_final_key_false_for_non_shuangpin() {
        use std::io::Write;

        let base_dir = std::env::temp_dir().join("wind_eng_sp_finalkey_ct_test");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        {
            let mut f = std::fs::File::create(schemas.join("wubi.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"wubi\"\n[engine]\ntype = \"codetable\"\n"
            )
            .unwrap();
        }

        let mut cfg = Config::default();
        cfg.schema.active = "wubi".into();
        cfg.schema.available = vec!["wubi".into()];

        let ov_dir = std::env::temp_dir().join("wind_eng_sp_finalkey_ct_ov");
        let _ = std::fs::remove_dir_all(&ov_dir);

        let mgr =
            EngineManager::with_store_override(&cfg, Some(&base_dir), None, Some(ov_dir.clone()));

        // 非双拼方案，任何键均应返回 false
        assert!(
            !mgr.shuangpin_final_key(b';'),
            "codetable 方案 `;` 应返回 false"
        );
        assert!(
            !mgr.shuangpin_final_key(b'k'),
            "codetable 方案 `k` 应返回 false"
        );

        let _ = std::fs::remove_dir_all(&base_dir);
        let _ = std::fs::remove_dir_all(&ov_dir);
    }

    /// installed_schemas 应返回所有已安装且受支持的方案，不受 available 限制。
    ///
    /// 测试方案：
    ///   - "dummy_active": codetable，active 方案（始终在 available）
    ///   - "sp_installed": pinyin + scheme="shuangpin"，已安装但**未**在 config.available
    ///     → installed_schemas 应包含它，available_schemas 不含它
    ///   - "unsupported_installed": pinyin + scheme="ziranma_xxx"，已安装但不受支持
    ///     → installed_schemas 不含它
    ///   - "ct_installed": codetable，已安装但未在 config.available
    ///     → installed_schemas 应包含它
    #[test]
    fn installed_schemas_includes_all_supported_not_just_available() {
        use std::io::Write;

        let base_dir = std::env::temp_dir().join("wind_eng_installed_schemas_test");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();

        // active（codetable）
        {
            let mut f = std::fs::File::create(schemas.join("dummy_active.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"dummy_active\"\n[engine]\ntype = \"codetable\"\n"
            )
            .unwrap();
        }

        // 已安装双拼（shuangpin），未在 available → 应出现在 installed_schemas
        {
            let mut f = std::fs::File::create(schemas.join("sp_installed.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"sp_installed\"\n[engine]\ntype = \"pinyin\"\n[engine.pinyin]\nscheme = \"shuangpin\"\n"
            )
            .unwrap();
        }

        // 已安装但不受支持（scheme="ziranma_xxx"）→ 应被过滤
        {
            let mut f =
                std::fs::File::create(schemas.join("unsupported_installed.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"unsupported_installed\"\n[engine]\ntype = \"pinyin\"\n[engine.pinyin]\nscheme = \"ziranma_xxx\"\n"
            )
            .unwrap();
        }

        // 已安装 codetable，未在 available → 应出现在 installed_schemas
        {
            let mut f = std::fs::File::create(schemas.join("ct_installed.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"ct_installed\"\n[engine]\ntype = \"codetable\"\n"
            )
            .unwrap();
        }

        // config.available 只含 active，其余方案未启用
        let mut cfg = Config::default();
        cfg.schema.active = "dummy_active".into();
        cfg.schema.available = vec!["dummy_active".into()];

        let ov_dir = std::env::temp_dir().join("wind_eng_installed_schemas_ov");
        let _ = std::fs::remove_dir_all(&ov_dir);

        let mgr =
            EngineManager::with_store_override(&cfg, Some(&base_dir), None, Some(ov_dir.clone()));

        let available = mgr.available_schemas();
        let installed = mgr.installed_schemas();

        // available 只含 active，未启用方案不在其中
        assert_eq!(available, vec!["dummy_active".to_string()]);

        // installed 含 active
        assert!(
            installed.contains(&"dummy_active".to_string()),
            "active 应在 installed_schemas 中，实际={installed:?}"
        );

        // installed 含未启用的双拼方案
        assert!(
            installed.contains(&"sp_installed".to_string()),
            "已安装 shuangpin 方案应在 installed_schemas 中，实际={installed:?}"
        );

        // installed 含未启用的 codetable 方案
        assert!(
            installed.contains(&"ct_installed".to_string()),
            "已安装 codetable 方案应在 installed_schemas 中，实际={installed:?}"
        );

        // 不受支持的方案被过滤掉
        assert!(
            !installed.contains(&"unsupported_installed".to_string()),
            "不支持的方案不应在 installed_schemas 中，实际={installed:?}"
        );

        // 结果有序（字典序）
        let mut sorted = installed.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            installed, sorted,
            "installed_schemas 应按字典序排序且无重复"
        );

        let _ = std::fs::remove_dir_all(&base_dir);
        let _ = std::fs::remove_dir_all(&ov_dir);
    }

    /// scan_shuangpin_layouts：合并扫描多目录、靠前目录（用户）优先、
    /// 跳过解析失败（缺 [finals]）的布局、按 id 字典序排序。
    #[test]
    fn scan_shuangpin_layouts_merges_user_priority() {
        use std::io::Write;

        let base = std::env::temp_dir().join("wind_eng_sp_layouts_test");
        let _ = std::fs::remove_dir_all(&base);
        let install = base.join("install");
        let user = base.join("user");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&user).unwrap();

        let write_layout =
            |dir: &std::path::Path, file: &str, id: &str, name: &str, finals: bool| {
                let mut f = std::fs::File::create(dir.join(file)).unwrap();
                let finals_sec = if finals { "[finals]\na = [\"a\"]\n" } else { "" };
                write!(f, "[meta]\nid = \"{id}\"\nname = \"{name}\"\n{finals_sec}").unwrap();
            };

        // 安装目录：xiaohe、mspy
        write_layout(&install, "xiaohe.toml", "xiaohe", "小鹤双拼", true);
        write_layout(&install, "mspy.toml", "mspy", "微软双拼", true);
        // 用户目录：新增 shoudao + 同名覆盖 xiaohe（改显示名）
        write_layout(&user, "shoudao.toml", "shoudao", "手道双拼", true);
        write_layout(&user, "xiaohe.toml", "xiaohe", "小鹤(用户版)", true);
        // 用户目录：损坏布局（缺 [finals]）应被跳过
        write_layout(&user, "broken.toml", "broken", "坏的", false);

        // dirs 顺序：用户优先
        let dirs = vec![user.clone(), install.clone()];
        let got = EngineManager::scan_shuangpin_layouts(&dirs);

        assert_eq!(
            got,
            vec![
                ("mspy".to_string(), "微软双拼".to_string()),
                ("shoudao".to_string(), "手道双拼".to_string()),
                ("xiaohe".to_string(), "小鹤(用户版)".to_string()),
            ],
            "布局枚举应合并、用户优先、跳过损坏、按 id 排序，实际={got:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Task 1：data_schema_id 拼音族折叠 + 未知方案返回自身 id。
    ///
    /// 策略：用 temp 目录写最小 schema TOML（与既有测试同模式）：
    ///   - "py_test"：engine.type="pinyin" → data_schema_id 应返回 "pinyin"
    ///   - "ct_test"：engine.type="codetable" → 返回自身 "ct_test"
    ///   - "nonexistent"：无此 schema 文件 → schema_engine_type=None → 返回自身
    #[test]
    fn data_schema_id_folds_pinyin_and_returns_self() {
        use std::io::Write;

        let base_dir = std::env::temp_dir().join("wind_eng_data_schema_id_test");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();

        // 拼音方案
        {
            let mut f = std::fs::File::create(schemas.join("py_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"py_test\"\n[engine]\ntype = \"pinyin\"\n"
            )
            .unwrap();
        }

        // 码表方案
        {
            let mut f = std::fs::File::create(schemas.join("ct_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"ct_test\"\n[engine]\ntype = \"codetable\"\n"
            )
            .unwrap();
        }

        let mut cfg = Config::default();
        cfg.schema.active = "ct_test".into();
        cfg.schema.available = vec!["ct_test".into(), "py_test".into()];

        let ov_dir = std::env::temp_dir().join("wind_eng_data_schema_id_ov");
        let _ = std::fs::remove_dir_all(&ov_dir);

        let mgr =
            EngineManager::with_store_override(&cfg, Some(&base_dir), None, Some(ov_dir.clone()));

        // 拼音方案折叠为 "pinyin"
        assert_eq!(
            mgr.data_schema_id("py_test"),
            "pinyin",
            "拼音方案 data_schema_id 应返回 pinyin"
        );

        // 码表方案返回自身 id
        assert_eq!(
            mgr.data_schema_id("ct_test"),
            "ct_test",
            "码表方案 data_schema_id 应返回自身 id"
        );

        // 未知方案（schema_engine_type=None）返回自身 id
        assert_eq!(
            mgr.data_schema_id("nonexistent"),
            "nonexistent",
            "未知方案 data_schema_id 应返回自身 id"
        );

        let _ = std::fs::remove_dir_all(&base_dir);
        let _ = std::fs::remove_dir_all(&ov_dir);
    }

    /// P2d Task 1：write_data_schema_id 混输按候选来源分流；非混输忽略 source。
    #[test]
    fn write_data_schema_id_routes_mixed_by_source() {
        use std::io::Write;

        let base_dir = std::env::temp_dir().join("wind_eng_write_data_schema_id_test");
        let schemas = base_dir.join("schemas");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&schemas).unwrap();

        // 拼音方案
        {
            let mut f = std::fs::File::create(schemas.join("py_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"py_test\"\n[engine]\ntype = \"pinyin\"\n"
            )
            .unwrap();
        }
        // 码表方案
        {
            let mut f = std::fs::File::create(schemas.join("ct_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"ct_test\"\n[engine]\ntype = \"codetable\"\n"
            )
            .unwrap();
        }
        // 混输方案
        {
            let mut f = std::fs::File::create(schemas.join("mx_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"mx_test\"\n[engine]\ntype = \"mixed\"\n[engine.mixed]\nprimary_schema = \"ct_test\"\nsecondary_schema = \"py_test\"\n"
            )
            .unwrap();
        }

        let mut cfg = Config::default();
        cfg.schema.active = "mx_test".into();
        cfg.schema.available = vec!["mx_test".into(), "ct_test".into(), "py_test".into()];

        let ov_dir = std::env::temp_dir().join("wind_eng_write_data_schema_id_ov");
        let _ = std::fs::remove_dir_all(&ov_dir);

        let mgr =
            EngineManager::with_store_override(&cfg, Some(&base_dir), None, Some(ov_dir.clone()));

        // 非混输：忽略 source，等价 data_schema_id
        assert_eq!(
            mgr.write_data_schema_id("py_test", CandidateSource::None),
            Some("pinyin".to_string()),
            "拼音方案忽略 source，折叠为 pinyin"
        );
        assert_eq!(
            mgr.write_data_schema_id("ct_test", CandidateSource::Pinyin),
            Some("ct_test".to_string()),
            "码表方案忽略 source，返回自身 id"
        );

        // 混输：按来源分流
        assert_eq!(
            mgr.write_data_schema_id("mx_test", CandidateSource::CodeTable),
            Some("ct_test".to_string()),
            "混输 + CodeTable → 主码表方案 id"
        );
        assert_eq!(
            mgr.write_data_schema_id("mx_test", CandidateSource::Pinyin),
            Some("pinyin".to_string()),
            "混输 + Pinyin → pinyin"
        );
        assert_eq!(
            mgr.write_data_schema_id("mx_test", CandidateSource::None),
            None,
            "混输 + None → 无法归因，跳过"
        );
        assert_eq!(
            mgr.write_data_schema_id("mx_test", CandidateSource::Phrase),
            None,
            "混输 + Phrase → 无法归因，跳过"
        );
        assert_eq!(
            mgr.write_data_schema_id("mx_test", CandidateSource::English),
            None,
            "混输 + English → 无法归因，跳过"
        );

        // mixed_primary_schema
        assert_eq!(
            mgr.mixed_primary_schema("mx_test"),
            Some("ct_test".to_string()),
            "混输方案的主码表方案 id"
        );
        assert_eq!(
            mgr.mixed_primary_schema("ct_test"),
            None,
            "非混输方案 mixed_primary_schema 返回 None"
        );
        assert_eq!(
            mgr.mixed_primary_schema("nonexistent"),
            None,
            "未知方案 mixed_primary_schema 返回 None"
        );

        let _ = std::fs::remove_dir_all(&base_dir);
        let _ = std::fs::remove_dir_all(&ov_dir);
    }
}
