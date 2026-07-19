# 输入引擎架构：从词库到候选（现状整理）

> **现状文档**（非设计差分）。基于 2026-07-06 对 `wind_input/crates/` Rust 实现的实际代码核查整理，
> 覆盖码表 / 拼音（全拼）/ 双拼 / 混输 / 英文五类引擎从词库加载到候选呈现的完整链路，
> 并对比各模式流程差异。行号为核查时快照，随代码演进可能漂移，以函数/结构名为准。
>
> 历史设计差分见 [redesign/engine.md](../redesign/engine.md)（2026-06-15，其中记录的 Rust 现状已过时）。

---

## 1. 总览：统一入口与公共契约

### 1.1 分层结构

```
按键 (wind-coordinator)
  └─ build_candidates()                    handle_candidate.rs —— 候选后处理管线（§8）
       └─ EngineManager::convert()          wind-engine/manager.rs —— 按活跃方案分发（§1.3）
            └─ dyn Engine::convert()        五类引擎实现之一（§3–§7）
                 └─ DictManager / CompositeDict   wind-dict —— 多层词库合并查询（§2）
```

### 1.2 公共契约

**`Engine` trait**（`wind-engine/src/engine.rs`）：核心方法 `convert(input, max_candidates) -> ConvertResult`，
另有 `reset()` / `engine_type()` / `max_code_length()` / `handle_top_code()` / `recheck_auto_commit()` /
`set_dict_enabled()`，以及拼音探测类方法 `is_whole_syllable_pinyin()` / `completed_syllable_count()` 等
（混输的拼音否决依赖后者，§7.3）。

**`EngineType`**：`Pinyin`（全拼/双拼共用）/ `CodeTable` / `Mixed` / `English`。

**`ConvertResult`**（engine.rs:18-42）：

| 字段 | 含义 |
|---|---|
| `candidates` | 引擎排序后的候选列表 |
| `preedit_display` | 组合区显示串（拼音含 `'` 分隔；码表为原始码） |
| `preedit_pinyin` | 拼音音节拆分形态（混输「高亮跟随」按高亮候选类型选原始码/拆分串） |
| `completed_syllables` / `partial_syllable` / `has_partial` | 拼音音节完成度（UI 用） |
| `should_commit` / `commit_text` | 全码自动上屏意向（协调器复核后才放行） |
| `should_clear` | 满码空码清空缓冲 |
| `is_empty` | 无候选 |

**`Candidate`**（`wind-candidate/src/candidate.rs:28-126`）关键字段：

- `text` / `code` / `comment`（编码提示或「拼」来源标记）
- `weight`（引擎权重，排序主键）/ `natural_order`（同权重自然序，含词库层偏移）
- `source: CandidateSource`（`CodeTable` / `Pinyin` / `English` / `Phrase` / `None`）——贯穿词频记账、
  智能过滤分组、上屏守护的核心标记
- 分类标志：`is_phrase` / `is_command` / `is_group`（短语系）、`is_fuzzy`（模糊音）、
  `is_prefix`（前缀补全，code 比输入长）、`is_partial`（子短语，code 比输入短）、`is_common`（常用字表，过滤用）
- `consumed_length`：候选上屏时消费的输入字节数；`0` 表示整串。拼音分段上屏的基石（§4.5）
- `meta`：`lexicon_name` / `is_user_dict` / `is_temp_dict` / `raw_weight` / `freq_boost`

候选比较函数 `wind_candidate::better()`（candidate.rs:131-139）：
`weight desc → natural_order asc → code asc → consumed_length desc → text asc`。

### 1.3 EngineManager：懒加载与方案分发

`wind-engine/src/manager.rs`（~2570 行）：

- 持 `HashMap<方案ID → Arc<dyn Engine>>`，**懒加载**：`active_engine()`（:507）按需触发
  `ensure_loaded()`（:453），single-flight 构建锁保证并发下只构建一次。
- 统一入口 `convert()`（:991）分发当前活跃引擎；`convert_with(schema_id, ...)`（:1177）指定方案转换
  （混输分段上屏、临时拼音用）。
- `switch_schema()` / `cycle_schema()` / `reload_from_config()`（配置热重载清缓存重建）。
- 引擎构建 `build_engine()`（:1272-1530）按方案 TOML 的 `[engine] type` 分流：

```toml
[schema]
id = "wubi86_pinyin"
[engine]
type = "mixed"              # codetable | pinyin | mixed | english
[engine.pinyin]
scheme = "shuangpin"        # 双拼方案的判定方式（quanpin/shuangpin）
[engine.pinyin.shuangpin]
layout = "xiaohe"           # 双拼布局（data/schemas/shuangpin/*.toml）
[engine.mixed]
primary_schema = "wubi86"   # 混输主（码表）
secondary_schema = "pinyin" # 混输次（拼音）
[[dictionaries]]
path = "dicts/xxx.dict.yaml"
default = true              # 主库标志；非 default 为扩展库
```

- 词频写分流 `write_data_schema_id()`（:734-745）：混输方案下按候选 `source` 路由——
  码表候选记入主码表方案 id，拼音候选统一折叠到 `"pinyin"`，其余跳过记频。
- 拼音候选编码提示 `codetable_reverse_hint()`（:332-353）：按主码表懒建反查索引，
  拼音候选 comment 填实际码表编码（保证与码表真实码一致，而非按字生成）。

---

## 2. 词库层（wind-dict）

### 2.1 三种存储格式

| 格式 | 文件 | 说明 |
|---|---|---|
| YAML 源 | `.dict.yaml` | RIME 风格 TSV：五笔 `code\ttext\tweight`，拼音 `text\tcode\tweight`。列序按**文件级**判定——头部 `columns:` 声明优先，无声明则整文件投票探测、默认 `text` 在前（`codetable.rs:resolve_columns`）。详见 [rime-dict-loading.md](./rime-dict-loading.md) |
| 二进制 | `.wdb` | Header + KeyIndex + DataSection + StringPool；V3 条目含 order 字段（`binformat.rs`） |
| 双数组 Trie | `.wdat` | Header + Base/Check 数组 + LeafTable + EntryRecords + StringPool + 可选简拼段（AbbrevSection）+ CharMap（`datformat.rs`） |

`.wdat` 支持**零拷贝 mmap 读取**（`WdatReader`）：精确查询 walk 编码验终止符；前缀查询 walk 前缀后
DFS 子树重建完整编码、按权重降序截断；`for_each_entry` 流式遍历（反查索引构建用）。
DAT 从已排序编码列表 BFS 直接构建，峰值内存仅 base/check 两数组。

### 2.2 缓存策略（CachedDict）

`cached.rs`：`enum CachedDict { Mmap(WdatReader), Memory(CodetableDict) }`。
加载时若 `.wdat` 缓存存在且**内容指纹**（sidecar，非 mtime）匹配源文件 → 直接 mmap；
否则加载 yaml → 写 `.wdat` → mmap 重开。缓存根 `%LOCALAPPDATA%\WindInput\cache\{方案}/`。
拼音另有合并缓存：`merged.wdb`（主库+import_tables）与 unigram 的 `.wdb` 缓存（manager.rs:1538+）。

### 2.3 多层合并（CompositeDict）

`DictManager`（manager.rs:15-52）持一个方案的 `CompositeDict`（composite.rs），层类型优先级
（layer.rs）：**Logic(0) > User(1) > Temp(2) > Cell(3) > System(4)**。

- 系统层 `SystemDictLayer` 包装 CachedDict 并打 `source` 标记；用户造词 `StoreUserLayer`（redb 持久化）、
  临时学习词 `StoreTempLayer`（会话级）来自 wind-store。
- 合并查询：遍历启用层收集候选 → **按 text 去重**（weight 继承最高值；前缀查询同 text 多码取最短码）→
  每层 natural_order 叠加 `layer_idx × PER_LAYER_NO_OFFSET(10M)` 保证层序 → `better()` 排序 → 截断。
- 层可热插拔：`set_layer_enabled()`（码表扩展库 `codetable-extra-*` 开关走此通道）。

---

## 3. 码表引擎（CodeTableEngine）

文件：`wind-engine/src/codetable/engine.rs`（~440 行）。

### 3.1 查询流程（`convert()`，:98-174）

```
输入码
 ├─ ① 精确匹配   dm.search(input)          → source=CodeTable
 ├─ ② 前缀匹配   dm.search_prefix(input)    仅 !single_code_input 时；按 text 与①去重
 └─ ③ 空码补全   search_prefix(input, 8) 取首个 code≠input 的候选
                  仅 single_code_complete 且①②为空且未满码时
 → better() 排序 → truncate（截断保护精确匹配，见下）
 → show_code_hint 时前缀候选 comment 标注剩余编码
 → 自动上屏判定 / 满码空码清空（has_longer_code 单次求值复用）
```

**截断保护精确匹配**：短输入（如单字母）前缀候选可达数百，纯按权重 `truncate` 会把低权重的精确
全码（五笔一/二级简码等 `code==input`）挤出配额丢失。超额时改为「精确优先」稳定分区截断——精确
候选必留、其余按 `better` 序填满剩余配额——再恢复 `better` 显示序。**不持久化 `is_prefix`**：跨来源
权重档位（混输拼音 ÷100 等）与纯码表显示序均不受影响。

### 3.2 上屏策略（CommitOptions，:14-31）

| 选项 | 行为 |
|---|---|
| `auto_commit_at_full` + `auto_commit_min_len` | 全码自动上屏：码长 ≥ min_len 且**恰一个精确匹配**且**无更长后继**（`decide_auto_commit()` :70；后继判定 `has_longer_code()` :54 用 `search_prefix(input, 64)` 查是否存在更长码） |
| `clear_on_empty_max` | 满码空码清空：无候选且码长 ≥ max_code_length 且无更长后继 → `should_clear` |
| `top_code_commit` | 顶码：见 `handle_top_code()`（:206）——输入**超过** max_code_length 且整串无精确匹配、无更长后继 → 取前 N 码 convert 首选上屏，余码返回续打 |
| `single_code_input` | 精确模式：禁前缀匹配 |
| `single_code_complete` | 精确模式下的空码补全 |
| `show_code_hint` | 前缀候选标注剩余编码 |

配置来源：全局 `schema.codetable.*` + 方案 `[engine.codetable]` 行为字段逐字段折叠（`Some` 覆盖 /
`None` 回落全局）。行为与引擎固定参数**同段同结构**收在 `CodeTableSpec`（`wind-config/src/schema.rs`）：
固定参数 `max_code_length` / `base_sort`（weight/natural）/ `input_chars`，行为参数为 tri-state `Option`。
方案作者可在 `.schema.toml` 内联行为基线；`schema_overrides/{id}.toml` 用**相同的 `[engine.codetable]` 段**
（设置页写入）经 `read_schema` 深合并覆盖之。已无独立 `CodetableOverride` 平行路径。

### 3.3 显示态复评（`recheck_auto_commit`）

引擎按**未过滤**候选判唯一（生僻同码字会导致不唯一而否决）；协调器智能过滤掉生僻字后若显示列表只剩
唯一精确全码 → 据显示候选复评放行（§8 第⑧步）。上屏判定与用户所见保持一致。

---

## 4. 拼音引擎（PinyinEngine，全拼）

文件：`wind-engine/src/pinyin/`。词库：`rime_pinyin` 主库合并 import_tables 缓存为 `merged.wdb` mmap；
语言模型 unigram（`lm.rs`，`UnigramLookup` trait：`log_prob()` / `char_based_score()` /
`boost_user_freq()`）从 `unigram.txt` 加载并缓存为 mmap `.wdb`。用户/临时造词层经
`with_store_layers()` 注入。

### 4.1 音节切分

- `SyllableTrie`（syllable.rs）：~417 个标准音节的字节级 Trie，`match_at()` 返回某位置全部可能音节。
- `Dag::maximum_match()`（dag.rs）：DP 求**覆盖最多字符**的音节切分（非贪心，如
  `henihejiele → he+ni+he+jie+le`）。
- 分隔符 `'`：硬边界。`segment_with_separators()` 按 `'` 分段各段独立切分；
  `map_consumed_over_separators()` 把 consumed_length 补偿回原始输入空间（mod.rs:325-343）。
- 模糊音（fuzzy.rs）：`FuzzyConfig` 11 个开关（zh_z/ch_c/sh_s/n_l/f_h/r_l + an_ang/en_eng/in_ing/
  ian_iang/uan_uang），`lookup_with_fuzzy()`（mod.rs:221）对各音节变体做笛卡尔积扩展查询
  （组合数 > 64 跳过），命中标 `is_fuzzy`。

### 4.2 六步候选生成（`convert()`，mod.rs:371-700）

| 步骤 | 内容 | 标志 |
|---|---|---|
| ① 精确查找 | `lookup_with_fuzzy(completed)`——以**完成音节前缀**（去尾部残码）为查询码与存储 code | — |
| ② Viterbi 整句 | `use_smart_compose` 且 ≥2 音节：LatticeBuilder 建词图（max_word_len=6，模糊变体 -0.5 惩罚）→ ViterbiDecoder DP 最优路径；权重 = `SENTENCE_WEIGHT_BASE(30M) + clamp(log_prob×1000)`，置顶 | 整句候选 insert(0) |
| ③ DAG 子短语 | 前 6 音节的各前缀子段查词（分段上屏候选） | `is_partial` |
| ④ 前缀补全 | `search_prefix(query, 30)` | `is_prefix` |
| ⑤ 简拼 | `AbbrevMatcher` 判定（每字母为音节首字母且非完整音节序列）→ `search_abbrev(query, 10)` | natural_order=999999 沉底 |
| ⑥ 用户/临时造词层 | store_layers 整串精确 + 子码 + 前缀，按 text 与系统词典去重 | — |

节点打分（lattice.rs `score_node()`）：unigram log_prob 为基础，叠加单字实词惩罚(-3.0)/虚词加成(+2.0)/
多字词典词加成(+3.0×√字数×freq_factor)/OOV 字符均值(-2.0) 等调整。

> **尾部残码前缀补全上浮**：输入尾带未完成音节时（`meiy` 的 `y`），step ④ 前缀补全产出的候选
> （如 `meiyou→没有`）**不标 `is_prefix=true`**。若标了会被引擎排序与协调器 `is_prefix asc` 重排
> 双重压到数百条 step ① 精确子串（`is_prefix=false`，没/每/美/…）之后，用户翻 15+ 页才见。
> 不标时 `push_unique` 自动判 `is_partial=false`（code 长于 query），`is_partial asc` 将补全候选
> 自然浮到 `is_partial=true` 的精确子串之上。无残码（`meiyou`）保持原行为。
>
> Viterbi 更新既有条目时同时清除 `is_partial`（整句是完整解读而非子短语），否则 30M 置顶但仍挂
> `is_partial=true` 会被残码补全 `is_partial=false` 反超。

### 4.3 分隔符边界过滤

候选 code 恰落在音节边界时，候选字数必须与所跨音节数一致（mod.rs:624-634）：
`xi'an` 强制 [xi,an] 后，单字「先」(xian, 1 字跨 2 音节) 被剔除；前缀补全不受影响。

### 4.4 排序层级（mod.rs:636-651）

`is_fuzzy asc（非模糊优先）→ is_prefix asc（完整/子短语优先于补全）→ is_partial asc（完整优先于子短语）
→ weight desc → natural_order asc`，再截断。

**裸声母单字优先**：打单个声母（`m`/`n`/`h`/`zh` 等，`syllables` 为空、无完整音节）时候选全为前缀
补全词，纯按词频排会让高频多字词（没有/目前）压过单字（吗/么），不合主流输入法直觉。故裸声母时
给**单字候选**加 `BARE_INITIAL_SINGLE_CHAR_BOOST`(1e7)——高于常规词频、低于整句底线
`PINYIN_SENTENCE_FLOOR`(2e7，不被 freq_rerank 误锚定）。**经 weight 表达**（非引擎排序），才能穿过
协调器 `build_candidates` 按 `(is_fuzzy, is_prefix, weight)` 的重排。仅裸声母生效——完整音节输入的
单字已靠 `is_prefix` 精确层级就位（`nihao` 仍 `你好` 优先）。

### 4.5 consumed_length（分段上屏）

code 是 query 前缀 → 只消费前缀长度，剩余拼音继续转换；否则消费整串。
双拼激活时经 `sp_result.map_consumed_length()` 回算双拼键数；含 `'` 时经分隔符补偿（mod.rs:653-672）。

### 4.6 造词反推读音

`generate_word_pinyin()`（generate.rs）三级策略：整词读音笛卡尔积回查命中 → 子词 DP 切分继承读音
（解决长词多音字）→ 逐字代表读音兜底。单字读音索引 `CharPinyinIndex` 从词典自身派生，懒构建。

---

## 5. 双拼（Shuangpin）

文件：`pinyin/shuangpin.rs`（~930 行）。**不是独立引擎**——双拼是拼音引擎的前置转换层：
方案判定 `schema.engine.pinyin.scheme == "shuangpin"`（manager.rs:251-273），构建时
`PinyinEngine::with_shuangpin(converter)` 注入，`convert()` 入口先把双拼键串转全拼，后续管线与全拼完全一致。

- **布局是数据不是代码**：`data/schemas/shuangpin/<id>.toml`（内置 xiaohe / ziranma / mspy / sogou /
  ziguang / abc），三表：`[initials]` 键→声母、`[finals]` 键→韵母列表、`[zero_initials]` 键→零声母音节表。
- 键对转换 `convert_pair()`（:238-305）三层：零声母（韵母交集/字面/matchesFinal）→ 常规声母+韵母
  （含 z↔zh/c↔ch/s↔sh 对偶兜底 `fuzzy_initial_partners()`）→ 重复键单音节（aa→a）。
- 奇数尾键作 partial 声母前缀（has_partial）。
- **位置映射**：每个转出的全拼字节记录双拼原始区间（`ConvertedSyllable{sp_start..sp_end, fp_start..fp_end}`），
  `map_consumed_length()` 使分段上屏语义在双拼键空间成立。
- preedit：双拼激活时组合区显示**原始按键**（按音节边界 `'` 分隔，`build_raw_preedit()`）；
  且剥除手动分隔符（`'` 仅全拼方案支持）。
- 选词热键避让：manager 缓存双拼韵母键集 `shuangpin_final_key()`（manager.rs:278-301）。

---

## 6. 英文引擎（EnglishEngine)

文件：`wind-engine/src/english.rs`（64 行）。**码表引擎的薄包装**：词库用码表格式
（`type = "english"` 方案），构建时 code 列**小写化**实现大小写不敏感前缀匹配（manager.rs:1375-1400）；
查询走精确 + 前缀，候选标 `source = English`。独立方案可直接使用，更常见的是被混输懒加载
（`schema.mix.enable_english`）。

---

## 7. 混输引擎（MixedEngine）—— 冲突处理与拼音否决

文件：`wind-engine/src/mixed/engine.rs`（~1030 行）。部件：`primary`（码表）+ `secondary`（拼音，可空）+
`english`（可空），策略参数经 `MixConfig` 注入（manager.rs:1286-1369 从 `schema.mix.*` 构造）。

### 7.1 输入路由（两条路径）

```
convert(input):
  input_len > max_code_len ──→ convert_overflow()（超长分支，§7.2）
  否则 ──→ 常规合并路径：
     码表全量查询（保存 should_commit 意向）
     + 拼音查询（仅 input_len ≥ min_pinyin_length，默认 2；短输入自然退化为纯码表）
     + 英文查询（enable_english 且 input_len ≥ min_english_length）
     → 加权 → 合并排序去重 → 上屏重评
```

### 7.2 冲突处理 ①：权重档位（双向夹击）

不同来源候选靠**档位加权**隔离（engine.rs:15-25, 176-196）：

| 档位（高→低） | 加权 |
|---|---|
| 码表精确（code==input） | `+codetable_weight_boost`（默认 1e7） |
| 短语 | `+PHRASE_WEIGHT_BOOST`（1M） |
| 英文精确（整词） | `+ENGLISH_EXACT_BOOST`（500K）※ |
| 码表前缀补全 | `+PARTIAL_MATCH_BOOST`（500K） |
| 英文前缀 | `+0`（保留词库原始权重，防短前缀刷屏） |
| 拼音 | `÷PINYIN_TIER_SCALE`（÷100，负数归 0）→ 压入 0~100K 低档 |

※ `english_candidates()` 的加权以常量 `ENGLISH_EXACT_BOOST`/`ENGLISH_PREFIX_BOOST` 为准（engine.rs:21-25）。

合并 `merge_sort_dedup()`：码表在前、拼音在后、英文混入 → `weight desc, natural_order asc` 稳定排序 →
**按 text 去重（HashSet 保留首个）** → 截断。

### 7.3 冲突处理 ②：拼音否决（veto）—— 统一入口

**统一判据 `pinyin_vetoes_commit(input, has_pinyin)`**（engine.rs:170-172），满码/顶码/显示态复评三条
上屏通路**共用同一套**（提交 847ca08 统一），杜绝「满码不否决、顶码却否决」的不一致：

```rust
(auto_commit_block_on_pinyin && has_pinyin) || is_ambiguous_pinyin_word(input)
```

**① 粗粒度守护 `auto_commit_block_on_pinyin`**：只要整串存在拼音候选就否决码表上屏。
**默认关**（代码默认与 `data/config.toml` 一致均为 false）——粗粒度一票否决太激进，实际生效的主力是②。

**② 拼音词拦截 `is_ambiguous_pinyin_word()`**（engine.rs:134-163，`block_commit_on_pinyin_word`
控制，默认**开**），命中任一即判「用户意图是拼音」：

- **(b) 单音节前缀（中途态）**：前 max_code_len 码的前缀**恰是 1 个完整拼音音节**
  （`is_whole_syllable_pinyin(prefix) && completed_syllable_count(prefix)==1`）→
  用户多半正在打拼音词中途（wangb→wangba→网吧），拦。
  这是区分 `wang`（1 音节，拦）与 `aipu`（ai+pu 2 音节，多为恰好像拼音的五笔码，放行「落实」）的关键。
- **(a) 整串强拼音词**：整串是完整音节序列，且拼音引擎首选是「≥2 汉字、消费整串
  （consumed_length==0 或 ≥ 整串）、weight ≥ `pinyin_word_min_weight`」的真实词——
  借拼音引擎自身排序识别强词（wangba→网吧 拦）。

**三条通路的接线**：

| 通路 | 位置 | 否决方式 |
|---|---|---|
| 满码自动上屏 | `convert()` engine.rs:441-450 | 取码表意向后：`!has_english && !pinyin_vetoes_commit(...)` 且上屏目标在合并结果中**存活**才放行；否决短路求值（仅码表确有意向时才跑拼音转换） |
| 顶码上屏 | `handle_top_code()` engine.rs:512-531 | 超码长时先查整串拼音得 has_pinyin，`pinyin_vetoes_commit` 命中 → 返回 None（放弃顶码继续组合）；`top_code_override_pinyin=true` 时**无视否决**强制倒向码表 |
| 显示态复评 | `recheck_auto_commit()` engine.rs:485-502 | 先按显示候选来源算 has_pinyin/has_english 走同一套否决（复评**不绕过**否决），再仅取 `source==CodeTable` 的候选委托主码表判唯一 |

配套：**英文守护** `auto_commit_block_on_english`（默认关）——满码上屏时合并结果存在英文候选则否决
（保护正在输入更长英文词的用户）。
**空码清空**：仅当主码表请求清空**且无拼音候选**（合法拼音序列留给拼音，不清空，engine.rs:453）。

### 7.4 超长分支（`convert_overflow()`，engine.rs:265-349）

`input_len > max_code_len` 时按 `pinyin_only_overflow` 分流（config.toml 默认 true）：

- **true（默认）**：仅查拼音 + 英文。长码特例：整串在码表有精确匹配或更长后继
  （`has_full_input_match || has_longer_code`）→ 追加码表候选并把拼音归一化降档。
- **false**：码表取前 N 码（+ 长码特例的整串候选）+ 拼音整串，统一加权混合竞争。

### 7.5 顶码/满码上屏与显示一致（协调器侧配合）

引擎层否决之外，协调器保证「**上屏即所见、非码表来源不上屏**」：

- **满码**（handle_candidate.rs:322-334）：自动上屏文本取**实际显示的首候选**（与空格/点选同源），
  且**仅当显示首选 source==CodeTable** 才上屏——若首选被 shadow 置顶为拼音、或码表精确字被智能过滤后
  仅剩拼音，则放弃自动上屏留给用户选。
- **顶码**（coordinator.rs:3413-3453）：字母键入前先记住「即将成为前缀」的缓冲及其**显示首选**
  （已过滤/重排/shadow 的用户所见），仅当其为码表来源时作为顶码文本；显示首选非码表 → 放弃顶码继续组合。
  多级溢出（前缀≠顶码前缓冲的罕见场景）才回退引擎顶码文本。
  背景：调频置顶/shadow 发生在协调器层，引擎 `handle_top_code` 内部 convert 看不到，会顶出权重首选
  而非显示首选。

### 7.6 混输其它

- **来源提示**：`show_source_hint`（默认关）给拼音候选 comment 加「拼」前缀（`add_source_hints()`）。
- **preedit**：拼音解析出 ≥2 完成音节时组合区用音节分隔串（ni'hao），否则原始码（单音节/纯五笔码不拆）。
- **分段上屏接力**（handle_candidate.rs:182-202）：`committed_text` 非空（必来自拼音选词——码表候选
  consumed_length=0 永不部分匹配）时，剩余编码**强制**按混输方案的 `[engine.mixed].secondary_schema`
  转换（`convert_with`），避免混输让码表抢首选（选「你」后 hao→虚）。注意不用全局 primary_pinyin
  （那是临时拼音↔临时双拼切换用的）。
- **临时拼音**：`[input.temp_pinyin]`（总开关 + 引导键，默认反引号）由协调器 pipeline 层分发，临时切到
  目标拼音方案，不在 MixedEngine 内。目标方案取全局 `schema.primary_pinyin`（空=全拼 `"pinyin"`），
  见 `temp_pinyin_target`。快捷输入 `;` 由内置 mix「快捷」融合方案接管（quick_input 作为成员）。
- **词库热插拔**：`set_dict_enabled` 转发主/次子引擎（扩展码表层在码表子引擎）。

---

## 8. 候选后处理管线（协调器层，所有模式统一）

`build_candidates()`（`wind-coordinator/src/handle_candidate.rs:177-340`），引擎返回后依次：

```
① engine convert（初始 limit 按引擎类型/码长阶梯，:160-171：码表 100/300/1000，拼音/混输 300）
② 短语注入（wind-phrase lookup + lookup_prefix）：
     静态/模板短语、$CC 命令（is_command）、$SS/$AA 组（is_group 二级展开）
     weight = PHRASE_WEIGHT_BASE + hit.weight
③ 层级排序：is_fuzzy asc → **is_partial asc** → is_prefix asc → weight desc → natural_order asc
     （Fuzzy＜子短语＜前缀补全＜完整匹配：与 PinyinEngine 内部排序一致。缺 `is_partial` 时混输 ÷100
     压缩权重后，高权重子串单字会靠 weight 反超低权重精确词组——如 `pingtan` 在混输下
     平(w=58 part=true)＞平摊(w=4 part=false)，前者插到词组前）
④ 按 text 去重（HashSet 保留首个）
⑤ apply_filter：填充 is_common（常用字表；短语豁免）→ wind_candidate::filter_candidates
⑥ apply_freq_rerank：用户词频重排（独立维度，绝不改 weight）
⑦ apply_shadow：shadow 规则删除过滤 + 置顶/移动重排（优先级最高，排序后应用）
⑧ 自动上屏复评与守护：
     引擎意向 or recheck_auto_commit（显示态复评，惰性）
     → 目标须在最终候选中存活（未被 shadow 删除）
     → 显示首选须 source==CodeTable 才 AutoCommit（§7.5）
```

### 8.1 智能过滤：按 (source, code) 分组

`wind-candidate/src/filter.rs`。`FilterMode`：`Gb18030`（不过滤）/ `General`（仅常用）/
`Smart`（智能）。Smart 规则：**按 `(CandidateSource, code)` 分组**，同组内存在常用词
（is_common/is_phrase/is_command/is_group）则滤掉非常用，无常用则整组保留。
按来源分组是提交 19d580f 的修复：混输下码表与拼音候选常共用同一 code 串（如 wang），
原先只按 code 分组会让拼音常用字误杀同码的码表生僻字（佢），导致混输码表表现与纯五笔不一致。

### 8.2 词频重排（freq_rerank.rs，两策略）

不修改 weight，是排序后的独立重排维度（词频数据在 wind-store，按 §1.3 的 schema id 分键空间；
仅 `consumed_length==0` 或 ≥ 输入长的候选参与取频）：

- **码表/混输：永久 used-first**（`rerank_codetable_usedfirst()` :46-91）。
  档位感知（`freq_tier()` :26-38）：0=码表精确全码、1=短语、2=码表前缀等、3=拼音/英文；
  **同档内** used-first（策略 `Top`=MRU / `Step`=按 count），**跨档永不反超**；
  重排前记录前 N 位、重排后回填的 `protect_top_n` 呈现保护。
- **拼音：衰减软置前**（`rerank_pinyin_decay()` :99-170）。
  整句豁免（weight ≥ PINYIN_SENTENCE_FLOOR 20M 恒顶）；层级保护（模糊<精确、补全<精确、
  子短语<完整，词频不得跨层反超）；衰减分 < 阈值则褪色失去置前资格（半衰期等参数
  `schema.pinyin.frequency.*`）。

### 8.3 Shadow 规则

`wind-candidate/src/shadow.rs`：用户「删词/置顶/移动」规则，删除过滤 + 按目标位置重排，
在过滤与词频之后应用（最高优先级）；自动上屏目标被 shadow 删除则不放行（⑧步存活复核）。

---

## 9. 各模式流程对比

| 阶段 | 码表 | 拼音（全拼） | 双拼 | 混输 | 英文 |
|---|---|---|---|---|---|
| 词库 | codetable 多层（主+扩展+用户+临时） | rime_pinyin merged.wdb + unigram LM + 用户/临时层 | 同拼音 | 主码表全套 + 拼音全套 + 可选英文 | 码表格式，code 小写化 |
| 输入预处理 | 无（原始码） | `'` 分段 + DAG 音节切分 + 模糊音扩展 | **先双拼→全拼**（Layout 表 + 位置映射），后同全拼 | 双路各自原样进子引擎 | 小写化 |
| 候选生成 | 精确 + 前缀 + 空码补全 | 六步：精确/Viterbi 整句/子短语/前缀/简拼/用户层 | 同全拼 | 码表全流程 + 拼音全流程 + 英文，档位加权合并 | 精确 + 前缀 |
| 引擎内排序 | better()（weight 主导） | 层级（模糊/前缀/子短语）→ weight | 同全拼 | 档位 weight（码表 1e7 >> 短语 1M >> 英文/前缀 500K >> 拼音 ÷100） | weight |
| 自动上屏 | 满码唯一精确且无更长后继 | 无 | 无 | 码表意向 + **拼音否决①② + 英文守护 + 存活复核 + 显示首选须码表** | 无 |
| 顶码 | 超满码顶前 N 码首选，余码续打 | 无 | 无 | 同否决①②后委托码表；`top_code_override_pinyin` 可强制 | 无 |
| 分段上屏 | 无（consumed_length=0） | consumed_length 前缀消费，余码续转 | 同全拼（映射回双拼键数） | 拼音候选支持；接力强制走 secondary_schema | 无 |
| 空码行为 | 满码空码清空（可配） | 不清空 | 不清空 | 码表请求清空且**无拼音候选**才清 | — |
| 词频重排 | used-first 永久档位 | 衰减软置前 | 衰减软置前 | 按候选 source 分流两策略 | 归入档位 3 |
| preedit | 原始码 | 音节 `'` 分隔 | 原始按键按音节分隔 | ≥2 音节用拼音拆分串，否则原始码；高亮跟随可切换 | 原始输入 |

后处理管线（短语/过滤/词频/shadow/复评，§8）对所有模式统一，由协调器执行。

---

## 10. 配置速查（实际生效默认值）

引擎相关配置三层合并：代码默认 → 系统 `data/config.toml` → 用户 `%APPDATA%\WindInput\config.toml`。
下表「默认」为**系统配置层实际值**（与代码默认不同处已注明）：

| 键 | 默认 | 说明 |
|---|---|---|
| `schema.mix.auto_commit_block_on_pinyin` | false（代码默认与 config.toml 一致） | 否决① 粗粒度：有拼音候选即否决 |
| `schema.mix.block_commit_on_pinyin_word` | true | 否决② 拼音词拦截（实际主力） |
| `schema.mix.pinyin_word_min_weight` | 0 | 0=仅结构判据（≥2 汉字消费整串） |
| `schema.mix.top_code_override_pinyin` | false | 顶码优先，无视拼音否决 |
| `schema.mix.pinyin_only_overflow` | true | 超码长仅拼音+英文 |
| `schema.mix.min_pinyin_length` | 2 | 拼音最小触发长 |
| `schema.mix.enable_english` / `min_english_length` / `auto_commit_block_on_english` | false / 3 / false | 英文混入及守护 |
| `schema.mix.show_source_hint` | false | 拼音候选「拼」标记 |
| `schema.codetable.*`（auto_commit_at_full / auto_commit_min_len / clear_on_empty_max / top_code_commit / show_code_hint / single_code_input / single_code_complete 等） | 见 config.toml | 可被 `schema_overrides/{id}.toml [codetable]` 按方案覆盖 |
| `schema.pinyin.use_smart_compose` | — | Viterbi 整句开关 |
| `schema.pinyin.fuzzy.*` | — | 模糊音 11 对开关 |
| `schema.pinyin.frequency.*`（half_life / base_scale / recency_peak） | — | 拼音词频衰减参数 |
| `schema.pinyin.auto_learn.*` | — | 自动造词 |
| `[engine.pinyin] scheme` / `[engine.pinyin.shuangpin] layout` | quanpin / — | 双拼判定与布局 |
| `[input.temp_pinyin]` enabled/schema/trigger_keys | true / pinyin / backtick | 临时拼音引导 |

---

## 11. 关键源文件索引

| 模块 | 文件 |
|---|---|
| 统一入口/构建/热重载 | `wind-engine/src/manager.rs` |
| Engine trait / ConvertResult | `wind-engine/src/engine.rs` |
| 码表引擎 | `wind-engine/src/codetable/engine.rs` |
| 拼音引擎 | `wind-engine/src/pinyin/mod.rs`（+ syllable/dag/fuzzy/lattice/viterbi/lm/scorer/generate） |
| 双拼转换 | `wind-engine/src/pinyin/shuangpin.rs`；布局 `data/schemas/shuangpin/*.toml` |
| 混输引擎（否决①②/档位/overflow） | `wind-engine/src/mixed/engine.rs` |
| 英文引擎 | `wind-engine/src/english.rs` |
| 词频重排 | `wind-engine/src/freq_rerank.rs` |
| Candidate / 智能过滤 / shadow | `wind-candidate/src/{candidate,filter,shadow}.rs` |
| 词库（格式/缓存/多层） | `wind-dict/src/{codetable,binformat,datformat,cached,composite,manager,layer,store_layer}.rs` |
| 后处理管线 / 顶码守护 | `wind-coordinator/src/handle_candidate.rs`、`coordinator.rs`（VK_A..Z 顶码段） |
| 配置结构与注册 | `wind-config/src/{config,schema,config_schema}.rs`、`data/config.toml` |
