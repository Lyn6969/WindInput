# 码表候选排序：基础排序全局化 + 中英文/字词分组 + 词库基序

> 本文是 `docs/redesign/frequency.md`（词频/排序权威设计）**第一层"基础排序 `base_sort`"的细化与修订**。
> frequency.md 定义了两层关系：第一层 `base_sort`（weight | natural）、第二层用户词频 `user_frequency`。
> 本文只处理**第一层基础排序**的三个缺陷与增强，不改动第二层词频语义（`freq_rerank` / `protect_top_n` 照旧）。

## 1. 问题陈述（用户诉求）

1. **保证现状不回归**：`base_sort = "weight"`（按词库权重降序）的方案行为必须完全不变。
2. **无权重按文件出现顺序**：`base_sort = "natural"`（自然序）时，若候选无权重，应按**词库文件中从上到下的出现顺序**排列，**不能退化成编码字母序**。
3. **中英文 / 单字 vs 词 的分组**：自动排序时，应能把不同类别（中文/英文、单字/多字词）分组，各组内再排序，而非混排。
4. **多候选可配置"基础排序"（词库基序）**：多个词库（基础词库 + 扩展词库）叠加时，应能配置一个**词库级基础排序**，让扩展词库的候选**总排在基础词库之后**。

## 2. 现状（含代码证据）

### 2.1 运行期排序链路（权威路径）

码表方案的真实运行期引擎是 `CodeTableEngine`（`wind-engine/src/codetable/engine.rs`），持有 `Arc<DictManager>`。
（注：`wind-engine/src/codetable/mod.rs` 中另有一个用 `weight_sort_key` 的 `CodetableEngine`，是旁支/历史实现，运行期码表方案不走它。）

```
方案配置 [[dictionaries]]（主库 default=true，扩展库 default_enabled）
  │  manager.rs::load_codetable_layers（每库独立层，不再合并 combined.wdat）
  ▼
DictManager → CompositeDict
  ├─ codetable-system         主库，恒 enabled，注册置首   manager.rs:1645
  ├─ codetable-extra-<id1>    扩展库，enabled=is_enabled()  manager.rs:1663
  └─ codetable-extra-<id2>    …
  │  CompositeDict.search_prefix：for source in sources { results.extend(...) }  composite.rs:42
  │  （只按层序拼接，自身不排序；主库结果在前）
  ▼
CodeTableEngine::convert（engine.rs:98）
  1. dm.search(input)          精确匹配，dedup by text
  2. dm.search_prefix(input)   前缀补充，dedup by text
  3. candidates.sort_by(better)          ★ 全局重排                 engine.rs:141
  4. 超额时「精确优先」分区截断，再 sort_by(better) 恢复显示序        engine.rs:147-155
  ▼
候选列表 →（协调器）freq_rerank + protect_top_n（frequency.md，第二层）
```

### 2.2 排序权威 = `better()` 比较器

`wind-candidate/src/candidate.rs:131`：

```rust
pub fn better(a: &Candidate, b: &Candidate) -> Ordering {
    a.weight.cmp(&b.weight).reverse()             // 1. weight 降序
     .then(a.natural_order.cmp(&b.natural_order)) // 2. natural_order 升序
     .then(a.code.cmp(&b.code))                   // 3. ★ code 字母序
     .then(a.consumed_length.cmp(&b.consumed_length).reverse())
     .then(a.text.cmp(&b.text))                   // 4. text 字典序
}
```

`Candidate`（candidate.rs:54）已有 `natural_order: i32` 字段，以及一批**分组语义标志**：
`is_common`、`is_phrase`、`is_prefix`、`is_partial`、`is_fuzzy`、`source: CandidateSource{CodeTable/Pinyin/English/Phrase}`。
另有 `better_natural()`（candidate.rs:144，精确优先 + natural_order）——但 `convert()` 用的是 `better()`，不是它。

### 2.3 `natural_order` 的来源 = 叶内序号（局部，非全局）

DAT 格式（`wind-dict/src/datformat.rs`）每条记录仅 10 字节 `{text_off u32, text_len u16, weight i32}`，**不存全局顺序**（datformat.rs:9）。
文档明言（datformat.rs:20-25）："叶内候选顺序 = 写入顺序，查询回调 order = 叶内序号 i，**不额外存储 order 字段**"。
读取时 `read_leaf_entries` 回调 `order = i`（叶内下标，每个 code 从 0 重新计）（datformat.rs:644），该值填入 `Candidate.natural_order`。

### 2.4 词库合并（反查用）与 live 层（查询用）

- **live 查询**：`load_codetable_layers` 每库独立成层（manager.rs:1593），CompositeDict 查询期合并去重。
- **combined.wdat 合并**（manager.rs:1748 `load_merged_dicts`，现仅供反查索引）：按 code 聚合，写入前 `entries.sort_by(|a,b| b.1.cmp(&a.1))`（仅 weight 降序，manager.rs:1799），**丢弃 order**，`WdatWriter::add(code, Vec<(text,weight)>)` 不接收 order（datformat.rs:243）。

### 2.5 相关配置现状（`wind-config/src/schema.rs`）

```rust
pub struct CodeTableSpec {          // [engine.codetable]
    pub max_code_length: usize,
    pub base_sort: String,          // "weight"(默认) / "natural"（自然序/inner_order） :94
    pub input_chars: String,
}
pub struct DictSpec {               // [[dictionaries]]
    pub id, label, description, path: String,
    pub dict_type: String,          // rime_codetable / rime_pinyin / english / codetable
    pub default: bool,              // 主词库
    pub default_enabled: Option<bool>,  // 扩展库默认启用（tri-state）
    pub enabled: Option<bool>,
    pub weight_as_order: bool,      // ★ 权重仅表示同码内排序序号（已有，但未接线到全局序）
    pub weight_spec: Option<WeightSpec>,  // 跨库权重归一化
}
```

## 3. 根因

无权重（或 `base_sort=natural`）时候选排序退化为编码字母序，根因有两条、叠加：

- **根因 A：DAT 未持久化全局顺序。** `natural_order` 是**叶内局部序号**（每 code 从 0 起），跨编码时大量并列为 0，无法表达"谁在词库文件里更靠前"。
- **根因 B：`better()` 的兜底是 `code.cmp`（字母序），且无"词库分组位"。** 当 `weight` 与 `natural_order` 都并列时，直接按编码字母序决定；同时它把不同层（主库/扩展库）的候选按 code 全局重排，**层序被摧毁** → 扩展库无法保证排在基础库之后。

> 一句话：`natural_order` 缺全局锚点（根因 A），`better()` 缺分组维度、兜底又是字母序（根因 B）。

## 4. 设计目标

| ID | 目标 | 对应诉求 |
|---|---|---|
| G1 | `base_sort="weight"` 行为零回归 | ①保证现状 |
| G2 | `base_sort="natural"` 无权重时按**词库文件出现顺序**（全局序），非字母序 | ②文件顺序 |
| G3 | 扩展词库候选**总排在基础词库之后**（词库基序） | ④词库基础排序 |
| G4 | 自动排序时支持**中英文 / 单字 vs 词**分组，组内再排序 | ③分组 |
| G5 | 分组/基序**可配置**，默认关闭以保 G1 | ①③④ |

## 5. 核心设计：分层排序键

把 `better()` 的隐式比较，显式化为一个**多级排序键元组**（全部升序比较，值越小越靠前）：

```
sort_key = ( group_rank , category_rank , primary , secondary , code , text )
```

| 层级 | 含义 | 取值 | 服务目标 |
|---|---|---|---|
| `group_rank` | 词库基序 | 主库=0，扩展库按方案顺序 1,2,…（`DictSpec.order` 或层序） | G3 |
| `category_rank` | 类别分组 | 按配置的分组顺序映射（见 §7） | G4 |
| `primary` | 主排序量 | `base_sort=weight` → `-weight`；`base_sort=natural` → `global_order` | G1/G2 |
| `secondary` | 次排序量 | `weight` 模式下 = `global_order`（同权稳定）；`natural` 模式下 = 0 | G1/G2 |
| `code` | 编码 | 字典序（**保留为最末兜底**，行为同现状） | 兜底 |
| `text` | 文本 | 字典序 | 兜底 |

要点：
- **各层是"字典序优先级"**，前一层相等才看后一层，天然表达"先分组、组内再排"。
- **`group_rank`、`category_rank` 默认恒为 0**（未配置时），此时 `sort_key` 退化为 `(0,0,-weight,global_order,code,text)`——与现状 `better()` 的 `weight desc → natural_order → code → text` **等价**（G1 零回归；仅 `natural_order` 语义从"叶内序"升级为"全局序"，见 §6）。
- `base_sort=weight` 是默认，`primary=-weight`：**现状行为**。
- `base_sort=natural` 时 `primary=global_order`：无权重按文件序（G2）。

## 6. 数据模型改动

### 6.1 DAT 持久化全局顺序（G2 前提，修根因 A）

让"全局文件行序"穿过 DAT，替代当前的叶内序号：

| # | 位置 | 改动 |
|---|---|---|
| D1 | `datformat.rs:9` 格式布局 + `VERSION 2→3` | EntryRecord 10→14 字节，新增 `order: u32`（即启用真正的 natural_order 字段，废除"靠写入顺序天然表达"）。**已实现**：`open()` 校验 version 不符即 bail，调用方回退重建 |
| D2 | `datformat.rs:243` `WdatWriter::add` | 签名 `Vec<(String,i32)>` → `Vec<(String,i32,u32)>`；`build_entry_records`(:214) 写入 order |
| D3 | `datformat.rs:640` `read_leaf_entries` | 从盘读第 4 字段作 order，回调用**读盘 order** 而非 `i` |
| D4 | `codetable.rs:205` `export_to_wdat` | 传 `(e.text, e.weight, e.order as u32)`（`CodetableDict` 的 `order` 已是全局文件行序，现成，见 codetable.rs:44/95） |
| D5 | `manager.rs:1799` combined 合并路径 | 聚合保留 order；`weight_as_order=true` 的库把 weight 作为 order 处理 |
| D6 | 缓存 | VERSION+1 触发 `is_cache_valid`/`combined_cache_fresh` 重建（cached.rs:46 / manager.rs） |

> `Entry` 已有 `order` 字段、`Candidate` 已有 `natural_order`，Entry→Candidate 链路只需把 order 透传（当前已透传，只是 order 值本身是局部的——D1~D3 把它改成全局值即可，**下游排序代码不动**）。

### 6.2 词库基序 `group_rank`（G3，修根因 B 之一）

- `load_codetable_layers`（manager.rs:1593）注册每层时，按 `DictSpec` 在 `[[dictionaries]]` 中的顺序赋 `group_rank`（主库 0、扩展库 1,2…）。
- Entry→Candidate 时把层的 `group_rank` 写入候选（`Candidate` 新增 `group_rank: i32`，或复用 `meta`）。
- `better()` 把 `group_rank` 置于 `weight` **之前或之后**取决于配置（见 §7 的"基序优先级"开关）：
  - **强基序**（扩展恒在基础后）：`group_rank` 为最高位 → 满足 G3 字面语义。
  - **弱基序**（仅同权/无权时扩展在后）：`group_rank` 排在 `weight` 之后、`code` 之前。默认建议**弱基序**（避免扩展库高频词被死压到基础库所有词之后）。

### 6.3 中英文 / 字词分组 `category_rank`（G4）

- 在 `CodeTableEngine::convert` 组装候选后、`sort_by` 前，给每个候选计算 `category_rank`：
  - 判定维度：**脚本类**（中文 / 英文/ASCII，用 `text.chars().all(|c| c.is_ascii())` 或首字符判定）+ **长度类**（单字 `chars().count()==1` vs 多字词）。
  - 映射：按配置的分组顺序表（§7）把 `(script, is_word)` 映射为一个 rank 整数；未配置=全 0（不分组，G1）。
- `category_rank` 置于 `group_rank` 之后、`primary` 之前 → 组内再按基础排序/权重。

## 7. 配置（`[engine.codetable]` / `[[dictionaries]]`）

沿用并扩展现有字段，**全部默认值 = 现状行为**：

```toml
[engine.codetable]
base_sort = "weight"        # 现有：weight | natural（G1/G2）
# 新增：
sort_grouping = "none"      # none(默认) | script | word | script_word
                            #   script      = 中文组 / 英文组
                            #   word        = 单字组 / 多字词组
                            #   script_word = 两维复合
group_order = ["cn", "en"]  # 可选：显式指定各组先后（缺省用内置默认）
dict_base_order = "weak"    # none(默认,不启用词库基序) | weak | strong（G3）

[[dictionaries]]
id = "base"
default = true              # group_rank=0
[[dictionaries]]
id = "ext_names"
default_enabled = true      # group_rank=1（扩展库，dict_base_order 生效时排基础库后）
weight_as_order = true      # 该库无真实权重，weight 列即同码内序号
```

- `sort_grouping="none"` + `dict_base_order="none"` → `category_rank`/`group_rank` 恒 0 → 完全等价现状。
- 这些是**方案级引擎参数**（`[engine.codetable]`），与 frequency.md 的全局 `schema.codetable`（词频/造词等用户可配项）分属两层，互不干扰。

## 8. 与第二层词频（frequency.md）的关系

- 本文只定义**基础排序（第一层）**产出的候选序，即 frequency.md §3 排序键里的 `<base_sort: weight desc | natural asc>` 那一项。
- 第二层 `freq_rerank`（used-first 分区）与 `protect_top_n`（前 N 保护，frequency.md:100）**在本层之上运行，不受影响**：它们以本层输出为基底做重排/回填。
- `protect_top_n` 的"记录引擎基础序前 N"正是记录本层结果——本层更正确（无权重按文件序、扩展在后）后，保护语义自动更符合直觉。

## 9. 兼容性与迁移

- **零回归保证**：所有新维度默认 0/none；`base_sort=weight` 且不配分组/基序时，`sort_key` ≡ 现 `better()`。
- **DAT 版本迁移**：`VERSION 2→3`，`open()` 校验版本不符即 bail，调用方（cached/load_merged_dicts）回退重建（现有内容指纹校验机制），无需手动清缓存。
- **风险点**：`better()` 被多引擎共用（码表/混输/拼音经不同路径调用）。改 `better()` 需确认拼音/混输路径：拼音候选 `natural_order` 默认 0、`group_rank` 0，行为不变；建议新增维度以"追加 `.then()`"方式插入，避免动已有层级顺序。

## 10. 实施清单（按依赖排序）

**阶段一：DAT 全局序（G2，根因 A）**
1. `datformat.rs` D1~D3：EntryRecord +order u32、VERSION+1、Writer/Reader 收发 order。
2. `codetable.rs:205` D4：`export_to_wdat` 透传 `e.order`。
3. `manager.rs:1799` D5：combined 合并保留 order + `weight_as_order` 处理。
4. 验证：无权重词库前缀查询按文件序（新增单测，构造乱序字母、同权、跨编码）。

**阶段二：词库基序（G3，根因 B-1）**
5. `Candidate` 新增 `group_rank`；`load_codetable_layers` 注册时按 dict 顺序赋值并透传。
6. `better()` 按 `dict_base_order`（weak/strong）插入 `group_rank` 维度。
7. `schema.rs` 新增 `dict_base_order` 字段 + 默认值 + 覆盖接线。

**阶段三：中英文/字词分组（G4，根因 B-2）**
8. `convert()` 排序前计算 `category_rank`（script/word 判定）。
9. `better()`（或 convert 内专用比较器）插入 `category_rank` 维度。
10. `schema.rs` 新增 `sort_grouping` / `group_order` + 接线。

**阶段四：回归与文档**
11. 现有码表单测（engine.rs `tests`，含 `truncate_protects_low_weight_exact_match` 等）全绿。
12. 更新 `docs/redesign/frequency.md` §3 指针指向本文的第一层细化。

## 11. 待定/需决策

1. **`dict_base_order` 默认 weak 还是 strong？** 本文建议 **weak**（扩展库高频词不被基础库全部低频词死压）；strong 更符合"总在之后"字面，但可能把扩展库常用词压得很低。→ 需产品决策。
2. **`category_rank` 的中英文判定粒度**：按候选 `text` 首字符，还是全 ASCII？英文词库 `source=English` 时可直接用 `source` 分组，更稳。
3. **`group_rank` 是否进 DAT**：多库合并成 combined 时可把 group_rank 折进全局 order 的高位（`order = group_rank * BIG + file_order`），从而 live 层无需新增 `Candidate` 字段。这是"用一个全局 order 同时表达 G2+G3"的更省改动方案，与 `codetable.rs::merge` 的 `entry.order += base_order`（codetable.rs:185）思路一致——可作为阶段二的替代实现，实施时二选一。

## 12. 实施进展

### 阶段一：DAT 全局序（G2）— ✅ 已完成（分支 `feat/codetable-candidate-ordering`）
- `datformat.rs`：wdat `VERSION 2→3`，`ENTRY_SIZE 10→14`，EntryRecord 增 `order: u32`；`build_section`/`write` 收发 order；`read_leaf_entries` 读盘 order（替代叶内序号 `i`）；`WdatWriter` 新增 `add_with_order`（`add`/`add_abbrev` 内部按 code 内序号补 order，向后兼容旧调用方）；`open()` 校验 version 不符即 bail → 调用方回退重建（防旧 v2 缓存按 14B 步长误读）。
- `codetable.rs`：`export_to_wdat` 改用 `add_with_order`，透传 `CodetableEntry.order`（全局文件行序）。
- 回归测试 `datformat::tests::prefix_no_weight_sorts_by_global_order_not_code`：构造 order 与字母序相反、同权，断言前缀查询按出现序（Z,M,A）而非字母序（A,M,Z）。
- 验证：wind-dict 28+2、wind-engine 119+17 测试全绿；整 workspace 编译通过。

### 阶段二：词库基序（G3 扩展在基础后）— ✅ 基本已具备（既有机制 + 阶段一强化）
- **关键发现**：`CompositeDict::merge_search` 早已实现 `PER_LAYER_NO_OFFSET` 每层 natural_order 偏移（composite.rs），`load_codetable_layers` 主库置首、扩展库其后注册，故等权/无权时扩展恒在基础之后——需求②的核心已由既有层序机制满足（已有测试 `composite::tests::distinct_text_kept_and_layer_order_breaks_ties` 覆盖）。
- 阶段一把 `natural_order` 从"叶内序号"升级为"全局文件序（0..数百万）"后，`PER_LAYER_NO_OFFSET` 由 `1e7 → 1e8`，避免超大词库层内序溢出到相邻层带。
- 故**无需**新增 `Candidate.group_rank` 字段与 `dict_base_order` 配置（原阶段二计划取消）；当前实现等价于"弱基序"（层序在等权时生效，不硬压扩展库高频词）。若日后需要"强基序"（扩展恒在基础全部之后，无视权重），再引入显式配置。

### 阶段三：中英文/单字·词分组（G4）— ⏳ 待实现
- 仍需在 `convert()` 排序前计算 `category_rank` 并接入比较器 + `schema.rs` 配置（`sort_grouping`/`group_order`）。这是尚未落地的增量，待决策点 §11-②（判定粒度：`source=English` vs `text` 脚本判定）后实施。
