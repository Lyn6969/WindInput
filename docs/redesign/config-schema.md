# 重设计差分：config / schema（配置与方案系统）

> 阶段 A 产物（最后一份）。Go 侧 3 个只读 agent 提取、关键论断 grep 抽验 file:line 属实；Rust 侧本人通读。
> 体量：Go `pkg/config` ≈ 3647 行 + `internal/schema` ≈ 3034 行；Rust `wind-config` ≈ 1260 行。
> **schema 是 engine/dict/store 所有质量特性的配置面**——它们的 spec 字段是那些特性的开关，本差分把配置面锁定。

---

## 1. 核心现状

### Schema：两套表示 + 简陋（已 grep 确认死代码）
- `wind-config/schema.rs` 的 `Schema`（70 行）**仅在 lib.rs 导出，无任何跨 crate 使用 = 死脚手架**。`RuntimeState`/`AppCompat` 同样无人用。
- 实际驱动引擎的是 `wind-engine/manager.rs` 的 `SchemaFile`（我在 engine 差分读过），字段极简：engine.type / codetable.{max_code_length,temp_pinyin} / pinyin.scheme / mixed.{primary,secondary,min_pinyin,boost} / dictionaries.{path,type,default,default_enabled} / learning.unigram_path。
- Go 有专门 `internal/schema`：丰富 Spec（CodeTableSpec ~22 字段 / PinyinSpec+Fuzzy 12 标志+Shuangpin / MixedSpec 10 字段 / DictSpec+WeightSpec+Role / EncoderSpec / LearningSpec(AutoLearn/AutoPhrase/Freq)）+ `factory.go`(1694, CreateEngineFromSchema) + SchemaManager + loader(三层合并+override) + learning。

### Config：合并不完整（现状 bug）
- `config.rs` 结构完整（general/schema/hotkeys/input/ui/features/compat/debug），但 `merge_from_file` 是**手写逐字段合并**且漏字段：schema 段漏 primary_codetable/primary_pinyin；input 段只合 6/~15；**features/compat/debug 三段完全不合并** → 用户这些配置静默失效。
- Go 用 yaml.v3 部分覆盖语义（TOML→map→YAML→struct 双序列化），**自动合并所有字段** + 版本迁移 + 保存只写 diff（yamldiff）。

### compat / runtime_state / migration：缺
- Rust app_compat.rs(22)/runtime_state.rs(19) 是 stub；无 migration。Go 有 per-process 兼容规则、运行时状态存取、schema_overrides、版本迁移链。

### 路径：Rust 较好
- `config.rs` 已有 app_dir_name（debug 变体隔离）/user_config_dir(漫游)/local_dir(本机)/cache_dir/log_dir/data_dir——对齐近期 commit 113385f。Go 另有便携模式、datadir.conf 自定义路径、macOS 例外、路径校验。

---

## 2. 决策：统一为一套丰富 Schema 驱动引擎

1. **删死脚手架** `wind-config/schema.rs` 现 Schema；建**一套丰富 Schema**（放 `wind-config` 或新 `wind-schema` crate），字段对齐 Go 的 Spec 体系。
2. `wind-engine` 的 `SchemaFile` 与 `build_engine` 改为消费这套 Schema——**schema 定义与引擎构建解耦**（Go 的 factory 在 schema 包，引擎是被构建对象）。
3. tri-state 字段一律 `Option<bool>`（修 Go 的 plain bool / *bool 混用坏设计）。
4. EngineBundle 的 `interface{}` → Rust **enum**（Pinyin/CodeTable/Mixed）。

---

## 3. Schema 富 Spec 清单（= engine/dict/store 质量特性的配置面）

> 这些字段就是前几份差分里"缺失能力"的开关。Schema 扩展是阶段 B 质量特性的**前置**：没有 spec 字段就无法配置 auto_commit / fuzzy / freq 参数。

- **CodeTableSpec**（对齐 engine.md §2）：max_code_length / auto_commit_at_full:Option<bool> / auto_commit_min_len / auto_commit_block_on_pinyin / clear_on_empty_max / top_code_commit / punct_commit / show_code_hint / single_code_input / single_code_complete / candidate_sort_mode / dedup_candidates / skip_single_char_freq / temp_pinyin / z_key_repeat / **weight_mode / prefix_mode / bucket_limit / short_code_first / charset_preference**（这些直接对应 codetable 引擎缺失项）。
- **PinyinSpec**（对齐 engine.md §1）：scheme(full/shuangpin) / **shuangpin.layout(ziranma/xiaohe/sogou/mspy)** / show_code_hint / use_smart_compose / candidate_order / **fuzzy(12 标志: zh_z/ch_c/sh_s/n_l/f_h/r_l/an_ang/en_eng/in_ing/ian_iang/uan_uang + enabled)**。
- **MixedSpec**（对齐 engine.md §3）：primary_schema / secondary_schema / min_pinyin_length(默认2) / codetable_weight_boost(默认1e7) / show_source_hint / enable_abbrev_match / pinyin_only_overflow(默认true) / enable_english / top_code_override_pinyin。
- **DictSpec**（对齐 dict.md）：id/label/path/type(codetable/rime_codetable/rime_pinyin) / default / default_enabled:Option<bool> / enabled:Option<bool> / role(system) / **weight_spec(median/max/min/mode:linear|log/target，归一化上限 10000)** / weight_as_order。
- **EncoderSpec**（造词/编码提示）：rules[{length_equal | length_in_range:[min,max], formula:"AaAbBaBb"}] / max_word_length / exclude_patterns。
- **LearningSpec**（对齐 store.md §3-4）：auto_learn{count_threshold:2, min_word_length:2, weight_delta:40, add_weight:800} / auto_phrase{min/max_phrase_len:2/5, idle_timeout_ms:5000, ...} / **freq{enabled, protect_top_n, half_life:72h, boost_max:2000, max_recency, base_scale, streak_scale, streak_cap}** / unigram_path / temp_max_entries:5000 / temp_promote_count:5。

> ⚠️ **词频默认值两套源**（已核实）：schema `FreqSpec` 注释默认（max_recency 300 / base_scale 100 / streak_scale 50 / streak_cap 250）≠ store `DefaultFreqProfile`（100/50/30/150）。Go 中 schema 仅在字段 >0 时覆盖 store 默认。**Rust 必须定唯一真值源**（建议 store 为默认、schema 为覆盖层，文档对齐），与 store.md §4 联动。

---

## 4. SchemaManager + 工厂 + 三层加载

Go（已核实）：
- `SchemaManager`：load（内置 `exeDir/schemas/` + 用户 `dataDir/schemas/` 同 ID 深拷贝叠加 + `schema_overrides.toml` 全局覆盖按 dict id patch）/ get / list / active；文件 `<id>.schema.toml` 优先 `.yaml`。
- `CreateEngineFromSchema`(factory.go:122) → 按 type 分 createCodeTable/Pinyin/Mixed，返回 `EngineBundle{SchemaID, Engine, SystemLayer, ExtraLayers}`。混输递归构建主码表+次拼音子引擎（spec 优先自身回退 primary/secondary）。

Rust 目标：`SchemaManager`（扫描/合并/激活）+ `build_engine` 消费富 Schema 产出 enum Engine + 层。三层合并（内置/用户/override）。混输递归构建已在 Rust manager.rs 有雏形，保留。

---

## 5. Config 合并 / 保存 / 迁移决策

1. **合并改 deep-merge `toml::Value`**：把默认/系统/用户三层的 `toml::Value` 表深合并，再 `try_into()` **一次性反序列化**——自动覆盖所有字段，**消除手写逐字段合并与漏字段 bug**，无维护负担。（对齐 Go 的"部分覆盖"效果，但 serde 原生、无 TOML→YAML 桥接。）
2. **保存只写 diff**：对比 (默认+系统) base 与当前，仅写差异（对齐 Go yamldiff）。
3. **版本 + 迁移框架从一开始就有**：顶层 `version`，迁移链按版本执行（即使当前无迁移步骤，预留框架）。
4. Go 用户配置可直接读：Go 现在也是 TOML、段结构相近，Rust 大体可直接加载 Go 的 config.toml（schema 文件需适配）——降低老用户迁移成本（与 store bbolt→redb 数据导入是两回事）。

---

## 6. compat / runtime_state / schema_overrides（阶段 C，配合 coordinator）
- **AppCompat**：per-process 规则（caret_use_top / skip_caret_pending / pin_candidate_position / host_render / 强制标点），按进程名匹配——对应 coordinator.md 的敏感字段/光标定位/应用级行为。**字段级合并**（修 Go 整体替换坏设计）。
- **RuntimeState**：last 中英文/全半角/标点、引擎类型、工具栏位置、候选固定位；与 `remember_last_state` 的关系（位置类始终持久化）。
- **schema_overrides**：每方案覆盖全局配置项——Rust 用**类型化**覆盖（修 Go 的 `map[string]any` 无校验 + 合并散落调用方）。

---

## 7. 路径（保留 Rust，补两项）
- 保留 Rust 现有漫游/本机/缓存/日志分离 + debug 变体隔离（近期成果）。
- 补：**便携模式**（exe 旁 marker → userdata）、**自定义数据目录**（datadir.conf）、路径合法性校验（设置界面用）——阶段 D。
- Rust 不需要 Go 的 legacy `.yaml` 双读回退（Rust 全程 TOML）。

---

## 8. Go 坏设计（不照搬）
1. TOML→map→YAML→struct 双序列化桥接 → Rust deep-merge toml::Value 一次反序列化。
2. plain bool 与 *bool 混用（无法区分未设置/false）→ 一律 Option<bool>。
3. EngineBundle.Engine `interface{}` 需类型断言 → enum。
4. createMixedEngine 380 行 + 重复"自身 spec 否则回退 primary" → 抽 helper/merge。
5. deepCopySchema 一律走 YAML → Rust `#[derive(Clone)]` 原生。
6. schema_overrides `map[string]any` 无类型 + 合并在调用方 → 类型化覆盖。
7. compat 同进程整体替换（丢 base 字段）→ 字段级合并。
8. 词频默认值两套源不一致 → 唯一真值源。
9. PagerBarDisplay 用 "" 作合法枚举（未设置/空三态不分）→ Option 或显式 variant。
10. keypaths 生成的 ident 不做 initialism（UiThemeName）→ Rust N/A。

## 9. Rust 现状要保留的优点
- 路径方案（漫游/本机/缓存/日志 + debug 变体）——近期扎实成果。
- 全程 TOML（无 Go 的 legacy YAML 双读包袱）。
- hotkey.rs 编译（CompiledHotkeys/Compiler/parse_hotkey/select_key_vks 较完整）。

---

## 10. 落地顺序 + 跨文档依赖
1. **Config 合并修复**（§5.1）：deep-merge toml::Value——**快、高价值**（修用户配置静默失效），尽早做。
2. **统一富 Schema + 删死脚手架**（§2/§3）：建丰富 Schema 类型 + 引擎消费它。**这是阶段 B 质量特性的前置**——每落地一个 engine/dict/store 特性，同步加它的 spec 字段（auto_commit/fuzzy/weight_mode/freq 参数…）。
3. **版本+迁移框架**（§5.3）：随 1 一起。
4. **compat/runtime_state/schema_overrides**（§6）：阶段 C，配合 coordinator pipeline。
5. **便携/datadir/校验**（§7）：阶段 D。

> 与 engine/dict/store/coordinator 四份差分共同构成阶段 A 全图：schema 是配置面、engine/dict/store 是质量核心、coordinator 是交互统一。每步 `wind_input/scripts/dev.sh ci` 把关。
