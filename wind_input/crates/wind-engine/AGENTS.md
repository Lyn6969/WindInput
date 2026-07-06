<!-- Parent: ../../AGENTS.md -->
<!-- Updated: 2026-07-06 -->

# wind-engine

## Purpose
输入引擎层：把用户编码转成候选词。Schema 驱动的引擎工厂——按方案 TOML 的 `engine_type` 构建四类引擎（拼音（含双拼）/ 码表 / 混输 / 英文），由 `EngineManager` 统一懒加载、切换并分发 `convert`。下游消费 wind-config（方案定义）、wind-dict（词库查询）、wind-store（用户/临时词频），上游被 coordinator 调用。
从词库到候选的全链路现状文档见 [docs/architecture/engine-candidate-pipeline.md](../../../docs/architecture/engine-candidate-pipeline.md)。

## Key Files
| File | Description |
|------|-------------|
| `src/lib.rs` | 对外导出：`EngineManager`、`Engine`/`ExtendedEngine`/`ConvertResult`/`EngineType`、四类引擎、`FreqSettings`/`FreqStrategy` |
| `src/engine.rs` | `Engine`/`ExtendedEngine` trait 与 `ConvertResult`（候选 + preedit + 上屏/分段语义字段）；trait 默认方法把顶码、造词出码、扩展词库热插拔做成可选能力 |
| `src/manager.rs` | `EngineManager`：方案注册表 + `build_engine` 工厂。懒加载、`switch_schema`/`cycle_schema`、`reload_from_config`/`invalidate_schema` 热更新、override 层深合并、主码表反查索引、双拼韵母键集、词频设置解析 |
| `src/freq_rerank.rs` | 词频重排（排序独立维度，**绝不改 weight**）：码表/混输 `rerank_codetable_usedfirst`（档位感知永久 used-first）、拼音 `rerank_pinyin_decay`（衰减软置前 + 整句豁免 + 阈值褪色）。由 coordinator 在引擎排序后调用 |
| `src/pinyin/mod.rs` | `PinyinEngine`：精确 → Viterbi 整句 → DAG 子短语 → 前缀补全 → 简拼 → store 造词层，按层级排序（完整 >> 子短语 >> 前缀 >> 模糊）。子模块 `dag`/`viterbi`/`lattice`/`lm`/`scorer`/`fuzzy`/`syllable`/`shuangpin`/`generate`/`parser` |
| `src/codetable/engine.rs` | `CodeTableEngine`：经 `DictManager`(CompositeDict) 精确 + 前缀查询；全码自动上屏、顶码、`clear_on_empty_max` 等上屏策略（`CommitOptions`） |
| `src/mixed/engine.rs` | `MixedEngine`：持码表主 + 拼音次 + 可选英文子引擎，分档加权合并（码表精确 +boost、短语 +1M、英文精确 +500K、前缀 +500K，拼音 ÷100 降档）；**拼音否决统一入口 `pinyin_vetoes_commit`**（否决①粗粒度默认关 / ②词强度默认开，满码/顶码/显示态复评三通路共用）；超码长走 `convert_overflow`（`pinyin_only_overflow` 分流） |
| `src/english.rs` | `EnglishEngine`：码表引擎薄包装（词库 code 列小写化，大小写不敏感前缀匹配），独立方案或被混输懒加载（`schema.mix.enable_english`） |

## For AI Agents

### Working In This Directory
- **引擎构建唯一入口是 `manager.rs::build_engine`**：读方案 TOML（用户目录 > 安装目录，再深合并 `schema_overrides/{id}.toml`），按 `engine_type` 分派；mixed 递归构建 primary/secondary 子引擎。新增方案字段须在此解析，并考虑是否需在 `reload_from_config` 热更新（否则改设置要重启才生效）。
- **码表行为分层（仅码表有 override）**：上屏等行为解析顺序为 方案 `schema_overrides/{id}.toml [codetable]`（带 `enabled` 总开关，逐字段 `Some` 覆盖）> 全局 `schema.codetable` > 内置默认；统一经 `CodetableGlobal::resolved()` / `resolve_codetable()`。拼音、混输**无方案 override**，只读全局 `schema.pinyin` / `schema.mix`。混输的码表类行为继承主码表 `schema.codetable`。调频/造词全局唯一按引擎分（`schema.codetable.frequency` / `schema.pinyin.frequency`）。详见 `docs/redesign/schema-config-layering.md`。
- **词频是与 weight 解耦的独立维度**：引擎 `convert` 只产出基础权重候选；`freq_rerank` 是 coordinator 排序后调用的纯函数，**不得在引擎内改 weight 做词频**。两套语义（码表永久 used-first / 拼音衰减褪色）不可混用。
- **拼音 vs 码表的根本差异**：拼音走连续解码（DAG 分词 + Viterbi/unigram 打分 + 层级排序），码表只做 `DictManager` 精确 + 前缀查表无评分。改拼音排序须同步 `pinyin/mod.rs` 引擎层与 `freq_rerank.rs` 重排层的 `is_fuzzy`/`is_prefix`/`is_partial` 层级判定（两处必须一致，否则分层失效）。
- **懒加载 + single-flight 构建锁**：`ensure_loaded` 抢方案专属 build_lock 后复查，避免后台预热与首次切换重复熔大词库；不同方案可并行构建。引擎缓存仅在 `invalidate_schema`/`reload_from_config` 清除（无 LRU 驱逐，与 Go 版不同）。
- **`convert` 永不 panic**：引擎错误降级为 `ConvertResult::default()`（空候选），勿在热路径用会 panic 的 `unwrap`。锁中毒统一 `unwrap_or_else(|e| e.into_inner())`。
- **扩展词库热插拔**：`set_dict_enabled` 直接翻 `codetable-extra-<id>` 系统层的 enabled 标志，无需重建引擎；启用集变化会失效反查索引（编码提示依赖启用词库合并）。
- **混输拼音否决默认值三处同源**：`auto_commit_block_on_pinyin` 默认关、`block_commit_on_pinyin_word` 默认开——`MixConfig::default()`（mixed/engine.rs）、`MixGlobal::default()` + serde default（wind-config/config.rs）、`data/config.toml [schema.mix]` 三处必须一致，改默认须同步全部三处。

### Testing Requirements
- 纯逻辑，host 可跑：`cargo test -p wind-engine`（单元测试 + `tests/engine_manager.rs` 集成测试；部分用例读 `data/schemas/` 真实数据文件）。
- 传递依赖的 wind-dict 仅在 `cfg(windows)` 下引入 `windows` crate；本仓开发机即 Windows，host 直接构建/测试即可，无需交叉编译或上设备。

## Dependencies

### Internal
- `wind-candidate` — `Candidate`/`CandidateSource`/`better` 排序
- `wind-dict` — `CachedDict`、`DictManager`/CompositeDict、`SystemDictLayer`/`StoreUserLayer`/`StoreTempLayer`、unigram mmap
- `wind-store` — redb 用户/临时词库与词频记录（`FreqRecord`/`FreqProfile`）
- `wind-config` — `Config`、`schema::Schema`/`DictSpec`、`CodeCommitConfig`、`PinyinGlobalConfig`

### External
- `tracing`（日志）、`anyhow`/`thiserror`（错误）、`serde`/`serde_yaml`/`toml`（方案与 override 解析）

## 全局约束
- 引用根 `AGENTS.md`：INFO 级日志只记方案 id / 条目数 / key 数，**不得含候选文本、用户输入或词库内容**；改完跑 `cargo fmt`。

<!-- MANUAL: 此行以下为人工补充区，重新生成时保留 -->
