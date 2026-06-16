# 重设计差分：engine（输入引擎）

> 阶段 A 产物。基于 2026-06-15 对两侧 engine 的真实代码核查（Go 侧由 4 个只读 agent 提取、
> 关键论断已用 grep 抽验 file:line 属实；Rust 侧由本人直接通读）。
> 方法：Go 设计要点 / Rust 现状 / 差距 / **Go 坏设计（不照搬）** / **Rust 目标边界（决策）**。
>
> 体量：Go `internal/engine` ≈ 1.1 万行生产代码；Rust `wind-engine` ≈ 2.5k 行。
> 这是"质量不如 Go"最集中的子系统——核心差距在**打分模型**与**码表/混输交互**。

---

## 0. 契约层（Engine / ExtendedEngine / ConvertResult）

### Go 现状
- `Engine` 接口：`Convert(input,max) []Candidate` / `Reset()` / `Type()`（engine.go:21）。
- `ExtendedEngine`：加 `GetMaxCodeLength` / `ShouldAutoCommit` / `HandleEmptyCode` / `HandleTopCode`（engine.go:33）。
- 富信息走 `ConvertResult` 结构（engine.go:50）：候选 + `ShouldCommit/CommitText/IsEmpty/ShouldClear/ToEnglish/NewInput` + 拼音预编辑字段 + `Timing`。
- **关键事实**：codetable.go **并未真正实现** `ExtendedEngine.ShouldAutoCommit/HandleEmptyCode/GetMaxCodeLength`——自动上屏逻辑内联在 `ConvertEx` 末尾的 `checkAutoCommit`，结果写进 `ConvertResult.ShouldCommit/CommitText`（codetable.go:744）。即 Go 自己也没走那套接口方法，富结果才是真实通路。`HandleTopCode` 是唯一真正作为独立方法存在的扩展能力（codetable.go:819），因为顶码要把"剩余码"回灌输入。

### Rust 现状
- `Engine::convert` 直接返回富 `ConvertResult`（engine.rs:37）——**比 Go 的 Convert/ConvertEx 双接口更统一**。
- `ConvertResult`（engine.rs:17）字段：candidates / preedit / completed_syllables / partial / has_partial / should_commit / commit_text / is_empty。**缺** Go 的 `should_clear / to_english / new_input` 与 `Timing`。
- `ExtendedEngine` trait（engine.rs:49）有 should_auto_commit / handle_empty_code / handle_top_code / max_code_length——**码表实现全是桩**（codetable/engine.rs:91-107 全返回 None/默认）。

### 决策（Rust 目标边界）
1. **保留**统一的 `convert() → ConvertResult`，不退回 Go 的 Convert/ConvertEx 双接口（Rust 这里本就更优）。
2. **扩展 `ConvertResult`**：补 `should_clear: bool`、`to_english: bool`、`english_text: String`、`new_input: Option<String>`（顶码剩余码）。让"自动上屏/空码清空/转英文/顶码"都通过结果字段表达，在 `convert()` 内计算——对齐 Go 真实通路。
3. **重新定位 `ExtendedEngine`**：删掉 should_auto_commit / handle_empty_code 这两个"伪接口"（Go 也没用），逻辑并入 convert()。仅 `max_code_length()` 与 `handle_top_code()` 保留为显式方法（顶码需协调器配合回灌输入，属真正的额外能力）。
4. `Timing` 暂不补（perf 埋点属 ROADMAP 阶段 D；现在留接口位即可，不要像 Go 那样在 codetable 包复制一份 timing 结构）。

---

## 1. 拼音引擎（pinyin）

### 1.1 Go 候选流水线（engine_ex.go convertCore，:60）
按序：双拼→全拼预处理 → Parse 音节 → Composition 预编辑 → **步骤0 命令精确匹配** → **步骤0b Viterbi 造句**（≥2 连续音节 + unigram + 完整码≥4）→ **步骤1 完整音节词组精确匹配（含模糊变体）** → **步骤1b 多切分并行打分** → **步骤2 子词组枚举（n..2 音节）** → **步骤4a/4 首段/首音节单字** → **步骤4b 纯 partial 多音节首字** → **步骤5 trailing partial 前缀查找** → **步骤6 简拼/纯简拼（字数=音节数规则）** → 排序 → Shadow → Filter → 代码提示 → 双拼后处理。

### 1.2 Go 打分模型（ranker.go RimeScorer.ScoreWithLM，:333）—— **这是质量核心**
```
nw = NormalizeWeight(dictWeight)        // [0,10000] → [-15,0]  (ranker.go:298)
nw += unigram.LogProb(text) * 0.3       // LM 对数线性加成
nw = clamp(nw, -20, 0)
score = exp(nw) + initialQuality + coverage
weight = int(score * 1_000_000)
```
- `initialQuality` 按流水线步骤取值（命令100 / Viterbi造句4.0 / 精确4.0 / 多切分3.5 / 子词组3.0 / 单字2.5~4.0 / 简拼3.0 / partial展开0.0…），决定**同分段不同来源候选的档位**。
- unigram：登录词 `log(freq/total)`；未登录多字词取各字均值；用户选词 `BoostUserFreq +0.5/次、封顶 5.0`（lm.go:126）。
- 可选 bigram：`log(0.7·e^bi + 0.3·e^uni)`，未登录回退 `uni-1.0`（lm.go:286）。
- Viterbi 句权重：`(LogProb+30)/30·10000` 线性映射（engine_ex.go:235）。

### 1.3 Rust 现状（pinyin/mod.rs:113）
流水线仅 **5 步**：精确 → Viterbi 整句（SENTENCE_WEIGHT_BASE=3e7 置顶）→ DAG 子短语 → 前缀(30) → 缩写。
打分**只有 `weight 降序 + natural_order 升序`**（mod.rs:229）——**没有 RimeScorer 的归一化/LM 混合/initialQuality/coverage**。`config`（show_code_hint/filter_mode/use_smart_compose/candidate_order）全标 `#[allow(dead_code)]` 未生效。无命令查询、无多切分、无简拼字数规则、无 Shadow、无代码提示、无模糊音热更新（fuzzy_config 静态）。双拼 `shuangpin.rs` 仅 3 行空模块，且方案在 manager 被 `is_supported()` 直接过滤（manager.rs:130/196）。

### 1.4 差距小结
| 能力 | Go | Rust | 影响 |
|---|---|---|---|
| 打分模型 | RimeScorer（归一+LM+iq+coverage） | 裸 weight 排序 | **候选排序质量差距的主因** |
| initialQuality 分档 | 每步细分 | 无 | 单字/词组/简拼混排无序 |
| 流水线步骤 | ~10 步 | 5 步 | 多切分/简拼/首字候选缺失 |
| 用户词频学习 | unigram.BoostUserFreq（加权到 weight，已弃）| 上层 coordinator 简单 boost | **重构**：见 [frequency.md](./frequency.md)，词频解耦为排序独立维度 |
| 双拼 | 完整 converter+内置方案+预编辑 | **完全缺失/被过滤** | 双拼用户无法用 |
| bigram | 可选 | 无 | 长句二阶上下文缺 |
| Shadow/代码提示/模糊音热更 | 有 | 无/弱 | 功能缺 |

### 1.5 Go 坏设计（不照搬）
- **三套并行词库/打分抽象**：`Scorer`（deprecated, pinyin.go:44）+ 未集成的 `Ranker`（ranker.go:49）+ 未集成的 `lexicon.go`（CodeTableLexiconAdapter/LexiconQuery 全程被 convertCore 绕过，getSource 永远返回 SourceSystem，lexicon.go:174）。→ Rust 只保留**一个** scorer，不引入未接线的抽象层。
- `Config.UseSmartCompose` 死字段（从不被读，engine_ex.go:199 只看 unigram!=nil）。→ 不增死配置。
- `CandidateOrder` 的 `"char_first"` 与 `"smart"` 代码路径完全相同（engine_ex_lookup.go:29）。→ 要么真正实现差异，要么合并为一个选项。
- initialQuality 硬编码散落各调用处（engine_ex.go 十余处）。→ Rust 提取为**具名常量/枚举**集中定义。

### 1.6 Rust 目标边界（决策）
1. **引入单一打分器** `RimeScorer` 等价物：`normalize_weight(dict_weight)` + `unigram.log_prob*0.3` + `initial_quality` + `coverage`，`×1e6`。这是阶段 B 的第一优先（最大质量杠杆）。
   - 注：打分器只产出**词库基础质量分**（基于权重+LM）。**用户词频不进打分器、不改 dict_weight**——它是打分之后的**独立重排步骤**（码表 used-first 可选模式 / 拼音衰减分叠加），详见 [frequency.md](./frequency.md)。engine 排序层需持 store 的 freq 只读访问。
2. `initial_quality` 用 `enum CandidateTier { Command, Sentence, ExactPhrase, MultiSeg, SubPhrase, LeadingChar, Abbrev, PartialExpand }` → f32 常量表，集中定义。
3. 补流水线步骤：命令查询、多切分并行打分、首段/首音节单字、纯简拼（字数=音节数）。
4. 双拼作为**独立工作项**（见 §4）：converter + **自定义映射数据（非硬编码内置方案）** + 双拼预编辑 + 解除 manager 过滤。
5. 接通 config：show_code_hint（代码提示）/ filter_mode / candidate_order；模糊音改 `ArcSwap` 热更新。
6. bigram 暂缓（unigram 打分到位后再评估收益）。

---

## 2. 码表引擎（codetable）

### 2.1 Go 现状（codetable.go）
- 富 `Config`（codetable.go:32）：`MaxCodeLength/AutoCommitAtFull/MinAutoCommitLen/AutoCommitBlockOnPinyin/ClearOnEmptyAt4/TopCodeCommit/PunctCommit/SingleCodeInput/SingleCodeComplete/ProtectTopN/WeightMode(auto|global_freq|inner_order)/PrefixMode(none|sequential|bfs_bucket)/BucketLimit(30)/CharsetPreference/ShortCodeFirst/CandidateSortMode…`
- 流水线（ConvertEx:358）：精确（经 CompositeDict 含短语/用户/系统层）→ 前缀（BFS 分桶，maxDepth=MaxCodeLength-inputLen）→ 前缀降权（PrefixWeightPenalty=2e6）→ 精确模式空码补全 → 合并去重 → 字符集偏好（单字/词组 +5e6）→ 排序 → ProtectTopN 回填 → Shadow → Filter+截断。
- `checkAutoCommit`（:744）确切规则：开关开 & inputLen≥MinAutoCommitLen & 精确匹配唯一(n==1) & 无更长后继 → 上屏该候选。
- `HandleTopCode`（:819）：input 严格 > MaxCodeLength & 无完整匹配/更长后继 → 取前 MaxCodeLength 码转换上屏首选，剩余码回灌 `newInput`。
- `WeightMode` 自动判定：码表无权重标记→inner_order（按文件序），有→global_freq。

### 2.2 Rust 现状（codetable/engine.rs:31，仅 107 行）
精确匹配 → 前缀匹配(max.50) → weight 排序 → 截断。`ExtendedEngine` **全桩**：should_auto_commit→None、handle_top_code→None、handle_empty_code→(true,false,"")。

### 2.3 差距 / 目标边界
**Rust 缺失（按阶段 B 优先级）**：
1. **自动上屏**（满码唯一精确）`checkAutoCommit` 规则 → 写入 ConvertResult.should_commit/commit_text。
2. **五码顶字** `handle_top_code` → 保留为显式方法，返回 (commit_text, new_input)。
3. **前缀降权 + BFS 分桶前缀**（PrefixMode/BucketLimit/maxDepth）：现在 Rust 前缀无降权、无深度控制，导致前缀候选与精确候选乱序。
4. **字符集偏好 / WeightMode / inner_order 重排**：码表无权重时按文件序，这是五笔等"字根序"体验的关键。
5. **ProtectTopN**：锁定码表原始前 N 位。
6. **Shadow**。
- `WeightAsOrder/inner_order` 对齐：Rust 当前完全依赖 weight，码表若是 inner_order 类型会错乱——需先在 wind-dict 暴露"是否有权重"标记（跨子系统依赖，记入 dict 差分）。

### 2.4 Go 坏设计（不照搬）
- `ConvertRaw` 与 `ConvertEx` 两套平行流水线（codetable.go:219 vs :358），仅 Shadow/Filter 差异 → Rust 单流水线 + 测试用入参开关。
- codetable 包内复制 `engineTiming` 结构（循环依赖妥协，:194）→ Rust timing 类型放 engine crate 共享。
- 自动上屏内联在 ConvertEx 而非接口方法（接口与实现脱节）→ Rust 已决定并入 convert()+ConvertResult（§0）。
- `sync.Pool` 复用去重 map（几十个候选的过度优化）→ Rust 直接 HashSet。
- 三个初始化入口（LoadCodeTable/LoadCodeTableBinary/RestoreCodeTableHeader）→ Rust 统一构造。

---

## 3. 混输引擎（mixed）

### 3.1 Go 现状（mixed.go，1229 行）—— 按输入长度分 4 条路径
- `inputLen < MinPinyinLength(2)` → `convertCodetableOnly`（:524，含英文）
- `2 ≤ inputLen ≤ maxCodeLen` → `convertMixed`（:807，并行码表+拼音+英文）
- `inputLen > maxCodeLen & PinyinOnlyOverflow` → `convertPinyinOnly`（:576）
- `inputLen > maxCodeLen & !PinyinOnlyOverflow` → `convertMixedOverflow`（:663）

**Tier 加权**（mixed.go:18-70，已 grep 核实）：码表精确 +CodetableWeightBoost(1e7) / 短语 +PhraseWeightBoost(1e6) / 拆分补全 +PartialMatchBoost(5e5) / 拼音 ÷PinyinTierScale(100)。纯简拼降权：3码 -2e6、4码+ -3.5e6（在 ÷100 **之前**）。英文：精确 +5e6、前缀 +1e6。
**其它**：跨来源学习（OnCandidateSelected 路由，拼音上屏也喂码表造词，:1155）、preedit 抑制（suppressNonPinyinPreedit，:402）、顶码歧义裁决（isPossiblePinyinSequence 链，:265）、来源提示"拼"、代码提示。

### 3.2 Rust 现状（mixed/engine.rs，118 行）
**仅 1 条路径**：码表候选加权（phrase/精确/前缀三档）+ 拼音 ÷100 + 合并去重排序截断。常量对齐（PHRASE 1e6 / PARTIAL 5e5 / TIER 100；码表 boost 来自配置默认 1e7）。自带注释承认"英文候选、简拼长度惩罚、convertMixedOverflow 精细档"**未实现**。无意图/长度路由、无英文、无跨来源学习、无 preedit 抑制、无顶码裁决。

### 3.3 差距 / 目标边界
1. **长度分路**：实现 codetable-only / mixed / overflow 三类（pinyin-only-overflow 可作为 mixed-overflow 的 PinyinOnlyOverflow 分支）。
2. **简拼长度惩罚**（3/4 码）——直接影响混输候选质量。
3. **英文候选**（需 manager 注入 english 查询，见 §5）。
4. **跨来源学习路由**（拼音上屏喂码表 charBuffer）——五笔混输"边打边学"的关键。
5. **preedit 抑制 / 顶码裁决**。

### 3.4 Go 坏设计（不照搬）
- `CodetablePrefixBoostRatio=6` 定义了但全程未用（mixed.go:23，死常量）。
- Shadow 三重应用（子引擎各一次 + 外层一次，靠幂等）→ Rust **只在最终合并后应用一次**。
- `suppressNonPinyinPreedit` 清空音节字段却保留 `IsPinyinFallback`，留不一致中间态 → Rust 抑制时一并清。
- 拼音"先降权再 ÷100"的顺序依赖无断言守护 → Rust 用类型/注释固化顺序。
- 英文候选在 overflow 路径缺失（行为不一致）→ Rust 各路径统一处理英文。

---

## 4. 双拼（shuangpin）——独立工作项

Go：`pinyin/shuangpin/`（converter.go 533 + schemes_builtin.go 342 + scheme.go 94 ≈ 969 行）——双拼键对→全拼转换、内置方案表（自然码/微软/小鹤等）、模糊音、双拼专用 preedit（engine_ex.go:667 buildShuangpinPreedit）。
Rust：仅 3 行空模块，方案被 manager 过滤。
目标：converter + 双拼 preedit + manager 解除 `is_supported()` 过滤 + `UpdateShuangpinLayout` 热切换。
**关键改进（不照搬 Go 硬编码内置方案）**：双拼布局做成**自定义映射数据**（键位→声母/韵母 + 所用符号，如 `;` 作某韵母），引擎只消费通用映射；常见布局（自然码/微软/小鹤/搜狗…）作**预置数据文件**随程序发布而非代码 enum——用户可改可加（见 config-schema.md §3b）。**体量约 600-900 行，建议作为阶段 B 后段单独推进**。

---

## 5. 引擎管理器（manager）

### 5.1 对比
| 维度 | Go（manager*.go ≈2569 行） | Rust（manager.rs 754 行） |
|---|---|---|
| 引擎构建 | 委托 `schema.CreateEngineFromSchema` + DictManager | 自己加载/合并词典（mmap combined.wdb）|
| 加载策略 | 缓存 + 切换时 evict（keep-set）+ 异步预热 | **懒加载**（仅活跃方案）|
| 混输构建 | 工厂内 | 递归构建 primary+secondary（manager.rs:402）|
| 临时拼音 | 词库层热插拔（ActivateTempPinyin）| temp_pinyin_target + convert_with |
| 英文 | 懒加载 english 词库 + 层激活 + SearchEnglish 注入 | **无** |
| 热配置 | UpdateFilterMode/Codetable/Pinyin/Mixed/Learning/Shuangpin | **无**（config 大多没接）|
| 词频学习 | 异步 channel + FreqHandler + 独立拼音 user 层 | 上层处理（待核）|

### 5.2 评价与边界
- **Rust manager 的懒加载 + mmap 合并缓存是优点，保留**（比 Go 一次性建+evict 更省内存，契合 ROADMAP 内存目标）。
- 需补：英文词库加载/注入（供混输 §3）、热配置入口（接通 config，对齐阶段 C）、词频学习通路核实。
- Go 坏设计（不照搬）：`tempSchemaID` 与 `currentID` 共用字段（语义混淆，manager.go:571）；evict keep-set 硬编码 5 类 ID 且无容量上限（:328）；切换时遍历**所有**方案 extraLayer 反注册（代价随方案数线性增长，:413）；混输 learning 两条不对称继承路径；三个热更新函数作用域不一致（全量 vs 仅当前）；learning channel 满静默丢弃（manager_convert.go:229）。→ Rust 用更清晰的方案切换状态机（active/saved/temp 分字段）+ 显式保留策略。

---

## 6. 阶段 B 落地优先级（建议）

1. **打分器**（§1.6.1）：单一 RimeScorer 等价 + initialQuality 常量表。**最大质量杠杆**，先做。
2. **码表交互**（§2.3）：自动上屏 + 五码顶字 + 前缀降权/BFS + WeightMode/inner_order（依赖 dict 暴露权重标记）。
3. **混输分路 + 简拼惩罚 + 跨来源学习**（§3.3）。
4. **拼音流水线补步**（命令/多切分/简拼/首字，§1.6.3）。
5. **双拼**（§4，独立较大项）。
6. **manager 英文 + 热配置**（§5.2，部分对齐阶段 C）。

> 每项落地前在本目录补一份更细的实现笔记或直接在 PR 描述；每步用 `wind_input/scripts/dev.sh ci` 把关。
> 注意跨子系统依赖：码表 WeightMode、双拼方案表、英文词库均牵动 wind-dict / wind-config，需在对应差分中同步记录。
