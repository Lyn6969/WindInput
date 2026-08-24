//! 词典缓存层：yaml 首次加载后写入 .wdat 缓存，后续直接 mmap 读取
//!
//! 与 Go 版 mmap 共享池对齐，显著降低内存占用。

use crate::codetable::CodetableDict;
use crate::datformat::{WdatReader, WdatWriter};
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

/// 反查索引的存储与查询已下沉到 [`crate::reverseidx`]（`.wridx` 格式，可 mmap）。
/// 这里保留再导出，是因为它的构建入口（`build_reverse_index_from`）天然属于本模块
/// ——它要遍历 [`CachedDict`]，而消费方一直是 `use wind_dict::cached::ReverseIndex`。
pub use crate::reverseidx::{CodeList, ReverseIndex};

/// 一条查询命中，含音节边界。由 [`CachedDict::search_with_boundary`] /
/// [`CachedDict::search_prefix_with_boundary`] 返回（拼音专用）。
#[derive(Debug, Clone)]
pub struct DictHit {
    /// 命中的词典 code：精确查询即查询串本身，前缀查询为该条目的完整 code。
    pub code: String,
    pub text: String,
    pub weight: i32,
    pub order: i32,
    /// 音节边界 bitmask，见 [`crate::binformat::DictEntry::boundary`]。0=无边界信息，降级回 DAG。
    pub boundary: u64,
}

/// 前缀查询中该条目是否**该保留**：词的音节数不超过 `max_syllables`。
///
/// 拼音「短输入音节数匹配」判据的词库层半边（策略与阈值在 `wind-engine` 的
/// `STRICT_SYLLABLE_MATCH_MAX`，`max_syllables` 由调用方按**输入自身**的音节数给出）。
///
/// ⚠️ **判据问的是「输入有几个音节」，不是「输入在这条候选的切分下占几个音节」。**
/// 后者曾被用过一版，会让 `xia` 因为存在 `xi|an`（西安）这种切分而被当成 2 音节输入、
/// 放行整批词组；同理 `ying` 会漏出 `yin|guo`（因果）。输入的音节数是**输入自己的属性**、
/// 全局唯一（`completed_len` 是词图的性质，与走哪条切分路径无关），不该外包给候选。
///
/// `boundary == 0`（无边界信息）**一律放行** —— 词库层无从判断，交调用方按需用 DAG
/// 现切兜底（见 `wind-engine` 的 `effective_boundary`）。职责分工：词库层只提前丢弃
/// **确定不合格**的条目以免它们白占 top-N 配额，最终判定仍在引擎层。
pub fn prefix_syllable_keep(boundary: u64, max_syllables: u32) -> bool {
    boundary == 0 || boundary.count_ones() <= max_syllables
}

/// `foo/bar.dict.yaml` → `foo/bar.wdat`（剥掉整个 `.dict.yaml` 后缀）。
///
/// **不能用 `Path::with_extension("wdat")`** —— 那只替换最后一级扩展名，会得到
/// `bar.dict.wdat`。本约定与 Go 版 `wdbOnlyInDir` 一致，从 Go 版迁移过来的词库文件名
/// 可直接使用。
///
/// 供 wdat-only 分发模式定位**用户投放**的二进制词库，与缓存路径无关——后者由
/// `EngineManager::cache_path` 生成并落在独立的 cache 目录。
///
/// 非 `.dict.yaml` / `.yaml` 结尾返回 `None`。
pub fn wdat_sibling(yaml_path: &Path) -> Option<std::path::PathBuf> {
    let name = yaml_path.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".dict.yaml")
        .or_else(|| name.strip_suffix(".yaml"))?;
    Some(yaml_path.with_file_name(format!("{stem}.wdat")))
}

/// 缓存词典：优先使用 mmap，回退到内存模式
pub enum CachedDict {
    /// mmap 零拷贝模式（低内存，wdat DAT 格式）。
    ///
    /// reader 经 [`crate::reader_pool`] 按文件路径共享：同一个 wdat 被多个方案引用时
    /// （如 pinyin / shuangpin / 混输子引擎同指 rime_frost）只映射一份。用 `Arc` 而非
    /// 独占持有，是为了让最后一个持有者释放时 mmap 随之解除——词库重建要 rename 覆盖，
    /// Windows 上映射未解除会 Access Denied。
    Mmap(Arc<WdatReader>),
    /// 内存模式（首次加载或缓存写入失败）
    Memory(CodetableDict),
}

impl CachedDict {
    /// 加载词典，自动使用 .wdat 缓存
    ///
    /// 流程：
    /// 1. 检查 .wdat 缓存是否存在且比 .yaml 新
    /// 2. 如果是，直接 mmap 打开
    /// 3. 如果否，加载 .yaml，写入 .wdat 缓存，然后 mmap 打开
    pub fn load(yaml_path: &Path) -> anyhow::Result<Self> {
        let wdat_path = yaml_path.with_extension("wdat");
        Self::load_at(yaml_path, &wdat_path)
    }

    /// 加载词典，使用指定的 .wdat 缓存路径（缓存可与源分离，如放
    /// `%LOCALAPPDATA%\WindInput\cache`，避免写入只读的安装目录）。
    pub fn load_at(yaml_path: &Path, wdat_path: &Path) -> anyhow::Result<Self> {
        Self::load_at_with(yaml_path, wdat_path, false)
    }

    /// 同 [`load_at`]，`lowercase_code=true` 时把 code 列小写化（英文词库）。
    /// 缓存命中时直接 mmap（缓存内已是小写码）；缓存重建时用 `load_lowercased`。
    pub fn load_at_with(
        yaml_path: &Path,
        wdat_path: &Path,
        lowercase_code: bool,
    ) -> anyhow::Result<Self> {
        // wdat-only：用户只投放编译好的二进制词库、不带 yaml 源（对齐 Go 的 wdb-only 分发）。
        //
        // 必须抢在 cache_is_valid 之前：本模式下压根没有 yaml 源，而 cache_is_valid 问的是
        // 「缓存与**源**是否一致」——对无源可比的场景，那个问题本身就没有意义。
        // （2026-08-24 起 `cache_fp::fingerprint` 把「源缺失」当作可哈希的稳定事实而非失败，
        // 故这里不再能靠「缺源必判失效」兜底，顺序更是必须的。）
        // 也必须抢在下面 `CodetableDict::load(yaml_path)?` 之前，那是硬失败点
        // （文件不存在直接 Err，后续写缓存/mmap 都不会执行）。
        if !yaml_path.is_file()
            && let Some(sidecar) = wdat_sibling(yaml_path)
            && sidecar.is_file()
        {
            return match crate::reader_pool::open_wdat(&sidecar) {
                Ok(reader) => {
                    info!(
                        "以 wdat-only 模式加载词库: {} ({} keys)",
                        sidecar.display(),
                        reader.key_count()
                    );
                    Ok(Self::Mmap(reader))
                }
                // 无 yaml 源可重建，只能明确失败：静默降级会让整个方案无引擎，
                // 而用户完全无从判断是文件版本不对还是路径没放对。
                Err(e) => Err(anyhow::anyhow!(
                    "wdat-only 词库 {} 加载失败: {}。该词库无 yaml 源，无法重建，请更新词库文件。",
                    sidecar.display(),
                    e
                )),
            };
        }

        // 按缓存文件 single-flight：同一个 wdat 常被多个方案引用（如 wubi86 独立引擎与
        // 混输的 primary 子引擎），缓存失效时会同时走到下面的 write_cache → rename。
        // 混输子引擎的构建不经 ensure_loaded，也就不受 build_locks 保护，故须在此自锁。
        let build_lock = crate::reader_pool::file_lock(wdat_path);
        let _build_guard = build_lock.lock().unwrap_or_else(|e| e.into_inner());

        // 检查缓存是否有效（同时充当拿锁后的复查：等待期间别的线程可能已重建完成）
        if Self::cache_is_valid(yaml_path, wdat_path, lowercase_code) {
            match crate::reader_pool::open_wdat(wdat_path) {
                Ok(reader) => {
                    info!(
                        "Using mmap cache: {} ({} keys)",
                        wdat_path.display(),
                        reader.key_count()
                    );
                    return Ok(Self::Mmap(reader));
                }
                Err(e) => {
                    warn!("Failed to open mmap cache: {}, falling back to yaml", e);
                }
            }
        }

        // 加载 yaml
        let dict = if lowercase_code {
            CodetableDict::load_lowercased(yaml_path)?
        } else {
            CodetableDict::load(yaml_path)?
        };
        info!(
            "Loaded yaml: {} ({} entries)",
            yaml_path.display(),
            dict.len()
        );

        // 确保缓存目录存在后写入 .wdat 缓存
        if let Some(parent) = wdat_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = Self::write_cache(&dict, wdat_path) {
            // 退化成内存模式：词库整份常驻堆而非 mmap，大词库代价可观。此前只记一行
            // 「Failed to write」，看不出后果，用户也就无从解释内存为何偏高。
            warn!(
                "写入 wdat 缓存失败 {}: {}。本次退化为内存模式——词库常驻堆而非 mmap，\
                 内存占用显著高于正常路径。常见原因是缓存目录不可写或磁盘空间不足。",
                wdat_path.display(),
                e
            );
            return Ok(Self::Memory(dict));
        }
        // 写内容指纹 sidecar，供下次按内容(而非 mtime)校验复用。
        // tag 带上 lowercase_code：同一份 yaml 在 english / 非 english 两种 dict_type 下
        // 解析结果不同，不区分就会在切换后复用大小写错误的缓存。
        crate::cache_fp::write_cache_fp(
            wdat_path,
            &[yaml_path],
            crate::cache_fp::dict_tag(lowercase_code),
        );

        // 用 mmap 重新打开缓存
        match crate::reader_pool::open_wdat(wdat_path) {
            Ok(reader) => {
                info!(
                    "Using mmap cache: {} ({} keys)",
                    wdat_path.display(),
                    reader.key_count()
                );
                Ok(Self::Mmap(reader))
            }
            Err(e) => {
                warn!("Failed to open mmap cache after write: {}", e);
                Ok(Self::Memory(dict))
            }
        }
    }

    /// 检查缓存是否有效（按源文件**内容指纹** + 解析方式 tag，不受 scp/部署刷新 mtime 影响）。
    fn cache_is_valid(yaml_path: &Path, wdat_path: &Path, lowercase_code: bool) -> bool {
        crate::cache_fp::cache_is_fresh(
            wdat_path,
            &[yaml_path],
            crate::cache_fp::dict_tag(lowercase_code),
        )
    }

    /// 将内存词典写入 .wdat 缓存
    fn write_cache(dict: &CodetableDict, wdat_path: &Path) -> anyhow::Result<()> {
        let mut writer = WdatWriter::new();

        // 遍历所有键，导出到 writer
        dict.export_to_wdat(&mut writer);

        if writer.key_count() == 0 {
            anyhow::bail!("No entries to write");
        }

        writer.write(wdat_path)?;
        info!(
            "Wrote .wdat cache: {} ({} keys)",
            wdat_path.display(),
            writer.key_count()
        );
        Ok(())
    }

    /// 精确查找
    pub fn search(&self, code: &str) -> Vec<(String, i32, i32)> {
        match self {
            Self::Mmap(reader) => reader
                .search(code)
                .into_iter()
                .map(|e| (e.text, e.weight, e.order))
                .collect(),
            Self::Memory(dict) => dict.search(code),
        }
    }

    /// 精确查找，**并返回音节边界**（[`DictHit::boundary`]）。
    ///
    /// 与 [`Self::search`] 并存而非替换它：边界只对拼音有意义，而 `search` 的消费方遍布
    /// 码表/英文/cmdbar/composite 等无音节概念的场景（全仓 60+ 调用点），不应被拼音的
    /// 需求污染接口。仅拼音引擎改用本方法。
    pub fn search_with_boundary(&self, code: &str) -> Vec<DictHit> {
        match self {
            Self::Mmap(reader) => reader
                .search(code)
                .into_iter()
                .map(|e| DictHit {
                    code: e.code,
                    text: e.text,
                    weight: e.weight,
                    order: e.order,
                    boundary: e.boundary,
                })
                .collect(),
            // 内存回退（yaml 直载，未走 wdat）：CodetableDict 保有 boundary，一并带出。
            Self::Memory(dict) => dict.search_with_boundary(code),
        }
    }

    /// 前缀查找，**并返回音节边界**。与 [`Self::search_prefix`] 并存，理由同
    /// [`Self::search_with_boundary`]。前缀补全候选（输入 ni → 补出「你好」nihao）同样需要
    /// 边界供双拼校验。
    pub fn search_prefix_with_boundary(&self, prefix: &str, limit: usize) -> Vec<DictHit> {
        match self {
            Self::Mmap(reader) => reader
                .search_prefix(prefix, limit)
                .into_iter()
                .map(|e| DictHit {
                    code: e.code,
                    text: e.text,
                    weight: e.weight,
                    order: e.order,
                    boundary: e.boundary,
                })
                .collect(),
            Self::Memory(dict) => dict.search_prefix_with_boundary(prefix, limit),
        }
    }

    /// 前缀查找（带边界），**只取音节数不超过 `max_syllables` 的条目**。
    ///
    /// 拼音短输入严格档专用：过滤下推到词库层，使 top-N 直接是 N 条合格条目，而不是
    /// 让注定被丢弃的词组吃光配额（实测 `d` 取 1000 条只剩 68 条单字）。
    /// 判据见 [`prefix_syllable_keep`]，两个后端共用同一份。
    pub fn search_prefix_with_boundary_syllable_capped(
        &self,
        prefix: &str,
        limit: usize,
        max_syllables: u32,
    ) -> Vec<DictHit> {
        match self {
            Self::Mmap(reader) => reader
                .search_prefix_syllable_capped(prefix, limit, max_syllables)
                .into_iter()
                .map(|e| DictHit {
                    code: e.code,
                    text: e.text,
                    weight: e.weight,
                    order: e.order,
                    boundary: e.boundary,
                })
                .collect(),
            Self::Memory(dict) => {
                dict.search_prefix_with_boundary_syllable_capped(prefix, limit, max_syllables)
            }
        }
    }

    /// 是否存在**严格长于** `prefix` 的编码（存在性判据，不物化候选）。
    /// 两个后端语义一致：mmap 走 DAT 转移表 O(字节种数)，内存走 BTreeMap 有序扫常数条。
    pub fn has_longer_code(&self, prefix: &str) -> bool {
        match self {
            Self::Mmap(reader) => reader.has_longer_code(prefix),
            Self::Memory(dict) => dict.has_longer_code(prefix),
        }
    }

    /// 前缀查找
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, String, i32, i32)> {
        match self {
            Self::Mmap(reader) => reader
                .search_prefix(prefix, limit)
                .into_iter()
                .map(|e| (e.code, e.text, e.weight, e.order))
                .collect(),
            Self::Memory(dict) => dict.search_prefix(prefix, limit),
        }
    }

    /// 简拼查找（声母缩写，如 `nh`）：返回该简拼对应的**全拼码**列表，已按权重降序、截断。
    /// 仅 wdat(Mmap) 的独立 AbbrevSection 支持；内存回退（yaml 未建简拼）返回空。
    ///
    /// **返回的是码不是词**（v5）——AbbrevSection 是二级索引，指向主键。调用方拿码去
    /// 主表装配候选，从而得到真实的 code 与 boundary；此前直接返回词，简拼候选只能把
    /// code 设成简拼串，词频遂与全拼输入分裂成两份计数。
    pub fn search_abbrev(&self, abbrev: &str, limit: usize) -> Vec<String> {
        match self {
            Self::Mmap(reader) => reader
                .search_abbrev(abbrev, limit)
                .into_iter()
                .map(|e| e.text)
                .collect(),
            Self::Memory(_) => Vec::new(),
        }
    }

    /// 遍历全部条目(供反查索引构建)。
    pub fn for_each_entry(&self, f: &mut dyn FnMut(&str, &str, i32)) {
        match self {
            Self::Mmap(reader) => reader.for_each_entry(f),
            Self::Memory(dict) => dict.for_each_entry(f),
        }
    }

    /// 构建反查索引:汉字/词 → 词库中的**全部**编码,按「码长升序→字典序升序」排列并去重。
    /// 供悬停 [编码] 段显示完整打法列表(如 `a/ab/abc`)与拼音编码提示(取末位=最长码,
    /// 全码最稳——简码可能被一级简码等占用)。取词库实际码,避免按字生成码却打不出的错配。
    pub fn build_reverse_index(&self) -> ReverseIndex {
        build_reverse_index_from(std::slice::from_ref(self))
    }

    /// 构建**单字全码表**：汉字 → 该字在本词库中的全码。供码表造词按 `[[encoder.rules]]`
    /// 公式组装词组编码（`wind_engine::encoder`）。
    ///
    /// # 全码判据（按序）
    ///
    /// 1. **上限闸**：滤掉码长 > `max_code_length` 的编码（`0` = 不设闸）。扩展词库塞进来的
    ///    5/6 码怪码在此被排除——否则它就是「最长码」，后续判据根本没机会参与。
    /// 2. **最长码长**：简码（如「工」=`a`）位数不够公式取第 2 码，必须取全码。
    /// 3. **权重降序**：同码长多码时取权重高者。
    /// 4. **码字典序升序**：最终确定性兜底。
    ///
    /// # 为什么第 4 条不是「首次出现」
    ///
    /// `CodetableDict::for_each_entry` 遍历的是 `HashMap`，**顺序不确定**；mmap 路径则为码
    /// 字典序。取「首次出现」会让同权同长的字在两条路径下、甚至两次构建间得到不同的码。
    /// 字典序是确定性代偿，且与 mmap 路径的天然顺序一致。
    pub fn build_single_char_full_codes(
        &self,
        max_code_length: usize,
    ) -> std::collections::HashMap<char, String> {
        build_single_char_full_codes_from(std::slice::from_ref(self), max_code_length)
    }

    /// 本词库**实际读取的文件**；内存模式无文件 → `None`。
    ///
    /// 供二级缓存（`.wridx` 反查索引）判定「我派生自哪几个文件、它们变了没有」。
    /// 取的是 reader 自报的路径而非调用方重新推导——`load_dicts_individually` 里
    /// wdat 路径的推导分了三个分支（普通 / english 小写 / rime_pinyin 合并），
    /// 在别处照抄一遍必然会有一天与它分叉。
    ///
    /// `None` 有明确后果：内存模式意味着这份词库根本没有稳定的磁盘产物，
    /// 以它为源的二级缓存不该落盘（见 `EngineManager::build_reverse_index_for`）。
    pub fn source_file(&self) -> Option<&Path> {
        match self {
            Self::Mmap(reader) => Some(reader.path()),
            Self::Memory(_) => None,
        }
    }

    /// 总条目数
    pub fn len(&self) -> usize {
        match self {
            Self::Mmap(reader) => reader.key_count() as usize,
            Self::Memory(dict) => dict.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 从**多个词库**构建反查索引，等价于「先合并成一份再构建」，但**不产出中间文件**。
///
/// # 为什么等价（这条等价性是删掉 `combined.wdat` 的全部依据）
///
/// [`ReverseIndex::build`] 自己就会按 `(text, code)` 去重、并把权重取为该词全部条目的
/// **最大值**。这恰好覆盖了合并步骤所做的一切（按 code 聚合、同 text 保留更高权重）——
/// 于是「喂各库条目的原始并集」与「喂合并后的结果」产出的索引**逐位相同**。
///
/// 真机实测（feihuzj2，11 库 253 万条）：两条路径都得到 2519693 词、95.4 MB，
/// 而直接构建 1.85 秒、经 `combined.wdat` 需 30 秒建文件 + 3.2 秒建索引。
///
/// 各库的 wdat 由 [`crate::reader_pool`] 按路径共享，活着的引擎通常已持有同一批 reader，
/// 故这里的「逐库打开」几乎零成本：不重新解析、不新增映射。
pub fn build_reverse_index_from(dicts: &[CachedDict]) -> ReverseIndex {
    ReverseIndex::from_bytes(serialize_reverse_index_from(dicts))
}

/// 同 [`build_reverse_index_from`]，但停在**序列化镜像**这一步。
///
/// 分出这个入口是为了让调用方能「先落盘、再从盘上打开」——那条路径能把索引字节
/// 从进程私有内存里彻底移出去（mmap），而 [`build_reverse_index_from`] 得到的
/// 索引恒常驻。两者产出的字节**完全相同**，故行为无差。
pub fn serialize_reverse_index_from(dicts: &[CachedDict]) -> Vec<u8> {
    let mut pairs: Vec<(String, String, i32)> = Vec::new();
    for d in dicts {
        d.for_each_entry(&mut |code, text, weight| {
            pairs.push((text.to_string(), code.to_string(), weight));
        });
    }
    crate::reverseidx::serialize(pairs)
}

/// 从**多个词库**构建单字全码表。判据与单库版完全一致（见
/// [`CachedDict::build_single_char_full_codes`] 的四条），只是候选来自各库并集。
///
/// 与反查索引同理，判据「码长 → 权重 → 码字典序」对条目到达顺序不敏感，
/// 故并集与合并结果等价——这也是它能一起摆脱 `combined.wdat` 的原因。
pub fn build_single_char_full_codes_from(
    dicts: &[CachedDict],
    max_code_length: usize,
) -> std::collections::HashMap<char, String> {
    use std::collections::HashMap;
    // 值存 (code, weight)；weight 仅用于比较，不外传。
    let mut best: HashMap<char, (String, i32)> = HashMap::new();
    for d in dicts {
        d.for_each_entry(&mut |code, text, weight| {
            let mut it = text.chars();
            let (Some(ch), None) = (it.next(), it.next()) else {
                return; // 只收单字条目
            };
            let len = code.chars().count();
            if len == 0 || (max_code_length > 0 && len > max_code_length) {
                return;
            }
            match best.get(&ch) {
                Some((cur, cur_w)) => {
                    let cur_len = cur.chars().count();
                    let better = len > cur_len
                        || (len == cur_len
                            && (weight > *cur_w || (weight == *cur_w && code < cur.as_str())));
                    if better {
                        best.insert(ch, (code.to_string(), weight));
                    }
                }
                None => {
                    best.insert(ch, (code.to_string(), weight));
                }
            }
        });
    }
    best.into_iter().map(|(k, (code, _))| (k, code)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codetable::CodetableDict;

    #[test]
    fn wdat_sibling_strips_the_whole_dict_yaml_suffix() {
        let p = Path::new("schemas/wubi86/wubi86_jidian.dict.yaml");
        assert_eq!(
            wdat_sibling(p).unwrap(),
            Path::new("schemas/wubi86/wubi86_jidian.wdat"),
            "须剥掉整个 .dict.yaml（与 Go 版 wdbOnlyInDir 一致）"
        );
        // 对照：with_extension 只换最后一级，会得到错误的名字——这正是不能用它的原因
        assert_eq!(
            p.with_extension("wdat"),
            Path::new("schemas/wubi86/wubi86_jidian.dict.wdat")
        );
        // 裸 .yaml 也支持
        assert_eq!(
            wdat_sibling(Path::new("a/b.yaml")).unwrap(),
            Path::new("a/b.wdat")
        );
        // 非 yaml 不推导
        assert!(wdat_sibling(Path::new("a/b.txt")).is_none());
        assert!(wdat_sibling(Path::new("a/b.wdat")).is_none());
    }

    fn wdat_only_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wind-wdat-only-{}-{}", std::process::id(), tag));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// wdat-only 分发：方案目录里只有编译好的 .wdat，没有 .dict.yaml 源。
    #[test]
    fn loads_wdat_only_when_yaml_is_absent() {
        let dir = wdat_only_dir("load");
        let yaml = dir.join("x.dict.yaml"); // 故意不创建
        let wdat = dir.join("x.wdat");
        let mut d = CodetableDict::empty();
        d.merge_single("a".into(), "啊".into(), 1, 0);
        let mut w = WdatWriter::new();
        d.export_to_wdat(&mut w);
        w.write(&wdat).unwrap();

        // 缓存路径同样不存在——wdat-only 必须绕过整个指纹/缓存流程
        let cache = dir.join("cache").join("x.wdat");
        let loaded = CachedDict::load_at_with(&yaml, &cache, false)
            .expect("yaml 缺失但同名 wdat 在场时应走 wdat-only");
        assert_eq!(loaded.search("a").len(), 1);
        assert!(
            !cache.exists(),
            "wdat-only 不应生成缓存副本，应原位直接 mmap"
        );

        drop(loaded);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 无源可重建时必须明确报错，而不是静默降级成「整方案无引擎」。
    #[test]
    fn wdat_only_unreadable_file_errors_with_actionable_message() {
        let dir = wdat_only_dir("corrupt");
        let yaml = dir.join("y.dict.yaml"); // 不存在
        let wdat = dir.join("y.wdat");
        std::fs::write(&wdat, b"this is not a valid wdat").unwrap();

        let Err(err) = CachedDict::load_at_with(&yaml, &dir.join("cache/y.wdat"), false) else {
            panic!("损坏的 wdat-only 词库必须报错，不能静默降级");
        };
        let msg = err.to_string();
        assert!(msg.contains("wdat-only"), "须指明是 wdat-only 模式: {msg}");
        assert!(msg.contains("无法重建"), "须告知无源可重建: {msg}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// yaml 在场时不得被 wdat 抢走——正常方案的行为一步都不能变。
    #[test]
    fn yaml_present_still_takes_the_normal_path() {
        let dir = wdat_only_dir("yaml-wins");
        let yaml = dir.join("z.dict.yaml");
        std::fs::write(&yaml, "# comment\nzz\t再\t1\n").unwrap();
        // 同时放一个内容不同的 wdat，若被误用则查得到 "抢"
        let mut d = CodetableDict::empty();
        d.merge_single("qq".into(), "抢".into(), 1, 0);
        let mut w = WdatWriter::new();
        d.export_to_wdat(&mut w);
        w.write(dir.join("z.wdat")).unwrap();

        let loaded = CachedDict::load_at_with(&yaml, &dir.join("cache/z.wdat"), false).unwrap();
        assert!(
            loaded.search("qq").is_empty(),
            "yaml 存在时绝不能走 wdat-only 分支"
        );

        drop(loaded);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 全码判据①②：简码不能胜出（否则公式取第 2 码越界），且超过 max_code_length 的
    /// 怪码被上限闸排除——这两条各自都足以让造词静默失效。
    #[test]
    fn single_char_full_code_prefers_longest_within_cap() {
        let mut d = CodetableDict::empty();
        // 「工」：一级简码 a + 全码 aaaa → 应取 aaaa（简码只有 1 位，取不到第 2 码）。
        d.merge_single("a".into(), "工".into(), 9999, 0);
        d.merge_single("aaaa".into(), "工".into(), 100, 1);
        // 「中」：全码 khk + 扩展库塞进来的 6 码怪码 → 上限闸(4) 排除怪码，取 khk。
        d.merge_single("khk".into(), "中".into(), 500, 2);
        d.merge_single("khkkhk".into(), "中".into(), 9999, 3);
        let cd = CachedDict::Memory(d);
        let idx = cd.build_single_char_full_codes(4);
        assert_eq!(
            idx.get(&'工').map(String::as_str),
            Some("aaaa"),
            "简码权重再高也不能当全码"
        );
        assert_eq!(
            idx.get(&'中').map(String::as_str),
            Some("khk"),
            "超 max_code_length 的怪码应被上限闸排除，即使它更长、权重更高"
        );
    }

    /// 上限闸关闭（0）时退化为纯「最长优先」——非定长码方案不应被误滤。
    #[test]
    fn single_char_full_code_cap_zero_disables_gate() {
        let mut d = CodetableDict::empty();
        d.merge_single("khk".into(), "中".into(), 500, 0);
        d.merge_single("khkkhk".into(), "中".into(), 100, 1);
        let cd = CachedDict::Memory(d);
        assert_eq!(
            cd.build_single_char_full_codes(0)
                .get(&'中')
                .map(String::as_str),
            Some("khkkhk"),
            "cap=0 应不设闸，取最长"
        );
    }

    /// 全码判据③④：同码长先比权重，权重相同再比码字典序（确定性兜底）。
    #[test]
    fn single_char_full_code_breaks_ties_by_weight_then_code() {
        let mut d = CodetableDict::empty();
        // 同为 2 码：权重高的 de 胜出。
        d.merge_single("dd".into(), "大".into(), 10, 0);
        d.merge_single("de".into(), "大".into(), 99, 1);
        // 同为 2 码且同权重：字典序小的 aa 胜出。
        d.merge_single("ab".into(), "式".into(), 50, 2);
        d.merge_single("aa".into(), "式".into(), 50, 3);
        let cd = CachedDict::Memory(d);
        let idx = cd.build_single_char_full_codes(4);
        assert_eq!(idx.get(&'大').map(String::as_str), Some("de"));
        assert_eq!(idx.get(&'式').map(String::as_str), Some("aa"));
    }

    /// 只收单字：多字词条不得进入全码表（否则「你好」会被当成一个"字"）。
    #[test]
    fn single_char_full_code_skips_multi_char_entries() {
        let mut d = CodetableDict::empty();
        d.merge_single("wqvb".into(), "你好".into(), 100, 0);
        d.merge_single("wqiy".into(), "你".into(), 100, 1);
        let cd = CachedDict::Memory(d);
        let idx = cd.build_single_char_full_codes(4);
        assert_eq!(idx.len(), 1, "多字词条应被跳过");
        assert!(idx.contains_key(&'你'));
    }

    /// 反查索引:同词收集全部码,码长升序 → 字典序升序,去重。
    #[test]
    fn reverse_index_collects_all_codes_sorted_by_len() {
        let mut d = CodetableDict::empty();
        // 「工」简码+全码 → 短码在前。
        d.merge_single("aaaa".into(), "工".into(), 100, 0);
        d.merge_single("a".into(), "工".into(), 100, 1);
        // 「中」唯一码 + 重复条目去重。
        d.merge_single("k".into(), "中".into(), 50, 2);
        d.merge_single("k".into(), "中".into(), 40, 3);
        // 「大」同长两码 → 字典序。
        d.merge_single("de".into(), "大".into(), 10, 4);
        d.merge_single("dd".into(), "大".into(), 99, 5);
        let cd = CachedDict::Memory(d);
        let idx = cd.build_reverse_index();
        assert_eq!(idx.len(), 3);
        assert_eq!(
            idx.codes_of("工").unwrap().iter().collect::<Vec<_>>(),
            vec!["a", "aaaa"]
        );
        assert_eq!(
            idx.codes_of("中").unwrap().iter().collect::<Vec<_>>(),
            vec!["k"],
            "重复条目应去重"
        );
        assert_eq!(
            idx.codes_of("大").unwrap().iter().collect::<Vec<_>>(),
            vec!["dd", "de"],
            "同长按字典序"
        );
        assert!(idx.codes_of("无").is_none());
        // 消费侧的两种取法
        assert_eq!(
            idx.codes_of("工").unwrap().last(),
            Some("aaaa"),
            "末位=全码"
        );
        assert_eq!(idx.codes_of("工").unwrap().join("/"), "a/aaaa");
    }

    #[test]
    fn reverse_index_arena_boundaries_dont_bleed() {
        // arena 是连续拼接的,相邻词/码若边界算错会串味。用等长且互为前缀的样本压这个边界。
        let mut d = CodetableDict::empty();
        d.merge_single("aa".into(), "甲".into(), 1, 0);
        d.merge_single("aaa".into(), "甲".into(), 1, 1);
        d.merge_single("bb".into(), "乙".into(), 1, 2);
        d.merge_single("cc".into(), "丙".into(), 1, 3);
        let idx = CachedDict::Memory(d).build_reverse_index();
        assert_eq!(
            idx.codes_of("甲").unwrap().iter().collect::<Vec<_>>(),
            vec!["aa", "aaa"]
        );
        assert_eq!(
            idx.codes_of("乙").unwrap().iter().collect::<Vec<_>>(),
            vec!["bb"]
        );
        // 「丙」按字节序排首位,其编码起点必须是 0 而非前一条目的终点
        assert_eq!(
            idx.codes_of("丙").unwrap().iter().collect::<Vec<_>>(),
            vec!["cc"]
        );
        assert!(idx.codes_of("").is_none(), "空词不应命中");
    }

    /// ★★★ **P1 的全部依据**：「逐库并集」与「先合并再构建」必须产出**完全相同**的索引。
    ///
    /// 这条等价性成立，才谈得上删掉 `combined.wdat`（230MB 文件 + 30 秒构建）。
    /// 样本刻意覆盖三种合并才会遇到的冲突形态——只有其中任一被处理错，两侧就会分叉：
    ///   ① 同 (text, code) 在两库权重不同  ② 同 text 不同 code 分散在两库
    ///   ③ 同 code 不同 text 分散在两库
    #[test]
    fn union_of_dicts_equals_merged_dict_for_reverse_index() {
        // 两个分库。
        let mut a = CodetableDict::empty();
        a.merge_single("aaaa".into(), "工".into(), 100, 0);
        a.merge_single("a".into(), "工".into(), 50, 1); // ② 同词另一个码
        a.merge_single("khk".into(), "中".into(), 10, 2); // ① 低权重那份
        a.merge_single("de".into(), "大".into(), 7, 3); // ③ 同码不同词
        let mut b = CodetableDict::empty();
        b.merge_single("khk".into(), "中".into(), 999, 0); // ① 高权重那份
        b.merge_single("de".into(), "太".into(), 8, 1); // ③ 同码另一个词
        b.merge_single("khkkhk".into(), "中".into(), 5, 2);

        // 合并版：按 combined 的语义（同 code 聚合，同 text 取更高权重）手工合成一份。
        let mut merged = CodetableDict::empty();
        merged.merge_single("aaaa".into(), "工".into(), 100, 0);
        merged.merge_single("a".into(), "工".into(), 50, 1);
        merged.merge_single("khk".into(), "中".into(), 999, 2); // 取 max
        merged.merge_single("de".into(), "大".into(), 7, 3);
        merged.merge_single("de".into(), "太".into(), 8, 4);
        merged.merge_single("khkkhk".into(), "中".into(), 5, 5);

        let union = build_reverse_index_from(&[CachedDict::Memory(a), CachedDict::Memory(b)]);
        let via_merged = CachedDict::Memory(merged).build_reverse_index();

        assert_eq!(union.len(), via_merged.len(), "词数必须一致");
        assert_eq!(
            union.image(),
            via_merged.image(),
            "两条路径的序列化镜像必须逐字节相同——这是「逐位等价」最直接的判据\
             （此前只比了总字节数，两份布局不同而大小恰好相等时会漏过）"
        );
        for text in ["工", "中", "大", "太"] {
            let u: Vec<_> = union.codes_of(text).unwrap().iter().collect();
            let m: Vec<_> = via_merged.codes_of(text).unwrap().iter().collect();
            assert_eq!(u, m, "{text} 的编码列表必须一致");
        }
        // 权重取 max 这一点单独验：它决定词语联想的排序，错了很难被上面几条发现。
        assert_eq!(
            union.texts_with_prefix("中", 9),
            via_merged.texts_with_prefix("中", 9),
            "联想排序（依赖每词的 max 权重）必须一致"
        );
    }

    /// 同上，针对单字全码表——它与反查索引共用同一条「摆脱 combined」的依据。
    #[test]
    fn union_of_dicts_equals_merged_dict_for_single_char_codes() {
        let mut a = CodetableDict::empty();
        a.merge_single("a".into(), "工".into(), 9999, 0); // 简码，权重极高但不该胜出
        a.merge_single("khk".into(), "中".into(), 10, 1);
        let mut b = CodetableDict::empty();
        b.merge_single("aaaa".into(), "工".into(), 1, 0); // 全码，应胜出
        b.merge_single("khk".into(), "中".into(), 999, 1);
        b.merge_single("khkkhk".into(), "中".into(), 9999, 2); // 超上限闸，应被排除

        let mut merged = CodetableDict::empty();
        merged.merge_single("a".into(), "工".into(), 9999, 0);
        merged.merge_single("aaaa".into(), "工".into(), 1, 1);
        merged.merge_single("khk".into(), "中".into(), 999, 2);
        merged.merge_single("khkkhk".into(), "中".into(), 9999, 3);

        let union =
            build_single_char_full_codes_from(&[CachedDict::Memory(a), CachedDict::Memory(b)], 4);
        let via_merged = CachedDict::Memory(merged).build_single_char_full_codes(4);
        assert_eq!(union, via_merged);
        assert_eq!(union.get(&'工').map(String::as_str), Some("aaaa"));
        assert_eq!(union.get(&'中').map(String::as_str), Some("khk"));
    }

    #[test]
    fn reverse_index_empty_is_empty() {
        let idx = CachedDict::Memory(CodetableDict::empty()).build_reverse_index();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(idx.codes_of("任意").is_none());
    }
}
