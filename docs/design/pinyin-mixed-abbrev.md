# 混合简拼：同一串里混用声母与全拼音节

> 状态：**立项，尚未动生产代码。**
> 由真机反馈引出（2026-07-30）：全拼下 `nhao` 候选为空，用户期望 `n`(简) + `hao`(全) → 「你好」。
> 属**功能缺失**，不是缺陷——这种形态从未被支持过。
>
> 前置阅读：`pinyin-code-domains.md`（三编码域、简拼索引 v5、边界贯通）。本文只讲混合简拼。

## 1. 用户期望与现状

多数输入法支持在一串里混用两种音节表示：

| 形态 | 例 | 含义 |
|---|---|---|
| 全简拼 | `nh` | n·h 两个声母 |
| 全拼 | `nihao` | ni·hao 两个完整音节 |
| **混合（声母在前）** | `nhao` | n(声母) + hao(全音节) |
| **混合（全拼在前）** | `nih` | ni(全音节) + h(声母) |

现在只支持前两种。后两种候选为空或只出前半截。

## 2. 实测：两种混合形态卡在**不同**的地方

`AbbrevMatcher::is_abbreviation`（`scorer.rs:24`）的判据是两条：
① 每个字母都得是某音节的首字母（`trie.is_prefix(单字母)`）；
② 整串**不能**是完整音节序列（`maximum_match` 覆盖长度 < 串长）。

实测（`SyllableTrie::new()`）：

| 输入 | `is_abbreviation` | 卡在哪 |
|---|---|---|
| `nh` | true | —（正常工作） |
| `nihao` | false | 判据②：完整音节序列 → 走全拼，**正确** |
| **`nhao`** | **true** | 判据都过、**进了简拼分支**，但索引里只有整串简拼 `nh`，查 `nhao` 无果 |
| **`nih`** | **false** | 判据①：`i` 不是任何音节的首字母 → **压根不进简拼分支**；DAG 切出 `ni`、`h` 成残码 |
| `xan` | true | —（正常工作） |
| `dblg` | true | —（正常工作） |
| `woain` | false | 判据① |

**结论：这不是一处改动能解决的。**
- `nhao` 要改的是**召回**（索引/投影只认整串简拼）
- `nih` 要改的是**判据**（逐字母检查排斥全拼音节的中间字母）

两者都还要求切分层能表达「这一段是声母、那一段是完整音节」。

## 3. 为什么召回改不动 —— 两处语义都是「整串」

**系统词侧**（`mod.rs` step 5）：查 wdat 的 `AbbrevSection`，v5 起存 `abbrev → 全拼码`。
索引键是**完整简拼串**（`nh`、`dblg`），没有「前 k 个字母是声母、其余是全拼」这种键。

**用户词侧**（`mod.rs` step 6）：枚举全部用户/临时词，用 `abbrev_of_code(code, boundary)`
把每个词投影成简拼串再比对。投影也是**整串**（`xianning` → `xan`），同样表达不了混合。

**切分层**（`dag.rs`）：`Dag` 的节点是**完整音节**。声母（`n`、`h`）不是音节，
进不了词图，所以「`n` + `hao`」这条路径根本构造不出来。

## 4. 方案候选

### 方案 A：切分层引入「声母节点」

给 `Dag` 增加一类节点：单字母声母，只在无法切出完整音节时作为候选边。切分结果因此可能是
`[声母 n][音节 hao]` 的混合序列，再由词图按「声母只约束首字母」的规则匹配词条。

- ✅ 一处改动同时解决两种形态；与 `SylSpan` 的三域表示天然相容（声母节点 flat 长度为 1）
- ⚠️ **直接改逐键候选生成热路径**，且会让路径数显著膨胀（每个位置多一种解释）
- ⚠️ 需要新的匹配规则：词图查询不再是「码相等」，而是「按位约束」

### 方案 B：查询扩展（把混合串展开成候选码集合）

在召回前把 `nhao` 展开成若干可能的全拼码（`nihao`/`nuhao`/`nvhao`…），逐个走主表精确查。

- ✅ 不动切分层与索引，改动局部
- ⚠️ 组合爆炸：每个声母位可对应几十个音节，多个声母位相乘不可控（需要按位剪枝 + 上限）
- ⚠️ 本质是「猜」，与 `pinyin-code-domains.md` 反复强调的「能取回真值就不要推断」相悖

### 方案 C：索引侧支持前缀简拼

`AbbrevSection` 额外存「前 k 个音节的声母 + 剩余全拼码」形式的键。

- ✅ 查询侧简单（仍是一次点查）
- ⚠️ 索引体积按 k 膨胀；wdat 又要 bump（刚从 v4→v5）
- ⚠️ 只解决 `nhao` 那一半，`nih`（声母在后）仍需判据改造

**倾向 A**，但它是热路径改动，必须先有评测对账（见 §6）。B 可作为 A 的兜底或过渡。

## 5. 必须保住的约束（上一轮踩过的坑，逐条都有代价）

1. **简拼候选的 `boundary` 与双拼校验的豁免关系。**
   `boundary_compatible` 靠「任一侧为 0 即放行」豁免简拼；wdat v5 给简拼候选填上真实
   boundary 后，那条隐式豁免失效，双拼下简拼**整批被误杀**（`ae2df59` 已显式豁免
   `is_abbrev`）。混合简拼候选的 boundary 同样与击键不同域，**必须确认走在豁免侧**。
   ★ 推而广之：**给一个原本恒为 0 的字段填真值前，先查谁在依赖它是 0**。
   模糊变体至今仍靠 `boundary=0` 豁免同一道校验。

2. **层级键两侧必须一致。** `cmp_match_layers` 第一键是 `is_abbrev`（`false < true`）。
   系统词简拼曾借 `is_prefix=true` 沉底、用户词简拼用 `is_abbrev=true`，两类同质候选
   分属两层 ⇒ 用户词被整层压住、**词频永远翻不过硬闸门**（`ae2df59` 已统一）。
   混合简拼候选要么并入 `is_abbrev` 层，要么明确定义新层级——**不能借用别的层级键**。

3. **音节数过滤不能丢。** 扁平码有损：`xian` 既是「西安」的 `xi|an` 也是「先」的 `xian`。
   简拼按「字母数 == `boundary.count_ones()`」过滤，否则 `xa` 会捞出「先/线/弦」一串单字
   且权重高得多、排在最前。混合形态下这条规则要重新定义（声母段与音节段的计数口径）。

4. **简拼判据须用原始击键 `raw_input`，不是 `query`。** 双拼下 `query` 已是转换结果，
   拿它判简拼永远匹配不到用户敲的串。全拼下两者相等，故对全拼零影响。

5. **`consumed_length` 的域。** 简拼候选消费整串 raw；混合形态下若只消费前半段（分步上屏），
   必须用 `interp::map_fp_to_raw` 那套 `SylSpan` 映射回击键域，不要另造字节扫描。

## 6. 验证要求

**评测基线**（改热路径前后必须逐位对账，`seed=20260721`）：

| 类别 | 样本 | top-1 | top-5 | MRR | 切分正确 |
|---|---|---|---|---|---|
| A 普通词 | 1000 | 83.90% | 98.80% | 0.9004 | 100.00% |
| B 缩合音短词 | 80 | 1.25% | 28.75% | 0.1260 | 1.25% |
| C 多音节含缩合音 | 1000 | 92.30% | 99.30% | 0.9518 | 94.80% |

```
cargo test -p wind-engine --test pinyin_eval -- --ignored --nocapture
```

⚠️ 需要 `build_dev/data`（`scripts/dev.ps1 d1` 产出）。**该目录缺失时依赖真实词库的测试族
会静默跳过、计数照常绿，唯一判据是耗时**（如 `pinyin_user_word_boundary` 在场 0.15s /
缺失 0.00s）。见 `project_build_dev_data_missing` 记忆。

引入混合简拼是**新增召回**，A/C 两类的 top-1 只应持平或上升；若下降，说明混合候选挤占了
正确的全拼候选，需要调层级或权重。

**单元测试**：优先自带 wdat 夹具（范例 `tests/pinyin_abbrev_index.rs`，走 `CachedDict::load_at`
的 wdat-only 模式），不依赖 `build_dev/data`，避免静默跳过。
⚠️ 写简拼测试**必须用歧义切分码**（`xi|an|ning`、`fan|gan` vs `fang|an`），
`cainiaoyizhan` 这类 `maximum_match` 恰好猜对的样本测不出任何东西。

**真机清单**：
- 全拼：`nhao` → 你好；`nih` → 你好；`nihao` 不变；`nh` 不变
- 双拼：同样几个串（双拼下简拼走 `raw_input`，且要确认没被边界校验误杀）
- 已有用例不回归：`xa` 不出单字、`dblg` 用户词与系统词同层、长词打 2 音节上浮

## 7. 代码位置

| 关注点 | 位置 |
|---|---|
| 简拼判据 | `wind-engine/src/pinyin/scorer.rs:24` `is_abbreviation` |
| 系统词简拼召回 | `wind-engine/src/pinyin/mod.rs` step 5 |
| 用户词简拼召回 | `wind-engine/src/pinyin/mod.rs` step 6 末段 |
| 整串投影 | `mod.rs` `abbrev_of_code(code, boundary)` |
| 简拼索引 | `wind-dict/src/datformat.rs` AbbrevSection（v5 存全拼码）；写入端 `wind-engine/src/manager.rs` `agg_ab` |
| 切分 | `wind-engine/src/pinyin/dag.rs` |
| 三域映射 | `wind-engine/src/pinyin/interp.rs` `SylSpan` / `map_fp_to_raw` |
| 双拼边界校验 | `mod.rs` `boundary_compatible` 调用处（含 `is_abbrev` 豁免） |
| 层级比较 | `wind-candidate/src/candidate.rs:312` `cmp_match_layers` |

## 8. 相关文档

- `pinyin-code-domains.md` —— 三编码域、简拼索引 v5、边界三层贯通（**先读**）
- `pinyin-boundary-aware-lattice.md` —— 边界感知词图、多路径切分、Phase 0 评测基础设施
- `docs/architecture/engine-candidate-pipeline.md` —— 候选装配全链路
