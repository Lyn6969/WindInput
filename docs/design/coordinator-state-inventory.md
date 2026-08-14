# Coordinator 状态清点：81 字段的锁形态、访问分布与分组评估

> 减重计划第 4 步（前三步：文件拆分 `b0e2199b`/`d3d128e0`、webdata 窄面化 `4bad3b89`、
> 平台下沉已查证取消）。本文**只清点、不改代码**；「可合并候选」是提案，动手前须逐条验证。
>
> 范围：`Coordinator` 结构体的 81 个字段（coordinator.rs `pub struct Coordinator`）。
> `State` 内部的输入态字段在唯一大锁之内，不构成并发拆分问题，不在本清单展开。

## 1. 线程模型：谁会进 Coordinator

锁形态只有对照线程模型才能评判。会调用 Coordinator 方法的线程：

| 线程 | 数量 | 入口 | 说明 |
|---|---|---|---|
| `bridge-client` | **每宿主进程连接一条** | `MessageHandler` 全部回调 | wind-bridge server.rs 为每个连接 spawn；多个应用进程的 TSF DLL **并发**进入按键/焦点/caret 回调 |
| UI 事件线程 | 1 | `handle_ui_event` | 桌面 `new()` 里 spawn，消费 `UiEvent` channel（候选点击/翻页/CandidateFlipped/全局热键） |
| RPC 线程 | service 侧 | `web_data_rpc`（经 `WebDataHost` 窄面）、`reload_user_config` | 设置页数据域 + 配置热重载 |
| push server 线程 | 1 | `on_push_client_connected`、各 `push_*` | 客户端握手时补推配置帧 |
| `FirstShowTimer` | 1（共享） | `fire_pending_first_show` | 首显兜底；覆盖式待办，绝不在其上执行阻塞调用 |
| 全屏探测线程 | 单飞 | 写 `fullscreen_cached` | `notify_toolbar_async`；`fullscreen_probing` 是单飞闸 |
| capslock 消费线程 | 1 | `handle_capslock_hook_press` | 钩子线程只投递 channel，动作在消费线程执行 |
| `wind-prepare` 预热线程 | 1 | **走完整按键路径**（喂 'a' + 退格） | 预热与真实路径同源，见 `prepare` 文档 |
| $CC 异步执行线程 | 按需 | 经 `self_weak` 升级 | 命令候选副作用；独立线程以避免持 state 锁回调自锁 |

★ **关键事实**：`handle_key_event` 不是单线程专属——bridge-client 按连接并发，预热线程也走同一条路。
这是「几乎每个字段都带锁/原子」的根本原因；任何「看起来单线程、可以去锁」的直觉在此不成立。

## 2. 字段清点（按簇）

访问分布来自 `self.<field>` 的静态统计（模块:次数）；webdata.rs 的计数是经
`WebDataHost` 窄面的方法调用，不是字段直连。语义句压缩自字段 doc 注释——**改动前请回读
struct 定义处的完整注释**，多数字段的「为什么是这个形态」写在那里。

### 2.1 核心两把大锁

| 字段 | 形态 | 语义 | 访问分布 |
|---|---|---|---|
| `state` | `Mutex<State>` | 全部输入态（缓冲/候选/分页/模式）唯一大锁 | 12 个模块 97 处；coordinator 30、message_handler 30、handle_menu 12 |
| `rt` | `RwLock<Arc<ConfigBundle>>` | 配置+派生缓存快照，热重载整体原子替换；访问统一经 `self.rt()` | 16 个模块 122 处，读远多于写（写仅 build/reload/refresh） |

### 2.2 不可变句柄与注入面（无锁）

构造时定值或 OnceLock 一次注入，之后只读——无并发问题。

| 字段 | 形态 | 语义 |
|---|---|---|
| `push_server` / `ui_tx` | `Arc` / `Sender` | push 通道 / UI 命令通道 |
| `engine_mgr` | `EngineManager` | 引擎管理器（内部自管并发），229 处最高频 |
| `store` / `stat_collector` | `Option<Arc<Store>>` / `Option<StatCollector>` | redb 持久化 / 统计采集（None=headless） |
| `common_chars` / `quick_formats` | 无锁表 | 常用字集 / 快捷格式表（启动加载后不变，改文件须重启） |
| `system_phrase_path` / `compat_dirs` / `themes_dir` | `Option<PathBuf>` | 路径快照（便携版/测试自定义口径） |
| `capslock_press_tx` | `Sender<()>` | 钩子→消费线程投递口（构造即建好） |
| `cmdbar_services` / `host_services` / `self_weak` / `host_render`(win) | `OnceLock` | 构造后注入惯例；host_services 未注入首取即固化默认 |

### 2.3 首显闸门 / caret 簇（13 个——最大的多锁簇）

集中在 `coordinator/first_show.rs` + `coordinator/message_handler.rs`。

| 字段 | 形态 | 语义 |
|---|---|---|
| `pending_first_show` | `Mutex<bool>` | 首帧延迟显示挂起中 |
| `pending_first_show_token` | `Mutex<u64>` | 兜底 timer 代际令牌（arm 自增，fire 比对作废旧任务） |
| `candidate_shown` | `Mutex<bool>` | 本组合是否已首显（后续刷新可立即下发） |
| `show_authorized` | `AtomicBool` | 显示授权，`notify_ui_update` 内 **swap 消费** |
| `first_show_was_provisional` | `AtomicBool` | 首显用了非权威坐标 → 权威帧到达时用放宽容差 |
| `caret_cache_verified` | `AtomicBool` | 坐标缓存被当前插入点验证过（fast 短兜底的前提显式化）；⚠ **刻意不复用** `last_authoritative_caret.2`，两者语义会分化 |
| `first_show_extended` | `AtomicBool` | 已进入长兜底等待（后续按键不重置计时） |
| `caret_independent` | `AtomicBool` | 宿主自绘候选条声明（Android），闸门直接放行 |
| `last_valid_caret` | `Mutex<(i32,i32,i32)>` | 最近有效坐标（无效时回退，防候选窗跑左上角） |
| `last_authoritative_caret` | `Mutex<(i32,i32,bool)>` | 上一轮权威坐标（probe「已 reflow」判据） |
| `composition_start` | `Mutex<(i32,i32,bool)>` | 组合起点坐标（嵌入预编辑锚点，组合内锁定首个有效值） |
| `last_key_at` | `Mutex<Option<Instant>>` | 上次按键时刻（仅为算出下项） |
| `last_key_interval_ms` | `Mutex<Option<u64>>` | **相邻按键**间隔（fast 档连续输入判据；⚠ 不能用 elapsed） |

### 2.4 候选视图（6）

| 字段 | 形态 | 语义 |
|---|---|---|
| `hover_index` | `AtomicI32` | 鼠标悬停目标；★ 原子量是为 `clear_hover` 免 state 锁（notify_ui_hide 40+ 调用点，加锁即死锁） |
| `candidate_flipped` | `AtomicBool` | 候选窗反转排列镜像（UI 侧单向写入，协调器推不出） |
| `candidate_vertical` | `Mutex<bool>` | 布局方向**基线**真相源（持久化） |
| `candidate_layout_sent` | `Mutex<bool>` | 实际下发方向的去重缓存（叠加模式意图后） |
| `hide_candidate_window` | `Mutex<bool>` | 候选窗隐藏开关（cmdbar 切换） |
| `preedit_display` | `Mutex<PreeditDisplay>` | 编码显示方式运行时态（统一权威） |

### 2.5 标点 / 符号（3）

| 字段 | 形态 | 语义 | 分布 |
|---|---|---|---|
| `punct` | `Mutex<PunctuationConverter>` | 引号左右状态 | handle_punct 5、message_handler 4 |
| `smart_symbol` | `Mutex<SmartSymbolArm>` | 智能符号同键连按待命态 | 同上 |
| `pair_tracker` | `Mutex<PairTracker>` | 配对跟踪栈（智能跳出） | handle_punct 9、key_gate 1 |

### 2.6 自动造词（3）

| 字段 | 形态 | 语义 |
|---|---|---|
| `auto_phrase` | `Mutex<AutoPhraseBuf>` | 连续单字缓冲；★ **刻意独立于 State**——终止信号来自不持 state 锁的 IPC 回调，塞进 State 逼出跨锁调用 |
| `last_self_commit` | `Mutex<Option<Instant>>` | 自家吐字时刻（区分 SelectionChanged 回声；打点收口 `commit_action` 一处） |
| `auto_phrase_writes` | `AtomicUsize` | 写入计数（临时词库淘汰节流） |

### 2.7 其余簇

| 簇 | 字段（形态） | 要点 |
|---|---|---|
| CapsLock | `capslock_hook`(Mutex)、`last_caps_inject`(Mutex) | 钩子只在用户配了 capslock 时存在（风险控制）；注入冷却防振荡 |
| 短语 | `phrases`(RwLock)、`system_phrase_entries`(RwLock) | 层可 rebuild；条目缓存作重读失败回退 |
| 简繁 | `s2t`(Mutex) | Mutex 兼容 reload 整体替换 |
| 反查/拆字/注释 | `reverse`(RwLock)、`chaizi_assets`(Mutex)、`comment_dict_paths`(Mutex) | 后两者是 reload 变更检测态 |
| 快捷输入 | `quick_adjust`(RwLock) | redb 真相的读缓存；⚠ 右键操作必须写库+回灌两件都做 |
| 工具栏 | `toolbar_positions`(Mutex)、`current_toolbar_monitor`(Mutex)、`fullscreen_cached`(AtomicBool)、`fullscreen_probing`(AtomicBool) | 前两者=位置记忆（handle_menu）；后两者=后台探测缓存+单飞闸 |
| 应用兼容/模式记忆 | `app_compat`(Mutex)、`active_compat`(Mutex)、`pid_names`(Mutex)、`mode_states`(Mutex)、`runtime_last`(Mutex) | app_compat 用 Mutex 是为菜单切换后立即重载；pid_names 会话级只增 |
| 主题/气泡 | `theme_name`(Mutex)、`theme_style`(Mutex)、`theme_index_labels`(Mutex)、`last_status_text`(Mutex) | webdata 经窄面语义方法取前两者；last_status_text=气泡去重 |
| 焦点气泡 | `pending_focus_tip`(AtomicBool)、`last_focus_tip_token`(Mutex) | 等权威坐标才弹（刻意无兜底 timer）；按宿主 token 去重不按 docMgr |
| 方案往返 | `schema_toggle_origin`(Mutex) | 带 `schema_generation` 代际自动失效（五条切换路径无法统一收尾） |
| 上屏历史 | `recent_commits`(Mutex)、`last_commit_len`(AtomicUsize) | 历史队列与撤销删除量**刻意分离**（时效语义不同） |
| 生命周期 | `readiness_state`(AtomicU8)、`eager_prewarm`(AtomicBool) | CAS 保证 prepare 只跑一次 |
| 统计 | `stat_recorded`(AtomicBool) | ★ 原子量避免与 state 锁冲突致死锁 |
| 诊断/密码框 | `last_input_diag`(Mutex)、`last_window_diag`(Mutex)、`password_suppress`(AtomicBool)、`password_suppress_enabled`(AtomicBool)、`input_diag_hud_visible`(AtomicBool)、`input_diag_sections`(Mutex)、`input_diag_frozen`(AtomicBool)、`input_diag_topmost`(AtomicBool) | 两个 diag 快照**分开存**（上报时机不同）；HUD 开关全是独立布尔，互不成组 |
| cmdbar 上下文 | `front_ctx`(Mutex) | (app,title,sel) 快照，darwin 上报 |

## 3. 勿动清单——锁形态背后的既有论证

以下形态是**修过 bug 之后的结论**，合并/改形态前必须推翻其原始论证：

1. `hover_index` 原子量：`clear_hover` 必须免 state 锁才能进 `notify_ui_hide`（40+ 调用点无法逐一确认持锁状态，加锁即埋死锁）。
2. `stat_recorded` 原子量：同类死锁规避。
3. `show_authorized`：swap 消费语义，Mutex<bool> 表达不了「读并清」的原子性。
4. `auto_phrase` 独立于 `State`：跨锁调用规避（见 2.6）。
5. `caret_cache_verified` 不并入 `last_authoritative_caret.2`：「有没有基准」与「值可不可信」当前取值恰好一致，但边缘输入下期望会分化。
6. `recent_commits` 与 `last_commit_len` 分离：「上过什么」与「光标前紧邻的还是不是它」时效语义不同。
7. `last_input_diag` 与 `last_window_diag` 分开：上报时机不同，合并就得回答「只到一半算什么」。
8. `quick_formats`（文件）与 `quick_adjust`（库镜像）分开：所有权设计（GUI 调整绝不回写文件）。

## 4. 可合并候选（提案，未实施）

| 组 | 现状 | 提案 | 依据 | 收益/风险 |
|---|---|---|---|---|
| A | `pending_first_show` + `pending_first_show_token` 两把 Mutex | `Mutex<FirstShowGate { armed: bool, token: u64 }>` | reset/arm/fire 三个访问点全部成对先后取锁；fire 的两段取值之间存在竞窗（当前被代际取代语义容忍） | 消除竞窗、少一次锁；改动局限 first_show.rs。低风险低收益 |
| B | `last_key_at` + `last_key_interval_ms` 两把 Mutex | 单 Mutex 小结构 | 唯一写点在 handle_key_event 同一段顺序更新 | 纯整洁性。低风险低收益 |
| C | `toolbar_positions` + `current_toolbar_monitor` | 单 Mutex 工具栏位置态 | 同为工具栏定位记忆，均在 handle_menu | 需先验证访问是否真的成组 |

**除 A/B/C 外不建议任何合并**：其余锁边界与死锁规避、调用路径、reload 语义强绑定
（见 §3），正确性论证成本远超收益。A/B/C 也不单独立项——顺路改到对应文件时捎带，
每次一组、跑全量测试。

## 5. 新增字段的归属判据

新加状态时按序自问：

1. **是输入态吗**（随组合开始/结束生死）→ 进 `State`，除非有 §3-4 那样的跨锁理由。
2. **是配置的派生缓存吗** → 进 `ConfigBundle`（热重载自动跟随）。
3. **要在不持 state 锁的路径上被访问吗**（`notify_ui_hide`、IPC 焦点回调、后台线程）→ 独立字段；能表达为单值就用原子量并写明兜底值语义，需要复合值才用 Mutex。
4. **属于已有簇吗** → 贴着簇放（struct 定义处相邻 + 注释交叉引用），不要散在末尾。
5. 无论落在哪，**把「为什么是这个形态」写进字段注释**——本清单 §3 的每一条都源自这样的注释，它们是防止未来会话回退错误设计的唯一屏障。
