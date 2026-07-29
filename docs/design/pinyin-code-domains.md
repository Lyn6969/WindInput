# 拼音编码域：raw / flat / syl 的统一表示

> 状态：**§3 的三层断链 L1/L2/L3 已全部接通**（`19e7e96` / `4e8395e` / `b6655b3`），
> 未真机验证。§6 的 Phase 4–6 仍待做。
> 本文是 `pinyin-boundary-aware-lattice.md` §11「Interpretation/SylSpan 抽象」条目的展开。
> 那里记录了该抽象缺位已三次致 bug；本文补上第四次现场（简拼），并据此重定了目标形状。
>
> §3 各节保留**修复前**的实测数据与代码片段作为病历，不是当前状态；每节标题已标注修复提交。

## 1. 为什么现在写

§11 把问题描述成「两个编码空间靠文档注释区分，注释拦不住调用方拿错」，列了三处现场：

| # | 现场 | 形态 |
|---|---|---|
| 1 | `shuangpin.rs:182` `map_consumed_length` | 双拼击键 ↔ 全拼 |
| 2 | `mod.rs:432` `map_consumed_over_separators` | 带 `'` 输入 ↔ 全拼 |
| 3 | 词频记账码（`30858f5` 已修） | 写端全拼域、读端击键域，键永不相等 |

第四次现场是**简拼**。追查它时发现，问题比「拿错域」严重得多：用户词库的音节边界
在三个层次上依次断开，最终**整个边界维度对用户词是死的**（§3）。
简拼只是这条断链最显眼的症状。

## 2. 三个域

```
  raw （击键域）        用户实际敲进去的字节
      双拼   siyr        分隔符  xi'an       简拼  xan
        │                   │                 │
        ▼                   ▼                 ▼
  syl （音节序列域）    ["si","yuan"]   ["xi","an"]   ["xi","an","ning"]
        │
        ▼  join("")
  flat（全拼扁平域）    siyuan          xian          xianning
                        ↑ 存储主键（词库 code、词频 key、用户词 code）
```

`flat` 是 `syl` 的**有损投影**——`join("")` 丢掉了边界。

### 2.1 映射性质

| 变换 | 做法 | 性质 |
|---|---|---|
| syl → flat | `join("")` | 有损（丢边界） |
| syl → raw（双拼） | 逐音节查布局表 | 双射（给定方案） |
| syl → raw（分隔符） | `join("'")` | 满射 |
| syl → raw（简拼） | 取每音节首字母 | 有损、多对一 |
| raw（双拼）→ flat | 需先解到 syl | 双射 |
| raw（分隔符）→ flat | 去 `'` | 满射 |
| **raw（简拼）→ flat** | — | **不存在** |

最后一行是本文的支点。`xan` 不对应任何唯一 flat 码，简拼**不可能**走「raw→flat 再查」的老路。
它天然只能反向：由候选的 syl 投影出简拼再与 raw 比对。

于是：**凡需要在 raw 与 flat 之间往返的地方，正确解法都是绕道 syl。**

### 2.2 存储主键保持 flat

flat 作为 `(schema, code, text)` 的 code **不变**。前缀查询是逐键候选生成的命脉，
`ni hao` 作为键会让 `niha` 无法前缀匹配。这条在 boundary 系列里已论证并落地
（边界作为 entry 侧元数据、不进 key）。

本文引入的空格音节码（§5.1）是**生成 / 传输 / 导出**的表示，落库时拆成 `flat + mask`——
正是 `parse_rime_line` 对系统词库已经在做的事。

## 3. 三层断链（实测）

系统词库一切正常：源 yaml 本就是 `你好\tni hao\t1200`，简拼与 boundary
由**同一次 `split(' ')`** 产出（`codetable.rs:787-806`），是真值而非推断。

用户词库则在三层上依次断开。

### L1 — 持久化格式无边界：备份还原清零（**已修** `19e7e96`，见 §6 Phase 1）

wdict 是四列 TSV，`WordIo { code, text, weight, count }`，没有边界。
`backup.rs:286-288` 的还原路径是 `clear_user_words` + `import`，清空后全是新键，
而 `import_user_words:318` 对新键硬编码 `enc_val(..., 0)`。

实测：

```
[1] 写入后        boundary = [5, 21]
[2] wdict 导出 →  columns: [code, text, weight, count]
                  nihao	你好	500	0
                  xianning	西安宁	800	0        ← 边界不在文本里
[3] 还原后        boundary = [0, 0]                ← 全灭
[4] 不清空直接导入 boundary = [5, 0]                ← 旧键保住、新键必为 0
```

`[4]` 说明 `import_user_words:327` 的「boundary 沿用旧值」确实生效，但它只能保护
**已存在的键**，救不了还原场景，也救不了从别处导入的新词。

### L2 — 传递链断裂：候选恒 0（**已修** `4e8395e`）

`store_layer.rs:13`：

```rust
fn record_to_candidate(r: UserWordRecord, is_temp: bool, is_prefix: bool) -> Candidate {
    let mut c = Candidate {
        text: r.text,
        code: r.code,
        weight: r.weight,
        is_prefix,
        ..Default::default()   // ← r.boundary 在这里被丢掉
    };
```

`search_user_words_prefix:183` 明明把 `boundary: b` 读出来了。实测：

```
[store]  user  nihao    boundary = 5        ← 落盘了
[store]  temp  xianning boundary = Some(21) ← 落盘了
[cand]   user  search        你好   boundary=0   ← 到候选变 0
[cand]   user  search_prefix 你好   boundary=0
[cand]   temp  search        西安宁 boundary=0
```

三条路径全断（user search / user search_prefix / temp search）。

**这是 P2a 的第三处同型遗漏。** P2a 修的是 `SystemDictLayer`，但有两条旁路绕开它：

| 旁路 | 状态 |
|---|---|
| `PinyinEngine` 直接持有 `CachedDict` | P2b 自己发现并补了（边界文档 §5.1「已知漏洞模式的复发」） |
| `StoreUserLayer` / `StoreTempLayer` → `record_to_candidate` | **至今未补** |

同一模式已复发两次，说明「贯通某字段」的难点不是改哪几行，而是**枚举全部构造
`Candidate` 的地方**——`..Default::default()` 让漏掉的字段静默取 0，编译器不提醒。

### L3 — 消费端重猜（**已修** `b6655b3`）

`mod.rs:314`：

```rust
fn abbrev_of_code(&self, code: &str) -> Option<String> {
    let syllables = self.segment_with_separators(code);   // ← maximum_match 重猜
```

即使 L1/L2 修好，这里仍会无视候选自带的边界。实测（真实词库，用户词「西安宁」，
真值切分 `xi|an|ning`）：

| 查询 | 期望 | 实际 |
|---|---|---|
| `xa` → 系统词「西安」 | 命中 | ✅ 命中（第 2 位） |
| `xan` → 用户词「西安宁」 | 命中 | ❌ **不命中** |
| `xn` → 用户词「西安宁」 | 不该命中 | ⚠️ **错误命中** |

既漏（真简拼打不出）又错（假简拼能打出）。

> **这是同一教训的第二次现场**。`pinyin_multipath.rs:82` 写着「整句候选的 boundary 必须是
> 解码器实际走的那条路径，而非 `maximum_match`」——即 `xianjiaotongdaxue` 那个案子。
> Phase 3 修好了整句路径，简拼路径还留在旧世界。
> 判据可复用：**凡「拿 flat 码现算音节」的函数都是把已知真值扔掉再猜**。
> 嫌疑人清单 = `segment_with_separators` / `Dag::build(..).maximum_match()` 的全部调用点。

### 3.1 受害范围：不止简拼

`boundary=0` 曾对**所有**消费者生效。L2 接通后四项一并恢复：

| 消费点 | 断链期的行为 | 接通后 |
|---|---|---|
| `abbrev_of_code` 简拼投影 | 回退 DAG 猜 → 歧义码上出错 | 采信真值（L3 一并修） |
| `boundary_compatible` 双拼校验 | 任一侧 0 即放行 → **一律不校验** | 真正校验，实测无误杀 |
| `should_promote_user_completion` 长词上浮（`mod.rs:1065`） | 恒走「无边界」分支 | 2 音节即可上浮 |
| `handle_candidate.rs:1388` 自动造词沿用边界 | 选中用户词候选时传 0 | 传真值 |

**双拼误杀风险已实测排除**：用户词的 boundary 要么是真值（造词/导入），要么是 0
（手输码在 `infer_boundary_for` 里码不一致时给 0），不会产生错值，故校验不会拒掉
本该出现的词。`dabologe` / `dabo` / `dabolo` 三种击键下用户词位次均相同或更前。

### 3.2 其余缺口

- **简拼候选的 code 是简拼串**（`mod.rs:983` / `mod.rs:1088` 覆盖成 `query`），
  于是同一个词在简拼与全拼下走两个独立词频计数。见 §5.2。
- **词频表 value 无 boundary**：redb 词频记录 12B（`count u32 + last_used i64`），
  是唯一不带边界的持久层。今天无害（词频只做排序），非阻塞项。
- **raw↔flat 映射三处各自实现**：`map_consumed_length`（双拼，**唯一正确的一处**，
  因为它持有 `ConvertedSyllable`）、`map_consumed_over_separators`（分隔符，字节级扫描）、
  简拼（无映射）。

## 4. 关键发现：目标抽象已经存在

§11 提议新造 `Interpretation` / `SylSpan{pinyin, raw_start/raw_end, fp_start/fp_end}`。
实际上它已经在仓库里，只服务双拼一家——`shuangpin.rs:141`：

```rust
pub struct ConvertedSyllable {
    pub pinyin: String,     // syl 域
    pub sp_start: usize,    // raw 域起（sp_ = shuangpin）
    pub sp_end: usize,      // raw 域止
    pub fp_start: usize,    // flat 域起
    pub fp_end: usize,      // flat 域止
}
```

字段一一对应，只是 `raw_` 被命名为 `sp_`。`SpConvertResult`（`shuangpin.rs:156`）
就是 `Interpretation`。

所以这不是「设计新抽象」，是**把双拼里已建好的三域对齐结构提升为通用表示**，
让全拼 / 分隔符 / 简拼三条路都产出同款结构。风险与工作量都比从零造低得多。

顺带解释一个历史现象：`map_consumed_length` 的 fallback 注释写着「覆盖 partial、
无效键对/简拼等场景」——简拼在双拼下走的是逐字节 `position_map` 兜底，不是音节对齐。

## 5. 决策

### 5.1 空格音节码作为生成 / 传输 / 导出的表示

**从文字生成编码时直接产出带空格的音节码**（`ni hao` 而非 `nihao` + bitmask），
落库时再拆成 `flat + mask`。与系统词库源格式统一。

**转换规则（落库端，两类方案通用）：有空格就按音节拆，没空格就 `boundary=0`。**
不需要知道方案类型：

| 场景 | 导出形态 | 导入结果 |
|---|---|---|
| 拼音用户词 | `ni hao` | 拆分 → mask ✅ |
| 五笔等码表 | `abcd`（无空格） | `boundary=0` ✅ |
| 老 wdict 文件 | `nihao`（无空格） | `boundary=0`，与现状等价 ✅ |

这一点重要，因为 store 层拿不到 `engine_mgr`、无从判断引擎类型。
注意**不能无脑调 `syllable_boundary_mask`**——它对无空格串返回 `0b1`（「整串一个音节」），
那对五笔是错的语义；`parse_rime_line` 的五笔分支正是硬编码 0 而非调用该函数。

**收益：**

1. **消除一整类错配 bug。** 现在 code 与 boundary 是两个独立值，可以不一致——
   `d4084b8` 已踩过「A 层的 code + B 层的 boundary」。空格表示让二者物理上不可分离。
2. **`CodeBuilder` 可整体删除**（`generate.rs:85`）。它的全部复杂度是「段内 mask
   左移 `base`」+ overflow 检测；空格表示下拼接就是拼接。
3. **顺带修掉 64 字节天花板。** `syllable_boundary_mask` 的 `pos >= 64` 与
   `CodeBuilder::overflow` 都会让超长词整体降级为 `boundary=0`。字符串没有这个限制。
4. **导出的词库人类可读，且能直接喂给 Rime 系输入法**（bitmask 写进文本没人看得懂）。
5. wdict 是 TSV，字段内空格天然安全；头部已有 `columns:` 声明机制，扩展无兼容负担。

**涉及改动面：** `generate_word_pinyin` 的返回类型（`(String, u64)` → 带空格的 `String`）、
`calc_add_word_code`、`infer_boundary_for`、wdict 导入导出两端、`committed_segs` 五元组
（boundary 可从 code 推出，退回四元组）。

### 5.2 简拼词频归并到全拼码

简拼选中的候选，词频记在**该词的全拼码**上，与全拼输入共用计数。
同一个词在同一方案下不应有两份互不相认的熟练度。

两侧代价差异很大：

**用户词侧——便宜。** `c.code` 本来就是全拼码，是 `mod.rs:1088` 主动覆盖成 `query` 的。
不覆盖即可，简拼串另存一个字段供显示。

**系统词侧——需要动格式。** `search_abbrev` 返回 `(text, weight, order)`，**不含全拼码**：
AbbrevSection 是 `abbrev → text` 的直接映射，丢了主键。三个选项：

| 方案 | 做法 | 评价 |
|---|---|---|
| a | AbbrevSection entry 增加 flat code 字段 | 改 wdat 格式、版本 bump |
| b | 由 text 反查全拼码 | 有歧义（多音字/一词多码），用新猜测掩盖旧猜测——**不采纳** |
| **c** | AbbrevSection 改存 `abbrev → flat_code`，再走正常 `dict.search` | **推荐**：简拼本就是二级索引，应指向主键而非数据 |

方案 c 顺带让简拼候选自动获得 boundary（走主表拿到的）。代价同为 wdat 格式变更，
但缓存靠内容指纹自动重建，无迁移负担——这条路 v2→v3、v3→v4 已走过两次。

## 6. 分阶段建议

**顺序是 L1 → L2 → L3，不可颠倒。** 只修 L2 不划算：修好之后用户还原一次备份、
或从别处导入词库，边界又回到 0，L1 会持续侵蚀 L2 的价值。
L3（简拼）是最后一步——它只是这条断链最显眼的症状。

### Phase 1 — L1：空格音节码贯通生成与持久化 ✅ **已完成**

按 §5.1 实施。新造的词与导入导出的词都带真边界。

**生成端**（`wind-engine`）：`generate_word_pinyin` 及 `Engine` trait / `EngineManager`
返回类型由 `(String, u64)` 改为带空格的 `String`；`CodeBuilder` 换成 `SpacedCode`
（音节 `Vec<String>` + `join(" ")`），mask 左移与 overflow 检测整体删除；`DpState.seg_mask`
字段消失。查词典处显式 `replace(' ', "")` —— 词典 key 是扁平的。

**持久化端**（`wind-store`）：新增 `wdict::{join_code_by_boundary, split_spaced_code}`；
接入 `collect_user_word_rows` / `import_user_words` / `preview_import_user_words` /
`export_temp_words_wdict` / `import_temp_words_wdict` / `import_temp_word_rows` /
`dict_export` 的 TempWords 段。`import_user_words` 的分类判据加入「仅补边界」也算 updated，
`preview` 同步对齐。

**外部格式**：`import_formats::normalize_code` 由「删空格」改为「折叠为单空格」——
rime 源 `你好\tni hao\t100` 的空格本就是作者标注的音节真值，此前在这里被丢掉。

**RPC 契约不变**：`dict.encode` / `dict.genPinyin` 仍回扁平码（UI 会把它回填进编码框
再提交，带空格会存成带空格的 key）。写入侧新增 `normalize_add_code` 统一规范化，
顺带让用户可在设置页用空格**显式声明切分**（优先于 `infer_boundary_for` 的推断兜底）。

**验证**：744 测试通过。回归 `boundary_survives_export_clear_import` 已反向验证
（撤掉导出端 join 即变红）。真实词库端到端实测——

| 词 | 生成 | 落库 |
|---|---|---|
| 西安 | `xi an` | `("xian", 5)` |
| 反感 | `fan gan` | `("fangan", 9)` |
| 方案 | `fang an` | `("fangan", 17)` |
| 西安交通大学 | `xi an jiao tong da xue` | `("xianjiaotongdaxue", 20757)` |

「反感 / 方案」这一对是关键：同一个扁平码 `fangan`，边界分别为 9 与 17，正是 §3 L3
里 DAG 猜错的案例——造词端现在给的是真值。「西安」得到 `xi an` 而非 `maximum_match`
的 `xian`，简拼 `xa` 所需的真值边界至此有了来源。

**仍未接通**：L2（`record_to_candidate`）没改，所以边界目前只到 redb，尚未到候选。
用户词的简拼 / 双拼校验 / 长词上浮**仍是旧行为**，须等 Phase 2。

### Phase 2 — L2：接上 `record_to_candidate` ✅ **已完成**（`4e8395e`）

一行的事，收益是 §3.1 表里四项同时活过来。

**行为变更实测**（真实词库，用户词「大菠萝哥」`da|bo|luo|ge`）：

| 输入 | 有边界 | boundary=0（旧行为） |
|---|---|---|
| `daboluoge` | 位次 0 | 位次 0 |
| `dabo` | 位次 1，上浮 | **未命中——30 个候选里没有** |
| `daboluo` | 位次 1，上浮 | 位次 1，上浮 |

差异只在 2 音节这一档，不是整体翻转。`e10933f`「用户长词打部分拼音即上浮」
对用户词此前从未真正生效。

> 原计划用 `pinyin_eval.rs` 兜底，**实际不适用**：该评测跑系统词库转换准确率，
> 走 `CachedDict` 不经 `record_to_candidate`，覆盖不到本改动。改以上述对照实验
> 加双拼误杀专项验证替代（见 §3.1 末）。这类「评测覆盖不到的改动」应当明说，
> 而不是跑一遍绿的评测充当证据。

### Phase 3 — L3：简拼采信真值边界 ✅ **已完成**（`b6655b3`）

`abbrev_of_code` 优先用候选自带的 boundary 投影声母，`boundary=0` 才回退 DAG。

实测（用户词「西安宁」，真值 `xi|an|ning`）：

| 查询 | 修复前 | 修复后 |
|---|---|---|
| `xan` | 未命中（漏） | 命中 |
| `xn` | 命中（错） | 不命中 |

回归用的正是歧义切分码。仓库原有两个简拼测试
（`store_layer_words_match_abbreviation`）用 `cainiaoyizhan` / `lanshoubing`——
`maximum_match` 恰好猜对，**测不出这个 bug**，是「测试样本避开失效分支」的典型，
与密码框分区那案同型。备用样本：`fan|gan`（vs `fang|an`）、`ping|an`（vs `pin|gan`）。

**遗留**：`Candidate { .. ..Default::default() }` 的其余构造点尚未逐一排查，
不能排除第四处同型遗漏。

### Phase 4 — 简拼词频归并（§5.2）— **用户词侧已完成**（`240e49a`），系统词侧待做

**用户词侧**：删掉简拼分支对 `c.code` 的覆盖，保留全拼码（连同同域的 boundary）。
`consumed_length` 不受影响——判据是 `query.starts_with(&c.code)`，简拼下
`xan` 不以 `xianning` 开头 ⇒ 落 else 分支取 `query.len()`，仍是「消费整串」。
这一条容易想当然地判错（以为 code 变长消费也变长），已显式断言锁住。

**系统词侧**未做，需按 §5.2 方案 c 改 wdat 格式（`AbbrevSection` 改存
`abbrev → flat_code` 再走主表查）。

### 附：词频列表的显示暂不处理

词频页的编码仍是扁平码。`FreqRecord` 只有 `count + last_used`，词频表是唯一
不带 boundary 的持久层（§3.2），拿不到边界就 join 不出空格。

**故意押后**：词频表的 code 是候选的 code，而简拼候选的 code 此前被覆盖成简拼串，
表里混着全拼码与简拼码两种东西——先做 Phase 4 把键清干净，再谈补边界，
否则补上的 boundary 也会挂在简拼码上。系统词侧归并完成后再选
§3.2 的「扩容 value」或「读时反查词典」。

### Phase 5 — 三域结构统一

`ConvertedSyllable` 提升为通用 `SylSpan`（`sp_*` → `raw_*`），全拼与分隔符路径
也产出 `Interpretation`。之后：

- `map_consumed_over_separators` 可由通用映射取代
- 模糊变体候选终于能校验边界（§11 遗留项）
- `generate.rs` 的 410 音节暴力反推可整体删除（§11 遗留项；注意会变更
  「代表读音 = 单字最高权重」语义，需单独确认）

**触及逐键候选生成热路径，同样需要评测基础设施先行。**

### Phase 6 — 词频表 boundary（非阻塞）

仅在设置页需要展示音节构成时才做。可按用户词表同款惰性扩容（`dec_val` 长度分支）。

## 7. 不做什么

- **不改存储主键**：flat 作为 code 保持不变（§2.2）
- **不用 text 反查全拼码**（§5.2 方案 b）：用新猜测掩盖旧猜测
- **不把隔音符号做成产品语义出口**：已在 boundary 系列否决
- **不引入 `Code = vector<SyllableId>` 形式的词典键**：已在 librime 对照中否决

## 8. 相关文档

- `pinyin-boundary-aware-lattice.md` —— 边界系列主文档，§11 为本文前身
- `docs/architecture/engine-candidate-pipeline.md` —— 候选装配全链路
