# 出简让全：把简码让位从词库层搬到运行时

> 状态：**已实施**。实现在 `wind_input/crates/wind-coordinator/src/short_code_yield.rs`，
> 开关为 `schema.codetable.short_code_yield_level`（0 关闭 / 2 一二级 / 3 全部，出厂 3）。
> 词库层的 `gen_dict` `[demotion]` 已同步退役。
> 相关：`docs/design/codetable-freq-short-code-protection.md`（调频侧的简码位保护，与本功能方向相反、语义须对齐）、
> `docs/architecture/candidate-sorting-rules.md`（三套排序系统的位置关系）

## 1. 问题

五笔有一个标准功能叫**出简让全**：一个字若已有简码（1/2/3 码），它的全码（4 码）首选位
就该让给词语——用户已经能用更短的码打出这个字，占着全码首选是浪费一个码位。

改造前这件事在**词库生成阶段**做（`gen_dict::apply_demotion`，现已退役），有两个偏差：

**偏差一：语义不是"出简让全"，是"有条件的降权"。** 触发要同时满足

```
第二候选权重 ≥ 800(词) / 1000(字)
且 gap/首选权重 ≤ 0.65(词) / 0.60(字)
```

标准的出简让全是**无条件**的。旧实现还允许**让给另一个单字**（`is_char` 分支自带一套阈值），
而这个功能的本意是让给**词语**。

**偏差二：它是发行词库的既成事实，用户关不掉。** 判定烘进了权重，要改只能重新生成词库。

## 2. 判据的关键性质：简码是全码的前缀

五笔的简码定义就是全码的前 N 码——「路」的三简 `kht` 是全码 `khtk` 的前缀。

所以「这个字有没有更短的码」**不需要 `text → code` 反查索引**（`wind-dict` 的检索接口全是
`code → text` 单向：`search` / `search_prefix` / `has_longer_code`），只需回答
「输入码的某个前缀上，首选是不是它」。

这一点让所有实现方案的判据部分都变得廉价，难点因此全在**在哪一层拿这个首选**。

## 3. 方案选择

### 3.1 三条路与否决理由

| 方案 | 做法 | 结论 |
|---|---|---|
| A. 留在 `gen_dict` | 改判据为无条件 + 只让给词 | 语义可改对，但**用户仍关不掉** |
| B. wdat 构建时预计算 | 加载 yaml 建缓存时扫全表打标 | **否决**，见下 |
| C. 引擎 `convert` 内查前缀 | 每次按键 `dm.search(prefix)` | **否决**，见 §3.3 |
| **D. 记录简码首选** | 输入过程中累积各码长的首选，全码时直接查 | **已实施**，见 §4 |

### 3.2 为什么否决 wdat 预计算

wdat 是运行时首次加载 yaml 生成的**本机缓存**（`cached.rs`），有 `PARSE_SEMANTICS_VERSION`
指纹机制，改判据能自动重建——看起来很合适。

但它经 `reader_pool` **按文件路径共享**：同一份 `wubi86_jidian.wdat` 可能被多个方案同时引用。
而开关是 per-user 的运行时设置，把开关状态烘进共享缓存，等于让一个方案的开关影响另一个方案。
**生命周期不匹配**，且为一个开关改动缓存格式代价过重。

### 3.3 为什么否决"每次按键搜索"

`dm.search(prefix)` 拿到的是**词典层的序**，而出简让全问的是「用户能不能用更短的码**一键**
打出这个字」——只有**用户实际看到的第一条**能回答这个问题。词典层序不含：

- `apply_freq_rerank` — 用户调频的结果
- `apply_shadow` — 用户的候选调整规则
- `apply_filter` — 检索范围过滤
- 用户词层 / 临时词层的合并

具体差别：用户调频把某字顶到 `kht` 首位，搜索方案看不见（词典层序没变），出简让全不触发。

## 4. 采纳方案：记录简码首选

### 4.1 原理

输入是渐进的：打 `khtk` 必然逐键经过 `k` → `kh` → `kht` → `khtk`，每一步都跑完整的候选生成。
把**每一步最终候选的首条**记进会话状态，到全码时信息已经齐了——零查询，且记的是真相。

### 4.2 落点是现成的

```rust
// wind-coordinator/src/handle_candidate.rs:578
pub(crate) fn build_candidates(&self, state: &mut State, limit: usize) -> (usize, InputOutcome)
```

它**已经持有 `&mut State`**，而候选定型的全过程都在这个函数体内：

```
candidate_display_order  →  按 text 去重  →  apply_filter
                         →  apply_freq_rerank  →  apply_shadow  →  空码补全收口
```

记录点即该函数末尾，**不需要新锁、新数据通路、新引擎接口**。`State` 加一个定长数组即可。

### 4.3 状态形状

```rust
/// 本次输入过程中各码长的候选首选（仅 1..=3 码）。
/// 记的是用户**实际看到的**首条，故天然含用户词 / 调频 / shadow 的效果。
pub(crate) shortcode_tops: [Option<(String, String)>; 3]   // (code, top_text)
```

### 4.4 失效靠拉取，不靠推送

设计阶段写的是「生命周期随输入缓冲，缓冲清空即清」。实施时发现
`input_buffer.clear()` 在协调器里有**十余个散落调用点**（`coordinator.rs` 五处、
`message_handler.rs` 四处以上），逐个接线正是本仓栽过多次的「N 处落点漏一处」。

改成在 `build_candidates` 开头按前缀关系统一淘汰：

```rust
// 记录的码不再是当前输入的前缀，它就不成立了
short_code_yield::evict_stale(&mut state.shortcode_tops, &state.input_buffer);
```

缓冲清空、光标中间编辑、方案切换全被这**一条**覆盖，一个 `clear()` 点都不用碰。
判据能从数据本身重新导出时，推送式失效就该换成拉取式。§5.1 的光标编辑陷阱因此
不再需要单独处理——它本来就是同一条规则的一个实例。

## 5. 三个必须处理的陷阱

### 5.1 记录必须带码，不能只按码长索引

退格不是唯一的改码方式——`State` 有 `input_cursor_pos`（编码区光标，字节偏移），用户可以在
**中间插入或删除**字符，此时缓冲长度不变而码已经变了：

```
打 kht        →  记 [2] = "路"
光标移到中间，h 改成 x  →  缓冲变 kxt，长度未变
再打 k        →  kxtk
```

只按码长取记录，就会拿 `kht` 的结论去判 `kxt`。**存 `(code, top)` 对、用时校验
`input.starts_with(code)`** 才是稳的；"退格时按长度截断"抓不住光标编辑。

实施时这条与 §4.4 合并成了同一个机制：既然要存码并校验前缀，那么「淘汰陈旧记录」和
「拒绝跨码复用」就是同一个判据的两个用法，一处实现即可。

### 5.2 判定必须扫全部三级，否则链式让位自我拆台

最隐蔽的一条。考虑「路」在二简 `kh` 就是首选的情况：

```
kh    首选 = 路
kht   → 触发让位（kh 已能打出「路」）→ kht 首选变成某个词
khtk  → 若只看最近一级：kht 的首选已不是「路」→ 判定"无简码" → 不让位   ✗
```

判据把自己擦掉了。**扫全部 1/2/3 级记录**即自洽：`khtk` 查到 `kh` 的首选是「路」，照样让位。

症状是"有的字让了有的没让"，从现象极难倒推，故必须有针对性用例。

### 5.3 缺记录时保守不让位

以下路径不走码表 `convert`（各有自己的 buffer）：临时英文、临时拼音、特殊模式、URL 模式。
中途经过它们会让某一级缺记录。**缺记录 ⇒ 保持词库原序**——宁可少让，不可让错。

## 6. 让位的施加

- **只有单字候选参与让位**（出简让全是字的概念）
- **只让给多字词**（用户明确要求"让给词语"，不让给另一个单字）
- 首选与接位者**都必须是码表来源**——让拼音/短语候选降位是 `source_tier` 的地盘，
  两套规则不能在同一个位置上打架
- 接位者不能是 `is_scope_filtered`（检索范围临时放宽补进来的候选，「追加在末尾、
  原有顺序纹丝不动」是它的硬约束）
- 同码内没有多字词候选时**不动**（没人可接）
- 字只**降一位**，其余候选相对次序不变：`路|昤|路上|路口` → `路上|路|昤|路口`
  （`candidates[..=pos].rotate_right(1)`）
- 在所有排序之后做一次局部调整，**不往 `candidate_display_order` / `source_tier` 加排序键**

最后一条是这个方案难度低的根本原因：它绕开了本仓反复踩的
「三套排序系统只接一套等于没接」（`freq_rerank::source_tier` 是首要键，会整体压过协调器显示序）。

### 6.1 档位：让到第几级简码

开关不是布尔而是**参与让位的简码级别上限**：`0` 关闭 / `2` 一二级简码置后 /
`3` 全部简码置后。别家输入法这两档都有，实测规模差 8 倍（30 个码 vs 239 个码）。

判据随之写成 **`当前码长 > level`**，而不是「当前码长 == 全码长」——后者要知道方案
有几码，换到非四码方案就错位。前者对任何码长的方案都成立：五笔配 3 则 4 码位让位；
5 码方案配 3 则 4、5 码位都让。

### 6.2 键名汉字码不豁免

`gen_dict` 的 `[protected_codes]` 保护 25 个键名汉字码（`aaaa`=工 `dddd`=大 …），
退役前的 `[demotion]` 一并跳过它们，理由是「有简码的字让位给词组」对键位约定不成立。

运行时实现**不设这层豁免**，规则统一。后果是开启后打 `dddd` 首选为「大厦」而非「大」，
`aaaa` 为「恭恭敬敬」。这是明确的取舍，不是遗漏；不接受的用户把档位调到 0 或 2。
`[protected_codes]` 的作用域因此收窄为「词库权重与上游序」，不再保「谁排第一」。

## 7. 必须一起做的决定：关掉 gen_dict 的 demotion

算法层做了，`gen_dict.toml` 的 `[demotion].enabled` 必须置 false，否则两套让位叠加、降两次。
**已执行**，该段标注为退役并写明勿重新开启。

附带收益已兑现：`wubi86_jidian.order_changes.tsv` 的首选偏离成因分布从

```
词频补权 278 + 简码降权 194 + 其它 3  = 475
```

变成

```
词频补权 286 + 其它 3                 = 289
```

「简码降权」整类消失，报告成为纯粹的「上游序 vs 词频」对比，定权重阈值时不必再把
降权那部分摘出去。

## 8. 落点

```
crates/wind-coordinator/src/short_code_yield.rs   判定 / 记录 / 淘汰，三个纯函数
crates/wind-coordinator/src/coordinator.rs        State.shortcode_tops
crates/wind-coordinator/src/handle_candidate.rs   三处接线（见下）
crates/wind-config/src/config.rs                  CodetableGlobal.short_code_yield_level
crates/wind-config/src/schema.rs                  方案级 Option<usize> 覆盖
crates/wind-config/src/config_schema.rs           注册（守门测试要求 data/config.toml 同步列出）
```

`build_candidates` 内的三处，次序不可换：

```
函数开头                          evict_stale   ← 每条返回路径都可能不走到记录点
  … 排序 / 去重 / apply_filter …
  apply_freq_rerank                             ← 让位必须在它之后
  apply_shadow / 空码补全收口
让位施加                          apply
记录本级首选                      record_top    ← 记在让位之后，记的是用户实际所见
state.candidates = candidates
自动上屏复评（读 first()）
```

## 9. 耦合点的实测结论

- **自动上屏**：设计阶段以为只需担心 `exact.count()`。实际上屏文本取自
  `handle_candidate.rs` 的 `state.candidates.first()`——让位改的正是它。两者不会撞上，
  因为让位要求同码有多字词（≥2 条候选）而自动上屏要求唯一精确匹配，条件互斥。
  这是推理不是观测，`codetable_short_code_yield.rs` 的用例族应当持续覆盖。
- **`ProtectPolicy.by_len`**：默认 `[1, 1, 0]` / `fallback: 0`。它保护的是 **1/2/3 码位**，
  本功能动的是**更长的码位**，两者不在同一码长上，**不打架**。但由此得到硬约束：
  让位必须排在 `apply_freq_rerank` 之后，因为深码位 `fallback = 0` 不设保护，
  先让位会被调频原样顶回去。
- **非五笔方案**：靠 `short_code_yield_level` 的方案级覆盖关掉。「短码首选 = 简码」
  这个等式只对五笔这类前缀式简码成立。

## 10. 测试

纯函数层 14 个用例在 `short_code_yield.rs` 内；端到端 5 个在
`crates/wind-coordinator/tests/codetable_short_code_yield.rs`。两层必须都有——本仓
「引擎全绿而用户打不出」是反复出现过的形态。

端到端用例选 `wqiy`（你 / 仰泳）而非 `khtk`（路 / 路程）：后者在发行词库里已被
退役前的 `[demotion]` 让过位，首选本就是词，测不出算法层有没有干活。

⚠️ `build_dev/data` 缺失时整族**静默跳过而计数照绿**。首次跑约 1.5s（建 `.wdat` 缓存），
之后 0.0x s 属正常，不能只按耗时判断——确认方式是故意改错断言看它是否变红。
