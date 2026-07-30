# 码表调频：按码长分级的首选保护

> 状态：**主仓已实施**（引擎 + 配置 + 文档 + 测试），设置页 GUI 与真机验证见 §9
> 相关：`docs/redesign/frequency.md` §8（`protect_top_n` 原始语义）、
> `docs/architecture/candidate-sorting-rules.md` §7.3（`rerank_codetable_usedfirst` 在三套排序系统中的位置）

## 1. 问题

自动调频基本可用，但**保护策略只有一个标量 `protect_top_n`，对所有码长一视同仁**。
对五笔这类有简码体系的方案，一简二简是肌肉记忆的核心，不该被一次误选永久改写；而全码位
恰恰是调频最该起作用的地方。一个标量表达不了这两件相反的事。

## 2. 根因：保护机制与威胁机制不在同一根轴上

### 2.1 简码的"钦定地位"建在权重轴上

词库生成阶段（`wind-tools/gen_dict/shortcode.rs`，参数见 `gen_dict.toml:51-58`）做了简码分层：

| 层级 | 判据 | 权重 |
|---|---|---|
| 一简 | 单字 + 码长 1 | 9999 起，组内按原库行序递减 |
| 二简 | 单字 + 码长 2 | 9950 起 |
| 三简 | 单字 + 码长 3 | 9000 起 |
| 普通词条 | 其余 | `regular_weight_max`（`config.rs` 校验强制 < 9000） |

`shortcode.rs` 的模块注释写着"这样词频排序再怎么变都动不了简码的首选地位"。
**该保证只覆盖词库生成阶段的 unigram 赋权，不覆盖运行时的自动调频。**

### 2.2 运行时调频绕过权重轴

`wind-engine/src/freq_rerank.rs` 的 `rerank_codetable_usedfirst` 比较链：

```
freq_tier(来源档位) → 有无词频记录（有 > 无） → strategy（count / last_used）
```

**全程不比较 weight**，且码表侧是永久 used-first（不衰减、不褪色）。于是权重 9999 的一简字
与 9998 的次选字在这一步完全平等：谁被选过谁上位，且永不回落。

### 2.3 数据事实：一简全部是"二选一"

统计发行词库 `wubi86_jidian.dict.yaml`（列序 `code / text / weight`）：

| 码长 | 条目 | 唯一码 | 平均每码 | 有竞争者的码 |
|---|---|---|---|---|
| 1（一简） | 50 | 25 | **2.00** | **25 / 25（全部）** |
| 2（二简） | 655 | 616 | 1.06 | 39 |
| 3（三简） | 5352 | 4696 | 1.14 | ~650 |
| 4（全码） | 82469 | 71795 | 1.15 | —— |

例：`a` → 工(9999) / 戈(9998)。一简 25 个码没有一个是安全的。出厂 `strategy = "top"`（MRU）
下选错一次当场翻转；`step` 下累积几次也必然翻转。

### 2.4 标量闸门的两个极端

`protect_top_n`（`config.rs:383`，出厂 `data/config.toml` = 1）：重排前抓基础序前 N 位、
重排后原序回填。

- `= 1`（现出厂值）：简码保住了，**但全码位首选也被永久锁死**——调频只对第 2 位以后有效；
- `= 0`：全码位调频正常，**一简二简当场失守**。

## 3. 已定决策

| # | 决策 | 取值 |
|---|---|---|
| D1 | 简码位保护强度 | 保护首选 1 位（第 2 位以后仍可调频） |
| D2 | 分级口径 | **按码长分级表**，而非"简码位/全码位"二分或相对全码长 |
| D3 | 全码位出厂值 | `protect_top_n` 由 1 改 0 |
| D4 | 是否迁移老配置 | **不迁移**——码表调频出厂 `enabled = false`，绝大多数用户无感 |

D4 的残留影响（已知并接受）：**已经手动开过调频**的老用户，其配置层里冻结着
`protect_top_n = 1`（配置写回从不剔除等于默认的键），因此全码位仍锁死，需自行在设置页改 0。
新增的分级键在他们配置里缺省，会自动取代码默认值，故简码保护对他们**自动生效**。

## 4. 配置形态

### 4.1 采用：展开为按码长的独立整数键

```toml
[schema.codetable.frequency]
enabled = false
strategy = "top"
protect_top_n = 0          # 兜底：未单列的码长（≥ 4 码）
protect_top_n_len1 = 1     # 一简位
protect_top_n_len2 = 1     # 二简位
protect_top_n_len3 = 0     # 三简位
```

等价于分级表 `[1, 1, 0, 0…]`，内部仍以一张表消费（见 §5.1）。

### 4.2 为什么不是单键数组 `protect_by_code_len = [1,1,0,0]`

配置注册表 `FieldType`（`config_schema.rs:14`）目前只有
`Bool / Int / Float / Str / Enum / StrList / Map / StructList`，**没有整数数组**。
新增 `IntList` 需要动 5 处 match（类型定义、`parse_str_value`、`type_label`、`validate`、
CLI 帮助），并且**设置仓要新增一种控件形态**——现有 manifest 只有
`toggle / select / number / …`，`protect_top_n` 用的是 `type = "number"` + `min/max`
（`settings_manifest.toml:172`）。展开成独立整数键则三个新项直接复用 `number` 行，
主仓零新类型、设置仓零新控件。

代价：分级上限固定为 3 级。这不构成限制——简码概念只对前 3 级有意义，
码长 ≥ 4 一律是"深码位"，走兜底值即可（含 `max_code_length` 为 5/6 的方案）。

### 4.3 为什么不是"按候选身份保护"（锁住钦定字本身）

运行时无法可靠识别"这条是词库钦定的简码字"：`Candidate` 没有 `shortcode_level`，
靠权重区间（≥ 9000）反推只对本仓 `gen_dict` 产物成立，古精86 / 深海等第三方码表直接失效。
要做需给 wdat 加列 + 生成器改造，成本远大于收益。**否决，勿再提。**

## 5. 实现设计

### 5.1 策略结构（放在 `wind-engine/src/freq_rerank.rs`）

```rust
/// 按**输入码长**分级的首选保护策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtectPolicy {
    /// 索引 0/1/2 = 码长 1/2/3。
    pub by_len: [usize; 3],
    /// 码长 ≥ 4（未单列的深码位）。
    pub fallback: usize,
}

impl ProtectPolicy {
    pub fn resolve(&self, code: &str) -> usize {
        match code.chars().count() {
            0 => 0,
            n if n <= 3 => self.by_len[n - 1],
            _ => self.fallback,
        }
    }
}

impl Default for ProtectPolicy {
    fn default() -> Self {
        Self { by_len: [1, 1, 0], fallback: 0 }
    }
}
```

**按输入码长而非候选码长**：保护的是"用户当前所在这个码位的钦定首选"。在精确档内
两者相等，但前缀补全候选的码更长——用候选码长会把分级判据搅成一锅。

### 5.2 保护名额只在精确档内取（顺带修的既有缺陷）

现实现 `protected` 直接 `take(protect_top_n)`，不区分候选是精确档还是前缀补全档。
当某码位精确候选不足 N 时，多余名额会把一个**前缀补全词**钉死——没有语义依据。

改为：

```rust
let protected: Vec<String> = candidates
    .iter()
    .filter(|c| c.is_exact_code)      // ← 新增
    .take(n)
    .map(|c| c.text.clone())
    .collect();
```

凑不满就少保护；某码位完全没有精确候选（打了词库里没有的码）→ 保护集为空，不保护。
这是正确行为：没有钦定首选可言。

`is_exact_code` 的置位规则见 `candidate-sorting-rules.md` §5（混输 overflow 分支已归一到
完整输入），此处直接复用，不新增判据。

### 5.3 混输行为：沿用同一策略（有意为之）

混输下 `code` 是输入缓冲（击键序列），打两个字母的拼音（如 `ni`）会被判成"二简位"。
经评估**保留该行为**：此时基础序第 1 位通常是码表精确候选（`cmp_exact_first` 与
`freq_tier` 都把它排在最前），保护它恰好符合既有的"五笔优先"硬约束。

⚠️ `config.rs:381` 的注释"仅纯码表生效"与实现不符——`freq_settings()` 走的是
"非拼音即码表配置组"，**混输一直在用这套值**。实施时一并订正注释。

### 5.4 落点清单

| # | 文件 | 改动 |
|---|---|---|
| 1 | `wind-config/src/config.rs:376` | `CodetableFrequency` 加 3 个 `usize` 字段 + 默认值 `1/1/0`；`protect_top_n` 默认保持 0（结构体默认本就是 0） |
| 2 | `wind-config/src/config_schema.rs:77` | 注册 3 个 `Int` 键 |
| 3 | `data/config.toml` | `protect_top_n` 1 → 0，写入 3 个新键与注释 |
| 4 | `wind-engine/src/freq_rerank.rs` | 新增 `ProtectPolicy`；`rerank_codetable_usedfirst` 末参 `protect_top_n: usize` → `policy: ProtectPolicy`；`protected` 加精确档过滤 |
| 5 | `wind-engine/src/manager.rs:44` | `FreqSettings.protect_top_n` → `protect: ProtectPolicy`；`freq_settings()` 组装（拼音分支给 `ProtectPolicy { by_len: [0;3], fallback: 0 }`） |
| 6 | `wind-coordinator/src/handle_candidate.rs:257` | 传 `settings.protect` |
| 7 | `wind-setting` 仓 | 3 个 `number` 行；现有 `protect_top_n` 的 label/hint 改成"全码位"口径；过五道守门测试（快照/mockdata 勿手改，按测试给出的命令重生成） |
| 8 | 文档 | `frequency.md` §8 追加分级语义；`candidate-sorting-rules.md` §7.3 的 `protect_top_n` 行改写；文档站 `../WindInputDocs` 的 config 参考页 + 用法页两处 |

### 5.5 实施顺序（红 → 绿）

1. 先写 §6 的**一简保护主用例 + 全码位对照组**，跑出「主用例红、对照组绿」；
2. 落 `ProtectPolicy` 与精确档过滤，主用例转绿、对照组保持绿；
3. 接配置与设置页；
4. 端到端与真机。

## 6. 测试计划

### 6.1 单元测试（`freq_rerank.rs`）

| 用例 | 断言 |
|---|---|
| `short_code_len1_protects_dict_head` | 一简位（`code="a"`）工/戈同为 tier 0，戈有词频记录 → 工仍居首 |
| `full_code_still_reranks_freely` | **对照组**：同一组数据放到 `code="aaaa"` → 戈正常上浮 |
| `short_code_len2_protects_under_mru` | 二简位 + `Top`（MRU）策略——MRU 在简码位危害最大（选一次到顶） |
| `protect_policy_resolve_by_len` | `resolve()` 各码长取值正确，码长 ≥ 4 落兜底，空码为 0 |
| `none_policy_degrades_to_no_protection` | `ProtectPolicy::NONE` → 与分级引入前一致 |
| `protect_slots_taken_from_exact_only` | 1 精确 + 2 补全、n=2 → 只保护 1 个，补全档内部照常按词频重排 |
| `no_exact_candidate_means_no_protection` | 全是补全候选 → 保护集空 |
| `protect_top_n_pins_original_head`（既有） | 兜底档 `fallback=1` 仍按原语义工作；构造改用精确档候选 |

**对照组是必需的**：只有主用例会在"全局硬保护"这种错误实现下同样变绿，
证明不了分级真的分了。

### 6.2 端到端（`wind-coordinator/tests/codetable_short_code_protect.rs`）

真实五笔词库 + 真实 redb 词频，三条：

| 用例 | 断言 |
|---|---|
| `short_code_head_survives_freq` | 「戈」记 5 次后打 `a` → 首选仍是「工」 |
| `short_code_protection_off_lets_freq_win` | **反向对照**：同一份词频、只关保护 → 首选变「戈」 |
| `full_code_still_reranks` | 全码位 `aaaa`（恭恭敬敬/工）→ 用过的正常上浮 |

**反向对照是这一族的关键**：主用例的「工」在「词频压根没生效」时同样会绿——store key 的
schema/code 域写错、调频开关没开、词频记录没进重排，任一条都会让主用例假绿。实施时它
**当场抓到了两个真问题**：① 我按 `cat -A` 解码词库时把「戈」误读成「戍」，测试数据是个
不存在的字；② 生僻字「菚」被常用字过滤挡在候选之外，`aaar` 这个全码位现场根本测不出重排
（已换成 `aaaa`）。

⚠️ 依赖 `build_dev/data`：**该目录缺失时这类用例全部静默跳过而计数照绿，唯一判据是耗时**
（正常 2s 量级 vs 跳过 0.0x s）。在 git worktree 里跑需先把 `build_dev/data` 链接过去
（`New-Item -ItemType Junction`），否则整族静默跳过。报告"全量通过"前必须核对耗时或
用 `--nocapture` 数「跳过」消息条数。

### 6.3 验证纪律

`freq_tier` 是 `rerank_codetable_usedfirst` 的首要键，会整体压过协调器显示序。
测简码保护必须让竞争双方**同档**（同为 tier 0 的精确候选），否则测到的是档位不是保护。

## 7. 风险与陷阱

1. **默认值变更对已开调频的老用户静默无效**（见 D4）——不做迁移是明确决策，
   但发布说明里要写一句。
2. **`build_dev/data` 缺失致端到端假绿**（见 6.2）。
3. **`protect_top_n` 的注释与实际作用域不符**（见 5.3），改动时顺手订正，
   否则下一个人会按注释以为混输不受影响。
4. **回填按 `text` 匹配**：`position(|c| c.text == text)` 在同文本多候选时可能锁错行。
   码表下罕见（同码同文本会被去重），**本次不改**，仅记录。
5. **用户词 / 自造词若落在简码位会一并受保护**：保护锁的是位置不是来源。
   五笔加词取的是全码，落到 1-2 码位的概率极低，**接受**。

## 8. 明确不做

- 不改写入端：简码位的选择照常记入 FREQ 表。保护只在读取端生效，
  用户把保护数调回 0 时历史仍在（可逆性优先）。
- 不引入"累计 N 次可突破保护"的逃生口：本轮取 D1 的简单语义，
  重度用户在简码位的偏好可通过候选调整（shadow）或加词表达。
- 不动 `freq_tier` 的档位定义。
- 不给拼音路径加分级保护（拼音无简码体系，`protect` 恒为空策略）。

## 9. 实施状态

已完成（主仓，分支 `worktree-codetable-freq-shortcode-protect`）：

- `ProtectPolicy` + 分级 `resolve` + 精确档过滤（`freq_rerank.rs`）
- 配置三处：结构体字段 / 注册表 / `data/config.toml`（`protect_top_n` 出厂 1 → 0）
- `FreqSettings` 与协调器接线
- 单测 8 条 + 端到端 3 条；`wind-engine` / `wind-config` / `wind-candidate` / `wind-coordinator`
  全量绿，clippy 无新增警告

待办：

1. **设置页 GUI**（`wind-setting` 仓）：3 个 `number` 行 + 现有 `protect_top_n` 的 label/hint
   改成「全码位」口径；过五道守门测试。
2. **真机验证**：打 `a` / `aa` 选次选字若干次后确认首选不变；`aaaa` 位确认调频仍生效。
3. 文档站 `../WindInputDocs`：config 参考页 + 用法页两处。

⚠️ 跑 `cargo test -p wind-coordinator` 会**真写** `%APPDATA%/WindInput/config.toml` 的
`schema.active`（实测被改成 `pinyin`）。本次已备份/恢复，后续同样要防。
