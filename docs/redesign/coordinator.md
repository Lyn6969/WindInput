# 重设计差分：coordinator（协调器 / 按键处理）

> 阶段 A 产物。Go 侧 3 个只读 agent 提取、核心架构断言 grep 抽验 file:line 属实；Rust 侧本人通读关键路径。
> 体量：Go `internal/coordinator` ≈ 1.81 万行生产代码（最大模块，~73 文件）；Rust `wind-coordinator` ≈ 3977 行。
> **本子系统是用户强调的"模式融合 + 统一按键处理"的落点**——重点是吸取 Go 新管线的**目标终态**。

> ⚠️ 关键判断：**Go 的 pipeline 是迁移进行中**（满是"第 0 批骨架/部分接管/待后续批次"注释，`decide()` 遇 Activate/Release 即回退旧路径，旧 `handleXxxKey` 仍被新 `Apply` 委托调用，special 游离于 registry 外，temp_english 导航仍内联）。Rust 应**吸取其设计意图（干净终态），不照搬半迁移的新旧双轨**。Rust coordinator 目前仍是单体、未站队，正好可**直接跃迁到干净 pipeline**，省掉 Go 的迁移包袱——这是重设计的最大红利。

---

## 1. Go 新架构总览（要吸取的目标设计）

四个正交概念（均已 grep 核实）：

### Processor（宿主 / 模式）— pipeline_processor.go:14
一个"模式"= 一个 Processor。接口方法：`Name() / Judge(ctx,key,data) Decision / Activate(dec)→(prefix,ok) / Release() / BufferText() / Capabilities() Capability / KeyHandlers() []KeyHandler / AcceptedProviders() []ProviderID`。
宿主实例：engine_default（兜底）/ temp_pinyin / temp_english / quick_input / special / url。

### CandidateProvider（候选源）— pipeline_provider.go:30，与宿主正交
接口：`ID() ProviderID / Rank() int / Query(buffer)→[]Candidate`（纯查询，禁副作用）。
Rank 段位（已核实）：Date 10 / Calc 20 / Number 30 / Pinyin 40 / RareChar 50 / English 60。多个 Provider 向同一宿主供候选，按 Rank 升序**分段拼接**合并（pipeline_merge.go），候选血缘由 `Candidate.Source/ConsumedLength` 携带，不另造类型。

### KeyHandler（责任链单元）— pipeline_keyhandler.go:16
`Judge(ctx,key,data) Decision`（**纯函数无副作用**）+ `Apply(c,key,data) Result`（有副作用）。
链 = `global（全局分流）++ host.KeyHandlers()（宿主特有）++ sharedNav（共享导航）`，**第一个非 Pass 者** Apply 并短路。

### Decider（决策器）— pipeline_decider.go
持单一 `host` 状态机 + `registry []Processor`（触发激活宿主，优先级高→低）+ `sharedNav`。统一职责：
- `tryActivateFromEmpty`（:183）：buffer 空时遍历 registry，首个 Judge→Activate 接管。
- `applyEngineDiff(needed Capability)`（:297）：用 `mounted` 位集**单点 diff** 挂卸引擎资源（拼音层/英文词库），杜绝散落不对称的 activate/deactivate。
- `reconcileHost`（:93）/ rewind 统一夺取回退。

### 纯数据决策 — pipeline_types.go
- `Verdict`（:12）：Pass / Handle / Activate / Release——判断与执行解耦，可单测。
- `Decision`：{Verdict, CommitIdx, TriggerKey, ActivateID, Residual}。
- `Capability` 位掩码：CapPinyinLayer / CapEnglishDict（+预留 Emoji/Url/Translate/CloudDict）。
- `CompositionPhase`：Cold（无→有）/ Hot（有→有换内容）/ Commit（上屏后开新）/ End（有→无）——composition 边界与宿主切换正交。

### EffectiveMode — coordinator.go:77
`Chinese / EnglishLower / EnglishUpper`，由 `chineseMode + capsLock` 派生（capslock on→大写英文，不改 chineseMode）。敏感字段用局部覆盖不改全局。

---

## 2. 统一按键处理流程

**buffer 非空时的优先级裁决** `decideBufferedTrigger`（mode_trigger.go:101，纯函数 A–F）：
```
A 双拼韵母键 → 送引擎
B 二候选键 + 候选足 → 选候选
C 三候选键 + 候选足 → 选候选
D 按 triggerModes() 顺序遍历，首匹配 → 顶码上屏 + 进模式
E 二三候选键候选不足 → overflow
F → 透传
```
`triggerModes()`（mode_trigger.go:79）返回**有序模式表**（快捷输入 > 临时拼音 > 特殊模式实例 > 临时英文）——**新增模式只需往列表插一项，无需散改 if**。这就是"统一按键处理"的核心：优先级集中在一处有序表达。

---

## 3. "模式融合"的六个机制（用户要吸取的精华）

1. **模式 = Processor 统一接口** → 一条分发链驱动所有模式，取代每模式一套 handler。
2. **触发优先级集中**：registry（buffer 空）+ triggerModes 有序表（buffer 非空），而非散落 if-mode。
3. **引擎资源单点 diff**：applyEngineDiff 用 mounted 位集，杜绝不对称挂卸（Go 注释明确此为修 bug 动机）。
4. **共享导航**：navKeyHandler 一份被多宿主复用，消除四套 handleXxxKey 重复的翻页/高亮（pipeline_nav.go 注释）。
5. **Judge/Apply 分离**：纯判断可单测，副作用集中在 Apply。
6. **候选多源融合**：date/calc/number/pinyin/rare/english 作为 Provider 按 Rank 合并进同一候选列表（如快捷输入里日期+计算+数字并列）。

---

## 4. Rust 现状

`coordinator.rs` 是 **2888 行单体**，主入口 `handle_key_event`（:2306，~390 行）：
- 模式靠 `State` 的**扁平布尔早退**分发：`temp_pinyin_mode`→`handle_temp_pinyin_key` / `quick_input_mode`→`handle_quick_input_key` / `temp_english_mode`→`handle_temp_english_key`（三套独立 handler）。
- 触发判断内联（shift+字母→临时英文、触发键→快捷输入/临时拼音），优先级写死在 if 顺序里。
- 一个大 `match key_code`（ESC/退格/空格/回车/数字/字母/方向/翻页/标点）。
- 注释已出现"对齐 Go decideBufferedTrigger"，说明作者知道 Go 的结构，但 Rust 仍是单体实现。
- `handle_*.rs`（handle_key/candidate/mode/temp/punct/lifecycle…）**全是 3 行桩**——模块化拆分从未做。
- **无** Processor/Provider/KeyHandler/Decider/Verdict 抽象；State 是扁平模式布尔（与 Go 旧设计同形）。
- **缺** Go 的 `confirmedSegments`（拼音分步确认/已确认段）、EffectiveMode 抽象、URL 模式、加词模式、special 模式、生僻字、敏感字段抑制、小键盘策略、智能符号。

---

## 5. 差距小结
| 维度 | Go 新设计 | Rust 现状 |
|---|---|---|
| 按键分发 | Processor + KeyHandler 责任链 | 单体 match + 模式布尔早退 |
| 模式抽象 | 统一 Processor 接口 | 各模式独立 handler 方法 |
| 触发优先级 | 有序 registry/triggerModes | if 顺序写死 |
| 候选源 | Provider 按 Rank 融合 | 引擎单一来源 |
| 引擎资源 | 单点位集 diff | 直接调 temp_pinyin_target/convert_with |
| 判断/执行 | Judge 纯 / Apply 副作用 | 混在一起 |
| 模式覆盖 | 6 宿主 + 6 provider | 3 模式（临时拼音/英文/快捷） |

---

## 6. Rust 目标边界（决策）

直取 Go pipeline 的**干净终态**，并用 Rust 类型系统优化掉 Go 的迁移包袱：

1. **模式 = trait Processor**：`engine_default/temp_pinyin/temp_english/quick_input/special/url` 各实现。`Judge(&Ctx,key)->Decision` / `activate` / `release` / `buffer_text` / `capabilities` / `key_handlers` / `accepted_providers`。
2. **候选源 = trait CandidateProvider**：`id/rank/query`，按 rank 合并（pinyin 委托 engine，date/calc/number/english/rare_char 各自）。这是 coordinator 级融合，与 engine 内 mixed 融合是**两级**（见 §8）。
3. **按键链**：`global ++ host.key_handlers() ++ shared_nav`，首个非 Pass 者 Apply。导航用**一份** shared nav（所有模式复用，含 temp_english——修 Go 未完成处）。
4. **Decision/Verdict/CompositionPhase/Capability 用 Rust enum/bitflags**：天然契合，Judge 返回 `Decision` enum，单测无需 mock 副作用。
5. **单一宿主真值源**：用 `enum ActiveMode` 或 `Box<dyn Processor>` 作**唯一**真值——**消除 Go 的 `c.xxxMode 布尔 + d.host 镜像`双真值源**（Go 需 reconcileHost 同步，是 bug 温床）。
6. **引擎资源单点 diff**：`apply_capability_diff(needed)` 用 bitflags，对称挂卸。
7. **触发优先级集中**：一个有序 `registry` + `decide_buffered_trigger` 返回优先级 enum（A–F），special **纳入** registry（修 Go 游离）。
8. **EffectiveMode enum**（Chinese/EnglishLower/EnglishUpper）+ 敏感字段局部覆盖。
9. **拼音分步确认**（confirmedSegments）作为 engine_default 宿主的状态补上（当前缺）；但 commit 路径设计要避免 Go 的"双轨状态污染每个出口"。
10. 入口 `handle_key_event` 瘦身为：toggle/热键/修饰预处理 → 进 pipeline 分发，不再是 595 行上帝函数。
11. **按键是否为"输入码"改查方案 `input_chars`**（见 config-schema.md §3b），**取代当前 `A-Z`(0x41–0x5A) 硬编码**。如五笔 `a-x`、含 `/test` 词条 `a-x/`、虎码 `a-z`。优先级：方案配置 > 全局 > 默认。双拼所用符号同样由方案映射决定，不再硬编码 `;`。

---

## 7. Go 坏设计 / 迁移包袱（不照搬）
1. 新旧双轨分发并存（595 行 handle_key_event + decider 穿插）→ Rust 单一 pipeline。
2. `c.xxxMode 布尔 + d.host` 双真值源、靠 reconcileHost 同步 → 单一 host 真值。
3. special 游离 registry 外、两条触发路径 → special 纳入统一 registry。
4. temp_english 导航仍内联（未完成）→ 全模式共享 nav。
5. `decide()` 半骨架遇 Activate/Release 回退旧路径 → 实现完整统一调度。
6. 旧 `handleXxxKey` 仍被新 Apply 委托 → 不保留遗留函数体。
7. `pinyinProvider.Query()` 丢 preedit、需持具体类型取 → Rust 返回元组/trait 带 preedit。
8. quickInputProcessor 双上下文（拼音 XOR 结构化）塞一个类型 → 拆分或干净建模。
9. HandleKeyEvent 595 行上帝函数；triggerModes 每键重建 slice。
10. confirmedSegments + inputBuffer 双轨态污染所有上屏出口 → 干净建模。
11. pendingReplay 时间窗口启发式（Excel 焦点切换兼容）→ 状态机。
12. lastOutputWasDigit + keyPrevDigitState 双快照字段 → 显式参数传递。

## 8. 与 engine/dict/store 的关系（两级融合）
- **engine 内融合**（engine.md mixed）：同一方案内码表+拼音候选分层加权。
- **coordinator 级融合**（本文 Provider）：跨来源（日期/计算/英文/生僻字/临时拼音）候选并入一个列表。
- 二者是不同层次：Provider 的 ProviderPinyin 委托 engine 出候选；engine 不感知 quick_input 等 coordinator 模式。
- 词库合并（dict composite）+ 词频重排（engine 排序层，见 [frequency.md](./frequency.md)）+ 打分（engine RimeScorer）都在 engine/dict 层；coordinator 只消费候选并管交互。三者经由 `Candidate.{weight,source,consumed_length}` 协作。

## 9. 落地顺序
coordinator 重构属 **阶段 C（交互层）**，不阻塞 engine/dict/store 的质量核心（阶段 B）。但架构现已锁定：
1. 阶段 B 完成质量核心后，先搭 Processor/Provider/KeyHandler/Decider 骨架 + engine_default 宿主跑通基本输入。
2. 逐个迁移模式为 Processor（临时拼音/英文/快捷输入 → special/url），每个用 shared nav。
3. 候选 Provider 融合（quick_input 多源）。
> 与 ROADMAP"模式融合/统一按键处理"原则呼应。每步 `wind_input/scripts/dev.sh ci` 把关。
