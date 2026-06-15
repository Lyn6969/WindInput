# WindInput — Go → Rust 迁移计划

## 目标

将 WindInput 的 Go 核心服务（`wind_input/`）完整迁移到 Rust，以实现：
- **内存占用大幅降低**（消除 GC 开销、紧凑数据结构、mmap 零拷贝）
- **保持 100% 协议兼容**（TSF DLL 和设置前端无需修改）
- **保持功能等价**（所有输入法行为完全一致）

## 不变的部分

- `wind_tsf/` — C++ TSF DLL（消费端，不改动）
- `data/` — 配置/码表/主题数据文件
- `installer/`、`scripts/` — 构建/安装脚本（适配 Rust 构建）

## 技术选型

| Go 组件 | Rust 替代 | 理由 |
|---------|----------|------|
| bbolt (BoltDB) | **redb** | 纯 Rust、mmap B+ tree、零 GC、API 简洁 |
| gg (2D graphics) | **tiny-skia** | 纯 Rust、CPU 光栅化、支持 path/fill/gradient/clip |
| go-winio (named pipe) | **windows-rs / tokio** | 原生 Named Pipe + async I/O |
| golang.org/x/image | **image** crate | PNG/JPEG 解码 |
| oksvg + rasterx | **resvg** | 纯 Rust SVG 渲染，性能更好 |
| go-toml | **toml** (toml-rs) | 标准 TOML 解析 |
| yaml.v3 | **serde_yaml** | YAML 解析 |
| slog | **tracing** | 结构化日志 |
| CGO DirectWrite | **windows-rs DirectWrite** | FFI 绑定，无 CGO 开销 |
| sync.Mutex / RWMutex | **std::sync::Mutex / parking_lot** | 更细粒度的并发控制 |
| goroutine + channel | **tokio + mpsc** | 异步 runtime |

## 模块结构

```
WindInput/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── wind-ipc/                 # IPC 协议定义 + 编解码
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── protocol.rs       # 命令码、Header、Payload 结构体
│   │   │   ├── codec.rs          # 二进制编解码器
│   │   │   └── rpc.rs            # JSON-RPC 协议（设置前端通信用）
│   │   └── Cargo.toml
│   │
│   ├── wind-bridge/              # Named Pipe 服务器 + 共享内存
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── server.rs         # 请求-响应管道服务
│   │   │   ├── push.rs           # 推送管道服务
│   │   │   ├── shared_memory.rs  # Host Render 共享内存
│   │   │   ├── handler.rs        # MessageHandler trait
│   │   │   └── deferred.rs       # 延迟处理器
│   │   └── Cargo.toml
│   │
│   ├── wind-config/              # 配置系统
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── config.rs         # Config 结构体 + 三层合并
│   │   │   ├── schema.rs         # Schema YAML 定义
│   │   │   ├── hotkey.rs         # 热键编译器
│   │   │   ├── app_compat.rs     # 应用兼容性规则
│   │   │   └── runtime_state.rs  # 运行时状态持久化
│   │   └── Cargo.toml
│   │
│   ├── wind-store/               # 数据库层
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── store.rs          # Store 核心（redb）
│   │   │   ├── user_words.rs
│   │   │   ├── temp_words.rs
│   │   │   ├── shadow.rs
│   │   │   ├── freq.rs           # 频率（含异步批处理）
│   │   │   ├── phrases.rs
│   │   │   ├── stats.rs
│   │   │   └── migration.rs
│   │   └── Cargo.toml
│   │
│   ├── wind-dict/                # 词典系统
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── composite.rs      # CompositeDict 多层词典
│   │   │   ├── layer.rs          # DictLayer trait
│   │   │   ├── trie.rs           # 内存前缀 trie
│   │   │   ├── binformat.rs      # wdb 格式 + mmap
│   │   │   ├── datformat.rs      # wdat 格式 + DAT
│   │   │   ├── store_layer.rs    # Store 桥接层
│   │   │   ├── hotcache.rs       # 热缓存
│   │   │   └── manager.rs        # DictManager
│   │   └── Cargo.toml
│   │
│   ├── wind-engine/              # 输入引擎
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── engine.rs         # Engine trait
│   │   │   ├── pinyin/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── parser.rs     # 音节解析
│   │   │   │   ├── syllable.rs   # SyllableTrie
│   │   │   │   ├── dag.rs        # DAG 构建
│   │   │   │   ├── lattice.rs    # 格子构建
│   │   │   │   ├── viterbi.rs    # Viterbi 解码
│   │   │   │   ├── lm.rs         # 语言模型 (unigram/bigram)
│   │   │   │   ├── scorer.rs     # 评分器
│   │   │   │   ├── shuangpin.rs  # 双拼转换
│   │   │   │   └── fuzzy.rs      # 模糊音
│   │   │   ├── codetable/
│   │   │   │   ├── mod.rs
│   │   │   │   └── engine.rs
│   │   │   ├── mixed/
│   │   │   │   ├── mod.rs
│   │   │   │   └── engine.rs
│   │   │   └── manager.rs        # EngineManager
│   │   └── Cargo.toml
│   │
│   ├── wind-candidate/           # 候选词类型
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── candidate.rs      # Candidate 结构体
│   │   │   └── filter.rs         # 过滤逻辑
│   │   └── Cargo.toml
│   │
│   ├── wind-transform/           # 文本变换
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── punctuation.rs    # 标点转换
│   │   │   ├── fullwidth.rs      # 全角/半角
│   │   │   ├── pair_tracker.rs   # 自动配对
│   │   │   └── s2t.rs            # 简繁转换
│   │   └── Cargo.toml
│   │
│   ├── wind-theme/               # 主题系统
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── theme.rs          # Theme 结构体
│   │   │   ├── views.rs          # ViewNode 定义
│   │   │   ├── palette.rs        # 调色板解析
│   │   │   ├── resolved.rs       # ResolvedV3
│   │   │   ├── manager.rs        # 主题加载管理
│   │   │   └── bgimage.rs        # 背景图/SVG
│   │   └── Cargo.toml
│   │
│   ├── wind-ui/                  # UI 渲染层（tiny-skia）
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── window.rs         # Layered Window 管理
│   │   │   ├── renderer.rs       # 渲染器（字体缓存、DPI）
│   │   │   ├── viewbox/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── types.rs      # View 结构体定义
│   │   │   │   ├── layout.rs     # measure + arrange
│   │   │   │   ├── paint.rs      # 三阶段绘制
│   │   │   │   ├── build.rs      # View 树构建
│   │   │   │   └── image.rs      # 图像解析缓存
│   │   │   ├── text/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── backend.rs    # TextBackend trait
│   │   │   │   ├── dwrite.rs     # DirectWrite 后端
│   │   │   │   └── freetype.rs   # FreeType 后端（跨平台）
│   │   │   ├── candidate_window.rs
│   │   │   ├── toolbar.rs
│   │   │   ├── tooltip.rs
│   │   │   ├── status.rs
│   │   │   ├── toast.rs
│   │   │   ├── popup_menu.rs
│   │   │   └── manager.rs        # UI 管理器 + 消息循环
│   │   └── Cargo.toml
│   │
│   ├── wind-cmdbar/              # 命令栏系统
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── parser/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── lexer.rs
│   │   │   │   └── parser.rs
│   │   │   ├── ast.rs
│   │   │   ├── eval.rs
│   │   │   ├── registry.rs
│   │   │   ├── context.rs
│   │   │   └── funcs/            # 34 个内置函数
│   │   └── Cargo.toml
│   │
│   └── wind-coordinator/         # 中央协调器
│       ├── src/
│       │   ├── lib.rs
│       │   ├── coordinator.rs    # Coordinator 主结构
│       │   ├── handle_key.rs     # 按键事件路由
│       │   ├── handle_candidate.rs
│       │   ├── handle_mode.rs
│       │   ├── handle_lifecycle.rs
│       │   ├── handle_temp.rs    # 临时模式
│       │   ├── handle_punct.rs
│       │   ├── handle_addword.rs
│       │   ├── handle_cmdbar.rs
│       │   ├── handle_tooltip.rs
│       │   ├── handle_config.rs
│       │   ├── hotkey_match.rs
│       │   ├── stats.rs
│       │   └── watchdog.rs
│       └── Cargo.toml
│
├── wind_service/                 # 主入口
│   ├── src/
│   │   └── main.rs               # 启动序列
│   └── Cargo.toml
│
├── wind_rpc/                     # RPC 服务
│   ├── src/
│   │   ├── main.rs               # 独立 RPC 进程（可选）
│   │   ├── server.rs
│   │   ├── router.rs
│   │   └── services/
│   │       ├── dict.rs
│   │       ├── shadow.rs
│   │       ├── system.rs
│   │       ├── stats.rs
│   │       ├── phrase.rs
│   │       └── config.rs
│   └── Cargo.toml
│
└── data/                         # 共享数据文件（符号链接或拷贝）
    ├── config.toml
    ├── schemas/
    └── compat.toml
```

## 迁移阶段

### Phase 0: 基础设施（第 1 周）
- [x] 初始化 git 仓库
- [ ] Cargo workspace 搭建
- [ ] 通用工具 crate（错误类型、日志初始化）
- [ ] CI 构建脚本

### Phase 1: 协议层（第 1-2 周）
- [ ] `wind-ipc` — 完整协议定义 + 编解码（从 Go 的 `ipc/` 和 `rpcapi/` 移植）
- [ ] `wind-bridge` — Named Pipe 服务器（tokio 异步）
- [ ] `wind-bridge` — 共享内存 Host Render
- [ ] 与现有 TSF DLL 联调验证

### Phase 2: 数据层（第 2-3 周）
- [ ] `wind-config` — TOML 三层合并配置
- [ ] `wind-store` — redb 数据库（bucket → table 映射）
- [ ] `wind-store` — 频率异步批处理
- [ ] `wind-dict` — 二进制格式 mmap 读取
- [ ] `wind-dict` — DAT 格式读取
- [ ] `wind-dict` — CompositeDict 多层架构
- [ ] `wind-dict` — StoreUserLayer / StoreTempLayer / StoreShadowLayer

### Phase 3: 引擎层（第 3-5 周）
- [ ] `wind-candidate` — Candidate 类型 + 排序 + 过滤
- [ ] `wind-transform` — 标点/全角/配对/简繁
- [ ] `wind-engine/pinyin` — 音节 Trie + DAG + 解析器
- [ ] `wind-engine/pinyin` — Lattice + Viterbi + 评分
- [ ] `wind-engine/pinyin` — 语言模型（unigram/bigram）
- [ ] `wind-engine/pinyin` — 双拼/模糊音
- [ ] `wind-engine/codetable` — 码表引擎
- [ ] `wind-engine/mixed` — 混合引擎
- [ ] `wind-engine` — EngineManager + SchemaFactory

### Phase 4: 渲染层（第 5-7 周）
- [ ] `wind-theme` — 主题加载 + 调色板 + ResolvedV3
- [ ] `wind-ui/text` — DirectWrite 后端（FFI）
- [ ] `wind-ui/viewbox` — box-model 布局引擎
- [ ] `wind-ui/viewbox` — tiny-skia 三阶段绘制
- [ ] `wind-ui` — 候选窗口 + Layered Window
- [ ] `wind-ui` — Toolbar / Tooltip / Status / Toast / PopupMenu
- [ ] `wind-ui` — Host Render 路径（写入共享内存）

### Phase 5: 业务逻辑（第 7-9 周）
- [ ] `wind-cmdbar` — 词法/语法/AST
- [ ] `wind-cmdbar` — 求值器 + 34 个内置函数
- [ ] `wind-coordinator` — 按键事件路由（完整优先级链）
- [ ] `wind-coordinator` — 候选选择/提交
- [ ] `wind-coordinator` — 模式切换
- [ ] `wind-coordinator` — 临时模式/快捷输入/特殊模式
- [ ] `wind-coordinator` — 热键匹配
- [ ] `wind-coordinator` — 统计/看门狗/内存修剪

### Phase 6: 集成（第 9-10 周）
- [ ] `wind_service` — 主启动序列
- [ ] `wind_rpc` — JSON-RPC 服务（65 个方法）
- [ ] `wind_rpc` — 事件广播系统
- [ ] 与 TSF DLL 完整联调
- [ ] 性能基准测试（内存/延迟对比）

## 关键设计决策

### 1. 异步 vs 同步

- **IPC Bridge**: tokio 异步（Named Pipe I/O）
- **引擎 Convert**: 同步（CPU 密集，无需 async 开销）
- **数据库**: 同步读写 + 异步频率批处理（tokio task）
- **UI 渲染**: 专用线程 + channel 命令分发（与 Go 的 `runCombinedLoop` 一致）

### 2. 内存管理策略

- **词典数据**: mmap 零拷贝（与 Go 相同）
- **用户数据**: redb mmap（比 bbolt 更紧凑）
- **候选词**: `Vec<Candidate>` 栈分配，避免小对象堆分配
- **渲染缓冲**: 预分配 `Vec<u8>` 复用（对应 Go 的 scratchPix）
- **字符串**: 尽量用 `&str` 引用 mmap 数据，避免 `String` 拷贝

### 3. 并发模型

- **IPC 线程**: tokio runtime（多线程）
- **UI 线程**: 专用线程 + `tokio::sync::mpsc` 命令通道
- **渲染线程**: 与 UI 线程同线程（Win32 消息循环要求）
- **频率刷新**: tokio task 定时器
- **协调器**: `Arc<Mutex<Coordinator>>`（对应 Go 的 `sync.Mutex`）

### 4. 协议兼容性

- 二进制协议必须**字节级兼容**（IpcHeader 8 字节、所有 Payload 结构体）
- JSON-RPC 协议必须**语义兼容**（方法名、参数格式、响应格式）
- Named Pipe 命名规则不变（`\\.\pipe\wind_input` 等）
- 共享内存格式不变（SharedRenderHeader 64 字节 + BGRA）

## 预期收益

| 指标 | Go 版本 | Rust 预期 | 改善 |
|------|---------|----------|------|
| 常驻内存 (RSS) | ~50-80 MB | ~15-25 MB | 60-70% |
| 启动时间 | ~500ms | ~200ms | 60% |
| 按键延迟 (P99) | ~5ms | ~2ms | 60% |
| 二进制大小 | ~15 MB | ~5 MB | 65% |
| GC 暂停 | 偶发 | 无 | 100% |

## 风险与缓解

| 风险 | 缓解措施 |
|------|---------|
| DirectWrite FFI 复杂 | 先用 FreeType 后端验证，DirectWrite 后期优化 |
| 82 个 coordinator 文件移植量大 | 按功能模块分批移植，每批独立测试 |
| redb 与 bbolt 行为差异 | 编写兼容层测试，确保 bucket→table 映射正确 |
| tiny-skia 性能不如 Go gg | tiny-skia 已有成熟优化，候选窗口通常 <1000x500px |
| TSF DLL 协议细节遗漏 | 逐字节对比测试，使用现有 Go 版本作为参考 |
