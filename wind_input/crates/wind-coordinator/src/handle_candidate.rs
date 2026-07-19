//! 候选处理：生成 / 过滤 / shadow / 词频重排 / 翻页导航 / 选词上屏 / 右键操作。
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。

use crate::coordinator::{
    Coordinator, DEFERRED_COMPOSITION_FALLBACK_MS, InputOutcome, LEARN_ADD_WEIGHT,
    PHRASE_WEIGHT_BASE, State, now_unix_secs,
};
use crate::pipeline::ModeKind;
use crate::preedit_cursor;
use tracing::{debug, warn};
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_candidate::{Candidate, CandidateMeta, CandidateSource};
use wind_config::hotkey;
use wind_keys::keymap;
use wind_store::freq::FreqRecord;
use wind_ui::manager::CandidateOp;

/// 候选统一层级排序（合并引擎候选 + 短语后的呈现序，与所选 `base_sort` 模式**同维度**）：
/// ① 非模糊优先于模糊；② 精确优先于前缀补全（`is_prefix`）；③ 完整匹配优先于子短语（`is_partial`）；
/// ④ 同层内按权重降序（`ignore_weight` 时跳过）；⑤ 词库基序（`base_order`）升序；⑥ 自然序升序。
///
/// - `is_partial` 不可少：混输 ÷100 压缩权重后，高权重子串单字（平 w=58 is_partial=true）会靠
///   weight 反超低权重精确词组（平摊 w=4 is_partial=false）；且须在 `is_prefix` 之后（对齐 PinyinEngine）。
/// - `base_order` 不可少：与引擎 `candidate::better`/`by_natural` 一致——`natural_order` 是**每库局部
///   出现序**（各库从 0 起），只能在同 `base_order` 档内当 tiebreaker；跨库直接比会让小库靠前词条
///   （如一简次选库「有时」no=24）反超主库深处词条（如「一」no=57285）。`base_order` 隔离这种跨库
///   比较，必须排在 `natural_order` 之前（对齐引擎 weight→base_order→natural_order 分层）。
/// - `ignore_weight`：`base_sort = "natural"` 时为 true——引擎的 `by_natural` **完全忽略权重**，纯按
///   base_order→natural_order 呈现；协调器须同样跳过 weight 维度，否则合并短语后重排会与引擎发散
///   （如 natural 模式下高权重次选库条目仍会靠 weight 反超低权重主库条目）。此时短语仍靠其
///   base_order/natural_order 默认 0 浮于顶部。
///
/// 排序规则：Exact >> Sub-phrase >> Prefix >> Fuzzy。
fn candidate_display_order(
    a: &Candidate,
    b: &Candidate,
    ignore_weight: bool,
) -> std::cmp::Ordering {
    let by_weight = if ignore_weight {
        std::cmp::Ordering::Equal
    } else {
        b.weight.cmp(&a.weight)
    };
    wind_candidate::cmp_match_layers(a, b)
        .then(by_weight)
        .then(a.base_order.cmp(&b.base_order))
        .then(a.natural_order.cmp(&b.natural_order))
}

/// 自动上屏最短码长的归一（纯函数）：配置 0 = 跟随全码长。
///
/// 复刻引擎侧 `CodeTableEngine::new` 的同名归一——那份藏在引擎构造函数里、只作用于其私有
/// `opts`，协调器取不到，故短语侧须在此重算。两处语义必须一致。
///
/// `max_code_length` 为 0（拼音等无「全码」概念的引擎，见 `Engine::max_code_length` 默认实现）
/// 时结果为 0 → 调用方的 `len < 0` 恒假 → 不设闸，与引擎侧同构降级。
fn resolve_auto_commit_min_len(configured: usize, max_code_length: usize) -> usize {
    if configured > 0 {
        configured
    } else {
        max_code_length
    }
}

impl Coordinator {
    /// 记录一次选词到 redb FREQ（词频维度：count+1、last_used=now，按 schema+code+text）。
    /// 词频是与权重解耦的独立维度（frequency.md），仅记真实使用数据；redb 事务即时持久。
    /// 未开启「自动调频」（`schema.codetable/pinyin.frequency.enabled`）时不记录，避免关闭功能后仍写库。
    pub(crate) fn record_selection(&self, code: &str, text: &str, source: CandidateSource) {
        if text.is_empty() {
            return;
        }
        // 上屏历史（命令栏 last(n) 用）：最近置前，限 16 条。
        {
            let mut h = self
                .recent_commits
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            h.push_front(text.to_string());
            if h.len() > 16 {
                h.truncate(16);
            }
        }
        if let Some(store) = &self.store {
            // 未开启「自动调频」则不记录（配置说关、代码却记的潜在 bug，对齐 apply_freq_rerank 的开关检查）。
            if !self.engine_mgr.freq_settings().enabled {
                return;
            }
            let schema = self.engine_mgr.active_schema_id();
            // 归属 id：非混输折叠自身/拼音；混输按候选来源分流，无法归因则跳过本次记频。
            let Some(schema) = self.engine_mgr.write_data_schema_id(&schema, source) else {
                return;
            };
            if let Err(e) = store.record_freq(&schema, code, text) {
                warn!("record_freq failed: {}", e);
            }
        }
    }

    /// 词频重排（独立维度，**绝不改 weight**）：按 redb 词频记录做档位感知的 used-first 稳定
    /// 重排——用过的候选（count>0）按策略上浮，未用候选保持基础(权重)序。对齐 frequency.md §3。
    ///
    /// 策略（engine.codetable.freq_strategy）：
    /// - `step`（默认/逐次提升）：count 降序、last_used 降序 tiebreak（累积使用才爬升，抗误选）。
    /// - `top`（一次到顶/MRU）：last_used 降序、count 降序 tiebreak（最近选的置该档之首）。
    ///
    /// 主开关 `learning.freq.enabled` 关闭则完全不重排（修"配置说关、代码却排"的潜在 bug）。
    /// 引擎类型分流：码表/混输走永久 used-first（§3），纯拼音走衰减软置前（§4）。
    /// 注：每候选一次 redb 点查（mmap 微秒级）；后续可下沉到引擎排序层。
    pub(crate) fn apply_freq_rerank(&self, candidates: &mut [Candidate], code: &str) {
        let Some(store) = &self.store else {
            return;
        };
        if code.is_empty() || candidates.len() < 2 {
            return;
        }
        let settings = self.engine_mgr.freq_settings();
        if !settings.enabled {
            return;
        }
        let active = self.engine_mgr.active_schema_id();
        // 归属方案解析：非混输单次折叠（现行为，零回归）；混输预解析两个子方案归属 id，
        // 循环内按候选来源选用（热路径纪律：非混输不走逐候选分支）。
        let is_mixed = self.engine_mgr.schema_engine_type(&active).as_deref() == Some("mixed");
        let schema = self.engine_mgr.data_schema_id(&active); // 非混输：拼音族折叠到 "pinyin"
        let (ct_id, py_id) = if is_mixed {
            (
                self.engine_mgr
                    .write_data_schema_id(&active, CandidateSource::CodeTable),
                self.engine_mgr
                    .write_data_schema_id(&active, CandidateSource::Pinyin),
            )
        } else {
            (None, None)
        };
        let input_len = code.len();
        // 取每个"消费整串"候选的词频记录。分段子候选（consumed_length < 整串，如「nihao」里的「你」
        // 只消费「ni」）的词频归属其自身前缀码，不能被整串码的历史计数上浮——否则单字会浮到整句
        // 「你好」之上。consumed_length==0 表示引擎未标注（码表型），视为整串匹配。
        let recs: std::collections::HashMap<String, FreqRecord> = candidates
            .iter()
            .filter_map(|c| {
                let consumes_all = c.consumed_length == 0 || c.consumed_length >= input_len;
                if !consumes_all {
                    return None;
                }
                // 混输按候选来源读子方案键空间（无法归因跳过）；非混输用统一 schema。
                let sid: &str = if is_mixed {
                    match c.source {
                        CandidateSource::CodeTable => ct_id.as_deref()?,
                        CandidateSource::Pinyin => py_id.as_deref()?,
                        _ => return None,
                    }
                } else {
                    &schema
                };
                match store.get_freq(sid, code, &c.text) {
                    Ok(Some(r)) if r.count > 0 => Some((c.text.clone(), r)),
                    _ => None,
                }
            })
            .collect();
        if recs.is_empty() {
            return;
        }
        // 词频重排归属 engine 排序层（frequency.md §5/§7）：本协调器只负责取词频记录、按引擎
        // 类型分流到纯函数。码表/混输永久 used-first（§3），纯拼音衰减软置前（§4）。
        if self.engine_mgr.is_pinyin() {
            let profile = self.engine_mgr.pinyin_freq_profile();
            wind_engine::freq_rerank::rerank_pinyin_decay(
                candidates,
                &recs,
                now_unix_secs(),
                profile,
            );
        } else {
            wind_engine::freq_rerank::rerank_codetable_usedfirst(
                candidates,
                &recs,
                code,
                settings.strategy,
                settings.protect_top_n,
            );
        }
    }

    /// 候选总数（测试/诊断用）
    pub fn debug_candidate_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .candidates
            .len()
    }

    /// 全部候选文本列表（不分页；测试/诊断用）
    pub fn debug_all_candidate_texts(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .candidates
            .iter()
            .map(|c| c.text.clone())
            .collect()
    }

    /// 候选词条操作（测试/诊断用）
    pub fn debug_candidate_op(&self, op: CandidateOp, page_local: usize) {
        self.candidate_op(op, page_local);
    }

    /// 首次加载候选上限（对齐 Go：短前缀小批量分级加载，长前缀近全量）。
    pub(crate) fn initial_candidate_limit(&self, input: &str) -> usize {
        let len = input.chars().count();
        match self.engine_mgr.current_engine_type() {
            Some(wind_engine::engine::EngineType::CodeTable) => match len {
                0 | 1 => 100,
                2 => 300,
                _ => 1000,
            },
            // 拼音 / 混输
            _ => 300,
        }
    }

    /// 用给定上限转换并构建候选（引擎 + 词频 boost + 短语 + 排序去重）。
    /// 返回引擎候选数（不含短语），供判断 has_more。不复位翻页/高亮。
    /// 返回 (引擎候选数, 输入结局)。结局含全码自动上屏 / 满码空码清空；自动上屏文本经
    /// shadow 复核后才放行，避免上屏被置顶删词移除的候选。调用方仅在「正向输入字母」时消费。
    /// 词库候选 value 内嵌特殊语法（`$CC` 命令 / `$Y` 模板 / `$AA`·`$SS` 组 / `{..}` 插值）的
    /// **统一展开汇聚点**。所有候选生成路径（正常 / 特殊模式 / 混输 overlay / 临拼 / 临英）在写入
    /// `state.candidates` 前均须过此点，保证 `$` 语法在全部输入方案一致生效（对齐 Go
    /// `dict.ValueExpander`；见 docs/redesign/unified-candidate-value-expansion.md）。
    ///
    /// - `$CC` → 标 `is_command`（选中由 `select_candidate`、顶屏由 `top_commit_command_guard` 执行动作）；
    /// - `$AA`/`$SS` 组 → **精确码**（候选码 == 当前输入）时逐成员炸开；**前缀**（候选码更长）时折叠为
    ///   单个组名候选（`is_group`，`group_code` = 完整码），选中经 `complete_to_group_code` 补全到完整码
    ///   重查 → 精确 → 展开（二级选择，与短语前缀分组一致）；
    /// - 模板 / 花括号插值 → 直接以展开文本上屏；
    /// - 普通候选（不含 `$` 与 `{`）经廉价预检零开销原样返回。
    ///
    /// `input` 为该路径当前编码缓冲（供 cmdbar 语法内 `input()` 求值）。已是 `is_phrase`/`is_command`
    /// 的候选（短语命中）跳过二次展开。
    pub(crate) fn finalize_candidates(&self, raw: Vec<Candidate>, input: &str) -> Vec<Candidate> {
        // 快路径：无任一候选含特殊语法（普通词库/拼音结果）→ 零拷贝原样返回。
        if !raw.iter().any(|c| {
            !c.is_phrase && !c.is_command && (c.text.contains('$') || c.text.contains('{'))
        }) {
            return raw;
        }
        let now = chrono::Local::now();
        let recent = self.recent_commits_snapshot();
        let clip = |_n: i64| -> String {
            #[cfg(windows)]
            {
                wind_ui::popup_menu::get_clipboard_text()
            }
            #[cfg(not(windows))]
            {
                String::new()
            }
        };
        let mut expanded: Vec<Candidate> = Vec::with_capacity(raw.len());
        for cand in raw.into_iter() {
            if cand.is_phrase || cand.is_command {
                expanded.push(cand);
                continue;
            }
            match wind_phrase::expand_dict_value(&cand.text, input, now, &recent, &clip) {
                wind_phrase::DictExpansion::None => expanded.push(cand),
                wind_phrase::DictExpansion::Single {
                    display,
                    command_src,
                } => {
                    let mut c = cand;
                    c.text = display;
                    if let Some(src) = command_src {
                        c.phrase_template = src;
                        c.is_command = true;
                    }
                    expanded.push(c);
                }
                wind_phrase::DictExpansion::Group { name, items } => {
                    // 精确码（候选码 == 输入，或引擎未给码信息）→ 逐成员炸开；
                    // 前缀（候选码更长）→ 折叠为组名候选，选中补全到完整码再展开。
                    if cand.code.is_empty() || cand.code == input {
                        for (display, command_src) in items {
                            let mut c = cand.clone();
                            c.text = display;
                            if let Some(src) = command_src {
                                c.phrase_template = src;
                                c.is_command = true;
                            }
                            expanded.push(c);
                        }
                    } else {
                        let mut g = cand;
                        g.group_code = g.code.clone();
                        g.group_name = name.clone();
                        g.group_template = g.text.clone(); // 源 $AA/$SS(..) 备查
                        g.text = name;
                        g.is_group = true;
                        expanded.push(g);
                    }
                }
            }
        }
        expanded
    }

    pub(crate) fn build_candidates(
        &self,
        state: &mut State,
        limit: usize,
    ) -> (usize, InputOutcome) {
        // 分段上屏进行中（committed 前缀非空 ⟺ 来自拼音选词——五笔候选 consumed_length=0
        // 永不部分匹配）：剩余编码强制按混输方案的拼音子方案转换，避免混输让五笔抢首选
        // （你↑选后 hao→虚）。拼音方案 id 取当前混输方案的 [engine.mixed].secondary_schema
        // （如 wubi86_pinyin → "pinyin"）。注意不用全局 primary_pinyin——那是给「临时拼音↔
        // 临时双拼」切换用的，对混输不适用。
        let pinyin_schema = if !state.committed_text.is_empty() {
            let active = self.engine_mgr.active_schema_id();
            self.engine_mgr
                .schema_merged(&active)
                .map(|s| s.engine.mixed.secondary_schema.clone())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        let result = match pinyin_schema {
            Some(ps) if self.engine_mgr.ensure_schema(&ps) => {
                self.engine_mgr
                    .convert_with(&ps, &state.input_buffer, limit)
            }
            _ => self.engine_mgr.convert(&state.input_buffer, limit),
        };
        // 拼音音节拆分形态（供「混输高亮跟随」按高亮候选类型选择显示原始码 / 拆分串）。
        // 码表 / 无拼音 → 空串（恒原始码）。state.preedit 本身由 sync_preedit_to_highlight
        // 按高亮重算（见 update_candidates 末尾 / apply_nav_key）。
        state.preedit_split_body = result.preedit_pinyin.clone();
        let engine_count = result.candidates.len();
        // 引擎给出的全码自动上屏意向（基于引擎候选；下方 shadow 后复核存活性）。
        let auto_commit = if result.should_commit && !result.commit_text.is_empty() {
            Some(result.commit_text.clone())
        } else {
            None
        };
        let should_clear = result.should_clear;

        // 词库候选 value 内嵌特殊语法统一展开（汇聚点：所有路径共用，见
        // finalize_candidates / docs/redesign/unified-candidate-value-expansion.md）。
        let mut candidates = self.finalize_candidates(result.candidates, &state.input_buffer);
        let phrases = self.phrases.read().unwrap_or_else(|e| e.into_inner());
        if !phrases.is_empty() {
            let recent = self.recent_commits_snapshot();
            // 剪贴板读取回调注入 wind-phrase（其不依赖平台 UI 层）：精确码命令 display
            // 含 {clip()}（如 coad）时按需读取；非 windows 返回空。
            let clip = |_n: i64| -> String {
                #[cfg(windows)]
                {
                    wind_ui::popup_menu::get_clipboard_text()
                }
                #[cfg(not(windows))]
                {
                    String::new()
                }
            };
            for hit in phrases.lookup(&state.input_buffer, &recent, &clip) {
                let is_command = hit.command_src.is_some();
                let is_system = hit.is_system;
                candidates.push(Candidate {
                    // text 存完整原文（仅一行化，不截断）——上屏须用原始文本，超长省略号截断
                    // 移到 UI 下发层（见 coordinator 候选映射）。传 0 表示不限长度。
                    text: Self::clamp_candidate_display(&hit.text, 0),
                    weight: PHRASE_WEIGHT_BASE + hit.weight,
                    is_phrase: true,
                    // $CC 命令短语：标记 is_command，phrase_template 暂存命令源；
                    // 选中时由 commit_selected 拦截，执行动作而非上屏 display 标签。
                    // 非命令短语 phrase_template 存原始记录文本（source_text，模板未展开），
                    // 供右键「禁用短语」按 (code, 原文) 定位 store 记录（对齐 Go PhraseTemplate）。
                    is_command,
                    phrase_template: hit.command_src.unwrap_or(hit.source_text),
                    meta: CandidateMeta {
                        is_system_phrase: is_system,
                        ..Default::default()
                    },
                    ..Default::default()
                });
            }
            // 前缀导航：敲 `zz`/`co` 等前缀（长度 ≥ min_prefix_length）列出所有该前缀的
            // marker 短语。**$CC 命令** → is_command（选中直接执行，group_code 作执行输入
            // 上下文）；**$SS/$AA 组** → is_group（选中补全到完整码再展开成员，二级选择）。
            let min_prefix = self.rt().config.input.phrase.min_prefix;
            // 精确匹配模式（`single_code_input`，仅纯码表方案）：默认抑制短语前缀枚举，只保留上面的
            // 精确码短语（`lookup`）——与码表引擎跳过 `search_prefix` 的行为对齐。混输不适用：其拼音半边
            // 恒前缀匹配，切精确会与拼音割裂（见 `EngineManager::is_codetable`）。
            // 例外——镜像码表引擎 `single_code_complete`：当前无任何候选（码表 + 精确短语均空）且未满码时，
            // 放行一次前缀枚举作补全，避免精确模式下彻底无候选（对齐引擎"精确空码取更长首选"语义）。
            let ct = self.engine_mgr.codetable_settings();
            let exact_only = self.engine_mgr.is_codetable() && ct.single_code_input;
            // 空码补全兜底：仅在精确模式抑制了前缀枚举时才可能触发。
            let complete_fallback = exact_only
                && ct.single_code_complete
                && candidates.is_empty()
                && state.input_buffer.chars().count() < self.engine_mgr.active_max_code_length();
            let mut prefix_hits = if !exact_only || complete_fallback {
                phrases.lookup_prefix(&state.input_buffer, &recent, min_prefix)
            } else {
                Vec::new()
            };
            if complete_fallback {
                // 补全**仅取首选一条**：引擎侧同分支只 push 一条（search_prefix().find()，
                // 见 codetable/engine.rs "空码补全：从更长编码取首个候选作提示"）。短语侧原样
                // 放行整串前缀命中，致精确模式下空码补全冒出多条「后续」，与码表规格分叉。
                //
                // 须先定序再取：lookup_prefix 由 HashMap 遍历产出、顺序不定（见 wind-phrase
                // lookup_prefix_at），直接 take(1) 取到的是随机一条而非首选。权重降序 + 文本
                // 兜底保证稳定；协调器后续的整体排序不改变「只有一条」这个事实。
                prefix_hits
                    .sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.text.cmp(&b.text)));
                prefix_hits.truncate(1);
            }
            for hit in prefix_hits {
                // 完整原文（仅一行化，不截断）：上屏用原始文本，截断加省略号由 UI 下发层负责。
                let text = Self::clamp_candidate_display(&hit.text, 0);
                let is_system = hit.is_system;
                let phrase_meta = || CandidateMeta {
                    is_system_phrase: is_system,
                    ..Default::default()
                };
                if let Some(src) = hit.command_src {
                    // $CC 命令短语：选中直接执行，不二级展开。
                    let code = hit.nav_code.unwrap_or_default();
                    candidates.push(Candidate {
                        text,
                        weight: PHRASE_WEIGHT_BASE + hit.weight,
                        is_phrase: true,
                        is_command: true,
                        phrase_template: src,
                        group_code: code,
                        comment: hit.comment,
                        meta: phrase_meta(),
                        ..Default::default()
                    });
                } else if let Some(code) = hit.nav_code {
                    // $SS/$AA 组短语：选中补全到完整码再二级展开。
                    // phrase_template 存原始记录文本：右键「禁用短语」按 (group_code, 原文) 定位。
                    candidates.push(Candidate {
                        text: text.clone(),
                        weight: PHRASE_WEIGHT_BASE + hit.weight,
                        is_phrase: true,
                        is_group: true,
                        group_code: code,
                        group_name: text,
                        comment: hit.comment,
                        phrase_template: hit.source_text,
                        meta: phrase_meta(),
                        ..Default::default()
                    });
                } else {
                    // 静态短语前缀命中（Literal/Template，command_src=None, nav_code=None）。
                    candidates.push(Candidate {
                        text,
                        weight: PHRASE_WEIGHT_BASE + hit.weight,
                        is_phrase: true,
                        is_prefix: true,
                        comment: hit.comment,
                        phrase_template: hit.source_text,
                        meta: phrase_meta(),
                        ..Default::default()
                    });
                }
            }
        }
        drop(phrases);
        // 候选层级排序：合并引擎候选 + 短语后按统一层级重排（见 `candidate_display_order`）。
        // base_sort=natural 时忽略权重，对齐引擎 by_natural（否则合并短语后重排会与引擎发散）。
        let ignore_weight = self.engine_mgr.active_base_sort_ignores_weight();
        candidates.sort_by(|a, b| candidate_display_order(a, b, ignore_weight));
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.text.clone()));
        // 检索范围过滤（填充常用标志后按模式过滤；对齐 Go 引擎内过滤）
        self.apply_filter(state, &mut candidates);
        // 用户词频重排（独立维度，used-first，绝不改 weight；frequency.md §3）
        self.apply_freq_rerank(&mut candidates, &state.input_buffer);
        // Shadow 规则：删除过滤 + 置顶/移动重排（优先级最高，排序后应用）
        self.apply_shadow(&mut candidates, &state.input_buffer);
        state.candidates = candidates;
        // 满码自动上屏「显示态」复评：引擎按未过滤候选判唯一（生僻同码字致不唯一被否决），
        // 但智能过滤后可能只剩唯一精确全码码表候选 → 据显示候选复评放行（逻辑与显示一致）。
        // 惰性：仅在引擎未给出上屏意向时复评。
        let auto_commit = auto_commit.or_else(|| {
            self.engine_mgr
                .recheck_auto_commit(&state.input_buffer, &state.candidates)
        });
        // 复核：仅当上屏目标在最终候选中仍存在（未被 shadow 删除）才放行自动上屏。
        // 词库 `$CC` 命令词条经 finalize_candidates 展开后 text 已改写为 display 标签，而引擎
        // 意向 commit_text 是原始 `$CC` 源 → 按 phrase_template 补匹配（否则意向恒被误否决）。
        let outcome = match auto_commit.filter(|t| {
            state
                .candidates
                .iter()
                .any(|c| &c.text == t || (c.is_command && &c.phrase_template == t))
        }) {
            Some(_) => {
                // 一致性：自动上屏文本取「实际显示的首候选」，与空格/点选同源，杜绝
                // "显示藏、全码上屏駏"的漂移（首候选已由档位排序保证是五笔精确全码）。
                // 守护：仅当显示首选是**码表来源**时才自动上屏；若显示首选是拼音/英文（被 shadow
                // 置顶，或码表精确字被智能过滤后仅剩拼音），则不自动上屏——上屏须与显示一致、
                // 非码表类不上屏，留给用户继续选。
                match state.candidates.first() {
                    // 词库 `$CC` 命令词条：纯文本求值上屏 / 含副作用异步执行（与短语命令同分流）。
                    Some(c) if c.is_command && c.source == CandidateSource::CodeTable => {
                        self.command_auto_outcome(c, &state.input_buffer)
                    }
                    Some(c) if c.source == CandidateSource::CodeTable => {
                        InputOutcome::AutoCommit(c.text.clone())
                    }
                    _ => InputOutcome::Normal,
                }
            }
            // 满码空码清空：`should_clear` 由码表引擎在追加短语**之前**计算（仅看码表候选）。
            // 协调器随后可能追加短语候选（zzbd 等短语专属码：码表无字但短语命中），故此处须以
            // 叠加短语后的最终候选复查——`state.candidates` 非空即不清，避免误清短语列表。
            None if should_clear && state.candidates.is_empty() => InputOutcome::Clear,
            None => InputOutcome::Normal,
        };
        // 短语自动上屏：码表未给出上屏意向（Normal）时，补齐短语侧——引擎判据看不到短语，
        // 唯一精确码短语 + 无更长后继时也应自动上屏（与码表「全码唯一自动上屏」对齐）。
        let outcome = match outcome {
            InputOutcome::Normal => self
                .phrase_auto_commit(state)
                .unwrap_or(InputOutcome::Normal),
            other => other,
        };
        (engine_count, outcome)
    }

    /// 短语自动上屏（`schema.codetable.auto_commit_at_full` 开启时）：当前输入的**唯一**候选是
    /// 精确码短语，且**无更长后继**（码表前缀扫描 + 短语码前缀扫描）→ 自动上屏。引擎的
    /// `decide_auto_commit` 只认码表候选（短语在引擎 convert 后由协调器追加、且候选 `code` 为空），
    /// 故短语从不进码表判据；此处补齐短语侧，判据与码表「全码唯一自动上屏」同构。
    ///
    /// - 普通短语 → 直接上屏其文本；
    /// - 纯文本命令（`$CC` 仅 `type` 文本、无副作用）→ 同步求值上屏其文本（与顶码 `eval_command_text_only` 同路）；
    /// - 含副作用命令 → [`InputOutcome::AutoCommand`]：清组合并异步执行（与空格选中命令同语义）；
    /// - `$SS`·`$AA` 组 / 前缀枚举短语 → 排除（不自动上屏，避免误展开/打断输入）。
    ///
    /// 门槛为「最短码长 + 唯一 + 无更长后继（含短语）」四闸串联，与引擎 `decide_auto_commit`
    /// 同构——两道缺一不可：`min_len` 管「够不够满码」，`has_longer_code` 管「还能不能接着打」。
    pub(crate) fn phrase_auto_commit(&self, state: &State) -> Option<InputOutcome> {
        let ct = self.engine_mgr.codetable_settings();
        if !ct.auto_commit_at_full {
            return None;
        }
        // 最短码长闸：与引擎 decide_auto_commit 的 `input.chars().count() < min_len` 同构。
        // 短语此前不设此闸，致 3 码短语（如 ocd）在 4 码方案里绕过「满码」语义直接上屏/执行。
        if state.input_buffer.chars().count() < self.phrase_auto_commit_min_len(&ct) {
            return None;
        }
        // 唯一候选。
        let [c] = &state.candidates[..] else {
            return None;
        };
        // 精确码短语（非前缀枚举 / 非组）。命令留待下方按纯文本/副作用分流。
        if !c.is_phrase || c.is_prefix || c.is_group {
            return None;
        }
        let input = &state.input_buffer;
        // 无更长后继：码表 + 短语两侧前缀扫描（避免短码短语打断更长输入）。
        if self.engine_mgr.has_longer_code(input) {
            return None;
        }
        if self
            .phrases
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .has_longer_code(input)
        {
            return None;
        }
        // 命令 → 统一分流（纯文本求值上屏 / 含副作用异步执行）；普通短语 → 直接文本。
        if c.is_command {
            return Some(self.command_auto_outcome(c, input));
        }
        if c.text.is_empty() {
            return None;
        }
        Some(InputOutcome::AutoCommit(c.text.clone()))
    }

    /// 短语自动上屏的最短码长门槛。
    ///
    /// **当前跟随主码表**的 `schema.codetable.auto_commit_min_len`：短语虽是独立体系，但
    /// 「满码自动上屏」的规格应与主码表一致，否则同一个 `auto_commit_at_full` 开关下短语与
    /// 码表行为分叉（原 bug：3 码短语在 4 码方案里直接上屏）。
    ///
    /// 预留：日后若要给短语独立门槛（如 `schema.phrase.auto_commit_min_len`），只需改本方法
    /// 的取值来源，`phrase_auto_commit` 的判据结构无需改动。
    fn phrase_auto_commit_min_len(&self, ct: &wind_config::CodetableGlobal) -> usize {
        resolve_auto_commit_min_len(
            ct.auto_commit_min_len,
            self.engine_mgr.active_max_code_length(),
        )
    }

    /// `$CC` 命令候选的自动上屏结局分流（短语命令 / 词库命令词条共用）：
    /// - 纯文本命令（动作链全 Text）→ 同步求值其文本 [`InputOutcome::AutoCommit`]；
    /// - 含副作用命令 → [`InputOutcome::AutoCommand`]（消费点经 `commit_command` 清组合 +
    ///   独立线程异步执行——Effect 回调 coordinator 自锁方法，此刻持 state 锁不可同步跑）；
    /// - 求值文本为空 → Normal（无可上屏内容，继续组合）。
    pub(crate) fn command_auto_outcome(&self, c: &Candidate, input: &str) -> InputOutcome {
        match self.eval_command_text_only(&c.phrase_template, input) {
            Some(t) if !t.is_empty() => InputOutcome::AutoCommit(t),
            Some(_) => InputOutcome::Normal,
            None => InputOutcome::AutoCommand(Box::new(c.clone())),
        }
    }

    /// 按当前检索范围过滤候选：先填充 is_common（常用字表），再按模式过滤。
    /// Gb18030 或数据缺失时不过滤（避免误删）。
    pub(crate) fn apply_filter(&self, state: &State, candidates: &mut Vec<Candidate>) {
        let mode = state.filter_mode;
        if mode == wind_candidate::FilterMode::Gb18030 || self.common_chars.is_empty() {
            return;
        }
        for c in candidates.iter_mut() {
            // 短语保留（is_phrase 已置位）；其余按常用字表判定
            if !c.is_phrase {
                c.is_common = self.common_chars.is_string_common(&c.text);
            }
        }
        let taken = std::mem::take(candidates);
        *candidates = wind_candidate::filter_candidates(taken, mode);
    }

    /// 应用 Shadow 规则：先按 deleted 过滤，再把 pinned 按目标位置重排。
    pub(crate) fn apply_shadow(&self, candidates: &mut Vec<Candidate>, code: &str) {
        if code.is_empty() {
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        // 候选调整按 data_schema_id 归属（拼音族折叠共享；码表/混输各自独立）。
        let schema = self
            .engine_mgr
            .data_schema_id(&self.engine_mgr.active_schema_id());
        let rec = match store.get_shadow_rules(&schema, code) {
            Ok(Some(r)) => r,
            _ => return,
        };
        // 纯重排逻辑下沉 wind_candidate（用元组解耦，避免该 crate 依赖 wind-store）。
        let pinned: Vec<(String, usize)> = rec
            .pinned
            .iter()
            .map(|p| (p.word.clone(), p.position))
            .collect();
        wind_candidate::apply_shadow(candidates, &rec.deleted, &pinned);
    }

    /// 根据输入缓冲更新候选（动态分级加载：首次小批量，翻页到边界再扩展）。
    /// 返回输入结局（全码自动上屏 / 满码空码清空）；多数调用方忽略，仅正向输入字母时消费。
    pub(crate) fn update_candidates(&self, state: &mut State) -> InputOutcome {
        state.candidates.clear();
        state.preedit = state.input_buffer.clone();
        state.preedit_split_body.clear();
        if state.input_buffer.is_empty() {
            state.has_more = false;
            state.candidate_input.clear();
            // 缓冲空但有已转换前缀（逐步转换中删空剩余拼音）：组合区仍显示前缀。
            state.preedit = state.committed_text.clone();
            return InputOutcome::Normal;
        }
        let limit = self.initial_candidate_limit(&state.input_buffer);
        let (engine_count, outcome) = self.build_candidates(state, limit);
        // Z 键重复上屏：输入恰为 "z" 且当前方案启用 z_key_repeat 时，把最近一次上屏内容作为
        // 首选候选注入到候选顶部（对齐 Go），供「z + 选词」重复上一次输入。
        if state.input_buffer == "z"
            && let Some(last) = self.z_key_repeat_text()
        {
            state.candidates.insert(
                0,
                Candidate {
                    text: last,
                    natural_order: -1,
                    ..Default::default()
                },
            );
        }
        state.candidate_input = state.input_buffer.clone();
        state.candidate_limit = limit;
        // 引擎返回数达到上限 → 可能还有更多未加载
        state.has_more = engine_count >= limit;
        // 候选变化：复位翻页与高亮（含清除鼠标悬停）
        state.current_page = 0;
        state.selected_index = 0;
        state.hover_index = -1;
        // 组合区按高亮候选类型重算（混输高亮跟随；含已转换前缀拼接）。
        self.sync_preedit_to_highlight(state);
        outcome
    }

    /// Z 键重复上屏：当前方案（码表/混输）启用 z_key_repeat 时返回最近一次上屏文本，否则 None。
    /// 混输继承主码表行为，故码表/混输统一读有效码表配置（全局 schema.codetable + 方案 override）。
    pub(crate) fn z_key_repeat_text(&self) -> Option<String> {
        let enabled = match self.engine_mgr.current_engine_type() {
            Some(wind_engine::EngineType::CodeTable) | Some(wind_engine::EngineType::Mixed) => {
                self.engine_mgr.codetable_settings().z_key_repeat
            }
            _ => false,
        };
        enabled
            .then(|| self.recent_commits_snapshot().into_iter().next())
            .flatten()
    }

    /// 扩展候选（翻页/下移到边界时调用）：上限翻倍（≤5000）重新加载，保持当前页/高亮。
    pub(crate) fn expand_candidates(&self, state: &mut State) {
        if !state.has_more || state.candidate_input != state.input_buffer {
            return;
        }
        let new_limit = (state.candidate_limit.saturating_mul(2)).min(5000);
        if new_limit <= state.candidate_limit {
            state.has_more = false;
            return;
        }
        let prev_len = state.candidates.len();
        // 翻页扩展不消费全码自动上屏（仅正向输入字母时才上屏）。
        let (engine_count, _) = self.build_candidates(state, new_limit);
        if state.candidates.len() <= prev_len {
            // 没有新增 → 已到底
            state.has_more = false;
            return;
        }
        state.candidate_limit = new_limit;
        state.has_more = engine_count >= new_limit;
        // 保持当前页/高亮不变（build_candidates 未改动它们）；按当前高亮重算组合区
        // （输入/高亮未变 → 形态不变，仅防御性同步）。
        self.sync_preedit_to_highlight(state);
    }

    /// 若 key_code 是配置的二/三候选键，返回页内候选偏移（1=次选/第2项，2=三选/第3项）。
    pub(crate) fn select_key_offset(&self, key_code: u32) -> Option<usize> {
        for group in &self.rt().config.keys.select_key_groups {
            let vks = hotkey::select_key_vks(group);
            if let Some(pos) = vks.iter().position(|vk| *vk == key_code) {
                return Some(pos + 1);
            }
        }
        None
    }

    /// 拼音手动音节分隔符判定的单一入口：`key_code` 是否应作为分隔符 `'` 压入缓冲。
    ///
    /// 每次按键实时求值（不缓存），使 `separator` 或 `select_key_groups` 热更新即时生效。
    /// 规则（对齐 Go `pinyin_mode_shared.go` 真 `auto` 语义）：
    /// - 非拼音引擎 / 双拼方案 → 恒 false（双拼 buffer 会与 preedit 发散）。
    /// - `none` → false；`quote` → 仅引号键(VK_QUOTE)；`backtick` → 仅反引号键(VK_BACKTICK)。
    ///   显式模式尊重用户指定值，不做动态判定（显式 quote 即用户自选覆盖选键行为）。
    /// - `auto`（默认/未知值）→ 动态避让候选选择键：若 `'`(VK_QUOTE) 当前展开为候选选择键
    ///   （`select_key_offset` 命中，默认 `semicolon_quote` 即含 `'`），则保留其选键功能、
    ///   改用反引号键作分隔符；否则 `'` 空闲，作分隔符（此时反引号不作分隔符）。
    ///
    /// 缓冲是否为空由调用方判定（空缓冲维持标点路径）。
    pub(crate) fn pinyin_separator_key(&self, key_code: u32) -> bool {
        use wind_keys::keymap::{VK_BACKTICK, VK_QUOTE};
        if !self.engine_mgr.is_pinyin() || self.engine_mgr.pinyin_is_shuangpin() {
            return false;
        }
        match self.engine_mgr.pinyin_separator_mode().as_str() {
            "none" => false,
            "quote" => key_code == VK_QUOTE,
            "backtick" => key_code == VK_BACKTICK,
            // auto（及其它未知值兜底）：' 被占作选择键 → 反引号作分隔符；否则 ' 作分隔符。
            _ => {
                if self.select_key_offset(VK_QUOTE).is_some() {
                    key_code == VK_BACKTICK
                } else {
                    key_code == VK_QUOTE
                }
            }
        }
    }

    /// 若 key_code 是配置的以词定字键，返回取字下标（0=取第 1 字，1=取第 2 字）。
    /// 默认 `select_char_keys` 为空 → 恒返回 None（功能禁用，零回归）。
    /// 键组须用 select_char_vks 解析（支持 comma_period/minus_equal/brackets）；
    /// select_key_vks 是次/三选键组（不含 brackets），误用会使 brackets 配置静默失效。
    pub(crate) fn select_char_index(&self, key_code: u32) -> Option<usize> {
        for group in &self.rt().config.keys.select_char_keys {
            let vks = hotkey::select_char_vks(group);
            if let Some(pos) = vks.iter().position(|vk| *vk == key_code) {
                return Some(pos);
            }
        }
        None
    }

    /// 当前页候选切片的 [start, end) 区间
    pub(crate) fn page_range(&self, state: &State) -> (usize, usize) {
        let pp = self.per_page(state.active);
        let start = state.current_page * pp;
        let end = (start + pp).min(state.candidates.len());
        (start, end)
    }

    /// 当前高亮候选的全局下标（页起点 + 页内高亮）
    pub(crate) fn highlighted_global_index(&self, state: &State) -> usize {
        let (start, _) = self.page_range(state);
        start + state.selected_index
    }

    /// 组合区「正文」形态选择（不含已转换前缀）：对齐微软五笔——按**当前高亮候选**的类型决定。
    /// - 无拆分形态（码表/无拼音，preedit_split_body 空）→ 恒原始码（input_buffer）。
    /// - 高亮候选为拼音来源 → 音节拆分串（preedit_split_body，如 baoan 的拼音 / saaa 的 sa'a'a）。
    /// - 高亮候选为码表/五笔（或短语等非拼音）→ 原始码（input_buffer，如 saaa 选「模式」时不拆）。
    fn effective_preedit_body<'a>(&self, state: &'a State) -> &'a str {
        if state.preedit_split_body.is_empty() {
            return &state.input_buffer;
        }
        // 分段上屏进行中（committed 前缀非空）：剩余编码已被 build_candidates 强制走拼音方案
        // 转换，故恒按拼音拆分显示，不再按候选来源切换——否则高权重短语候选顶到首位时，
        // 后段会被显示成原始码形态（看似「又以五笔处理」）。与 build_candidates 强制拼音对齐。
        if !state.committed_text.is_empty() {
            return &state.preedit_split_body;
        }
        let hi = self.highlighted_global_index(state);
        let want_split = state
            .candidates
            .get(hi)
            .map(|c| c.source == wind_candidate::CandidateSource::Pinyin)
            // 无候选（极少见）：有拆分形态则倾向拆分（纯拼音空候选边界）。
            .unwrap_or(true);
        if want_split {
            &state.preedit_split_body
        } else {
            &state.input_buffer
        }
    }

    /// 当前 overlay 模式的 (缓冲, 光标) 编辑视图。`None` = 普通输入（用 `input_buffer` 那套）。
    /// 五个 overlay 各有独立缓冲字段，这里是它们唯一的收敛点——缓冲编辑一律经此，勿裸 push/pop。
    pub(crate) fn overlay_buf_edit(state: &mut State) -> Option<preedit_cursor::BufEdit<'_>> {
        let st = state;
        Some(match st.active? {
            ModeKind::TempPinyin => {
                preedit_cursor::BufEdit::new(&mut st.temp_pinyin_buffer, &mut st.temp_pinyin_cursor)
            }
            ModeKind::TempEnglish => preedit_cursor::BufEdit::new(
                &mut st.temp_english_buffer,
                &mut st.temp_english_cursor,
            ),
            ModeKind::Url => preedit_cursor::BufEdit::new(&mut st.url_buffer, &mut st.url_cursor),
            ModeKind::Special(_) => {
                preedit_cursor::BufEdit::new(&mut st.special_buffer, &mut st.special_cursor)
            }
            ModeKind::Mix(_) => {
                preedit_cursor::BufEdit::new(&mut st.mix_buffer, &mut st.mix_cursor)
            }
        })
    }

    /// overlay caret 换算的四要素 (只读前缀, 缓冲, 显示主体, 光标)，与各模式
    /// `update_*_candidates` 的 `state.preedit` 组装同源（preedit = 前缀 + 主体）。
    ///
    /// 临拼 / mix 的主体是引擎 `preedit_display`（含插入的音节分隔符，与缓冲不同形），取自
    /// `overlay_body`；临英 / 特殊 / URL 的主体恒等于自身缓冲，直接用缓冲。
    fn overlay_caret_parts(state: &State) -> Option<(String, &str, &str, usize)> {
        Some(match state.active? {
            ModeKind::TempPinyin => (
                format!("{}{}", state.temp_pinyin_prefix, state.committed_text),
                &state.temp_pinyin_buffer,
                &state.overlay_body,
                state.temp_pinyin_cursor,
            ),
            ModeKind::Mix(_) => (
                format!("{}{}", state.mix_prefix, state.committed_text),
                &state.mix_buffer,
                &state.overlay_body,
                state.mix_cursor,
            ),
            ModeKind::TempEnglish => (
                state.temp_english_prefix.clone(),
                &state.temp_english_buffer,
                &state.temp_english_buffer,
                state.temp_english_cursor,
            ),
            ModeKind::Special(_) => (
                state.special_prefix.clone(),
                &state.special_buffer,
                &state.special_buffer,
                state.special_cursor,
            ),
            ModeKind::Url => (
                String::new(),
                &state.url_buffer,
                &state.url_buffer,
                state.url_cursor,
            ),
        })
    }

    /// overlay 模式组合区光标的 TSF 位置（UTF-16 单元）。非 overlay 时回退为串尾。
    pub(crate) fn overlay_caret(&self, state: &State) -> u32 {
        match Self::overlay_caret_parts(state) {
            Some((prefix, buffer, body, cursor)) => {
                preedit_cursor::caret_utf16(&prefix, buffer, body, cursor)
            }
            None => state.preedit.chars().count() as u32,
        }
    }

    /// 组合区光标在 preedit 显示串内的**字节偏移**，供自绘候选窗画插入符。
    /// 统一普通输入与 overlay 两条路径（各自的前缀 / 主体来源不同，见两个 `*_caret_parts`）。
    /// 与 `state.preedit` 同源，故可安全用于 `&state.preedit[..n]` 切片。
    pub(crate) fn ui_caret_bytes(&self, state: &State) -> usize {
        match Self::overlay_caret_parts(state) {
            Some((prefix, buffer, body, cursor)) => {
                preedit_cursor::caret_display_bytes(&prefix, buffer, body, cursor)
            }
            // 普通输入：前缀 = 已转换前缀，主体 = 当前高亮所决定的显示形态。
            None => preedit_cursor::caret_display_bytes(
                &state.committed_text,
                &state.input_buffer,
                self.effective_preedit_body(state),
                state.input_cursor_pos,
            ),
        }
    }

    /// overlay 模式的编码区光标移动（左右 / Home / End）。`None` = 该键不是光标移动键，
    /// 调用方继续分派。
    ///
    /// Delete 不在此处：各模式「删空缓冲」的收尾不同（退出模式 / 回退已转换段），故与各自的
    /// Backspace 臂合并处理。光标移动不重算候选（光标不参与引擎查询），只重发 caret。
    pub(crate) fn overlay_cursor_key(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> Option<KeyAction> {
        if !matches!(
            data.key_code,
            keymap::VK_LEFT | keymap::VK_RIGHT | keymap::VK_HOME | keymap::VK_END
        ) {
            return None;
        }
        let moved = {
            let mut ed = Self::overlay_buf_edit(state)?;
            match data.key_code {
                keymap::VK_LEFT => ed.move_left(),
                keymap::VK_RIGHT => ed.move_right(),
                keymap::VK_HOME => ed.home(),
                _ => ed.end(),
            }
        };
        // 已在边界（含缓冲空、只剩只读前缀）：吃掉不透传，否则宿主光标会跳出组合区。
        if !moved {
            return Some(KeyAction::Consumed);
        }
        let text = state.preedit.clone();
        let caret_pos = self.overlay_caret(state);
        // 不重算候选，但仍须刷新 UI：自绘编码栏要据新 caret 重画插入符。
        self.notify_ui_update(state);
        Some(KeyAction::UpdateComposition { caret_pos, text })
    }

    /// 回退最后一个已转换段：把它消费的码并回剩余编码**前部**并重转候选。
    /// Backspace（段回退优先于光标）与 Delete（删空剩余编码后）共用，对齐 Go
    /// `handleBackspace` / `popConfirmedSegment`。
    ///
    /// 光标一律拉到剩余编码末尾：回退的码插在缓冲前部，光标留在原处会落进这段码中间，
    /// 语义不清。无段可退时（理论边界）吃掉按键，不透传。
    pub(crate) fn pop_committed_seg(&self, state: &mut State) -> KeyAction {
        let Some((code, _, _, _)) = state.committed_segs.pop() else {
            return KeyAction::Consumed;
        };
        state.committed_text = state
            .committed_segs
            .iter()
            .map(|(_, t, _, _)| t.as_str())
            .collect();
        state.input_buffer = format!("{}{}", code, state.input_buffer);
        state.input_cursor_pos = state.input_buffer.len();
        self.update_candidates(state);
        let display = state.preedit.clone();
        let caret_pos = self.composition_caret(state);
        self.notify_ui_update(state);
        KeyAction::UpdateComposition {
            caret_pos,
            text: display,
        }
    }

    /// 普通模式组合区光标的 TSF 位置（UTF-16 单元），与 `sync_preedit_to_highlight` 同源：
    /// 二者都以 `committed_text` 为前缀、`effective_preedit_body` 为主体，故 caret 与所发的
    /// 组合区文本恒对齐（高亮在拼音↔码表候选间移动导致主体在拆分串↔原始码间切换时亦然）。
    pub(crate) fn composition_caret(&self, state: &State) -> u32 {
        preedit_cursor::caret_utf16(
            &state.committed_text,
            &state.input_buffer,
            self.effective_preedit_body(state),
            state.input_cursor_pos,
        )
    }

    /// 按当前高亮候选类型重算 `state.preedit`（混输高亮跟随）。含已转换前缀（逐步转换）拼接。
    /// 仅普通模式（active==None）有意义；覆盖层模式各自维护 preedit，不应调用此方法。
    pub(crate) fn sync_preedit_to_highlight(&self, state: &mut State) {
        let body = self.effective_preedit_body(state).to_string();
        state.preedit = if state.committed_text.is_empty() {
            body
        } else {
            format!("{}{}", state.committed_text, body)
        };
    }

    /// overlay 候选模式的导航分派：码表型（特殊/临拼，及不含 quick_input 的 mix）`-`/`=` 作翻页；
    /// 文本型（临英）、表达式型（快捷输入）、含 quick_input 的 mix（`-`/`=` 是运算符输入）不把
    /// `-`/`=` 当导航。由 active 自判。
    pub(crate) fn handle_candidate_nav(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> Option<KeyAction> {
        let include_printable = match state.active {
            Some(ModeKind::Special(_)) | Some(ModeKind::TempPinyin) => true,
            Some(ModeKind::Mix(idx)) => !self.mix_has_quick_input(idx),
            _ => false,
        };
        self.apply_nav_key(state, data, include_printable)
    }

    /// 提交某个候选（记录原始简体词频后清空状态），返回上屏文本（按需简繁转换）。
    pub(crate) fn commit_candidate(
        &self,
        state: &mut State,
        text: &str,
        source: CandidateSource,
    ) -> String {
        self.record_selection(&state.input_buffer, text, source);
        let out = self.maybe_s2t(state, text);
        state.input_buffer.clear();
        state.preedit.clear();
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        out
    }

    /// 拼音类「消费码」：候选自带 code（拼音段）则用之，否则退回整个输入缓冲。
    pub(crate) fn cand_code(buf: &str, cand: &Candidate) -> String {
        if cand.code.is_empty() {
            buf.to_string()
        } else {
            cand.code.clone()
        }
    }

    /// 主输入路拼音选词 —— 组合区逐步转换（C）。
    /// 部分匹配（候选只消费缓冲前缀）：把汉字并入 `committed_text` 前缀、裁剪缓冲、重转剩余，
    /// **留在组合区不上屏到应用**，返回 UpdateComposition。
    /// 完整匹配（消费整串）：整体上屏 `committed_text + 候选` 到应用，触发自动造词（L），清空。
    /// 规整候选显示文本：换行/制表 → 空格（杜绝多行候选），`max`>0 时超长截断加省略号。
    /// 短语生成层以 `max=0` 调用（仅一行化、不截断，`text` 存完整原文供上屏）；
    /// 显示层长度截断由 `UiCandidateConfig::truncate_display`（`ui.candidate.max_chars`）负责。
    pub(crate) fn clamp_candidate_display(s: &str, max: usize) -> String {
        let one_line: String = s
            .chars()
            .map(|c| {
                if c == '\n' || c == '\r' || c == '\t' {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        if max == 0 || one_line.chars().count() <= max {
            one_line
        } else {
            let head: String = one_line.chars().take(max).collect();
            format!("{head}…")
        }
    }

    /// 前缀导航候选选中：把输入缓冲补全到该组完整码并重查候选（展开成员/精确命令），
    /// 实现"敲 zz → 选标点 → 展开标点字符"的二级选择。返回新 preedit 显示文本。
    pub(crate) fn complete_to_group_code(&self, state: &mut State, group_code: &str) -> String {
        state.input_buffer = group_code.to_string();
        state.input_cursor_pos = state.input_buffer.len();
        let _ = self.update_candidates(state);
        self.notify_ui_update(state);
        state.preedit.clone()
    }

    pub(crate) fn commit_selected(
        &self,
        state: &mut State,
        cand: &Candidate,
        candidate_pos: i32,
    ) -> KeyAction {
        // 前缀导航候选：补全输入到该组完整码并重查展开（二级选择，不上屏组名）。
        if cand.is_group {
            let code = cand.group_code.clone();
            let display = self.complete_to_group_code(state, &code);
            return KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            };
        }
        // $CC 命令候选：执行动作而非上屏 display 标签。
        if cand.is_command {
            return self.commit_command(state, cand);
        }
        let total = state.input_buffer.len();
        let consumed = cand.consumed_length;
        let code = Self::cand_code(&state.input_buffer, cand);
        let partial =
            consumed > 0 && consumed < total && state.input_buffer.is_char_boundary(consumed);
        // 词频按候选实际编码记账（分段时为前缀码，如「ni」而非整串「nihao」）。
        self.record_selection(&code, &cand.text, cand.source);
        // 输入统计：每次选词记一段（分段逐字选各段各记一次，不重复整串）；
        // 在 partial 分支之前，两分支都经此处一次。
        self.record_commit(
            &cand.text,
            code.len() as u32,
            candidate_pos,
            wind_store::stats::CommitSource::Candidate,
        );
        if partial {
            state
                .committed_segs
                .push((code, cand.text.clone(), cand.source, cand.boundary));
            state.committed_text.push_str(&cand.text);
            state.input_buffer = state.input_buffer[consumed..].to_string();
            // 分步确认消费掉前缀码：剩余编码整体左移，光标落到剩余码末尾（对齐 Go）。
            state.input_cursor_pos = state.input_buffer.len();
            let _ = self.update_candidates(state); // preedit 已含前缀（update_candidates 内拼接）
            let display = state.preedit.clone();
            let caret_pos = self.composition_caret(state);
            self.notify_ui_update(state);
            KeyAction::UpdateComposition {
                caret_pos,
                text: display,
            }
        } else {
            state.committed_segs.push((
                code.clone(),
                cand.text.clone(),
                cand.source,
                cand.boundary,
            ));
            let final_simplified = format!("{}{}", state.committed_text, cand.text);
            self.learn_phrase_on_commit(state); // 自动造词（多段组成的词）
            // 6b: 临时词使用累积（对齐 Go LearnWord-on-commit）：选中临时层候选也推进晋升计数。
            // 点查代替候选层标记：一次 redb 读，未命中即非临时词，零成本略过。
            // is_group/is_command 已在 commit_selected 入口提前返回；is_phrase 由本条件显式过滤
            //（短语无临时词晋升语义），此处均为普通候选。
            if !cand.is_phrase
                && let Some(store) = &self.store
            {
                let active = self.engine_mgr.active_schema_id();
                if let Some(schema) = self.engine_mgr.write_data_schema_id(&active, cand.source)
                    && let Ok(Some(_)) = store.get_temp_word(&schema, &code, &cand.text)
                {
                    let promote_count = if self.engine_mgr.is_pinyin() {
                        self.engine_mgr.auto_learn_settings().promote_count
                    } else {
                        self.engine_mgr
                            .codetable_settings()
                            .auto_phrase
                            .promote_count
                    };
                    // 选中已存在的临时词：learn_temp_word 内部沿用旧 boundary，仅当旧值为 0
                    // （v1 遗留/无信息）时用候选自带的边界补上。
                    if let Ok(count) = store.learn_temp_word(
                        &schema,
                        &code,
                        &cand.text,
                        LEARN_ADD_WEIGHT,
                        cand.boundary,
                    ) {
                        self.maybe_promote_temp(
                            store,
                            &schema,
                            &code,
                            &cand.text,
                            count,
                            promote_count,
                        );
                    }
                }
            }
            let out = self.maybe_s2t(state, &final_simplified);
            self.reset_pinyin_composition(state);
            self.notify_ui_hide();
            Self::commit_action(out, true)
        }
    }

    /// 数字键选词统一入口（num 为 1-based：1-9 选页内对应候选，10 表示主键盘 `0` 选第 10 个）。
    /// 命中当前页范围 → 选词上屏；越界 → 走 overflow 策略（对齐 Go handleNumberKey）。
    pub(crate) fn handle_number_key_select(&self, state: &mut State, num: usize) -> KeyAction {
        let (start, end) = self.page_range(state);
        let idx = start + (num - 1);
        if idx < end {
            let cand = state.candidates[idx].clone();
            // 数字键页内位置 = num-1（候选首选率统计）。
            return self.commit_selected(state, &cand, (num - 1) as i32);
        }
        self.handle_overflow_number_key(state, num)
    }

    /// 数字键超出当前页候选范围时的处理（对齐 Go handleOverflowNumberKey）。
    /// 依 `input.overflow.number_key`：ignore 吞键 / commit 上屏高亮候选 /
    /// commit_and_input 上屏高亮候选并追加数字字符。无候选或无有效高亮时一律吞键。
    pub(crate) fn handle_overflow_number_key(&self, state: &mut State, num: usize) -> KeyAction {
        if state.candidates.is_empty() {
            return KeyAction::Consumed;
        }
        let hi = self.highlighted_global_index(state);
        if hi >= state.candidates.len() {
            return KeyAction::Consumed;
        }
        let behavior = self.rt().config.keys.overflow.number_key.clone();
        match behavior.as_str() {
            "commit" => {
                let cand = state.candidates[hi].clone();
                self.commit_selected(state, &cand, state.selected_index as i32)
            }
            "commit_and_input" => {
                let full_width = state.full_width;
                let cand = state.candidates[hi].clone();
                let act = self.commit_selected(state, &cand, state.selected_index as i32);
                let digit = (b'0' + (num % 10) as u8) as char;
                let digit = if full_width {
                    wind_transform::fullwidth::to_full_width(&digit.to_string())
                } else {
                    digit.to_string()
                };
                Self::append_to_insert_text(act, &digit)
            }
            // "ignore" 及未知值：吞键无效（保留组合，不上屏）
            _ => KeyAction::Consumed,
        }
    }

    /// 次/三选键（`;`/`'`）越界（页内候选不足以命中目标位次）时的处理（对齐 Go
    /// handleOverflowSelectKey）。须排在模式触发键判定之后调用——若该键同时是模式触发键
    /// （如 `;` 触发快捷输入），候选不足时应优先进模式而非走此 overflow。
    /// 依 `input.overflow.select_key`：ignore 吞键 / commit 上屏高亮候选 /
    /// commit_and_input 上屏高亮候选并追加（转换后的）触发键字符。`key_char` 为触发键产生的
    /// 字符（如 `'`），`prev_char` 为光标前字符（用于数字后智能标点）。
    pub(crate) fn handle_overflow_select_key(
        &self,
        state: &mut State,
        key_char: char,
        prev_char: u16,
    ) -> KeyAction {
        let behavior = self.rt().config.keys.overflow.select_key.clone();
        // 无候选（缓冲非空但无候选）：commit 清组合，commit_and_input 清组合并输出该字符。
        if state.candidates.is_empty() {
            return match behavior.as_str() {
                "commit" => {
                    self.reset_pinyin_composition(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
                "commit_and_input" => {
                    let piece = self.convert_punct(state, key_char, prev_char);
                    self.reset_pinyin_composition(state);
                    self.notify_ui_hide();
                    Self::commit_action(piece, state.chinese_mode)
                }
                _ => KeyAction::Consumed,
            };
        }
        let hi = self.highlighted_global_index(state);
        if hi >= state.candidates.len() {
            return KeyAction::Consumed;
        }
        match behavior.as_str() {
            "commit" => {
                let cand = state.candidates[hi].clone();
                self.commit_selected(state, &cand, state.selected_index as i32)
            }
            "commit_and_input" => {
                // 触发键字符按标点流水线转换（在提交前取，chinese_punct 等状态不受提交影响）。
                let piece = self.convert_punct(state, key_char, prev_char);
                let cand = state.candidates[hi].clone();
                let act = self.commit_selected(state, &cand, state.selected_index as i32);
                Self::append_to_insert_text(act, &piece)
            }
            // "ignore" 及未知值：吞键无效（保留组合，不上屏）
            _ => KeyAction::Consumed,
        }
    }

    /// 以词定字：从当前高亮候选词中取第 `char_index` 个字符上屏（0-based，对齐 Go
    /// handleSelectChar）。返回 `None` 表示「无法以词定字」——无候选 / 无缓冲 / 候选词长度不足 /
    /// 命中的是未展开的组候选（组名不可作字源）——交调用方按 overflow 策略处理。
    pub(crate) fn handle_select_char(
        &self,
        state: &mut State,
        char_index: usize,
    ) -> Option<KeyAction> {
        if state.candidates.is_empty() || state.input_buffer.is_empty() {
            return None;
        }
        let hi = self.highlighted_global_index(state);
        if hi >= state.candidates.len() {
            return None;
        }
        let cand = state.candidates[hi].clone();
        // 未展开的组候选（cand.text 是组名如「标点符号」）不可作字源 → 吞键，让用户先展开
        // （与 commit_selected 的组候选二级选择一致）。
        if cand.is_group {
            return Some(KeyAction::Consumed);
        }
        let runes: Vec<char> = cand.text.chars().collect();
        // 候选词长度不足 → None，由调用方按 overflow 处理
        if char_index >= runes.len() {
            return None;
        }
        // 词频学习：以词定字应记实际选的「单字」（非整词），否则造词策略会误判为多字词；
        // 仅普通候选（无副作用命令 Action）才学（对齐 Go len(cand.Actions)==0）。
        if cand.actions.is_empty() {
            let code = Self::cand_code(&state.input_buffer, &cand);
            self.record_selection(&code, &runes[char_index].to_string(), cand.source);
        }
        // 拼接已确认段前缀 + 选中单字，整体按简繁模式转换（与 commit_selected 一致）。
        let combined = format!("{}{}", state.committed_text, runes[char_index]);
        let out = self.maybe_s2t(state, &combined);
        let chinese = state.chinese_mode;
        self.reset_pinyin_composition(state);
        self.notify_ui_hide();
        Some(Self::commit_action(out, chinese))
    }

    /// 以词定字的完整流程，含 overflow 策略（对齐 Go handleSelectCharWithOverflow）。
    /// 仅在缓冲非空或有候选时调用（空缓冲且无候选的 `,`/`.` 应作普通标点，由调用方放行）。
    /// 先尝试正常以词定字；失败（词长不足/空码）则按 `input.overflow.select_char_key` 处理，
    /// 三策与 select_key overflow 同构：ignore 吞键 / commit 上屏高亮 / commit_and_input 追加字符。
    pub(crate) fn handle_select_char_with_overflow(
        &self,
        state: &mut State,
        char_index: usize,
        key_code: u32,
        prev_char: u16,
    ) -> KeyAction {
        if let Some(act) = self.handle_select_char(state, char_index) {
            return act;
        }
        // None：候选词长度不足 / 空码。触发键字符用于 commit_and_input 追加。
        let key_char = crate::coordinator::punct_char(key_code, false);
        let behavior = self.rt().config.keys.overflow.select_char_key.clone();
        // 空码（缓冲非空但无候选）
        if state.candidates.is_empty() {
            return match behavior.as_str() {
                "commit" => {
                    self.reset_pinyin_composition(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
                "commit_and_input" => {
                    let piece = key_char
                        .map(|c| self.convert_punct(state, c, prev_char))
                        .unwrap_or_default();
                    self.reset_pinyin_composition(state);
                    self.notify_ui_hide();
                    Self::commit_action(piece, state.chinese_mode)
                }
                _ => KeyAction::Consumed,
            };
        }
        let hi = self.highlighted_global_index(state);
        if hi >= state.candidates.len() {
            return KeyAction::Consumed;
        }
        match behavior.as_str() {
            "commit" => {
                let cand = state.candidates[hi].clone();
                self.commit_selected(state, &cand, state.selected_index as i32)
            }
            "commit_and_input" => {
                let piece = key_char
                    .map(|c| self.convert_punct(state, c, prev_char))
                    .unwrap_or_default();
                let cand = state.candidates[hi].clone();
                let act = self.commit_selected(state, &cand, state.selected_index as i32);
                Self::append_to_insert_text(act, &piece)
            }
            _ => KeyAction::Consumed,
        }
    }

    /// 把附加文本拼到 InsertText 结局尾部（用于 overflow commit_and_input 追加数字/标点）；
    /// 其它 KeyAction（如分段选择产生的 UpdateComposition）原样返回。
    pub(crate) fn append_to_insert_text(act: KeyAction, extra: &str) -> KeyAction {
        match act {
            KeyAction::InsertText {
                text,
                new_composition,
                mode_changed,
                chinese_mode,
                has_new_composition,
            } => KeyAction::InsertText {
                text: format!("{}{}", text, extra),
                new_composition,
                mode_changed,
                chinese_mode,
                has_new_composition,
            },
            other => other,
        }
    }

    /// $CC 命令候选选中：清理组合区、隐藏 UI，把命令源放独立线程异步执行。
    /// **异步是必须的**：控制器经 Weak 回调 handle_menu_command 等自锁方法，而此刻本线程
    /// 仍持 state 锁（std::sync::Mutex 非可重入），同线程重入即死锁——交独立线程待本次按键
    /// 处理释放锁后再跑（对齐 Go「不在 SearchCommand 持锁路径里再 Lock」的约束）。
    pub(crate) fn commit_command(&self, state: &mut State, cand: &Candidate) -> KeyAction {
        // 命令 nav（从前缀列举选中）携完整码 group_code，用它作执行输入上下文
        // （让 code()/input() 等按完整码求值）；精确码命令 group_code 空 → 用当前缓冲。
        let input = if cand.group_code.is_empty() {
            state.input_buffer.clone()
        } else {
            cand.group_code.clone()
        };
        self.reset_pinyin_composition(state);
        self.spawn_command_action(cand, input)
    }

    /// `$CC` 命令执行核心：隐藏 UI + 把命令源放独立线程异步执行，返回 `ClearComposition`。
    /// **不做**任何缓冲/模式状态重置——调用方须在调用前完成本路径的退出（正常路径经
    /// `commit_command` 的 `reset_pinyin_composition`；overlay 路径经各自 `exit_*`）。
    /// `input` 为命令 `input()`/`code()` 求值上下文（正常路径=输入缓冲；overlay=其编码缓冲，
    /// 须在退出前捕获）。异步执行的死锁规避见 [`Self::spawn_command`]。
    pub(crate) fn spawn_command_action(&self, cand: &Candidate, input: String) -> KeyAction {
        let src = cand.phrase_template.clone();
        self.notify_ui_hide();
        self.spawn_command(src, input);
        // ClearComposition 而非 Consumed：清掉应用里已输入的命令码（如 "coen"），
        // 否则 composition 残留（Consumed 仅吞键、不结束 composition）。type() 的上屏文本
        // 由命令线程经 push 管道单独提交。
        KeyAction::ClearComposition
    }

    /// overlay 路径（特殊模式/临拼/临英/混输）选中候选的**命令前置守卫**：
    /// 若 `cand` 是 `$CC` 命令候选 → 先以 `code`（该 overlay 的编码缓冲）为上下文捕获，
    /// 执行退出闭包清 overlay 状态，再异步执行动作，返回 `Some(action)`；非命令 → `None`，
    /// 调用方按各自文本上屏语义继续。统一所有 overlay 的 `$CC` 选中执行入口。
    pub(crate) fn overlay_commit_command(
        &self,
        state: &mut State,
        cand: &Candidate,
        code: &str,
        exit: impl FnOnce(&Self, &mut State),
    ) -> Option<KeyAction> {
        if !cand.is_command {
            return None;
        }
        let input = if cand.group_code.is_empty() {
            code.to_string()
        } else {
            cand.group_code.clone()
        };
        exit(self, state);
        Some(self.spawn_command_action(cand, input))
    }

    /// 顶屏点统一命令分流：若当前高亮候选是 $CC 命令，执行命令（异步，语义与按空格
    /// `commit_selected` 一致——上屏命令动作结果而非 display 标签），返回 `Some(action)`；
    /// 否则返回 `None`，调用方按普通候选顶屏。
    ///
    /// 用于标点 / 运算符 / 智能符号 Hold / 进其它模式等所有「顶高亮候选」路径，修复命令候选被
    /// 顶屏时错把 display 文本当普通文本上屏的问题（这些路径绕过了 `commit_selected` 的命令守卫）。
    /// 命令候选被顶屏时按「执行命令」处理，触发键（标点 / 模式键）字符不再单独上屏——与空格选中
    /// 命令候选行为一致（命令占据整段缓冲，无独立前缀）。
    pub(crate) fn top_commit_command_guard(&self, state: &mut State) -> Option<KeyAction> {
        if state.candidates.is_empty() {
            return None;
        }
        let idx = self
            .highlighted_global_index(state)
            .min(state.candidates.len() - 1);
        if !state.candidates[idx].is_command {
            return None;
        }
        let cand = state.candidates[idx].clone();
        Some(self.commit_command(state, &cand))
    }

    /// 顶码「文本上屏 + 余码续打」收尾（码表候选 / 普通短语 / 纯文本命令 / 引擎回退文本共用）。
    /// 记账（顶码归属码表来源）→ 设余码为缓冲 → 刷新候选 → 复位首显延迟 → 按 `top_commit_mode`
    /// 返回 `InsertText`（pre_confirm）或 `CommitThenDeferComposition`（direct_commit，余码
    /// keyup 延迟重开）。`top_text` 空（理论边界）时跳过记账、仅刷新余码组合。
    pub(crate) fn commit_top_text(
        &self,
        state: &mut State,
        prefix: &str,
        top_text: String,
        remainder: &str,
    ) -> KeyAction {
        if !top_text.is_empty() {
            // 顶码上屏是码表机制，归属码表来源。
            self.record_selection(prefix, &top_text, CandidateSource::CodeTable);
            // 顶码即上屏首选（pos=0），code_len=被顶出的前缀码长。
            self.record_commit(
                &top_text,
                prefix.len() as u32,
                0,
                wind_store::stats::CommitSource::Candidate,
            );
        }
        state.input_buffer = remainder.to_string();
        state.input_cursor_pos = state.input_buffer.len(); // 顶码后余码续打，光标在余码末尾
        let _ = self.update_candidates(state); // 余码候选（不再消费其结局）
        let preedit = state.preedit.clone();
        // 顶码 = 部分上屏 + 余码续组合：宿主光标因 top_text 插入而前移，余码组合起点已变。
        // 复位首显延迟，使余码候选窗延迟到 reflow 后的新坐标首显、重锁组合起点（对齐 Go）。
        self.reset_first_show();
        self.notify_ui_update(state);
        let has_comp = !remainder.is_empty();
        // direct_commit：真提交顶出文本，余码新组合延迟到触发键 keyup 才开（仅有余码时分叉）。
        if has_comp
            && self.rt().config.input.top_commit_mode == wind_config::TopCommitMode::DirectCommit
        {
            return KeyAction::CommitThenDeferComposition {
                commit_text: top_text,
                deferred_composition: preedit,
                timeout_ms: DEFERRED_COMPOSITION_FALLBACK_MS,
            };
        }
        KeyAction::InsertText {
            text: top_text,
            new_composition: has_comp.then_some(preedit),
            mode_changed: false,
            chinese_mode: true,
            has_new_composition: has_comp,
        }
    }

    /// 「顶屏文本 + 进模式新组合」收尾（进特殊模式 / 临时拼音 / mix 融合共用）：与顶码
    /// `commit_top_text` 同一 `top_commit_mode` 分流——direct_commit 且有新组合（引导键
    /// 前缀）→ `CommitThenDeferComposition` 真提交、新组合延迟到触发键 keyup 才开；
    /// pre_confirm → `InsertText` 聚合（文本并入 TSF `_pendingCommitPrefix`、留组合内）。
    /// 新组合为空（直达热键进入无引导符）时无组合可重开、无 diff 合并之虞，
    /// 两种模式都直接真提交（对齐顶码无余码分支）。
    pub(crate) fn commit_then_new_composition(&self, text: String, new_comp: String) -> KeyAction {
        if new_comp.is_empty() {
            return KeyAction::InsertText {
                text,
                new_composition: None,
                mode_changed: false,
                chinese_mode: true,
                has_new_composition: false,
            };
        }
        if self.rt().config.input.top_commit_mode == wind_config::TopCommitMode::DirectCommit {
            return KeyAction::CommitThenDeferComposition {
                commit_text: text,
                deferred_composition: new_comp,
                timeout_ms: DEFERRED_COMPOSITION_FALLBACK_MS,
            };
        }
        KeyAction::InsertText {
            text,
            new_composition: Some(new_comp),
            mode_changed: false,
            chinese_mode: true,
            has_new_composition: true,
        }
    }

    /// 含副作用命令（`$CC` 里带 shell/key/clip 等 Effect）顶码：异步执行动作（消费 prefix 整段、
    /// 无同步上屏文本），余码作为新一轮输入缓冲走标准候选刷新 + 新组合。副作用多为开应用 /
    /// 切设置——前者焦点变化自动取消余码组合（无害），后者不改焦点、余码组合正常续打。
    /// 不走 direct_commit 延迟重开（无同步 commit 文本，无 diff 合并之虞）。
    pub(crate) fn top_commit_command_with_remainder(
        &self,
        state: &mut State,
        cand: &Candidate,
        prefix: &str,
        remainder: &str,
    ) -> KeyAction {
        // 命令 input：nav 命令携完整码 group_code，否则用被顶出的前缀码 prefix（对齐 commit_command）。
        let input = if cand.group_code.is_empty() {
            prefix.to_string()
        } else {
            cand.group_code.clone()
        };
        // 无余码（理论边界）→ 退化为普通命令选中（清组合，异步执行）。
        if remainder.is_empty() {
            self.reset_pinyin_composition(state);
            return self.spawn_command_action(cand, input);
        }
        let src = cand.phrase_template.clone();
        state.input_buffer = remainder.to_string();
        state.input_cursor_pos = state.input_buffer.len(); // 顶码后余码续打，光标在余码末尾
        let _ = self.update_candidates(state); // 余码标准候选刷新
        let preedit = state.preedit.clone();
        self.reset_first_show();
        self.notify_ui_hide(); // 隐藏命令码 UI（余码候选窗随后由 notify_ui_update 重开）
        self.spawn_command(src, input); // 异步执行副作用（Effect 回调 coordinator 锁必须异步）
        self.notify_ui_update(state);
        KeyAction::InsertText {
            text: String::new(), // 空上屏：命令占 prefix，无同步文本
            new_composition: Some(preedit),
            mode_changed: false,
            chinese_mode: true,
            has_new_composition: true,
        }
    }

    /// 在独立线程执行命令源（解析→求值→按序跑动作；type 文本经 push 提交、其余为副作用）。
    pub(crate) fn spawn_command(&self, src: String, input: String) {
        let Some(this) = self.self_weak.get().and_then(std::sync::Weak::upgrade) else {
            warn!("cmdbar: self_weak 未装配，命令跳过");
            return;
        };
        std::thread::spawn(move || {
            this.run_command_candidate(&src, &input);
        });
    }

    /// 把命令产生的文本经 push 管道提交给活动客户端（命令在独立线程执行，走 push 而非 KeyAction）。
    pub(crate) fn push_commit_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let encoded = wind_ipc::codec::encode_commit_text(text, None, false, true, false);
        self.push_server.push_commit_to_active(&encoded);
    }

    /// 候选词条操作（右键菜单）：调整 Shadow 规则并即时重排重绘。
    /// code 取当前输入码（state.input_buffer）；按方案隔离。
    /// 删除按候选来源分流（对齐 Go handleCandidateDelete）：短语软禁用 / 用户词・临时词真删 /
    /// 系统词 shadow 隐藏。菜单已按同规则灰显，此处判定为 defensive（热键路径也经此）。
    pub(crate) fn candidate_op(&self, op: CandidateOp, page_local: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() || state.input_buffer.is_empty() {
            return;
        }
        let (start, end) = self.page_range(&state);
        let idx = start + page_local;
        if idx >= end || idx >= state.candidates.len() {
            return;
        }
        let cand = state.candidates[idx].clone();
        let word = cand.text.clone();
        let code = state.input_buffer.clone();
        let schema = self.engine_mgr.active_schema_id();

        // $SS/$AA 展开成员：顺序/成员由短语定义决定，拒绝一切 shadow/删除双轨漂移。
        if crate::handle_menu::candidate_is_group_member(&cand) {
            return;
        }
        let is_move = matches!(
            op,
            CandidateOp::MoveTop | CandidateOp::MoveUp | CandidateOp::MoveDown
        );
        // 拼音普通候选禁调位（无稳定位置语义，pin 与衰减软置前冲突）；命令候选例外。
        if is_move
            && !cand.is_command
            && matches!(
                self.engine_mgr.current_engine_type(),
                Some(wind_engine::EngineType::Pinyin)
            )
        {
            return;
        }
        // 已在首位：置顶是冗余规则，直接忽略（菜单已灰显，热键路径 defensive）。
        if matches!(op, CandidateOp::MoveTop) && idx == 0 {
            return;
        }
        let last = state.candidates.len().saturating_sub(1);
        if let Some(store) = &self.store {
            // 候选调整按 data_schema_id 归属（拼音族折叠）；Delete 分支仍传原始 schema，
            // 供 delete_candidate_by_source 对用户词/临时词按来源分流（混输）。
            let sh_schema = self.engine_mgr.data_schema_id(&schema);
            // None cand_id：码表静态词无动态短语 id。redb 事务持久，无需显式落盘。
            let r = match op {
                CandidateOp::MoveTop => store.pin_shadow(&sh_schema, &code, &word, None, 0),
                CandidateOp::MoveUp => {
                    store.pin_shadow(&sh_schema, &code, &word, None, idx.saturating_sub(1))
                }
                CandidateOp::MoveDown => {
                    store.pin_shadow(&sh_schema, &code, &word, None, (idx + 1).min(last))
                }
                CandidateOp::Delete => self.delete_candidate_by_source(&schema, &code, &cand),
                CandidateOp::Reset => store.remove_shadow_rule(&sh_schema, &code, &word),
            };
            if let Err(e) = r {
                warn!("candidate op failed: {}", e);
            }
        }

        // 重新构建候选（会重新应用 Shadow）并重绘
        self.update_candidates(&mut state);
        self.notify_ui_update(&state);
    }

    /// 右键「删除」按候选来源分流：
    /// - 短语 → `set_phrase_enabled(false)` 软禁用（设置页可恢复）+ 重建短语层即时生效；
    ///   code 优先取导航完整码 `group_code`，text 用原始记录文本 `phrase_template`（display
    ///   可能是模板展开后文本，直接用会在 store 里 miss）。
    /// - 用户词/临时词 → store 真删；schema 取写归属 id（混输按来源分流、拼音族折叠共享），
    ///   code 优先取候选自带存储码（双拼下 input_buffer 是双拼串、存储码是全拼）。
    /// - 其它（系统码表/拼音）→ shadow 软隐藏；单字同样允许（旧版单字保护已取消：
    ///   shadow 按 code+word 键控，仅该编码下隐藏，设置页可恢复）。
    fn delete_candidate_by_source(
        &self,
        schema: &str,
        code: &str,
        cand: &Candidate,
    ) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        if cand.is_phrase {
            let raw = if cand.phrase_template.is_empty() {
                cand.text.as_str()
            } else {
                cand.phrase_template.as_str()
            };
            let pcode = if cand.group_code.is_empty() {
                code
            } else {
                cand.group_code.as_str()
            };
            store.set_phrase_enabled(pcode, raw, false)?;
            self.rebuild_phrases();
            return Ok(());
        }
        if cand.meta.is_user_dict || cand.meta.is_temp_dict {
            let Some(sid) = self.engine_mgr.write_data_schema_id(schema, cand.source) else {
                debug!("delete_candidate: 无法归因存储方案，跳过 '{}'", cand.text);
                return Ok(());
            };
            let dcode = if cand.code.is_empty() {
                code
            } else {
                cand.code.as_str()
            };
            return if cand.meta.is_user_dict {
                store.remove_user_word(&sid, dcode, &cand.text)
            } else {
                store.remove_temp_word(&sid, dcode, &cand.text)
            };
        }
        // 候选调整（系统词软隐藏）按 data_schema_id 归属（拼音族折叠）。
        store.delete_shadow(&self.engine_mgr.data_schema_id(schema), code, &cand.text)
    }

    /// 候选词操作热键匹配（对齐 Go matchCandidateActionKey，但 `0` 扩展为第 10 候选）。
    /// template ∈ {"ctrl+number","ctrl+shift+number"}，命中返回 1-based 页内序号(1-10)，否则 0。
    /// 数字键 1-9 → 序号 1-9；`0` → 序号 10（候选窗最多 10 项，与主键盘/小键盘选词一致）。
    fn match_candidate_action_key(
        template: &str,
        has_ctrl: bool,
        has_shift: bool,
        key_code: u32,
    ) -> usize {
        // 0x30..=0x39 = '0'..'9'；'0' 映射为第 10 个候选。
        let num = match key_code {
            0x30 => 10,
            0x31..=0x39 => (key_code - 0x30) as usize,
            _ => return 0,
        };
        match template.trim().to_lowercase().as_str() {
            "ctrl+number" if has_ctrl && !has_shift => num,
            "ctrl+shift+number" if has_ctrl && has_shift => num,
            _ => 0,
        }
    }

    /// Ctrl+数字 / Ctrl+Shift+数字 置顶/删除当前页候选（对齐 Go handle_key_event 候选热键段）。
    /// 仅中文模式 + 正常码表输入态（有候选 + 有输入码 + 非独占模式）生效；命中即消费按键。
    /// 复用 `candidate_op`（页内序号驱动的 shadow 改写 + 重排重绘）。
    pub(crate) fn handle_candidate_action_hotkey(&self, data: &KeyEventData) -> Option<KeyAction> {
        use wind_ipc::protocol::{MOD_CTRL, MOD_SHIFT};
        if data.modifiers & MOD_CTRL == 0 {
            return None;
        }
        let has_shift = data.modifiers & MOD_SHIFT != 0;
        let h = &self.rt().config.keys;
        // 删除优先匹配（与 Go 顺序一致：DeleteCandidate 先于 PinCandidate）。
        let del =
            Self::match_candidate_action_key(&h.delete_candidate, true, has_shift, data.key_code);
        let pin =
            Self::match_candidate_action_key(&h.pin_candidate, true, has_shift, data.key_code);
        let (op, num) = if del > 0 {
            (CandidateOp::Delete, del)
        } else if pin > 0 {
            (CandidateOp::MoveTop, pin)
        } else {
            return None;
        };
        // 门控：仅正常码表输入态。独占模式 input_buffer 必空，故下方判定亦自然排除之。
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.chinese_mode
                || state.active.is_some()
                || state.candidates.is_empty()
                || state.input_buffer.is_empty()
            {
                return None;
            }
        }
        // candidate_op 自行重新加锁并做页范围/来源分流校验。
        self.candidate_op(op, num - 1);
        Some(KeyAction::Consumed)
    }

    /// 点击选词：提交页内第 N 个候选，经 push 管道异步上屏（对齐 Go PushCommitText）。
    ///
    /// 主输入路（`active == None`）复用键盘选词的 [`Self::commit_selected`]，其返回的 KeyAction
    /// 经 [`Self::push_mouse_action`] 翻译成 push 消息——分步提交（候选只消费缓冲前缀，如
    /// 「nihao」选「你」）由此与数字键完全一致：组合区留活、剩余码续查候选。此前鼠标独走
    /// `commit_candidate`（无条件清空缓冲），故点选分段候选会丢弃剩余编码、丢失已确认前缀段，
    /// 并把词频错记到整串码上。
    ///
    /// overlay 模式（临拼/特殊/临英/混输，`active != None`）在键盘侧由各自的专用处理器接管、
    /// 不经 `commit_selected`（见 coordinator 内 `state.active` 的单点分派），故仍走原
    /// 「整串提交 + 彻底复位」路径，不向其引入未定义的分段语义。
    pub(crate) fn mouse_select(&self, page_local: usize) {
        let _ = self.mouse_select_action(page_local);
    }

    /// [`Self::mouse_select`] 的实现，返回主输入路实际推送的 KeyAction 供测试断言
    /// （overlay / `$CC` 命令 / 越界等不经 push 的路径返回 None）。
    fn mouse_select_action(&self, page_local: usize) -> Option<KeyAction> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() {
            return None;
        }
        let (start, end) = self.page_range(&state);
        let idx = start + page_local;
        if idx >= end || idx >= state.candidates.len() {
            return None;
        }
        // $CC 命令候选：执行动作而非上屏 display 标签（释放锁后异步执行，避免重入死锁）。
        if state.candidates[idx].is_command {
            let src = state.candidates[idx].phrase_template.clone();
            // 命令 nav 携完整码 group_code 作执行输入；精确码命令用当前缓冲。
            let gc = state.candidates[idx].group_code.clone();
            let input = if gc.is_empty() {
                state.input_buffer.clone()
            } else {
                gc
            };
            state.active = None;
            drop(state);
            self.notify_ui_hide();
            self.spawn_command(src, input);
            return None;
        }
        // 主输入路：与数字键同一条提交路径（is_group 的二级选择亦由其内部处理）。
        if state.active.is_none() {
            let cand = state.candidates[idx].clone();
            let chinese_mode = state.chinese_mode;
            // 鼠标页内位置 = page_local（候选首选率统计，与数字键的 num-1 同义）。
            let act = self.commit_selected(&mut state, &cand, page_local as i32);
            drop(state);
            // commit_selected 已按分支自行 notify_ui_update / notify_ui_hide，此处不再重复。
            self.push_mouse_action(&act, chinese_mode);
            return Some(act);
        }
        // ── 以下为 overlay 模式（active != None）路径 ──
        // 前缀导航候选：补全输入到完整码并重查展开（二级选择，鼠标点击同键盘选中）。
        if state.candidates[idx].is_group {
            let code = state.candidates[idx].group_code.clone();
            self.complete_to_group_code(&mut state, &code);
            return None;
        }
        let text = state.candidates[idx].text.clone();
        let source = state.candidates[idx].source;
        let chinese_mode = state.chinese_mode;
        let out = self.commit_candidate(&mut state, &text, source);
        // 鼠标提交后彻底复位各输入模式，避免遗留状态
        state.active = None;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_prefix.clear();
        state.temp_english_buffer.clear();
        drop(state);

        self.notify_ui_hide();
        let encoded = wind_ipc::codec::encode_commit_text(&out, None, false, chinese_mode, false);
        // 仅推给活动客户端，避免广播导致多个 TSF 端重复上屏
        self.push_server.push_commit_to_active(&encoded);
        debug!(
            "mouse_select: overlay 整串提交 '{}' (page_local={})",
            out, page_local
        );
        None
    }

    /// 鼠标点选页内第 N 个候选（测试/诊断用）：返回主输入路实际推送的 KeyAction
    /// （`UpdateComposition` = 分步提交，组合区留活；`InsertText` = 整串上屏）。
    pub fn debug_mouse_select(&self, page_local: usize) -> Option<KeyAction> {
        self.mouse_select_action(page_local)
    }

    /// 鼠标选词产生的 KeyAction → push 管道消息。
    ///
    /// 键盘选词把 KeyAction 交回 TSF 按键管线应答，鼠标点击没有按键上下文（不在 OnKeyDown
    /// 的应答里），只能自行编码经 push 管道投递。仅覆盖 `commit_selected` 的两种返回：
    /// - `UpdateComposition`（分步提交 / 二级选择）→ `CMD_UPDATE_COMPOSITION`，组合区留活。
    ///   C++ 侧 IPCClient 异步 reader 与 TextService 的 `SetUpdateCompositionCallback` 自 Go 版
    ///   起就在位（注释即写 "mouse click partial confirm"），Rust 侧此前从未发过此包。
    /// - `InsertText`（整串提交）→ `CMD_COMMIT_TEXT`。
    ///
    /// 两者均带副作用，故一律 `push_commit_to_active` 定向投递（非广播），避免多个 TSF 端重复。
    fn push_mouse_action(&self, act: &KeyAction, chinese_mode: bool) {
        match act {
            KeyAction::UpdateComposition { text, caret_pos } => {
                let encoded = wind_ipc::codec::encode_update_composition(text, *caret_pos);
                self.push_server.push_commit_to_active(&encoded);
                debug!("mouse_select: 分步提交，组合区留活 preedit='{}'", text);
            }
            KeyAction::InsertText { text, .. } => {
                let encoded =
                    wind_ipc::codec::encode_commit_text(text, None, false, chinese_mode, false);
                self.push_server.push_commit_to_active(&encoded);
                debug!("mouse_select: committed '{}'", text);
            }
            other => {
                debug!("mouse_select: 无需推送的 KeyAction {:?}", other);
            }
        }
    }

    pub(crate) fn commit_action(text: String, chinese_mode: bool) -> KeyAction {
        KeyAction::InsertText {
            text,
            new_composition: None,
            mode_changed: false,
            chinese_mode,
            has_new_composition: false,
        }
    }
}

#[cfg(test)]
mod auto_commit_min_len_tests {
    //! 最短码长归一：须与引擎 `CodeTableEngine::new` 的同名归一保持一致。
    use super::resolve_auto_commit_min_len;

    #[test]
    fn zero_follows_max_code_length() {
        // 0 = 跟随全码长（五笔 4 码）。
        assert_eq!(resolve_auto_commit_min_len(0, 4), 4);
    }

    #[test]
    fn explicit_value_wins_over_max_code_length() {
        assert_eq!(resolve_auto_commit_min_len(2, 4), 2);
        assert_eq!(resolve_auto_commit_min_len(6, 4), 6);
    }

    #[test]
    fn no_max_code_length_disables_gate() {
        // 拼音等引擎 max_code_length()=0 → 门槛 0 → 调用方 `len < 0` 恒假 → 不设闸。
        assert_eq!(resolve_auto_commit_min_len(0, 0), 0);
    }
}

#[cfg(test)]
mod finalize_candidates_tests {
    //! 候选值展开汇聚点 `finalize_candidates`：所有输入方案共用，保证 `$` 语法一致生效。
    use super::*;
    use std::sync::Arc;
    use wind_config::config::Config;

    fn coord() -> Arc<Coordinator> {
        Coordinator::new_headless(Config::default(), None)
    }

    fn cand(text: &str) -> Candidate {
        Candidate {
            text: text.to_string(),
            ..Default::default()
        }
    }

    fn cand_code(text: &str, code: &str) -> Candidate {
        Candidate {
            text: text.to_string(),
            code: code.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn aa_group_expands_inline_when_code_absent() {
        let c = coord();
        // 无码信息（code 空）→ 视为精确，逐成员炸开。
        let out = c.finalize_candidates(vec![cand(r#"$AA("数字", "①②③")"#)], "sz");
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["①", "②", "③"], "$AA 应一对多炸开为逐字符候选");
        assert!(out.iter().all(|c| !c.is_command && !c.is_group));
    }

    #[test]
    fn aa_group_expands_inline_at_exact_code() {
        let c = coord();
        // 精确码（候选码 == 输入 "arrx"）→ 逐成员炸开。
        let out = c.finalize_candidates(vec![cand_code(r#"$AA("箭头", "←↑→↓")"#, "arrx")], "arrx");
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["←", "↑", "→", "↓"]);
    }

    #[test]
    fn aa_group_collapses_to_name_at_prefix() {
        let c = coord();
        // 前缀（候选码 "arrx" 长于输入 "arr"）→ 折叠为组名候选，不炸开。
        let out = c.finalize_candidates(vec![cand_code(r#"$AA("箭头", "←↑→↓")"#, "arrx")], "arr");
        assert_eq!(out.len(), 1, "前缀应折叠为单个组名候选");
        assert_eq!(out[0].text, "箭头", "折叠候选显示组名");
        assert!(out[0].is_group, "折叠候选标 is_group");
        assert_eq!(
            out[0].group_code, "arrx",
            "group_code 为完整码，选中补全后重查展开"
        );
        assert!(!out[0].is_command);
    }

    #[test]
    fn marks_cc_command_and_keeps_source() {
        let c = coord();
        let src = r#"$CC("切简繁", ime.toggle("s2t"))"#;
        let out = c.finalize_candidates(vec![cand(src)], "co");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "切简繁", "$CC display 作候选文本");
        assert!(out[0].is_command, "$CC 应标 is_command");
        assert_eq!(out[0].phrase_template, src, "命令源留存供选中执行");
    }

    #[test]
    fn plain_candidates_pass_through_unchanged() {
        let c = coord();
        // 普通词 + 含 $ 但非语法文本（价格$5）：均原样保留，零干预。
        let out = c.finalize_candidates(vec![cand("你好"), cand("价格$5")], "nh");
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["你好", "价格$5"]);
        assert!(out.iter().all(|c| !c.is_command));
    }

    #[test]
    fn already_command_candidate_is_not_re_expanded() {
        let c = coord();
        // 已是 is_command 的候选（短语命中）：跳过二次展开，原样保留。
        let mut pre = cand(r#"$AA("x","ab")"#);
        pre.is_command = true;
        let out = c.finalize_candidates(vec![pre], "x");
        assert_eq!(out.len(), 1, "已标命令不应被再炸开");
        assert!(out[0].is_command);
    }

    fn cand_ordered(text: &str, base_order: i32, natural_order: i32, weight: i32) -> Candidate {
        Candidate {
            text: text.into(),
            code: "y".into(),
            base_order,
            natural_order,
            weight,
            ..Default::default()
        }
    }

    /// 回归：跨词库排序须以 `base_order` 隔离，`natural_order` 只在同档内当 tiebreaker。
    /// 复刻 flypy「y」现场——主库「一」(base_order=0, natural_order 大) vs 一简次选库「有时」
    /// (base_order=2, natural_order 小)：修复前协调器仅按 natural_order 升序会把「有时」拉到首位，
    /// 修复后「一」（更小 base_order）应稳居首位。两种模式（含/不含权重）都成立（此处权重均 0）。
    #[test]
    fn base_order_wins_over_cross_dict_natural_order() {
        let yi = cand_ordered("一", 0, 57285, 0);
        let youshi = cand_ordered("有时", 2, 24, 0);
        for ignore_weight in [false, true] {
            // 故意以「有时」在前的顺序放入，确保是排序而非原序决定结果。
            let mut cands = vec![youshi.clone(), yi.clone()];
            cands.sort_by(|a, b| candidate_display_order(a, b, ignore_weight));
            assert_eq!(
                cands[0].text, "一",
                "base_order=0 主库候选应排在 base_order=2 次选库候选之前（ignore_weight={ignore_weight}）"
            );
            assert_eq!(cands[1].text, "有时");
        }
    }

    /// natural 模式忽略权重：主库低权重条目（base_order 0）须排在次选库高权重条目（base_order 1）
    /// 之前，与引擎 `by_natural` 一致；weight 模式则相反（高权重靠前）。证明 ignore_weight 生效。
    #[test]
    fn natural_mode_ignores_weight_weight_mode_respects_it() {
        let main_low = cand_ordered("主低", 0, 100, 1); // 主库、低权重
        let extra_high = cand_ordered("扩高", 1, 5, 999); // 次选库、高权重
        // weight 模式：权重降序主导 → 扩高(999) 在前。
        let mut w = vec![main_low.clone(), extra_high.clone()];
        w.sort_by(|a, b| candidate_display_order(a, b, false));
        assert_eq!(w[0].text, "扩高", "weight 模式高权重应靠前");
        // natural 模式：忽略权重 → base_order 升序主导，主库(0) 在前。
        let mut n = vec![main_low, extra_high];
        n.sort_by(|a, b| candidate_display_order(a, b, true));
        assert_eq!(
            n[0].text, "主低",
            "natural 模式忽略权重、按 base_order 升序，主库应靠前"
        );
    }
}
