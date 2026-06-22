# 输入统计功能迁移计划（Go → Rust 完整对齐）

> worktree: `WindInput-stats`　分支: `feat/input-stats`（从 main `4ffbf21` 分出）
> 目标: 把 Go 版 `WindInput` 的**完整输入统计**迁移到 Rust 版，含采集器架构。
> 决策（已确认）: ①一次性完整对齐 Go　②引入 StatCollector 采集器（内存聚合 + 后台 flush）

---

## 0. 现状与差距（结论）

Rust **并非完全缺失**统计，而是只迁了「极简骨架」：`wind-store/src/stats.rs` 仅持久化每日 `{chinese, english}`，
顶层单点 `coordinator.rs:1737 record_input_stats()` 按最终文本分类中英文直写 redb。
前端 `StatsPage.vue`(1059 行) 按 Go 富模型设计，但后端不产富字段 → 速度/码长/首选率/小时分布/按方案**全显示 0**。

| 能力 | Go | Rust 现状 | 本计划 |
|---|---|---|---|
| 每日 中文/英文 | ✓ | ✓ | 保留 |
| 标点/数字其他 分类 | ✓ | ✗ | Stage1 |
| 24 小时分布 Hours[24] | ✓ | ✗ | Stage1+2 |
| 码长 sum/count/dist[6] | ✓ | ✗ | Stage1+3 |
| 首选率 CandPosDist[5] | ✓ | ✗ | Stage1+3 |
| 活跃时间/速度/最快速度 | ✓ | ✗ | Stage1+2 |
| 按方案 BySchema | ✓ | ✗ | Stage1+3 |
| 按来源 BySource[N] | ✓ | ✗ | Stage1+3 |
| 连续天数 streak | ✓(meta) | ✓(查询算) | Stage1 改 meta 维护 |
| 全局 StatsMeta | ✓ | ✗ | Stage1 |
| 采集器(聚合+30s flush+跨天) | ✓ | ✗(每键直写) | Stage1+2 |
| TSF 英文批量上报 | ✓ | ✗(Rust 暂无TSF英文路径) | 预留接口，暂不接 |
| clear/prune/RPC | ✓ | ✓ | Stage4 扩展 |

## 1. 耦合度分层（决定合并冲突风险）

| 层 | 文件 | 耦合 | 冲突风险 |
|---|---|---|---|
| 数据/存储 | `wind-store/src/stats.rs` | 🟢低 | 改自己文件，serde(default) 向后兼容旧数据 |
| 采集器 | `wind-store/src/stat_collector.rs`（**新文件**） | 🟢低 | 零冲突 |
| 配置 | `wind-config/src/config.rs` | 🟡中 | 已知热点+主仓有未提交改动；只追加 StatsConfig 字段 |
| RPC | `wind-coordinator/src/webdata.rs` | 🟡中 | 追加分支/扩展返回，冲突可控 |
| 顶层采集接线 | `coordinator.rs: record_input_stats / handle_key_event_policed` | 🟡中 | 改动集中在已独立函数 |
| **散布注入** | 9 个 `handle_*.rs` 上屏函数 | 🔴**高** | **合并主战场**，隔离为 Stage3 单独提交 |
| 前端 | `StatsPage.vue` | 🟢零 | 已完整，后端补字段即点亮 |
| 服务装配 | `apps/service` | 🟢低 | 创建/关闭 collector |

**合并策略**: Stage1/2/4 合并友好（新文件 / 独立函数 / 追加分支）。Stage3 是唯一高冲突区——
它要碰 coordinator 的 9 个 handle 文件（另两个会话也在改 coordinator），故**独立成最后一个提交**，
合并时挑其它会话不在改 coordinator 的窗口快速 rebase 合入。

---

## Stage 1: 数据模型扩展 + 采集器（wind-store，零耦合）
**Goal**: wind-store 内完成全部数据结构、存储 CRUD、StatCollector，纯单元测试覆盖，不碰任何其它 crate。
**Success Criteria**:
- `DailyStats` 扩展为全字段（对照 Go `DailyStat`），旧 `{chinese,english}` 数据用 `#[serde(default)]` 仍可读
- 新增 `StatsMeta`（存入现有 `META` 表的 `stats_meta` key，**不新增表定义**，减少对 store.rs 改动）
- 新增 `CommitSource` 枚举 + `StatEvent` 结构
- 新增 `StatCollector`（`stat_collector.rs`）：内存聚合 / 跨天检测 / 活跃时间(15s阈值) / 后台 30s flush / pause-resume / reset
- `speed_per_minute(chars, active_secs)`（5s 下限，对照 Go）
- streak 改由 meta 维护（`update_streak`，对照 Go `updateStreak` 1.5 天容差）
**Tests**（移植 Go `store/stats_test.go` + `stat_collector` 行为）:
- record→daily 累加、区间查询、跨天 flush、活跃时间累计(10s 累加/90s 跳过)、streak、clear/prune、classify_chars(含标点/数字)
- 旧数据向后兼容反序列化测试
**Status**: ✅ Complete（32 单测全绿；下游 wind-coordinator 仍编译；commit `<stage1>`）

## Stage 2: 顶层采集接线（采集器替换每键直写，单点）
**Goal**: coordinator 持有 StatCollector，`record_input_stats` 走采集器；服务装配 collector 生命周期。
**Success Criteria**:
- `Coordinator` 持有 `Option<StatCollector>`（或 Arc），替换现有 `store.record_stat` 直写
- `record_input_stats()` 改为构造 `StatEvent`（顶层能拿到的字段：字符分类 / 小时 / 活跃时间 / schemaID / source 兜底推测），调 `collector.record()`
- 引入 `stat_recorded` 标志位，每键开始重置（对照 Go `handle_key_event.go:71`）
- `handle_key_event_policed` 末尾做 **fallback**（对照 Go `recordCommitFallback`）：仅当未被 Stage3 注入命中时按推测来源记录
- 采集器随 `Coordinator::build` 构造（与 store 共享 Arc）；退出 flush 靠 Drop + 后台 30s。
  无 store.pause/resume 调用点（Rust 暂无热替换），collector.pause/resume 保留备用。
- `record_commit(text,code_len,candidate_pos,source)` 富字段采集入口 + `stat_recorded` AtomicBool；
  `record_input_stats` 改为顶层 fallback。track_english 仅留作 TSF 路径，普通上屏按 Go 记录全部分类。
**Tests**: collector 接线后端到端单测（webdata stats 测试仍绿）；跨天/flush 不丢数据
**Status**: ✅ Complete（11 测试绿；workspace 整体编译通过；commit `<stage2>`）

## Stage 3: 散布注入富字段（碰 9 个 handle 文件）⚠️ 合并主战场
**Goal**: 在各上屏路径注入 `record_commit(text, code_len, candidate_pos, source)`，产出码长/首选率/按方案/按来源。
**注入点**（已勘明，文件:函数）:
- `handle_candidate.rs:442 commit_selected` → SourceCandidate（code_len, idx, is_partial）
- `handle_temp.rs:90 commit_temp_pinyin_selected` → SourceTempPinyin
- `handle_temp.rs:323+ 临时英文` → SourceTempEnglish（无码长）
- `handle_quick.rs:112+ 快捷输入` → SourceQuickInput（无码长）
- `handle_special.rs:112+ 特殊模式` → SourceSpecialMode
- `handle_url.rs:116 网址` → **SourceUrl(Rust 特有)**（无候选位）
- `handle_mode.rs mix_select` → **SourceMix(Rust 特有)**
- `coordinator.rs:2073 顶码` → SourceRawInput(idx=0)
- `coordinator.rs:1849 模式切换` → SourceModeSwitch
- `coordinator.rs:2141-2199 标点` → SourcePunctuation(可能含候选)
**Success Criteria**:
- `CommitSource` 含 Rust 特有 Url/Mix；Go 的 FullWidth/TSFDirect 保留枚举值（暂不产出）
- 每个注入点设 `stat_recorded=true`，fallback 只兜未命中路径
- 词频 `record_selection`（已存在）与统计采集**保持独立**，不互相耦合
**Tests**: 各来源/码长/首选率分类的针对性单测（构造 KeyEvent 序列断言 collector 聚合值）
**Status**: ✅ Complete（3a `ab13cba` 候选核心 + 3b 来源细分；含 committed 段的点只记剩余 buffer 避免 fallback 重复计；标点 piece 显式记；13 测试绿）
**合并提示**: 已碰 coordinator.rs/handle_candidate/temp/mode/quick/special/url 共 7 文件；合并前 `git fetch && rebase main` 集中解决 coordinator 冲突。

## Stage 4: RPC 扩展 + 前端点亮
**Goal**: webdata 输出富字段，前端 StatsPage 不再显示 0。
**Success Criteria**:
- `stats.summary` 扩展返回：today_chinese/english、active_days、daily_avg、streak_max、max_day、avg_code_len、first_select_rate、today/overall/max_speed（对照前端 `StatsSummary` 17 字段）
- `stats.daily` 返回完整 `DailyStatItem`（h[24]/cld/cpd/bs/src/as）
- 修 clear/prune 同步采集器内存（clear→reset；prune→flush+recalc+resume）
- 事件推送 `onStatsEvent`：**延后**（onMounted 已 loadData 拉取，实时推送为优化非阻塞）
**Tests**: RPC 富字段完整性测试；前端 `pnpm build`(vue-tsc+vite) 通过
**Status**: ✅ Complete（13 后端测试绿；workspace 编译通过；前端 build 通过；commit `<stage4>`）

## Stage 5: 验证 + 合并
**Goal**: 全绿 + Windows 实测 + 合入 main。
**Success Criteria**:
- `cargo test --workspace` 全绿；`pnpm build` 绿
- Windows 实测：热力图有数据、速度/首选率/码长非 0、跨天正确、clear/prune 生效
- rebase main 解决冲突，合并；按项目约定 push（仅 exe，data/ 单独 scp，见 memory windows-deploy-paths）
**Status**: Not Started

---

## 数据兼容性备注
- redb 表 `STATS_DAILY` 不变，value JSON 由 `{chinese,english}` 扩展为全字段，靠 `#[serde(default)]` 向后兼容已有用户数据，无需迁移脚本。
- `StatsMeta` 存入现有 `META` 表（key=`stats_meta`），避免改 `store.rs` 表定义（降低与他会话冲突）。首次启动 meta 缺失时由 `recalculate_stats_meta` 从 daily 重建。

## 与其它会话的协调
- 主仓未提交改动（`config.rs`/`cache_fp.rs`）留在主仓，不进本 worktree。
- 提交一律用**显式路径**（不 `git add -A`），避免卷走他人未提交文件（见 memory concurrent-claude-sessions-git-hazard）。
- coordinator.rs / config.rs 是多会话热点：本计划把对它们的改动压到最小且集中（Stage2 集中、Stage3 隔离）。
