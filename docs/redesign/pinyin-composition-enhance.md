# 拼音组合区逐步转换 + 自动造词 增强设计

> 背景：当前 Rust 版「分段上屏」把选中的单字**直接上屏到应用**，导致
> ① 选「你」后「hao」留缓冲但已与「你」割裂，无法分步组词、无法自动造词；
> ② 混输模式选「你」后剩余「hao」落到五笔引擎。
> 标准拼音输入法（Rime / Google / 搜狗 / 本项目 Go 原版）用的是**组合区内逐步转换**模型。
> 本文档基于 Go 原版（`WindInput/wind_input`，权威参考）调研结论给出 Rust 实现方案。

## 〇、网络调研结论（libime / sunpinyin / Rime，2026-06）

Go 原版只是参考之一且不够好（词频朴素）。对照主流开源拼音引擎，确立两条主线：

**组合区模型**（libime `PinyinContext`、Rime、Go 三者一致）：
- 输入态 = `selected`（已选定段，按音节切分的汉字前缀）+ 剩余拼音 + cursor。
- 选候选 = 把该候选作为一段加入 `selected`、cursor 后移、对剩余拼音重出候选；**不上屏**。
- 全部选定（剩余空）→ 整体上屏，并调 `learn()` 更新用户模型（造词/调频）。
- 退格在剩余空时回退最后一个 `selected` 段（你→ni 可继续编辑）。

**词频 / 语言模型**（libime 为现代标杆，直击"词频不好"）：
- 候选排序是**综合打分**：LM 分（KenLM n-gram，最高 trigram，Kneser-Ney 回退）
  ＋用户频率（`HistoryBigram`）＋模糊惩罚＋历史加权；**不是**"用过即硬置顶"。
- `HistoryBigram` = **三池加权 + 指数衰减**：选词后频率增长、记词对共现、旧数据随时间淡化。
- `AutoPhraseDict`：隐式学习高频多字序列（自动造词）。
- **本项目现状之弊**：`apply_freq_rerank` 按 count 硬排序、完全压过权重，且无衰减、无 LM、
  无词对——单字用一次即压过整句。这是要重构的核心。

> 现状 Rust 拼音引擎已有 lattice + Viterbi + unigram，**缺 bigram LM**（与 Go 风险表一致）。
> 本轮先把"词频作加权 boost 融入打分（带衰减）"做对；bigram LM 作为后续增强项。

参考来源：
- libime 架构（Fcitx5）：PinyinContext / PinyinDecoder / Lattice / LanguageModel(KenLM) /
  UserLanguageModel / HistoryBigram(三池指数衰减) / AutoPhraseDict（deepwiki.com/fcitx/libime）
- sunpinyin：backoff bigram/trigram SLM + Viterbi lattice（github.com/sunpinyin/sunpinyin）
- Rime：express_editor 记忆回车提交短语、userdb 用户词库学习（github.com/rime/librime）

## 一、Go 原版模型（调研结论）

组合区由三段组成，**选词只在组合区内转换，不上屏**，全部转换完才整体上屏：

```
显示 preedit = prefix(触发键，可空) + committed(已转换汉字) + remaining(未转换拼音)
                                      └── 「你」          └── 「hao」
```

参考：`internal/coordinator/pinyin_mode_shared.go`
- 选词 `selectPinyinModeCandidate` (L313-344)：
  - **部分匹配**（`cand.ConsumedLength < len(buffer)`）：`committed += text`；
    `buffer = buffer[ConsumedLength:]`；重出剩余候选；返回**组合区更新**（不退出、不上屏）。
  - **完整匹配**（消费整串）：`exitMode(true, committed+text)` → 整体上屏应用 + 记历史/造词。
- 空格 = 选当前高亮候选（L76-85）；回车 = 上屏拼音原码 / 已转换前缀（L88-102）。
- 长句 + 尾残码：解析分 `completedSyllables` + `partialSyllable`，**lattice/Viterbi 只跑完成音节**，
  `ConsumedLength` 不跨残码，残码留在 buffer 显示（`engine_ex.go` L96-127）。输入 `nihaom`
  首候选仍是「你好」，显示「你好m」。
- 造词：完整上屏后 `OnCandidateSelected(code,text)`（`pinyin.go` L384-442）→ 单字仅 boost
  unigram；词（≥2 字）走 `LearningStrategy.OnWordCommitted`（`schema/learning.go` L70-94）：
  系统词库已有则跳过 → 优先写临时层，达晋升阈值再升用户词库。

## 二、当前 Rust 现状与差距

| 能力 | 现状 |
|---|---|
| 组合区 committed 前缀 | **无**（State 只有 `input_buffer`/`temp_pinyin_buffer`/`mix_buffer`） |
| 选词分段 | 有 `consumed_length`，但选词后**直接上屏**（`commit_selected` 返回 InsertText） |
| 长句尾残码 | 引擎 sentence 解码在**整串**（含残码）上跑 lattice → 残码毁掉整句，退化单字（bug①根因） |
| 造词基建 | **已具备**：`wind-store` `add_user_word`/`on_word_selected`(阈值加权)、`temp_words`(晋升)；`wind-dict` User 层 |
| 造词接线 | **未接**到拼音上屏路径 |

## 三、Rust 实现方案（分阶段）

### P. 引擎长句尾残码（修 bug①，最契合、风险低）
`wind-engine/src/pinyin/mod.rs convert()`：当前用整串 `input` 建 lattice。改为：
- 取 `completed = 连续完成音节拼接`（已有 `compute_composition` 得 `completed_syllables`/`partial`）；
- lattice/Viterbi 在 `completed` 上跑，sentence 候选 `consumed_length = completed.len()`；
- 残码 `partial` 不参与整句，保留在 preedit 显示。
- 验证：`nihaom` 首候选 `你好`（consumed=5），preedit `你好m`/`ni hao m`。

### C. 组合区逐步转换（修 bug②核心，主拼音 + 临时拼音）—— 仅拼音类，码表不动
1. State 增 `committed_text: String` + `committed_segs: Vec<(code,text)>`（每段拼音码与汉字，
   供退格逐段回退与造词时拼完整码）。**仅拼音/临拼/混输文本透镜使用；五笔等码表模式不引入。**
2. preedit 显示 = `committed_text` + 引擎 preedit(remaining buffer)。caret 用显示串字符数。
3. 选词改写（替换现 `commit_selected`/`commit_temp_pinyin_selected`）：
   - 部分匹配：push 段 (消费码, cand.text)；`committed_text += cand.text`；buffer 裁剪；
     重出剩余候选；返回 **UpdateComposition**（留在模式内，不发 InsertText 到应用）。
   - 完整匹配（剩余被消费空）：上屏 `committed_text + cand.text` 到应用；触发造词/调频（见 L/F）；清空退出。
4. 按键语义（已与用户确认）：
   - **空格** = 选当前高亮候选（默认行为，走 3 的逻辑）。
   - **回车** = 上屏「输入缓冲当前显示」：即 `committed_text + 剩余拼音原码`（已转中文的照样上屏，
     剩余拼音按原码上屏），然后退出。
   - **退格** = 剩余拼音非空则删其末字符；剩余为空则**回退最后一个 committed 段**（「你」→ 还原成 `ni`
     进缓冲继续编辑，主流拼音行为）。
5. Esc / 失焦 / 模式切换：清空 committed 段与缓冲。

### M. 混输文本透镜复用 C
mix 文本透镜（拼音/英文成员）选词走同一组合区模型，剩余拼音仍由 `convert_with` 出候选，
不再落到默认（五笔）引擎。数字透镜（计算）不涉及分段，保持整体上屏。

### F. 词频重构（直击"词频不好"）—— ⏸ 本轮暂缓
> 用户决定：词频后续统一处理（五笔与拼音的词频需求不同，需统一方案再做）。本轮不动 F，
> 沿用上轮修复后的 `apply_freq_rerank`（已排除分段子候选）。下文为后续参考。

把朴素"用过即硬置顶"改为**加权 boost 融入排序 + 时间衰减**（借鉴 libime HistoryBigram 思想，简化版）：
- 词频记录除 count 外存 `last_used`（已有 redb 记录可扩展）；有效分 = f(count, 衰减(now−last_used))。
- 重排不再整列硬排序覆盖权重，而是把"有效分"作为对基础权重的**加成**（log/缩放后相加），
  保持引擎权重（整句置顶、词长优先）主序，用户偏好仅在同档内上浮，避免单字越过整句。
- 仍遵守上轮修复：分段子候选（consumed<整串）不按整串码计频。
- （后续增强项，本轮可不做）bigram 词对：记录相邻已选段的共现，提升整句解码。

### L. 自动造词接线（增强；需临时层——已与用户确认）
完整上屏（含分步组成）时：
- 单字：boost unigram / freq（沿用 `record_selection` 调频）。
- 词（≥2 字，由 committed 各段码拼成完整码 → 完整词）：查系统词库无则**优先写临时层
  `temp_words`，达晋升阈值再升用户词库**（`add_user_word`）；key=完整拼音码、text=完整词。
- 下次输入该拼音串即出该词并随频次（F 的衰减加权）上浮。

## 四、风险 / 注意
- coordinator 热路径（~5200 行），改动集中在拼音/临拼/mix 三处选词与 preedit 组装；
  其余模式（五笔/英文/URL/特殊/计算）不受影响。
- 提交纪律：仅 `git add` 自己的文件（并发会话在改 wind-ui 主题）。
- host 可测：引擎 P 段、store 造词；coordinator C/M 段靠交叉编译 + 设备验证。
