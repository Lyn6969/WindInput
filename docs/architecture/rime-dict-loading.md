# Rime 词库（`.dict.yaml`）格式契约与加载链路

> **现状文档**。基于 2026-07-19 对 `wind-dict` / `wind-engine` 实际代码的核查整理，
> 并对照 rime/librime 源码逐条查证格式语义。行号为核查时快照，以函数/结构名为准。
>
> 候选生成与排序见 [engine-candidate-pipeline.md](./engine-candidate-pipeline.md)（本文是其 §2 的展开）。

**术语**：本项目的**方案**文件是 `*.schema.toml`（TOML，不是 rime 的 `schema.yaml`）；
**词库**文件才是 rime 的 `*.dict.yaml`。本文只讲词库。

---

## 1. librime 格式契约（权威事实）

以下均查证自 `rime/librime` 源码，非推测。**任何"对齐 Rime"的改动应以本节为准。**

### 1.1 列定义 `columns:`

`src/rime/dict/dict_settings.cc` 的 `GetColumnIndex`：

| 情形 | librime 行为 |
|---|---|
| **未声明 `columns:`** | 默认 text=0, code=1, weight=2；**stem 返回 -1（不可用）** |
| 已声明 | 纯粹的「列名 → 下标」查表，无校验、无固定集合 |
| 支持的列名 | 恰好四个：`text` / `code` / `weight` / `stem` |
| 未知列名 | 惰性：**占一个下标位、顺延其后各列，但永不被读** |
| **必需列** | **只有 `text`**。缺 code/weight/stem 只是跳过该字段 |
| 声明了 columns 却漏写 text | **整个文件被静默丢弃**（仅一条 error log） |
| 多余列 | 静默忽略。逐字段做 `num_columns > x_column` 越界保护，**不是整行门槛** |

> **无 `code` 列是合法形态**：这类词库交给 encoder，按方案字表 + `encoder.rules`
> 从构成字反推编码（「自动编码/自动注音」）。**本项目未实现该特性**，见 §6 R1。

### 1.2 权重语义

`src/rime/dict/entry_collector.cc`：

- 裸数字（`87`）→ **绝对权重**，原样使用
- **`%` 后缀（`50%`）→ 按预设词库（essay）中该词权重的百分比缩放**，需 `use_preset_vocabulary`
- 空 → 取预设词库权重，无则 0
- 解析失败 → warn + 0，**不中止**

单位是任意原始计数，librime 后续会归一化，故绝对刻度无意义，只有库内比例有意义。

### 1.3 注释与正文

- 判据是 `line[0] == '#'`，**单字符**。`## xxx` 与 `# xxx` 同路跳过
- **`##` 在 librime 中无任何特殊语义**
- 唯一特例：**整行恰好等于 `# no comment`** → 关闭后续注释处理，此后 `#` 开头的行按**数据**解析
  （给需要 `#` 作编码或词条的词库用）。全行精确匹配，非前缀
- YAML 头部读到**独占一行的 `...`** 为止；librime 要求 `name` 与 `version` 非空

### 1.4 不属于 Rime 的第三方约定

`##` 分组名与 `dict_grouped: true` 来自第三方 Electron 词库编辑器
**[KyleBing/wubi-dict-editor](https://github.com/KyleBing/wubi-dict-editor)**
（README 明载「成组显示 组为以 `##` 开头」）。本仓 wubi86 词库的相关注释来自上游
`KyleBing/rime-wubi86-jidian`。

该设计是**故意安全**的：librime 丢弃所有 `#` 行，编辑器才能把分组结构藏在注释里而两不相扰；
`dict_grouped` 同为 librime 从不查询的未知键。**若要支持分组，那是新增我方语义，不是"对齐 Rime"。**

（雾凇拼音 rime-ice 用 `##### 容错词`，也只是普通注释，不是同一套约定。）

---

## 2. 我们的实现与 librime 的差异

| 语义 | librime | WindInput | 差异后果 |
|---|---|---|---|
| 无 `columns:` 时列序 | 恒 `[text, code, weight]` | **投票探测**列序，权重仍取第 3 列 | 见 §3.3；兜底默认与 librime 一致 |
| 必需列 | 仅 `text` | **text 与 code 都必需** | 缺任一 → 整库跳过并 ERROR（不静默降级），见 §6 R1 |
| `stem` 列 | 用于造词/词素反查 | 占位顺延下标，不取用 | 无对应特性，无后果 |
| weight `%` 后缀 | 按预设词库缩放 | 解析失败 → 0，并计入 `ParseStats.bad_weight` 汇总告警 | 见 §6 R5 |
| `use_preset_vocabulary` | essay 预设词库 | 未实现 | `%` 无基准可缩放 |
| `# no comment` | 关闭注释处理 | **未实现**（50 个真实词库零命中） | 见 §6 m1 |
| `name`/`version` 校验 | 要求非空 | 不校验 | 我们更宽松，方向无害 |
| `sort:` 字段 | — | **未消费**（恒 weight↓ + order↑） | 声明 `sort: original` 无效 |
| `encoder.rules` 自动编码 | 支持 | 未实现 | 见 §6 R1 |
| 重复条目 | 不去重 | 不去重 | 行为对齐，但无重复计数诊断 |
| 空 text / 空 code | 跳过 | 跳过并计数 | 对齐 |
| 行首空白 | `trim_right`，保留前导 | 同（`trim_line_end`） | 对齐 |

---

## 3. 解析

### 3.1 两条路径

| | A `CodetableDict::load_impl` | B `parse_rime_entries_parallel` |
|---|---|---|
| 位置 | `codetable.rs:251` | `codetable.rs:579` |
| 触发 | `CachedDict::load_at_with` 缓存未命中时（`cached.rs:74`）——**所有 `rime_codetable` / `english` 词库** | **唯一触发者** `load_rime_pinyin_dict`（`manager.rs:2105`），主表与每个 `import_tables` 子表各一次 |
| 返回 | `CodetableDict`（BTreeMap，按 code 分组、组内权重降序） | 扁平 `(fulls, abbrevs)`，**顺序不保证** |
| 简拼 | 计算了但 **`load_impl` 丢弃不用** → Memory 模式 `search_abbrev` 恒空（`cached.rs:214`） | 收进独立 `AbbrevSection` |
| 并行 | 否 | 是（正文 ≥1MB 且多核） |
| `order` | 全文件出现序 | 不产出，由调用方按权重排序后重新赋叶内序号 |

两者共用行解析 `parse_rime_line`（`codetable.rs:482`）——**这是唯一的行解析点**。
（历史上 `load_impl` 另有一份重复实现，两份各自演化正是列序 bug 的温床，已于 b7e5b71 合并。）

### 3.2 列规格判定（文件级）

`ColumnSpec { text_col, code_col, weight_col, has_syllables }`，**判定一次、全文固定**。
列序是文件属性，逐行决策本身就是错的。

```
resolve_columns(content, body, path) -> Option<ColumnSpec>
  └─ parse_columns_header(header) -> ColumnsDecl
       ├─ Usable(spec)   声明完整 → 采样正文填 has_syllables
       ├─ MissingCode    声明了但无 code → ERROR + 整库跳过（不降级探测！）
       ├─ MissingText    声明了但无 text → ERROR + 整库跳过（librime 亦丢弃）
       └─ Absent         无声明 → detect_layout(body) 投票
            扫正文前 200 行 / 攒够 32 票
            is_code_shape = 小写字母 | 数字 | 空格 | ' | ; | / | -
            两列恰有一列像码 → 投该方向；都像/都不像 → 弃权
            平票或零票 → TextFirst（与 librime 默认一致）
            并输出 WARN，区分「投票得出」与「无有效判据、用默认」，
            且两种 columns 写法都给出示例
```

声明块两种 YAML 写法都认：块序列（`columns:` 换行 + `  - text`）与流式（`columns: [text, code]`）。

> **残缺声明为何不降级探测**：`columns: [text, weight]`（librime 的自动编码词库）若降级，
> 探测会把权重列当成编码（`is_code_shape("100")` 为真），静默灌进一整库数字编码，
> 而唯一的日志还写着「未声明 columns:」——排查者去看头部发现声明存在，就把正确线索排除了。

**判据必须建在 code 列而非 text 列**：code 的形态约束是强的（码只能像码），
text 可以是任何东西（汉字、`@`、`$CC("[End]", key.seq("End"))`、英文单词）。
对无约束的一侧做形态测试等于赌——这正是修复前的 bug 成因。

### 3.3 探测机制的实际价值（诚实评估）

实测 50 个真实词库中 28 个无声明，故探测确实是常态路径。但探测的兜底默认（TextFirst）
与 librime 默认一致，意味着**探测在绝大多数情况下只是复述默认值**；它真正创造价值的
场景只有「无声明的 code-first 词库」，而现存词库中**零命中**。

保留它的理由是防御第三方 code-first 词库；代价是引入了「单行字段左移可投反向票」的
风险面（§6 R2）。若未来收紧，可考虑「仅当探测结果与默认不同且票数压倒性时才采纳」。

### 3.4 行解析要点

```rust
parse_rime_line(line, lowercase_code, spec, &mut stats)
  trim_line_end(line)         只剥行尾，前导保留（词条本身可能是 U+3000）
  空行 / '#' 开头             → 跳过
  parts.len() < required_cols → 跳过 + stats.short
      required_cols = max(text_col, code_col) + 1
      ⚠️ 权重不计入——否则两列词库（如 12_kf 全 26 行皆两列）会被整体丢弃
  text 或 code 为空           → 跳过 + stats.empty_field
  spec.has_syllables          → 按空格切音节取首字母做简拼（≥2 音节）、
                                 syllable_boundary_mask 算边界、code 去空格拼平
  否则                        → boundary=0、无简拼
  weight: 空 → 0；解析失败 → 0 + stats.bad_weight
```

`ParseStats` 在收尾时**仅在非零时**输出一条汇总 WARN（干净词库不刷屏）；
并行路径各块独立累加后合并，不引入跨线程共享。

**音节边界的真值来源是源数据里的空格**（`ni hao`）——词库作者写下的、无需推断的事实。
丢掉它就只能靠 DAG 反猜切分，而 DAG 只按覆盖字符数最大化，`xian` 是 `xi'an` 还是 `xian`
它无从分辨。`boundary = 0` 表示「无边界信息」，消费方须降级回 DAG。

---

## 4. 加载链路

```
Config / active_schema
  └─ EngineManager::with_store_override                    manager.rs:218
       ├─ CACHE_DIR.get_or_init(Config::cache_dir)         manager.rs:225
       └─ ensure_loaded(active)   [per-schema single-flight] manager.rs:539
            └─ build_engine(schema_id)                     manager.rs:1475
                 ├─ read_schema                            manager.rs:1435
                 │    用户目录优先 → 安装目录；+override 深合并
                 │    dictionaries 按 id 稀疏合并，override 只能改 enabled
                 │
                 ├─[mixed]   → 递归 build_engine(primary/secondary/english)
                 ├─[english] → load_codetable_layers (lowercase_code=true)  :1579
                 ├─[pinyin]  → load_dictionary                              :1608
                 │     ├─ 单库 & rime_pinyin → load_rime_pinyin_dict        :2038
                 │     │     └─ 读头 YAML → import_tables → 各子表并行解析 → merged.wdat
                 │     └─ 多库 → load_merged_dicts → combined.wdat          :1948
                 └─[codetable] → load_codetable_layers                      :1710
                       └─ 每库 CachedDict::load_at_with                 cached.rs:51

DictManager（每方案一个）→ CompositeDict
  ├─ StoreUserLayer  (User=1, base_order -3)
  ├─ StoreTempLayer  (Temp=2, base_order -2)
  ├─ SystemDictLayer "codetable-system"      (System=4, 恒 enabled)
  └─ SystemDictLayer "codetable-extra-<id>"  × N（enabled 是 AtomicBool，可热插拔）
       ↓ merge_search：跳禁用层 / 按 text 去重（继承更高 weight，
       ↓                前缀取最短码且 boundary 随行）/ sort_by(better)
       ↓ better: weight↓ → base_order↑ → natural_order↑ → code → text
```

**触发时机**：进程启动（仅活跃方案同步构建）、后台逐方案预热、方案切换、首次按键懒加载、
配置热重载（清空全部引擎）、单方案失效（override 写入/方案包导入/删除）。
**扩展词库开关不重建引擎**，只翻 `AtomicBool`。

**路径解析**：`DictSpec.path` 相对 `schemas/` 目录；用户目录（`%APPDATA%\WindInput[Dev]\schemas\`）
优先于安装目录（`data/schemas/`）。

> **注意**：本项目**不读取**官方 Rime/小狼毫的 `%APPDATA%\Rime\` 目录（代码中零引用）。
> 用户同时安装 Rime 时，两套词库互不影响。

---

## 5. 缓存

### 5.1 格式

| 格式 | Magic / 版本 | 用途 |
|---|---|---|
| **wdat** (`datformat.rs`) | `WDAT` / **v4** | **生产链路唯一使用的词库格式**。双数组 Trie，零拷贝 mmap；v3 加 `order`，v4 加 `boundary` + 独立 `AbbrevSection` |
| wdb (`binformat.rs`) | `WDIC` | **当前生产链路不使用**（仅测试引用）。`DictEntry::boundary` 类型定义仍被 wdat 复用 |
| unigram (`unigram.rs`) | `WUNI` / v1 | 词频，与词库无关 |

缓存根：`%LOCALAPPDATA%\WindInput[Dev]\cache\{方案名}\`。
`CACHE_DIR` 为 None 时**回退写到源文件旁**（安装目录可能只读）。

### 5.2 新鲜度判定

`cache_fp.rs`：sidecar `<cache>.fp` 存 SipHash 指纹。

```rust
fingerprint(sources) =
    hash( PARSE_SEMANTICS_VERSION            // ← 关键
        + Σ(file_name + len + 全部内容 + 0xff 分隔) )
```

**刻意不用 mtime**：scp/部署会刷新 mtime，导致每次全量重建（300MB）。

**`PARSE_SEMANTICS_VERSION` 的意义**：指纹若只覆盖源数据，它回答的是「**源文件**变了吗」，
而真正该回答的是「这份缓存和**当前程序**会产出的结果一致吗」。两者平时重合，**恰在解析器
被修复时分叉**——源文件没变则指纹不变，旧缓存被判新鲜而复用，于是**解析器修复对存量用户
静默失效**，且表现为最难排查的「明明改了却没生效」。

> **改动解析语义（列序、注释、权重、边界⋯⋯）必须 +1。** 历史：1 = 列序逐行按 ASCII 猜；
> 2 = 文件级判定 + 读 `columns:` 声明。

---

## 6. 已知缺口与风险台账

> 严重度按**对本项目实际加载路径**的影响评定。标注 ⓛ 者为**潜伏**：当前出厂词库与用户
> 方案词库均零命中，仅在导入特定第三方词库时触发。

### R1 ✅已修 声明了 `columns:` 但无 `code` 列
`codetable.rs:203` `code_col: find("code")?` —— 缺 code 即整个 `parse_columns_header`
返回 `None`，随即降级探测。而 librime 只要求 `text`，**无 code 列是合法的自动编码词库**。

后果：探测会把权重列当成编码（`is_code_shape("100")` 为真），全库以数字串为编码入库。
更糟的是唯一的日志说「**未声明 columns:**」——而该文件明明声明了，排查者会照日志去看头部、
发现声明存在，从而排除这条正确线索。

真实形态样本：`columns: [text, weight]` 的腾讯词向量词库（98 万行）。
**当前不在本项目加载路径上**（存在于官方 Rime 目录，我们不读）。

**已修**：`parse_columns_header` 返回三态 `ColumnsDecl`（`Usable` / `MissingCode` /
`MissingText` / `Absent`），残缺声明**整库跳过并 ERROR 明示原因**——宁可显式少一个库，
不可静默塞进垃圾编码。诊断也不再把「声明不完整」误报成「未声明」。
仍未支持的是 librime 的自动编码本身（`encoder.rules`），那需单独立项。

### R2 ✅已修 `line.trim()` 先于 `split('\t')` → 前导空白的 text 被吞
`codetable.rs:483` 先 trim 再 split。Rust `str::trim` 按 Unicode White_Space 判定，
**U+3000 全角空格属于该集合**。故 `　\tcokg\t\t全角空格` 会被削成 `cokg\t\t全角空格`，
字段整体左移 → text=`cokg`、code=空串；只有两字段的行则直接被 `required_cols` 丢弃。

连锁风险：`vote_layout`（`:82`）同样先 trim 后 split，这类行会投出**反方向的票**。

出厂词库与用户方案词库经正文区逐行扫描**零命中**；命中样本仅存在于上游
rime-wubi86-jidian 原始文件（每库 1–3 条）。

**已修**：改为 `trim_line_end`（只剥行尾，对齐 librime 的 `trim_right`），前导一律保留。
`vote_layout` 同步改用它。并新增空 text / 空 code 守卫，避免字段左移的副产物落进 `entries[""]`。

### R3 ✅已修 WARN 建议了一种我们自己解析不了的语法
`codetable.rs:219` 建议写 `columns: [text, code, weight]`（YAML flow 序列），
而 `:183` 的块起点判据是 `trimmed == "columns:"` **精确相等**，flow 形式不匹配。

用户照日志建议修改 → 保存 → 警告依旧、行为依旧走探测 → 认定「改了没用」，
转而怀疑构建/部署。**本项目已多次踩过这类误判**，代价很高。

**已修**：`parse_columns_header` 现同时支持流式（`columns: [text, code, weight]`）与
块式两种写法，警告文案也两种都列出。

### R4 音节语义的判据（已部分修复，残留缺口见下）
曾以 `code_col < text_col` 二分五笔/拼音，即把「列顺序」当「词库类型」。
librime 完全允许 `columns: [code, text, weight]` 的**拼音**词库，旧判据会把它整库当形码
→ **简拼表整体丢失、音节边界全为 0**，双拼与整句逻辑集体降级回 DAG 猜切分。

现判据（`ColumnSpec::has_syllables`）= `code 列采样到空格` **或** `text 列在 code 列之前`：

- **空格**是音节的正面证据，与列顺序无关 → 修好了上面那个严重方向。
- **列顺序**保留作兜底，因为「无空格」推不出「无音节」：`好\thao` 这样的单音节拼音词条
  同样没有空格，而它的 `0b1`（整串是一个音节）是**真信息**，双拼真值校验要用。
  > 此处踩过一次：起初只用「有无空格」判定，结果只含单音节的拼音词库 boundary 全归零，
  > 直接打挂双拼真值校验的回归测试（`shuangpin_uses_own_split_for_lookup_not_dag`）。

**残留缺口**：声明成 `[text, weight, code, stem]` 的**形码**库（真实样本
`tigercode/tigress.dict.yaml`，113,245 条）仍被判为有音节，每条拿到 `boundary=0b1`。
与判据引入前行为一致（无回归），且码表引擎不消费 boundary，故当前无损害。

**根治须把 schema 的 `dict_type` 传进解析层**——那才是权威判据。
无空格时「单音节拼音」与「形码」在**数据层面根本无法区分**，任何数据侧启发式都只是近似。

### R5 ✅已修 静默失败点（原先无任何日志）
| 位置 | 情形 | 后果 |
|---|---|---|
| `codetable.rs:585` | 无 `...` 分隔行 | **零条目且完全无日志**（路径 B 在 `resolve_columns` 之前就 return，连 WARN 都没有） |
| `codetable.rs:527` | weight 解析失败（如 `50%`） | 静默变 0，丢失词频 |
| `codetable.rs:482` | text 或 code 为空 | 无守卫，产出 code 为空串的垃圾条目 |
| `manager.rs:2059` | `import_tables` 子表不存在 | 静默跳过 |
| `manager.rs:2044-2052` | rime 主表读/解析失败 | `.ok()?` 三处，静默 None |

**已修**：新增 `ParseStats`（`short` / `empty_field` / `bad_weight`），两条解析路径各自
累计、并行块之间合并，收尾时**仅在非零时**输出一条汇总 WARN（干净词库不刷屏）。
缺 `...` 分隔行、`import_tables` 子表缺失也都补了日志。
这不修任何 bug，但把整类静默失败变成可见——投入产出比最高的一项。

### R6 ✅已修 缓存与当前行为不一致的其余路径
`PARSE_SEMANTICS_VERSION` 之外原有两处（均已验证）：

1. **`combined.wdat` 指纹漏掉 import_tables 子表**：`manager.rs:1952` 只把各库主表路径
   喂给指纹，而 `:1969` 对 `rime_pinyin` 源会展开全部子表。改子表 → 内层 `merged.wdat`
   正确重建，但 `combined.wdat` 指纹不变 → **复用陈旧件**，反查/编码提示静默返回旧数据。
2. **`dict_type` / `lowercase_code` 不参与指纹**：`cached.rs:115` 只喂 yaml 路径，
   `cache_path` 也只由源文件名派生。把某库在 english ↔ 非 english 之间切换，`.yaml`
   字节不变 → 指纹命中 → **永久复用大小写错误的 wdat**。
3. 附带：`unigram.wdb` 也挂在 `PARSE_SEMANTICS_VERSION` 上，但该常量的历史记录只描述
   码表列序语义，改 unigram 解析时没有自然动机去 +1。

**已修**：① 抽出 `EngineManager::rime_source_paths`，combined 与 merged 两层共用，
指纹覆盖全部真实输入（此前两处各写一份，正是这条陈旧路径的成因）；
② `cache_fp` 的读写增加 `tag` 参数——`dict_tag(lowercase_code)` 区分英文库的大小写化，
`COMBINED_CACHE_TAG` / `MERGED_CACHE_TAG` / `UNIGRAM_TAG` 区分缓存种类，
各自演进互不牵连。

### R7 其他
- ~~**`CodetableDict::merge` 零调用点**~~ —— **已删除**。其 `order` 重算用的是每个 code
  内的局部序号，与 `order` 承担的「文件全局出现序」语义冲突，属「留着会被误用」的陈旧 API。
- ~~**`binformat` 版本号宽松接受**~~ —— **查证后不成立**：模块文档明载「支持 V2（10 字节
  条目）和 V3（14 字节，含 Order）」，`read_entry` 亦按 `entry_size >= 14` 条件读 order。
  窄 stride 对 v1/v2 是**正确**的，属有意的向后兼容而非静默误读；改成严格相等反而会破坏
  读取合法的旧缓存。**保持现状。**
- **`is_code_shape` 字符集**：已补 `;` `/` `-`（双拼、仓颉常用）。刻意**不收** `.` 与 `,`
  ——符号类词库的词条本身常常就是这两个字符，收进来会让 text 列也呈码形态，得不偿失。
- **零票与压倒性多数**已在日志里明确区分（「无一行给出有效判据，直接采用默认列序」
  vs「依据正文投票」）。
- **`rime_source_paths` 的 YAML 头部定位是朴素子串搜索**（`content.find("---")` /
  `after.find("...")`），而非 `codetable.rs::rime_body_offset` 那种行精确匹配。
  若头部某个值里含字面量 `...`，会被提前截断从而读不到 `import_tables`。
  现存词库无一命中；要收紧须把行精确的分隔行定位提升为 wind-dict 的公开 API。
