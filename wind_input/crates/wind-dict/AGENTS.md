<!-- Parent: ../../AGENTS.md -->
<!-- Updated: 2026-06-29 -->

# wind-dict

## Purpose

多层复合词典系统。定义 `DictLayer` 查询接口与 `CompositeDict` 跨层聚合引擎，负责 wdb/wdat 二进制格式的生成与 mmap 零拷贝查询，并将 `wind-store` 用户词/临时词桥接为可挂载的查询层。向上为各引擎 crate 暴露 `DictManager` 作为每方案唯一词典入口，向下依赖 `wind-candidate`（候选类型）与 `wind-store`（持久化后端）。

## Key Files

| File | Description |
|------|-------------|
| `src/lib.rs` | 对外导出 `CompositeDict`、`DictLayer/LayerType/MutableLayer`、`DictManager/SystemDictLayer`、`StoreUserLayer/StoreTempLayer` |
| `src/layer.rs` | `DictLayer`/`MutableLayer` trait；`LayerType` 枚举（Logic=0 \< User=1 \< Temp=2 \< Cell=3 \< System=4，数值越小优先级越高） |
| `src/composite.rs` | `CompositeDict` 跨层聚合；去重继承更高权重；前缀查保最短码；`set_layer_enabled` 热插拔；含完整单元测试 |
| `src/manager.rs` | `DictManager` 持有每方案一个 `CompositeDict`；`SystemDictLayer` 把 `CachedDict` 包为 `DictLayer`，用原子标志支持运行时启停 |
| `src/store_layer.rs` | `StoreUserLayer` / `StoreTempLayer` 把 `wind-store` 查询包装为 `DictLayer`（User/Temp 层） |
| `src/binformat.rs` | wdb 格式 `DictReader`（mmap 二分查询，V2/V3）/ `DictWriter`（原子 .tmp→rename 写入）；`for_each_entry` 供反查索引构建 |
| `src/datformat.rs` | wdat（DAT）格式，含独立 `AbbrevSection`（简拼区）；`CachedDict::Mmap` 实际使用的格式 |
| `src/cached.rs` | `CachedDict`（Mmap/Memory 两种模式）；yaml→wdat 缓存流程 + **wdat-only 模式**（用户只投放二进制词库、无 yaml 源时原位直接加载，加载失败不可重建）；`build_reverse_index` 构建汉字→编码反查表 |
| `src/reader_pool.rs` | wdat/unigram mmap reader 进程级共享池（按缓存文件路径复用，存 `Weak` 不持强引用——mmap 期间 rename 会 Access Denied，池持强引用会让词库重建永久失败；新鲜度归 `cache_fp`，本池只管「同一路径只 mmap 一份」） |
| `src/cache_fp.rs` | 基于源文件内容指纹（SipHash sidecar `.fp`）的缓存有效性校验，解决部署刷新 mtime 导致恒重建的问题 |
| `src/codetable.rs` | `CodetableDict` 内存 BTreeMap；自动检测五笔/拼音 Rime 格式；`parse_rime_entries_parallel` 多线程解析（>1MB 走 `thread::scope`），输出全拼+简拼两组 |
| `src/trie.rs` | 前缀 Trie，供英文词库回退路径等组件使用 |
| `src/hotcache.rs` | 热点码查找缓存层 |
| `src/unigram.rs` | 单字频数据结构 |

## For AI Agents

### Working In This Directory

- **每方案独立 DictManager**：Rust 采用「每方案引擎各持一个 `DictManager`（含 `CompositeDict`）」而非 Go 的「单 composite 切层」。切方案由 EngineManager 缓存引擎实现，`DictManager` 本身无 `SwitchSchema`——不要在 `DictManager` 上添加跨方案切换逻辑。

- **CompositeDict 去重语义**：同 `text` 已存在时，保留高优先层（先注册、LayerType 数值更小）的 `code` / `natural_order`，但**继承任意层中更高的 `weight`**（刻意跨层取值，使用户词不因低权重丢失码表词的自然排序位）；前缀查同 `text` 多码取最短码。该语义在 `composite.rs merge_search` 中有精确注释，修改前必读。

- **LayerType 排序不变量**：`register_layer` 按 `layer_type as u8` 稳定排序，同类型层按注册先后决定层内优先级（主库先于扩展）。`LayerType::System` 优先级最低（4），`Logic` 最高（0）；层内顺序偏移为 `PER_LAYER_NO_OFFSET = 10_000_000`，不得随意调整。

- **binformat entry_off 是字节偏移**：`DictKeyIndex.entry_off` 表示 EntryRecords 区内的**字节偏移**（累计条目数 × entry_size），而非条目数本身——历史上曾被误写为条目数导致非首 key 全部读乱码。修改 `DictWriter` 写入逻辑时务必保持此语义，见 `binformat.rs` 写入注释与往返回归测试。

- **缓存格式为 wdat（DAT），非 wdb**：`CachedDict` 走 `datformat.rs` 的 wdat 格式（含 AbbrevSection 简拼），而非旧的 wdb；`cache_fp.rs` 用内容指纹而非 mtime，缓存路径可与源分离（`CachedDict::load_at`），部署时不会因 mtime 刷新恒重建。

- **Shadow 不是查询层**：`ShadowRecord`（pin/delete 规则）不挂进 `CompositeDict`，其应用由引擎在候选排序后由调用方执行；本 crate 只在 `store_layer.rs` 桥接 `wind-store` 的查询，不持有 Shadow 状态。

### Testing Requirements

- `wind-dict` 在 `[target.'cfg(windows)'.dependencies]` 中依赖 `windows` crate（条件编译），但各测试均为文件 I/O 与纯内存逻辑，不直接调用 Windows API。
- Windows 下可直接运行：`cargo test -p wind-dict`；跨平台 CI 注意 Windows 条件依赖仅在 Windows target 下链接，Linux/macOS host 可跑但不会链接 windows crate。

## Dependencies

### Internal

- `wind-candidate` — `Candidate` 类型、排序函数 `better`、`CandidateSource`
- `wind-store` — `Store`、`user_words::UserWordRecord`（经 `store_layer.rs` 桥接）

### External

- `memmap2` — mmap 零拷贝词库文件
- `serde` / `serde_yaml` / `toml` — 词库/配置反序列化
- `anyhow` / `thiserror` — 错误处理
- `tracing` — 结构化日志

## 全局约束

提交前跑 `cargo fmt`；日志 INFO 级不得含词库条目内容（见根 `AGENTS.md`）。

<!-- MANUAL: 此行以下为人工补充区，重新生成时保留 -->
