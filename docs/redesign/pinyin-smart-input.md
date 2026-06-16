# 重设计：主流智能拼音输入（权威设计）

> 目标：做到主流智能拼音水平——整句/长句生成、自适应、模糊音、简拼。
> 本文为拼音引擎与拼音词库存储的**单一真值源**，取代 engine.md §1 / dict.md §5 中关于拼音的相关表述。

## 0. 背景与纠正（重要）
此前 dict.md §5 把"弃 wdat、拼音也统一到 wdb"列为决策——**这是错误判断**。原因：
- 智能拼音的核心是**词网格(lattice)构建**：对输入音节流的每个起点，需要"**公共前缀搜索**"
  （common-prefix-search）——一次下行遍历就拿到从该位置出发、覆盖 [i,i+1)、[i,i+2)… 各跨度的**所有词**。
- 这是 **trie** 的拿手好戏；而 **wdb（排序数组 + 二分 + 顺序扫）不适配**——用它得对每个跨度做一次二分，
  退化为 O(n²) 次二分 + 字符串比较，且大前缀顺序扫昂贵。
- **redb 同样不适配 lattice**：B-tree 的 range 只能高效做"前缀=P 的所有 key"，而 lattice 要的是
  "**是输入前缀的所有 key**"（方向相反），仍退化为 O(n²) 次 exact-get。
- 因此 Go 的 **wdb(码表) + wdat(拼音=双数组 Trie)** 分法是**按访问模式分的正确设计**，不是历史包袱。

## 1. 智能拼音引擎管线（目标态）
1. **拼写/切分**：内存音节 trie（~数百音节）把字母流切成**音节格**（含模糊音变体、简拼、末尾 partial、多切分歧义）。
2. **词网格 lattice**：对每个起点做 **common-prefix-search**（key=规范化音节连写），拿到覆盖各跨度的词。
   须 union **系统词库 + 用户/临时词**。一次下行遍历 = O(剩余长度)，远优于 O(n²) 次查找。
3. **打分**：归一化词权重 + 语言模型（unigram **+ bigram**，主流智能必需）+ 用户词频衰减（frequency.md）+ 覆盖/initialQuality。
4. **解码**：Viterbi / beam search 在 lattice 上求最优句 + top-K 备选路径（整句备选）。
5. **候选产出**：整句首选 + 词级候选 + 前缀补全 + 简拼候选。

## 2. 拼音词库存储（按访问模式分；纠正格式决策）
- **码表系统词库 → wdb**（排序数组，只读 mmap）：精确为主 + 有限前缀，足够。**保留**（只读）。
- **拼音系统词库 → 只读 mmap trie**（即 wdat 的角色，这次做对）：
  核心原语 = **common-prefix-search**（lattice）+ 前缀补全 + 精确查找；
  key=规范化音节连写（如 `nihao`），value=候选列表（文本+权重）的偏移。
  - **不手搓 DAT**（Go ~1400 行，易错）。用**成熟纯 Rust crate**（满足"纯 Rust 无 C 依赖"交叉编译硬约束）：
    - **`crawdad` / `yada`**：纯 Rust 双数组 trie，**自带 `common_prefix_search`**（为分词/形态分析而生，正是 lattice 所需），id→侧表（值 blob）。
    - **`fst`（BurntSushi）**：最成熟、mmap、key→u64（偏移）、**额外支持 Levenshtein 自动机**（可优雅实现模糊音/容错）；common-prefix-search 需用其 raw FSA 节点下行遍历（可行，稍底层）。
    - **排除 `marisa`**（C++ 依赖，破坏纯 Rust 交叉编译）。
  - **落地前做小基准**（store-for-system 议题同款）：构建耗时 / 文件体积 / common-prefix-search 延迟 / 模糊音支持，据此三选一。倾向 crawdad/yada（API 直接对口）或 fst（成熟+模糊音红利）。
  - **简拼（声母连写）**：并行 abbrev trie，或同 trie 加标记位。
- **用户/临时词 → redb**（可变，store ops 已实现）：但 lattice 需对用户词也做 common-prefix-search。
  方案：方案激活时把用户/临时词**载入内存 trie**（量小，构建快），与系统 trie 结果合并；
  避免对每个跨度走 redb exact-get（慢）。
- **复合查询面**：dict composite 为拼音暴露 `common_prefix_search(syllables, start) -> 各层候选`，
  union 系统 trie + 用户/临时内存 trie；词频/shadow 仍按 frequency.md/dict.md 在排序阶段应用。

## 3. 语言模型（升级 engine.md §1.2）
- **bigram 升为阶段 B 必做**（不再"暂缓"）——主流"智能"长句的关键。
- unigram：登录词 `log(freq/total)`，OOV 回退字均值/最小概率（已有 mmap unigram）。
- bigram：key=(w1,w2)→logprob 的只读 mmap（fst/排序数组/单独文件，体量可控时甚至全内存）；
  插值 `log(λ·P_bi + (1-λ)·P_uni)`，未登录回退 `uni − backoff`（Go lm.go 有可参考公式）。
- 句子分 = Σ 词项[ 归一化权重 + λ_lm·LM ] + 词频衰减 + 覆盖/iq；Viterbi 保留 top-K 路径。

## 4. 与既有差分的关系
- **frequency.md**：拼音词频仍解耦——衰减分作为 lattice 词项的**加成维度**（归一化分数层），不改词库 weight。
- **dict.md §5**：本文**撤销**其"弃 wdat 统一 wdb"；改为"码表 wdb / 拼音 只读 mmap trie"。其余（abbrev 段、top-K、hotcache）按访问模式归入对应格式。
- **engine.md §1**：lattice 的查找原语改为 trie common-prefix-search（非 wdb 二分）；scorer 公式见 §3。
- **redb**：仍是可变层（user/temp/freq/shadow）；拼音系统 trie 是只读 mmap，不在 redb。

## 5. 落地顺序（阶段 B 拼音质量核心）
1. 选定并接入拼音 trie crate（基准后定 crawdad/yada/fst）：系统拼音词库 = 只读 mmap trie + common-prefix-search。
2. lattice 构建改用 common-prefix-search（系统 trie ∪ 用户内存 trie）。
3. bigram LM 接入 + scorer 升级（unigram+bigram 插值 + 归一化 + iq + 词频衰减）。
4. Viterbi/beam top-K 整句 + 备选；模糊音（fst Levenshtein 或音节层模糊扩展）。
5. 简拼 abbrev trie。
> 每步 `wind_input/scripts/dev.sh ci` 把关。
