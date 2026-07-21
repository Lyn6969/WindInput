# 拼音词图的音节边界感知改造

分支：`feat/pinyin-syllable-code`（worktree `wt-pinyin-code`）
状态：规划中，尚未动生产代码

## 1. 问题

输入 `lianzhengtixing`（廉政提醒），首选整句为「李安整体性」。

用户诉求经澄清后收窄为：**不要让「李安」这种低频专名过于轻易地占据 `lian` 这个音**，而非要求「廉政提醒」上首位。

## 2. 根因

两个设计叠加，产生了一个**结构上不该存在的状态**。

### 2.1 切分只保留一条路径

`Dag::maximum_match`（`wind-engine/src/pinyin/dag.rs:51`）是 DP 最大覆盖切分，回溯后返回单个 `Vec<String>`。

对 `lianzhengtixing`：位置 0 上 `match_at` 按长音节优先返回 `["lian","li"]`，`lian` 先写入 `dp[4]`；`li`+`an` 随后以相同覆盖长度到达位置 4，被 `dag.rs:72` 的严格大于判据 `covered > dp[end]` 丢弃。

**实测切分恒为 `["lian","zheng","ti","xing"]`，`li|an` 从不产生。**

### 2.2 词图查询不校验边界

`lattice.rs:190` 与 `:206`：

```rust
let code: String = syllables[start..end].join("");   // "lian"
let results = dict.search(&code);                     // 裸 search，不带边界
```

词库中「李安」的编码是 `li an`（两个音节），存储时拍平为 `lian`，边界另存于 `DictEntry::boundary`（`wind-dict/src/binformat.rs:99`）。`search` 不校验该字段，于是「李安」从单音节边 `lian` 上被返回。

### 2.3 畸形状态与打分放大

结果是词图里出现一个 **2 个汉字只占 1 个音节跨度** 的节点：

```
李安  start=0 end=4  syls=["lian"]  log_prob=-11.0656
```

`score_node`（`lattice.rs:117`）的长词加成按**汉字数**计：

```rust
log_prob += BASE_CONTENT_WORD_BONUS * (char_count as f64).sqrt() * freq_factor;
```

「李安」用 1 个音节换 2 个字，白拿 `+1.03` 加成，以 0.30 分之差压过高频单字「连」（`-11.3698`），进而使整句「李安整体性」(-22.3249) 险胜「廉政提醒」(-23.1961)。

**实测同类畸形词条共 1110 条，其中 1107 条被 unigram 收录**（即绝大多数真的会拿到加成）。它们有共同结构：后字为零声母字（安/额/鹅/爱/奥/岸/昂），与前字连写后被最大匹配吞成单音节 —— `xi+an→xian`、`qi+e→qie`、`yu+e→yue`、`gu+ai→guai`。

## 3. 已否决的方案

**「字数 ≠ 音节跨度则不发长词加成」** —— 已实测，否决。

- 目标达成：「李安」在 `lian` 格从第 1 掉到第 2
- 但代价不可接受：`qietubiao` 从「企鹅图表」变成「且图表」，`liandaoyan` 从「李安导演」变成「连导演」
- 根本缺陷：该规则**完全不看词频**，「李安」（低频人名）与「企鹅」（高频常用词）在判据下是同一类东西，只能一视同仁地把 1110 个词全部降权

**教训**：这是在下游给一个上游造成的畸形状态打补丁。方向不错（与 librime 同向），但地基不对、力度不足，卡在「谁也没赢」的中间地带。

## 4. librime 对照

| 维度 | librime | 我们 |
|---|---|---|
| 切分 | 保留全部路径的图（`EdgeMap = start→end→{音节集}`） | DP 只留一条 |
| 消歧时机 | 延后到打分，用 credibility 软罚 | 提前在切分，硬丢弃 |
| 词典键 | `class Code : public vector<SyllableId>` | 扁平串 + 旁路 boundary（查询不校验） |
| 边界正确性 | 机制保证，假阳性不可能 | 查询绕过 → 假阳性 |
| 长度偏好 | 每词固定罚 `-18.42`（按**词数**） | `+3.0*sqrt(汉字数)`（按**字数**） |
| 歧义音节 | `CheckOverlappedSpellings` 罚 `-23.03` | 无对应机制 |
| 单字/虚词特判 | 无 | `-3.0` / `+2.0` 硬编码 |

关键源码：
- `ref/weasel/librime/src/rime/algo/syllabifier.cc:75-137`（建图保留全路径）、`:243-276`（歧义音节罚）
- `ref/weasel/librime/src/rime/dict/vocabulary.h:21`（Code 定义）
- `ref/weasel/librime/src/rime/gear/grammar.h:18-26`（每词固定罚）

librime 对「李安」的压制是**两道软惩罚叠加**（歧义音节 `-23.03` + 多一个词 `-18.42`，合计约 `-41.4`），并以隔音符号作为用户主动消歧的出口 —— 隔音符号会让长边在 prism 里根本匹配不上（`syllabifier.cc:80-84`），而非「标记边界」。

**注意**：该机制对「李安」和「西安」是一视同仁重罚的。这是一套产品语义（缩合音词不打隔音符号就压制），不是单纯的打分技巧。

### 4.1 真实 Rime 行为实测（用户提供，2026-07-21）

在 Rime 中以两种拼音方案测试：**输入 `xian` / `qie` 时，「西安」「企鹅」排在单字之后，约第 4/5 位。**

对照我们当前的行为：`xian` 的「西安」第 3 位（w=6091），`lian` 的「李安」第 5 位（w=1361）。

**结论：两边的候选层已经很接近，差距不在候选层。** librime 那个 `-23.03` 歧义罚的可观测效果主要落在**整句/词组合成**上 —— 缩合音词在候选列表里依然处于第 4/5 位这种完全可用的位置，并未被赶出列表。

这与 B（边界感知词图）单独实施的形态一致：B 只影响词图/整句合成，候选路径原封不动。

**因此 Phase 4 的歧义音节罚很可能不必要**，待 Phase 1/2 数据确认。若 B 落地后整句层面的行为已与 Rime 相当，则不引入 `-23` 罚 —— 不为一个已解决的问题增加机制。

## 5. 关键发现：不需要改二进制格式

librime 的 `Code = vector<SyllableId>` 形态**不应照搬**，原因有二：

1. **wdat v4 的边界数据已经足够。** `DictEntry::boundary: u64`（`binformat.rs:94-99`）存的是 code 中各音节起始字节位的 bitmask，与音节切分**一一对应、无损**（音节长度 = 相邻起始位之差），唯一限制是 code ≤ 64 字节。真值来自 rime 源数据 `你好\tni hao` 中的空格。

2. **`wind-dict` 是码表引擎共用的。** 码表编码是五笔字根串，无音节语义。全仓 `search*` 调用点 114 处，拼音专属的只有十几处。librime 没有码表引擎共用同一个 trie，我们有。

**这个判断在仓库里已有先例。** `d096846` 的作者留了字条（`wind-dict/src/cached.rs:167-170`）：

> 边界只对拼音有意义，而 `search` 的消费方遍布码表/英文/cmdbar/composite 等无音节概念的场景，不应被拼音的需求污染接口。仅拼音引擎改用本方法。

**且 `search_with_boundary` 这个 API 已经存在**（`cached.rs:171`，返回带 `boundary` 的 `DictHit`），`search_prefix_with_boundary` 也在（`:192`）。基础设施是齐的，缺的只是 `lattice.rs` 去用它。

### 5.1 这是一次已知漏洞模式的复发

`684999e`（双拼真值校验）的提交信息记录过：P2a 漏了一处 —— 拼音引擎直接持有 `CachedDict` 而不经 `SystemDictLayer`，所以 `lookup_with_fuzzy` 仍在用 `search()`，修复前看到的全是 0 边界、等于没设防。

**同一个漏洞现在原样存在于 `lattice.rs`。** 这不是新问题，是同类问题的第二次发作。

## 6. 三根柱子

| | 内容 | 单独实施的后果 |
|---|---|---|
| **A** 多路径切分 | 切分图取代单一路径 | 扁平键仍让「李安」**同时**从 `lian` 单音节边被捞到 → 重复计入，比现状更乱 |
| **B** 边界感知查询 | `lattice.rs` 改用 `search_with_boundary` + 校验 | 见 6.1，代价比预期小得多，但有一类真实伤亡 |
| **C** 打分调整 | 按词数固定罚取代按字数加成；歧义音节罚 | 依赖 A/B 才有意义 |

### 6.1 对 B 单独实施的重新评估

初判「B 单独实施会让『西安』打不出来」**是错的**，混淆了两条独立路径：

- **词图路径**（受 B 影响）：只用于 Viterbi 整句合成
- **候选路径**（不受 B 影响）：`dict.search` 直接产出的普通候选

实测佐证：`convert("xian",10)` 中「西安」位列第 3（w=6091），来自候选路径；且单音节输入根本不跑 Viterbi（需 `syllables.len() >= 2`）。因此 B 单独实施后，「西安」「李安」**仍是可选候选，只是不再参与整句合成** —— 这正是期望的行为。

**但 B 单独实施确有一类真实伤亡：跨多音节且内含缩合音的词。**

以 `xianjiaotongdaxue` 为例：「西安交通大学」的真实边界是 `xi|an|jiao|tong|da|xue`（6 音节），而当前切分给出 5 个音节。边界校验会判定不匹配 → 该词被逐出词图 → **打不出来**。它 5 个音节未超 `max_word_len=10`，走的是词图而非 step 1.5 兜底，无法被兜住。

**这正是 A 存在的理由**：多路径切分下，图中会保留 `xi|an|jiao|tong|da|xue` 这条路径，该词以真实的 6 音节跨度合法匹配。

### 6.2 核心问题与 Phase 0 的实测修正

原问题：1110 条畸形词条中，有多少只需 B 即可修复，有多少必须付 A 的代价？

**Phase 0 实测已给出分布，且推翻了本文档此前的刻画。**

本文档第 2.3 节曾把 1110 条整体描述成「李安」那一族（`xi+an→xian` 式两字缩合）。实测分布（判据见 `tests/pinyin_eval.rs`，入池前置：纯 CJK、2~8 字、汉字数 == 真值音节数、`mm` 完整覆盖 input）：

| 类别 | 判据 | 实测条数 | 语义 |
|---|---|---|---|
| **B** | `mm != true` 且 `mm.len() == 1` | **81** | 整词塌缩进**单个**音节边（李安/币安/巨额）—— 即「N 字占 1 音节跨度」的畸形节点本身 |
| **C** | `mm != true` 且 `mm.len() >= 2` | **1021** | 跨多音节、内部某处边界不符（长安大道/险恶环境/李奥瑞克） |

**「李安」那一族只有 81 条，占 7.4%；92.6% 是多音节词，即必须靠 A 才能救的那一类。**

此外实测发现本文档此前完全未计入的一批：**另有 3347 条「音节数相同但切法不同」**（`暗暗` 真值 `an|an` vs 切分 `a|nan`；`爱党爱国` 真值 `ai|dang|ai|guo` vs 切分 `ai|dan|gai|guo`）。这批同样过不了边界校验。

**C 类总计 4362 条 —— Phase 2 的伤亡面比原估计大四倍。**

判据说明：采用 `mm.len()` 而非「是否含零声母字」，因为前者直接刻画**缺陷的结构形态**（「占据单音节边」才是"短词被误提升"的机制），后者只是该形态的常见成因。

**注意**：分布 ≠ 归因。上表说明的是「有多少词的边界与切分不符」，尚未说明「B 单独实施后各类的命中率实际掉多少」。后者由 Phase 1 实测回答。

## 7. 分阶段计划

### Phase 0 — 评测基础设施（**先于任何生产代码改动**）

现状：**批量评测为零**。拼音相关 77 个内联测试全是单点 `assert_eq!`，最大的「批量」是两个 9 对的手写数组，且都是逐条断言 —— 一条回归整个测试红，**无法回答「100 句里多对了几句」**。更糟的是内联测试夹具用 `CodetableDict::empty()` + `merge_single` 构造，`boundary` 恒为 0，**在边界校验上等于没设防**（`mod.rs:1602` 的注释已警告过）。

数据是齐的，缺的是 harness：

- `build_dev/data/schemas/pinyin/cn_dicts/base.dict.yaml` — 376,820 行 `词\t拼音(空格分隔)\t权重`，**拼音中的空格即音节真值**，可直接生成「输入串 → 期望输出 + ground truth 切分」
- `.../ext.dict.yaml` — 235,599 行
- `.../unigram.txt` — 607,270 行 `词\t频次`，可用于按词频分层抽样
- 加载模板可复用 `wind-engine/tests/pinyin_long_word.rs:16-33` 的 `data_dir()` + `EngineManager`

产出：
1. 评测集生成器（分层抽样，覆盖普通词 / 1110 条缩合音词 / 多音节含缩合音词三类）
2. 聚合打分 harness（top-1 命中率、top-5 命中率，按类别分组）
3. 基线数字

**验收：能在改动前后跑出可比的命中率，并按类别拆分。**

#### Phase 0 已完成（2026-07-21）

产出：`wind_input/crates/wind-engine/tests/pinyin_eval.rs`（两个 `#[ignore]` 测试，无新依赖）。零生产代码改动。

运行：

```bash
cd wind_input
cargo test -p wind-engine --release --test pinyin_eval -- --ignored --nocapture pinyin_eval_report
# 环境变量：WIND_PINYIN_EVAL_OUT / _N(默认1000) / _SEED(默认20260721) / _DUMP(默认40)
```

一轮约 4 秒，同种子结果完全可复现。

**基线（seed=20260721, top_n=10）：**

| 类别 | 总体 | 样本 | top-1 | top-5 | MRR |
|---|---|---|---|---|---|
| A 普通词 | 596,506 | 1000 | 77.00% | 98.10% | 0.8631 |
| B 缩合音短词 | 80 | 80（全量） | 1.25% | 28.75% | 0.1260 |
| C 多音节含缩合音 | 4,362 | 1000 | 87.20% | 99.20% | 0.9268 |

交叉验证：harness 测得 `lian` 的「李安」排第 5 位，与 §4.1 用户在真实 Rime 中的观察一致。

#### ⚠️ 读数陷阱（必读，否则 Phase 1/2 结果必被误读）

1. **B 类 top-1 恒约等于 0，是判据的必然结果，不是缺陷。** B 类输入串恒为单音节（`bian`/`lian`/`jue`），首选必然是高频单字（便/连/绝）。**B 类唯一有信息量的指标是 top-5 与 MRR。**

2. **Phase 2 之后 B 类指标「变差」是修复生效的证据，不是回归。** B 类词本就不该进词图参与整句合成。**绝不可将其用作门禁** —— harness 被刻意设计为测量工具而非 `assert_eq!` 测试，正是为了防止有人为了让它变绿而把修复改回去。

3. **A 与 C 的绝对值不可横向比较。** C 类 top-1（87.2%）高于 A（77.0%），因 C 类词平均更长、同音竞争者少；A 类混入大量二字词、同音噪声重（如 `ziji` 期望「自激」而首选「自己」）。**只能各自纵向比 delta。**

已知观察点（Phase 2 的直接验证靶）：`lianfenxi` 期望「链分析」实得「李安分析」；`shifoulian` 期望「是否立案」实得「是否李安」。

### Phase 1 — 归因测量

用 Phase 0 的 harness，量化 6.2 那个问题：缩合音词里「只需 B」与「必须 A」各占多少。

**验收：给出比例数字，据此决定 A 的取舍。这是 A 是否立项的唯一依据。**

### Phase 2 — B：边界感知词图

`lattice.rs:190,206` 改用 `search_with_boundary`，建节点时校验 `boundary` 与 `syllables[start..end]` 的期望 mask。现成工具已有：`syllables_boundary_mask`（`mod.rs:431`）、`boundary_compatible`（`mod.rs:502`）。

预估十几行。注意事项：

- 模糊变体路径（`mod.rs:297,315`）**刻意置 `boundary: 0`** —— 词典边界在变体码空间（`zhongguo`）而候选 code 在用户原码空间（`zongguo`），偏移不同域。这是已记录的永久缺口，**本阶段不碰**，需确保校验逻辑对 `boundary == 0` 降级放行而非拒绝
- `boundary == 0` 还表示「无边界信息」（五笔码 / code 超 64 字节 / 旧格式），同样必须降级放行

**验收：命中率不低于基线；1110 条缩合音词类别的畸形节点消除率。**

### Phase 3 — A：多路径切分（**取决于 Phase 1 的数据**）

若 Phase 1 表明「必须 A」的比例不可忽略，才启动。

连带必改（`maximum_match` 共 5 个调用点）：

| 位置 | 依赖程度 |
|---|---|
| `lattice.rs:173` | **最深**。音节轴索引需重构为真正的字符位置图 |
| `mod.rs:670` | **深**。`completed_len` → `consumed_length`（分段上屏字节数）必须单一确定值 |
| `mod.rs:212` `compose_segment` | **必须单路径**，见下 |
| `mod.rs:242` `segment_with_separators` | 中等 |
| `scorer.rs:43` | 弱，只用总覆盖长度 |

**未决的产品问题 —— 预编辑区显示。** `compose_segment`（`mod.rs:209-231`）把 `syllables.join("'")` 作为 preedit 返回，用户看到的 `li'an'zheng` 即出自此处。切分变多路径后必须选一条显示，两个选项：

- (a) 锚定到当前首选候选对应的切分 —— 语义一致，但选词时 preedit 会跳变
- (b) 保留 `maximum_match` 专供显示，多路径只用于查询 —— 显示稳定，但可能与首选候选不一致

**此项需用户定夺，不由实现方自行决定。**

### Phase 4 — C：打分调整

在 A/B 落地后重新评估：

- 长度偏好从 `+3.0*sqrt(汉字数)` 改为 librime 式的按词数固定罚
- 是否引入 `CheckOverlappedSpellings` 式的歧义音节罚，以及是否采纳「隔音符号作为出口」这套产品语义（需先由用户在真实 Rime 中验证 `xian`/`qie` 的实际行为）
- `LOG_PROB_MIN = -15.0` 的 clamp 悬崖（`lattice.rs:116`）：低于下限的词加成整个归零，「廉政」(-15.4175) 即因此一分未得。悬崖是否改为平滑衰减

**注意**：A+B 落地后用户的原始诉求未必自动解决 —— 「李安」届时会以真实的 2 音节身份合法竞争，其 unigram 分（-12.09）本就高于「廉政」（-15.42）。**真正兑现诉求的是本阶段的歧义音节罚。**

## 8. 确认不受影响

- **码表 / 五笔 / 英文 / cmdbar**：`wind-engine/src/codetable/engine.rs` 全走 `dm.search*`，只要保持 `search` / `search_with_boundary` 双 API 并存即零波及
- **双拼路径**（`mod.rs:653-670`）：`syllables` 直接来自 `sp_result.syllables`（真值），从不走 DAG。**目标形态其实已在双拼那边跑通，全拼是唯一还在猜的路径**
- **`wind-reverse` / `wind-phrase` / `wind-coordinator`**：对 `Dag`/`SyllableTrie`/`maximum_match` 零引用
- **wdat 二进制格式与 `PARSE_SEMANTICS_VERSION`**：v4 数据已足够，无需动

## 9. 风险与回滚

- 每个 Phase 独立提交，可单独回滚
- **A 与 B 若都要做，必须在同一次合并中落地** —— 中间态在两个方向上都是退化（见第 6 节表格）
- `composite.rs` 去重/换最短码时 boundary 必须随 code 走（`d4084b8` 已踩过此坑）
- 无性能基准（无 `benches/`、无 criterion）。多路径切分的计算量增长**目前测不出来**，若启动 Phase 3 需先补基准

## 10. 相关文档

- `docs/architecture/rime-dict-loading.md` —— 边界的**权威文档**（L152-154 解析契约、L177-179 真值语义、L232 wdat v4 版本表、L251-257 `PARSE_SEMANTICS_VERSION` 递增规则）
- `docs/architecture/engine-candidate-pipeline.md` —— §4.1/§4.2 描述 DAG 与候选流程，但**尚未更新 P1/P2 的边界工作**，不提 `Candidate.boundary`、不提双拼真值校验，L434-435 对双拼的描述已过时。读时注意
- `docs/redesign/pinyin-smart-input.md`、`docs/redesign/dict.md` —— 均早于边界工作，dict.md §5 部分已过时

## 11. 历史包袱与已知缺口

边界系列（2026-07-17，均已在 main）：

- `d096846` P1 —— wdat v3→v4，EntryRecord 14B→22B，加 `boundary`。旧缓存靠内容指纹失配自动重建，无迁移代码
- `d4084b8` P2a —— 贯通 `Candidate.boundary`、`SystemDictLayer`、redb 16B→24B（`dec_val` 用宽松守卫实现惰性升级）
- `684999e` P2b —— 双拼真值校验；**自身发现并修补了 P2a 漏掉的一处**（见 5.1）
- `343773e` —— **推翻了 `5fa06fe` 的整体降级思路**并删除 `sp_fully_covers` 及其测试

提交信息中记录的未完成工作（仓库里**没有**叫「P3/P4」的东西）：

- Interpretation/SylSpan 抽象；模糊音按真边界逐音节展开；`generate.rs` 的暴力反推可整体删除
- `CharPinyinIndex` 的 410 音节暴力反推未动（改动会变更「代表读音 = 单字最高权重」语义）
- 手输码路径（webdata dict.add/update）无法插桩，只能靠 `infer_boundary_for` 缓解
- 模糊变体候选永久不校验边界，待跨域偏移映射
- `rime-dict-loading.md` L318-323：`[text, weight, code, stem]` 形码词库仍被标 `boundary=0b1`
