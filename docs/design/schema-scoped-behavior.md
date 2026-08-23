# 方案级行为覆盖（标点 / 候选布局 / 短语加载）与英文方案原文候选

状态：📝 设计定稿，**未实施**（本轮只出文档，代码另开会话）
起因：英文方案（`data/schemas/english.schema.toml`）在真实使用中暴露四处「方案级表达力缺失」——
切到英文要英文标点、要竖排、不要加载短语，以及英文特有的「输入即内容」导致词频置顶反而挡住原文。

前三项是**同一类问题**：本仓已有 `[key_actions]` / `[session_actions]` / `[overlay]` 三段方案级配置，
但标点、候选布局、短语加载这三件事至今只有全局一份。第四项是英文引擎独有的语义问题，单独处理。

> 术语前提：**三种「英文」必须先分清**（英文半角 / 临时英文 / 英文方案），
> 见 `docs/design/`（`project_schema_switch_and_english` 记忆条目）。本文全篇「英文方案」
> 指 `[engine] type = "english"` 的可切换 active 方案，不是 Shift 切的英文半角。

---

## 一、落点与命名

### 1.1 判据

`docs/architecture/config-design-rules.md` §R2 一句话：**实例身份从哪来，配置就落到哪**。
标点态、候选布局、短语加载这三件事，在本需求里的身份来源都是「当前是哪个方案」⇒ 落方案文件，
覆盖模型 A（整文件替换 + `schema_overrides/{id}.toml` 深合并折叠）。

英文原文候选的身份来源不是方案实例，而是**英文引擎这个类型**（临英与英文方案共用同一条能力、
同一个 `english` 数据桶），⇒ 落全局 `[schema.english]`，覆盖模型 B。

### 1.2 段名

三个新段一律**复用既有模块名**，不造新词：

| 新段 | 对应的既有名字 | 字段 |
|---|---|---|
| `[punct]` | 全局 `input.punct` | `mode` |
| `[candidate]` | 全局 `ui.candidate` | `layout` |
| `[phrases]` | `wind-phrase::PhraseLayer` / store `phrases` 表 | `enabled` / `categories` / `exclude_categories` |

⚠️ 与 `[overlay].candidate_layout` 的关系：**两段并存、语义不同，不合并**。
`[overlay]` 那份是「本方案**被叠加激活期间**」的布局（有进入/退出生命周期）；
`[candidate]` 这份是「本方案**作为常驻 active 方案期间**」的布局。一个方案可以两段都写
（快符表作 overlay 时竖排、万一被用户设成常驻时横排），取值互不干扰。
字段名之所以一个叫 `candidate_layout`、一个叫 `layout`，是因为后者段名已含 `candidate`，
再写就是 `candidate.candidate_layout`（违反 R3 的路径冗余）。

### 1.3 方案文件示例（english）

```toml
[punct]
mode = "english"          # follow（默认） | chinese | english

[candidate]
layout = "vertical"       # follow（默认） | vertical | horizontal

[phrases]
enabled = false           # 英文方案不加载短语：date/tel 这类码与英文词直接撞车
```

### 1.4 复杂度评估：方案文件配置面全景

加这三段之后，`.schema.toml` 的完整配置面（13 段 → 16 段）：

| 段 | 字段数 | 归属 | 内置 5 方案里几个写了 |
|---|---|---|---|
| `[schema]` | 7 | 作者 | 5 |
| `[engine]` | 1 | 作者 | 5 |
| `[engine.codetable]` | 15 + `frequency` 子段 5 | 作者 4 项 / **用户** 11 项 | 3 |
| `[engine.pinyin]`（含 `.shuangpin`） | 2 | 作者 | 2 |
| `[engine.mixed]` | 2 | 作者 | 1 |
| `[engine.chaizi]` | 3 | 作者 | 1 |
| `[engine.aux_code]` | 3 | 作者 + 用户 | 1 |
| `[[dictionaries]]` | 10 / 条 | 作者 + 用户（`enabled`） | 5 |
| `[weight_spec]` | 5 | 作者（`dict weight-check` 算出） | 0 |
| `[encoder]` | 3 + rules | 作者 | 1 |
| `[key_actions]` | 键值表 | **用户** | 0 |
| `[session_actions]` | 键值表 | **用户** | 0 |
| `[overlay]` | 5 | 作者 + 用户 | 0 |
| **`[punct]`** | 1 | **用户** | 1（english） |
| **`[candidate]`** | 1 | **用户** | 1（english） |
| **`[phrases]`** | 3 | **用户** | 1（english） |

两个判断：

**① 新增三段与 `[key_actions]` / `[session_actions]` / `[overlay]` 同类，不是新物种。**
原有 13 段里 10 段是**方案作者**的领域（码表规格、词库、编码规则、权重归一化），
只有 3 段是纯用户偏好。新增三段全部落在后者，把「用户偏好」从 3 段扩到 6 段——
这一族原本就偏薄，**正是它薄才导致本文这四个需求全都无处安放**。

**② 复杂度的真实度量不是结构体里有几段，是一个典型方案文件里有几行。**
TOML 段不写就完全不存在：`wubi86.schema.toml` 加完之后一个字符都不变，只有
`english.schema.toml` 多 6 行。最后一列的写入率（1/5，且是同一个方案）说明了这点。

⇒ **保持三个独立段，不合并成一个 `[behavior]` 大段**。合并的代价：字段名要加前缀
（`phrase_categories` / `phrase_exclude`）反而更长；段名与全局域一一对应的关系断掉
（用户要另学一套映射）；且 `[punct]` 以后要加 `custom_mappings`（§2.4）时会变成大杂烩。

---

## 二、`[punct]`：方案级标点

### 2.1 现状

`state.chinese_punct` 是**运行时布尔状态**，全局唯一（`coordinator.rs:354`）。
它的写入点已有三类：用户 `toggle_punct`（`message_handler.rs:148`）、
`input.punct.follow_mode` 随中英模式联动、per-app `initial_punct`（`app_compat.rs:187`）。
读点分散在 `convert_punct` / `active_pairs` / 语言栏图标 / `push_config` 等处。
**方案维度完全没有。**

### 2.2 字段

```rust
/// 方案级标点态意图（`[punct]` 段）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PunctIntent {
    #[default]
    Follow,     // 不干预，沿用全局 / 用户当前状态
    Chinese,
    English,
}
```

取值词汇刻意与 `LayoutIntent` 同构（`Follow` 打头、`Follow` 是 default），
理由同 `CommentTemplateOverride` 的注释：**让「方案级」与「全局」在用户眼里是同一件事的两个层级**。

### 2.3 为什么做成通用能力而不是英文特判

1. 落点通路已经全部现成——方案文件段 → `merge_toml` → `EngineManager::active_*()` 带缓存，
   `key_actions` / `session_actions` / `[overlay]` 三段都走它，通用化的增量成本≈0；
2. 只判英文等于再加一个 `if schema_id == "english"` 分支。本仓已经为「`hidden` 被当作
   overlay 的代理判据」付过一次代价（三处，见 `docs/redesign/overlay-mode-config.md`），
   不要再种一个「english 被当作『需要英文标点的方案』的代理判据」；
3. 真实需求已经不止英文：日文/俄文方案、纯符号方案、代码输入方案都可能要英文标点。

### 2.4 ⚠️ 不做的部分

**本轮不把 `input.punct` 整段方案级化**（`custom_mappings` / `smart_after_digit` / `smart_list`）。
段名留成 `[punct]` 就是为了以后能加，但那是另一个量级：`custom_mappings` 是
`HashMap<String, Vec<String>>` 且有严格的**列序契约**（见 `project_punct_selchar_colorder_fix`），
方案级覆盖要先回答「整表替换还是逐键合并」，与本需求无关。

---

## 三、`[candidate]`：方案级候选布局

### 3.1 现状

`layout.rs::intent_for` 已经是**唯一**决策点，纯函数、声明式重算（刻意不做「进入存快照 / 退出回放」，
理由见该文件头部注释）。现优先级：

```
加词 > 独占模式（mix / special / 临拼 / 临英 / URL）> 全局基线
```

方案层在这条链上完全缺席——`[overlay].candidate_layout` 只在 `ModeKind::Special` 臂被读到。

### 3.2 改动

`intent_for` 新增一个参数 `schema: LayoutIntent`（取自 `EngineManager::active_candidate_layout()`），
链变成：

```
加词 > 独占模式 > 【手动值（代际有效）】 > 方案 > 全局基线
```

「手动值」那一层见第四节。

### 3.3 ★ `Follow` 的语义必须重新定义为「跟随下一层」

加了方案层之后，`LayoutIntent::Follow` 不再等价于「跟随全局基线」，而是**链式回落到下一层**。
这是本节唯一的语义变更，也是唯一能区分新旧实现的地方，测试必须钉住这一格：

| 模式意图 | 方案意图 | 全局基线 | 期望 | 旧实现会给出 |
|---|---|---|---|---|
| `Follow` | `Vertical` | 横排 | **竖排** | 横排 ❌ |
| `Horizontal` | `Vertical` | 竖排 | 横排 | 横排 ✅（模式层本就优先） |
| `Follow` | `Follow` | 竖排 | 竖排 | 竖排 ✅ |

实现上把 `intent_for` 的返回从 `LayoutIntent` 改成两段 `Option` 折叠：

```rust
mode_intent(...)                    // Option<LayoutIntent>，None = 本模式无意见
    .filter(|i| *i != LayoutIntent::Follow)
    .or(manual_layout(gen))          // 第四节
    .or_else(|| schema_intent.non_follow())
    .map_or(baseline, |i| i == Vertical)
```

⚠️ 现有 `intent_for` 末尾那句 `.unwrap_or_default()`（下标越界回落 `Follow`）要保留——
它是「热重载删掉了该 mix 实例」的兜底，与本次分层无关，且**当年是测试先红才发现的**
（随手写的 `.min()` 钳到末项会把用户没选过的横排塞给他）。

---

## 四、冲突规则：代际感知的手动覆盖

标点与布局共用这一套。这是本设计的核心，**两者必须同一心智模型**，否则用户要记两套规则。

### 4.1 规则

> 方案意图是**默认值**；用户在该方案期间手动改的值胜出，但只在**当前代际**内有效。

```
effective = if manual.generation == engine_mgr.schema_generation() { manual.value }
            else { schema_intent.or(global_baseline) }
```

行为序列：

| 动作 | 标点态 | 说明 |
|---|---|---|
| 切到英文方案 | 英文标点 | 方案意图生效 |
| 按 `toggle_punct` | 中文标点 | 手动值胜出，本代际内一直有效 |
| 切到五笔方案 | 五笔的意图 / 全局 | 代际 +1，手动值自动失效 |
| 切回英文方案 | **英文标点** | 手动值早已失效，回到方案意图 |

### 4.2 为什么是代际而不是「切方案时设一次」

`finish_user_schema_switch`（`handle_mode.rs:1267`）自己的注释写明：
**「本函数只覆盖五条切方案路径中的两条」**。加上启动时载入 `schema.active` 一条都不走，
命令式的「切方案时设一次」**必然漏接**，而漏接的表现是「配了没反应」——本仓最高频的报障形态。

代际方案不需要枚举切换路径：`EngineManager::schema_generation` 已存在
（`manager.rs:237`，只在活跃方案变时 +1），往返键 `toggle_schema` 的授权判据用的正是它
（`handle_mode.rs:1118`）。

⚠️ 反向提醒：`schema_generation` **不能**当 `invalidate_schema` 的失效判据
（设置页改 `schema_overrides` 不 bump 代际，见 `key_resolver.rs:26`）。
本处要的恰好就是「活跃方案变了」这一个语义，用它是对的。

### 4.3 实现形态：惰性代际同步（不是纯声明式）

布局是「算出来再下发」的派生值，可以纯声明式；**标点不是**——`state.chinese_punct` 是一个被
七八处代码直接读取的状态字段（语言栏图标、工具栏、`convert_punct`、`active_pairs`、
`push_config`、智能符号的 press1 快照……）。把它们全改成 `effective_chinese_punct(state)`
是一次大范围散射，且智能符号那处存的是**状态快照**，语义会歪。

⇒ 保留 `state.chinese_punct` 为唯一真相源，加一个**幂等的惰性同步点**：

```rust
/// 代际变化时把方案级意图落到运行时状态。幂等：代际未变即刻返回。
fn sync_schema_scope(&self, state: &mut State) {
    let gen = self.engine_mgr.schema_generation();
    if state.schema_scope_gen == gen { return; }
    state.schema_scope_gen = gen;
    state.punct_manual = None;          // 手动值随代际失效
    state.layout_manual = None;
    if let Some(v) = self.active_punct_intent().resolve() {
        state.chinese_punct = v;
    }
}
```

调用点：`handle_key_event` 入口、`push_state_update`、`show_status`、语言栏图标刷新。

★ **这个形态的关键性质：漏调一个点的后果是「晚一拍」而不是「永不生效」**——
下一次按键必然经过 `handle_key_event`。这正是它优于命令式写法的地方，也是可以接受
「调用点不止一个」的理由。命令式写法漏一条路径就是永久失效。

### 4.4 手动值的写入点

| 写入点 | 写 `punct_manual` | 写 `layout_manual` |
|---|---|---|
| `toggle_punct`（`message_handler.rs:148`） | ✅ | — |
| `input.punct.follow_mode` 联动（切中英模式） | ✅ 视同手动 | — |
| per-app `initial_punct` | ✅ 视同手动（宿主意图比方案意图更具体） | — |
| 命令栏 `ime.toggle("layout")` | — | ✅ |
| 智能符号临时置 `chinese_punct=false`（`message_handler.rs:778`） | ❌ **不写**，它 saved/restore 成对 | — |

⚠️ 最后一行是必须显式排除的：那段代码借用 `chinese_punct` 做「英全」的临时表达，
前后成对恢复，不是用户意志。写进手动值会让一次智能符号操作冻结方案意图。

### 4.5 优先级为什么是「模式 > 手动 > 方案」

手动值的作用域是**当前方案期间**；模式（临英、快符、加词面板）是更内层的临时态，
其布局意图是「这段时间的候选长这样」，本就应该压过用户对整个方案的偏好——
且模式退出后手动值自动重新生效，用户不会感到失控。这与现状一致（现在模式意图就压过基线镜像），
所以这条不是新规则，只是把方案层插在了手动值之下。

---

## 五、英文方案的原文候选

### 5.1 问题

英文引擎的特殊性：**输入串本身就是合法上屏内容**。而 `schema.english.frequency` 一旦把某个词
顶到首位，用户想上屏所打原文就只剩回车这一条路（且回车是终结性动作，打断连续输入流）。
码表方案没有这个问题——`aaaa` 不是可上屏文本。

### 5.2 形态：与临时英文完全一致

临英路径（`handle_temp.rs:764-813`）已经把这件事做对了，直接对齐：

```
输入 hel（hello 已被词频顶到词库段首）
  1. hel      ← 原文，空格直接上屏
  2. hello
  3. help
  4. helmet
```

三条已经踩平的判据原样搬过来：

1. **原文候选不带 `source` / `code`**。写端 `if cand.source != English { return }` 据此排除，
   否则会写出「读端按候选码永远查不中」的孤儿词频键（与「短语有文本无码位恒不记词频」同一先例）；
2. **rerank 只作用于词库段**（`cands.split_off(dict_start)`）。这正是需求要的那个保证：
   原文钉在最前、词频置顶只在词库候选内部生效；
3. **精确去重**，不是小写去重。`hello` 只抹掉词库里字面相同的那条，不能连 `Hello` 一起抹。

### 5.3 落点：四个键，两侧各自独立

```toml
[input.temp_english]
raw_candidate = true      # 新增，默认 true（＝保持既有行为）
case_variants = true      # 既有，默认 true，不动

[schema.english]
raw_candidate = true      # 新增，默认 true
case_variants = false     # 新增，默认 false
```

| 键 | 默认 | 变更性质 |
|---|---|---|
| `input.temp_english.raw_candidate` | `true` | **新增可配置性**，行为不变（临英原本恒有原文候选、不可配） |
| `input.temp_english.case_variants` | `true` | 既有键，**一个字都不改** |
| `schema.english.raw_candidate` | `true` | 新能力（英文方案原本没有原文候选） |
| `schema.english.case_variants` | `false` | 新能力，默认关 |

★ **两侧完全对称、各自独立**：同一能力在两个作用域各有一份，各自默认。
理由是**用户对这两个场景的需求本就可能相反**——中文里插一个英文词（临英）与长时打英文
（英文方案）是两种输入节奏，把谁绑给谁都会有人被迫接受不想要的行为。

这**不是**两个真相源。真相源指同一件事有两处配置；这里是同一能力的两个作用域实例，
与 `candidate_layout` 在六处模式各有一份完全同形（本仓已确立的做法：同名字段、各作用域
一份、各自默认）。**`case_variants` 两侧默认值相反，本身就是「场景不同」的证据**——
若强行合并成一个键，无论取哪个默认都会静默改掉一侧的既有行为。

⇒ 因此 §8.1 阶段 C **不含**任何键迁移：四个键各自独立，`input.temp_english.case_variants`
原样留在原处。（早先设计稿里「必须同时迁移，否则两个真相源」那句是按「合并成一个键」
写的，已随本节作废。）

### 5.4 共用实现，不共用配置

★ **四个键、两条路径，但只能有两个函数**：
`push_raw_english_candidate(&mut Vec<Candidate>, buf)` 与 `push_en_case_variants(...)`，
由 `handle_candidate.rs`（英文方案）与 `handle_temp.rs`（临英）各自按自己那对键调用。
两份实现分叉只是时间问题——`phrase_owns_code` 的注释里已记过一次同型教训。

`case_variants` 的既有实现 `en_case_variants(&buf)` 已是独立纯函数，直接复用。

### 5.5 ⚠️ 新出现的边界：两个键同时关闭 ⇒ 候选可能为空

关掉 `raw_candidate` 后，「打词库里没有的词」这件事就只剩回车这一条出口——这本身是
用户可以选的取舍（有人就是想要「打英文时总是走词库补全」）。真正的风险在**两个键
同时关闭且词库无命中**时：候选列表为空。

**临英侧：既有判据是对的，不必改代码。**
`handle_temp.rs` 空格臂的判据是 `!state.candidates.is_empty()`（**实际候选是否为空**），
不是 `show_candidates` 配置项 ⇒ 空候选会正确落到兜底分支「上屏缓冲原文 + 按配置补空格」。
⚠️ 但那一分支的注释写的是「无候选（`show_candidates` 关闭）」——**成因从此多了一个**，
实施时要更新注释，否则下一个人会以为这条路只有关候选显示时才走得到。
回车臂同理（`space_as_input && !candidates.is_empty()`，判据同样落在实际候选上）。

**英文方案侧：主路径的空候选行为是本设计唯一没有既存判据可依的地方，实施时必验。**
英文引擎 `should_commit` 恒 `false`、无顶码，所以「空候选 + 按空格」走的是主路径的
通用分支。要确认它上屏的是输入串本身而不是吞键——若是吞键，就是「打了一串英文按空格
什么都没发生」。这条必须有端到端测试，判据落在**实际上屏文本**上。

同型教训：[`project_english_commit_space`]「给开关找接线点前先问『那条路走得到吗』」。

---

## 六、`[phrases]`：方案级短语加载与分类

### 6.1 现状

`wind-store/src/phrases.rs:3` 的模块注释白纸黑字：**「短语是全局的（不分方案）」**。
`PhraseLayer` 是全局单例 `RwLock`，从 store 的 `enabled_phrases_for_input()` 一次性建成。
store 里 key = `code + text`，`PhraseValue` 有 `weight / position / is_system / enabled / shadows_system`，
**没有 category**。

### 6.2 字段（一次做到位）

方案文件：

```toml
[phrases]
enabled = true                    # 默认 true
categories = ["常用", "工作"]      # 空/不写 = 不限制（全部分类）
exclude_categories = ["日期"]      # 空/不写 = 不排除；在 categories 之后再减
```

```rust
pub struct PhrasesSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 白名单。**空 = 不施加这一项限制**（全部分类），不是「一条都不要」。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// 黑名单。空 = 不排除。在 `categories` 之后再减。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_categories: Vec<String>,
}
```

### ★ 这里刻意**没有**三态

设计稿曾把 `categories` 写成 `Option<Vec<String>>`，用「键缺失 = 全部 / 空数组 = 一条都不要」
表达三态。复查后作废：**「一条都不要」已经由 `enabled = false` 表达**，
`enabled = true` + `categories = []` 是一个语义重复的状态。既然重复，就不必区分缺失与空。

⇒ 两个字段都用朴素 `Vec<String>`，语义**完全对称**：**空 = 不施加这一项限制**。
好处有三：不必动用 `config-design-rules` §R3「三态只允许两式」那套规约；GUI 直接是两个
多选列表，没有「清空列表」与「未设置」的歧义；以后加第三个过滤维度时形态可以照抄。

判据可复用：**给一族过滤器加维度前，先问「这个三态里有没有一态已经被别的字段表达了」**。

### 6.2.1 未分类短语（`category == ""`）

白名单里写空串即匹配未分类项：`categories = ["", "工作"]`。
不引入「default」之类的映射名——存储层本来就是空串，多一层映射就多一处要对齐的地方。

⚠️ **分类 UI 落地之前，store 里所有存量短语的 `category` 都是 `""`**。
此时任何非空 `categories`（如 `["工作"]`）都会把全部短语过滤掉，表现成「短语突然全没了」。
文档站与设置页 hint 必须写明这一点，否则这是本功能上线后的第一个报障。

store 侧 `PhraseValue` 加：

```rust
#[serde(default)]
pub category: String,     // "" = 未分类
```

serde `default` 保证向后兼容：既有 redb 记录反序列化后 `category == ""`。
「未分类」参与匹配的规则：`categories` 里写 `""` 才匹配未分类项（不写就不匹配），
这样「只要已分类的短语」是可表达的。

### 6.3 ⚠️ 本需求最大的风险：短语查询有六个消费点

| # | 位置 | 作用 |
|---|---|---|
| 1 | `handle_candidate.rs:692` `lookup` | 精确码短语候选 |
| 2 | `handle_candidate.rs:790` `lookup_prefix` | 前缀枚举候选 |
| 3 | `handle_candidate.rs:1154` `phrase_owns_code`（内含 `has_exact_code` + `has_longer_code`） | **顶码 / 全码自动上屏的否决闸** |
| 4 | `handle_candidate.rs:1166` `phrase_has_exact_code` | 顶码兑现前的复核 |
| 5 | `handle_temp.rs:92` `lookup` | 临拼/临英夺取判定 |
| 6 | `handle_temp.rs:93` `lookup_prefix` | 同上 |

★★ **只漏掉 #3 的表现**：英文方案下短语候选不出现了（#1/#2 已过滤），但顶码与自动上屏
仍被短语层否决 ⇒ **打字卡住不上屏，且没有任何日志**。这与
`project_pinyin_entry_boundary_contract` 里「闸口漏接一段无任何报错」是同一形状。

⇒ **解药是编译期强制，不是自觉**：把 `PhraseLayer` 的六个查询方法的签名全部改成
**必填** `scope: &PhraseScope` 参数（而不是加一个 `Option` 或另开一族 `*_scoped` 方法）。
漏接 = 编译失败。新增查询方法时同样躲不过。

```rust
pub struct PhraseScope<'a> {
    enabled: bool,
    categories: &'a [String],   // 空 = 不限制
    exclude: &'a [String],
}
impl PhraseScope<'_> {
    pub fn admits(&self, e: &PhraseEntry) -> bool { ... }
    /// 关闭态快路径：`enabled == false` 时所有查询直接返回空/false。
    pub fn is_closed(&self) -> bool { !self.enabled }
}
```

### 6.4 作用域判据：复用 `effective_data_schema`

**不要**新写一个「当前是哪个方案」的判据。`effective_data_schema` 已经是词频读端
（`apply_freq_rerank_in`）、写端（`record_selection_in`）、右键菜单（`candidate_op_scope`）
三处共用的归属源，临英在它下面返回 `Some("english")`。用它 ⇒ 临英自动按英文方案的
`[phrases]` 走，不必另立判据，也不会出现「候选归 english 桶、短语归五笔」的错配。

⚠️ **绝不能改用 `overlay_engine_schema`**：它在 `show_candidates = false` 时返回 `None`
（它回答的是「要不要出候选」，不是「数据算谁的」）。这条坑 2026-08-21 已经踩过一次。

### 6.5 取值通路

照抄 `active_key_actions`（`manager.rs:1954`）的现成形态：

```rust
pub fn active_phrases_spec(&self) -> Arc<PhrasesSpec>   // per-schema 缓存 + invalidate_schema 失效
pub fn active_punct_intent(&self) -> PunctIntent
pub fn active_candidate_layout(&self) -> LayoutIntent
```

三个方法同构：`read_schema`（方案文件内联 + `schema_overrides` 已 `merge_toml` 合并）
→ 取段 → 缓存 `Arc`。读不到方案（文件缺失/解析失败）时返回 default = 不覆盖。

⚠️ 与 `active_key_actions` 一致：**混输方案不下钻到 `primary_schema`**。
这三段都是「用户在这个方案里想要什么」，属于方案自身的交互属性，不像码表行为那样是
「这张码表怎么工作」。混输方案想配就在自己文件里配。

---

## 七、测试矩阵

### 7.1 纯函数层（不构造 Coordinator）

| 用例 | 钉住 |
|---|---|
| `intent_for` 三层折叠 × {模式 Follow/非 Follow} × {方案 Follow/非 Follow} × {基线竖/横} | §3.3 那张表，**必测 `模式 Follow + 方案 Vertical + 全局横排 ⇒ 竖排`**（唯一区分新旧语义的格） |
| `PunctIntent::resolve` 三态 | Follow 不干预 |
| `PhraseScope::admits` × {未分类项, 已分类项} × {categories 空/含""/含名} × {exclude 命中/不命中} | §6.2 的「空 = 不限制」对称语义；**必测 `categories=["工作"]` 把未分类项全部排除** |

### 7.2 代际机制

| 用例 | 钉住 |
|---|---|
| 切方案 → 手动 toggle → 切走 → 切回 | §4.1 那张行为表**逐行**断言 |
| 智能符号的 saved/restore 不写手动值 | §4.4 最后一行 |
| `sync_schema_scope` 幂等（连调两次结果相同） | 惰性同步的前提 |
| 只经 `handle_key_event` 一条路（不调其余同步点）也能生效 | §4.3 的「晚一拍而非失效」性质 |

### 7.3 短语闸门（最重要）

| 用例 | 钉住 |
|---|---|
| `[phrases] enabled=false` 下六个消费点**逐个**断言 | 不能只测候选列表；**必须单独断言 `phrase_owns_code == false`** |
| 英文方案下打 `date` 能正常上屏 `date` | §6.3 的真实故障场景端到端 |
| 变异验证：把 #3 的过滤删掉 | 必须精确红在「顶码卡住」那条，其余绿 |

★ 断言落在 `desired_vertical()` / `phrase_owns_code()` 的**返回值**上，不要断言
「有没有发出 UiCommand」——`sync_candidate_layout` 的去重缓存在值没变时本就不发，
测试会拿不到信号却看起来通过（`layout.rs` 已记载过这个假绿源）。

### 7.3.1 英文候选（四个键）

| 用例 | 钉住 |
|---|---|
| 四个键的 2×2×2×2 里，**两侧互不影响**（改临英那对不动英文方案，反之亦然） | §5.3 的「各自独立」 |
| `raw_candidate=false` + `case_variants=false` + 词库无命中 → 空候选 → 空格 | §5.5：临英必须上屏缓冲原文；**英文方案必须上屏输入串而不是吞键** |
| 英文方案 `case_variants` 默认关 / 临英默认开 | 默认值本身要有测试，否则「只翻默认值」一条都不红 |

### 7.4 已知假绿源

1. **`schema.english.frequency.enabled` 出厂 `false`**，`Config::default()` 也是 `false`。
   测英文原文候选与词频的交互时**必须显式打开**，否则测的是一个关着的功能。
   同型：`top_code_commit`（默认 false、出厂 true）。
2. 词典缺失时整族**静默跳过**（判据是耗时非 0.00s），worktree 需自备 `build_dev`。
   ⇒ 布局与代际那两组刻意选**不依赖 `build_dev/data`** 的路径（临英/纯函数）。
3. 主方案取 **wubi86 而非 english**：active 若也是英文，「按 active 归属」这种错误实现
   同样能通过，等于什么都没锁住。

---

## 八、跨仓清单与实施顺序

### 8.1 主仓（WindInput）

| 阶段 | 内容 |
|---|---|
| A | `wind-config/schema.rs`：`PunctIntent` + `[punct]` / `[candidate]` / `[phrases]` 三段结构体；`manager.rs` 三个 `active_*()` + per-schema 缓存并入 `invalidate_schema` |
| B | 代际机制：`State` 加 `schema_scope_gen` / `punct_manual` / `layout_manual`；`sync_schema_scope` + 四个调用点；`layout.rs::intent_for` 插方案层 |
| C | 英文原文候选与变形：`push_raw_english_candidate` / `push_en_case_variants` 两个共用函数 + **四个键**（`input.temp_english.raw_candidate` 新增默认 true、`schema.english.raw_candidate` 新增默认 true、`schema.english.case_variants` 新增默认 false；`input.temp_english.case_variants` 原样不动）。三个新键各走 REGISTRY / `data/config.toml` / 文档站。**无任何键迁移**，见 §5.3 |
| D | 短语闸门：store `PhraseValue.category`；`PhraseLayer` 六个方法签名改必填 `scope`；`effective_data_schema` 接线；`data/schemas/english.schema.toml` 写 `[phrases] enabled = false` |

A→B→C→D 顺序有依赖（B 依赖 A；C、D 独立于 B，可并行）。
D 风险最高（改 store schema + 六处闸门），放最后单独验证。

### 8.2 wind-setting（设置仓）

- 方案设置对话框加「标点 / 候选布局」两项，走既有 `saveConfig` 稀疏 diff 通路；
- ★★ **同一次保存里的多个改动来源必须写进同一份 cfg、只调一次 `saveConfig`**——
  它是「整份 cfg 与方案文件 diff、结果全量重写 override」，分两次调后一次会把前一次整个覆盖掉；
- ★ **保存只改本 UI 管的字段，在原对象上 `insert`，不重建整个段**——段里还有手写字段
  （`[phrases].categories` 就是典型：本轮无 GUI，只能手写），用字面量重建会静默抹掉；
- 三个新键（`input.temp_english.raw_candidate`、`schema.english.raw_candidate` / `case_variants`）走全局 manifest，各要过**五道守门测试**（REGISTRY / L2 /
  manifest / 快照 / 覆盖率）；
- ⚠️ 设置仓靠 `path = "../WindInput/..."` 依赖主工作区 ⇒ 在 feature worktree 里恒绿、
  **合并后才红**。收尾工作要算进「合并后」。

### 8.3 文档站（WindInputDocs）

- config 参考页 + 用法页**两处**都要改（加配置项的固定动作）；
- 方案文件三个新段是文档站从未记录过的方案文件字段族，需要新开一节
  （`keys.key_actions` 当年就漏过一轮，0.114 里根本没有那个键的记录）；
- `<Since>` 只能放标题 / 表格行 / 列表项；`lint:links` 读构建产物，须先 build。

### 8.4 Android

`[punct]` / `[candidate]` / `[phrases]` 都在方案文件里，随方案包走，**Android 侧无需手工同步**。
但那三个新键是全局 config 键 ⇒ 若要在移动端跟随，需要同步 Android assets
的手工副本（是否跟随是产品决策，实施时要问）。

---

## 九、已否决 / 暂缓（勿再提）

| 项 | 结论 |
|---|---|
| 只对英文做标点特判 | ⛔ 否决。见 §2.3——本仓已为「用 `hidden` 代理 overlay」付过一次三处修改的代价 |
| `[punct]` 本轮涵盖 `custom_mappings` | ⏸ 暂缓。列序契约要先单独回答「整表替换还是逐键合并」，见 §2.4 |
| `[candidate]` 与 `[overlay].candidate_layout` 合并成一段 | ⛔ 否决。两者语义不同（常驻期 vs 叠加期），一个方案可以两段都写，见 §1.2 |
| `case_variants` 下放英文方案 | ✅ **本轮做**，默认 `false`。2026-08-21 的否决理由实际只对 `symbol_chars`/`space_as_input` 成立，见 §5.3 |
| `symbol_chars` / `space_as_input` 下放英文方案 | ⛔ 否决（2026-08-21 用户拍板）。它们会改标点键既有语义 |
| 把两侧 `case_variants` / `raw_candidate` 合并成一个键 | ⛔ 否决。用户对两个场景的需求本就可能相反；`case_variants` 默认值相反就是证据，合并必然改掉一侧的既有行为，见 §5.3 |
| 临英原文候选保持强制不可配 | ⛔ 作废（用户 2026-08-23 决定）。四个键两侧对称、各自独立，见 §5.3 |
| `categories` 用 `Option<Vec<String>>` 表达三态 | ⛔ 作废。「一条都不要」已由 `enabled = false` 表达，三态里有一态是重复的，见 §6.2 |
| 「方案意图恒胜、用户改不动」 | ⛔ 否决。标点态带工具栏与语言栏图标，锁死会表现成「图标闪一下又弹回」——本仓同类现象已报障多次 |
| 「切方案时设一次」的命令式写法 | ⛔ 否决。`finish_user_schema_switch` 只覆盖五条路径中的两条，见 §4.2 |
| 按方案重建 `PhraseLayer` | ⛔ 否决。切方案要重建、漏刷新即错；改为 lookup 时传 scope，声明式自愈，见 §6.3 |
| 短语过滤器做成可选参数 / 另开 `*_scoped` 方法族 | ⛔ 否决。漏接无任何报错正是本需求最大风险，必须编译期强制，见 §6.3 |

---

## 相关文档

- `docs/architecture/config-design-rules.md` —— 配置准入 / 落点 / 命名 / 默认值纪律
- `docs/redesign/overlay-mode-config.md` —— `[overlay]` 段下沉，本文的直接前身
- `docs/design/mode-candidate-layout.md` —— 模式级布局三态，本文 §3 的基础
- `docs/design/schema-key-actions.md` / `session-key-actions.md` —— 方案级配置段的既有先例
- `docs/config-key-migration.md` —— 改已发布键类型时的 Value 层迁移
