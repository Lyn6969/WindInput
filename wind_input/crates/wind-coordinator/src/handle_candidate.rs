//! 候选处理：生成 / 过滤 / shadow / 词频重排 / 翻页导航 / 选词上屏 / 右键操作。
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。

use crate::coordinator::{now_unix_secs, Coordinator, InputOutcome, State, PHRASE_WEIGHT_BASE};
use crate::pipeline::ModeKind;
use wind_config::hotkey;
use tracing::{debug, warn};
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_candidate::Candidate;
use wind_store::freq::FreqRecord;
use wind_ui::manager::CandidateOp;

impl Coordinator {
    /// 记录一次选词到 redb FREQ（词频维度：count+1、last_used=now，按 schema+code+text）。
    /// 词频是与权重解耦的独立维度（frequency.md），仅记真实使用数据；redb 事务即时持久。
    pub(crate) fn record_selection(&self, code: &str, text: &str) {
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
            let schema = self.engine_mgr.active_schema_id();
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
        let schema = self.engine_mgr.active_schema_id();
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
                match store.get_freq(&schema, code, &c.text) {
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
            wind_engine::freq_rerank::rerank_pinyin_decay(candidates, &recs, now_unix_secs());
        } else {
            wind_engine::freq_rerank::rerank_codetable_usedfirst(
                candidates,
                &recs,
                code,
                settings.strategy,
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
    pub(crate) fn build_candidates(&self, state: &mut State, limit: usize) -> (usize, InputOutcome) {
        // 分段上屏进行中（committed 前缀非空 ⟺ 来自拼音选词——五笔候选 consumed_length=0
        // 永不部分匹配）：剩余编码强制按拼音方案转换，避免混输让五笔抢首选（你↑选后 hao→虚）。
        let result = if !state.committed_text.is_empty()
            && !self.rt().config.schema.primary_pinyin.is_empty()
        {
            self.engine_mgr.convert_with(
                &self.rt().config.schema.primary_pinyin,
                &state.input_buffer,
                limit,
            )
        } else {
            self.engine_mgr.convert(&state.input_buffer, limit)
        };
        // 组合区只显示输入码/拼音
        state.preedit = if result.preedit_display.is_empty() {
            state.input_buffer.clone()
        } else {
            result.preedit_display
        };
        let engine_count = result.candidates.len();
        // 引擎给出的全码自动上屏意向（基于引擎候选；下方 shadow 后复核存活性）。
        let auto_commit = if result.should_commit && !result.commit_text.is_empty() {
            Some(result.commit_text.clone())
        } else {
            None
        };
        let should_clear = result.should_clear;

        let mut candidates = result.candidates;
        if !self.phrases.is_empty() {
            let recent = self.recent_commits_snapshot();
            let max_disp = self.rt().config.input.phrase.max_display_chars;
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
            for hit in self.phrases.lookup(&state.input_buffer, &recent, &clip) {
                let is_command = hit.command_src.is_some();
                candidates.push(Candidate {
                    text: Self::clamp_candidate_display(&hit.text, max_disp),
                    weight: PHRASE_WEIGHT_BASE + hit.weight,
                    is_phrase: true,
                    // $CC 命令短语：标记 is_command，phrase_template 暂存命令源；
                    // 选中时由 commit_selected 拦截，执行动作而非上屏 display 标签。
                    is_command,
                    phrase_template: hit.command_src.unwrap_or_default(),
                    ..Default::default()
                });
            }
            // 前缀导航：敲 `zz`/`co` 等前缀（长度 ≥ min_prefix_length）列出所有该前缀的
            // marker 短语。**$CC 命令** → is_command（选中直接执行，group_code 作执行输入
            // 上下文）；**$SS/$AA 组** → is_group（选中补全到完整码再展开成员，二级选择）。
            let min_prefix = self.rt().config.input.phrase.min_prefix_length;
            for hit in self
                .phrases
                .lookup_prefix(&state.input_buffer, &recent, min_prefix)
            {
                let code = hit.nav_code.unwrap_or_default();
                let text = Self::clamp_candidate_display(&hit.text, max_disp);
                if let Some(src) = hit.command_src {
                    candidates.push(Candidate {
                        text,
                        weight: PHRASE_WEIGHT_BASE + hit.weight,
                        is_phrase: true,
                        is_command: true,
                        phrase_template: src,
                        group_code: code,
                        comment: hit.comment,
                        ..Default::default()
                    });
                } else {
                    candidates.push(Candidate {
                        text: text.clone(),
                        weight: PHRASE_WEIGHT_BASE + hit.weight,
                        is_phrase: true,
                        is_group: true,
                        group_code: code,
                        group_name: text,
                        comment: hit.comment,
                        ..Default::default()
                    });
                }
            }
        }
        candidates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.text.clone()));
        // 检索范围过滤（填充常用标志后按模式过滤；对齐 Go 引擎内过滤）
        self.apply_filter(state, &mut candidates);
        // 用户词频重排（独立维度，used-first，绝不改 weight；frequency.md §3）
        self.apply_freq_rerank(&mut candidates, &state.input_buffer);
        // Shadow 规则：删除过滤 + 置顶/移动重排（优先级最高，排序后应用）
        self.apply_shadow(&mut candidates, &state.input_buffer);
        state.candidates = candidates;
        // 复核：仅当上屏目标在最终候选中仍存在（未被 shadow 删除）才放行自动上屏。
        let outcome = match auto_commit.filter(|t| state.candidates.iter().any(|c| &c.text == t)) {
            Some(_) => {
                // 一致性：自动上屏文本取「实际显示的首候选」，与空格/点选同源，杜绝
                // "显示藏、全码上屏駏"的漂移（首候选已由档位排序保证是五笔精确全码）。
                match state.candidates.first() {
                    Some(c) => InputOutcome::AutoCommit(c.text.clone()),
                    None => InputOutcome::Normal,
                }
            }
            None if should_clear => InputOutcome::Clear,
            None => InputOutcome::Normal,
        };
        (engine_count, outcome)
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
        let schema = self.engine_mgr.active_schema_id();
        let rec = match store.get_shadow_rules(&schema, code) {
            Ok(Some(r)) => r,
            _ => return,
        };
        // 纯重排逻辑下沉 wind_candidate（用元组解耦，避免该 crate 依赖 wind-store）。
        let pinned: Vec<(String, usize)> =
            rec.pinned.iter().map(|p| (p.word.clone(), p.position)).collect();
        wind_candidate::apply_shadow(candidates, &rec.deleted, &pinned);
    }

    /// 根据输入缓冲更新候选（动态分级加载：首次小批量，翻页到边界再扩展）。
    /// 返回输入结局（全码自动上屏 / 满码空码清空）；多数调用方忽略，仅正向输入字母时消费。
    pub(crate) fn update_candidates(&self, state: &mut State) -> InputOutcome {
        state.candidates.clear();
        state.preedit = state.input_buffer.clone();
        if state.input_buffer.is_empty() {
            state.has_more = false;
            state.candidate_input.clear();
            // 缓冲空但有已转换前缀（逐步转换中删空剩余拼音）：组合区仍显示前缀。
            state.preedit = state.committed_text.clone();
            return InputOutcome::Normal;
        }
        let limit = self.initial_candidate_limit(&state.input_buffer);
        let (engine_count, outcome) = self.build_candidates(state, limit);
        // 拼音逐步转换：组合区 = 已转换前缀 + 剩余拼音显示（前缀恒空于码表模式，无副作用）。
        if !state.committed_text.is_empty() {
            state.preedit = format!("{}{}", state.committed_text, state.preedit);
        }
        state.candidate_input = state.input_buffer.clone();
        state.candidate_limit = limit;
        // 引擎返回数达到上限 → 可能还有更多未加载
        state.has_more = engine_count >= limit;
        // 候选变化：复位翻页与高亮（含清除鼠标悬停）
        state.current_page = 0;
        state.selected_index = 0;
        state.hover_index = -1;
        outcome
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
        // 保持当前页/高亮不变（build_candidates 未改动它们）
    }

    /// 若 key_code 是配置的二/三候选键，返回页内候选偏移（1=次选/第2项，2=三选/第3项）。
    pub(crate) fn select_key_offset(&self, key_code: u32) -> Option<usize> {
        for group in &self.rt().config.input.select_key_groups {
            let vks = hotkey::select_key_vks(group);
            if let Some(pos) = vks.iter().position(|vk| *vk == key_code) {
                return Some(pos + 1);
            }
        }
        None
    }

    /// 当前页候选切片的 [start, end) 区间
    pub(crate) fn page_range(&self, state: &State) -> (usize, usize) {
        let pp = self.per_page();
        let start = state.current_page * pp;
        let end = (start + pp).min(state.candidates.len());
        (start, end)
    }

    /// 当前高亮候选的全局下标（页起点 + 页内高亮）
    pub(crate) fn highlighted_global_index(&self, state: &State) -> usize {
        let (start, _) = self.page_range(state);
        start + state.selected_index
    }

    /// overlay 候选模式的导航分派：码表型（特殊/临拼，及不含 quick_input 的 mix）`-`/`=` 作翻页；
    /// 文本型（临英）、表达式型（快捷输入）、含 quick_input 的 mix（`-`/`=` 是运算符输入）不把
    /// `-`/`=` 当导航。由 active 自判。
    pub(crate) fn handle_candidate_nav(&self, state: &mut State, data: &KeyEventData) -> Option<KeyAction> {
        let include_printable = match state.active {
            Some(ModeKind::Special(_)) | Some(ModeKind::TempPinyin) => true,
            Some(ModeKind::Mix(idx)) => !self.mix_has_quick_input(idx),
            _ => false,
        };
        self.apply_nav_key(state, data, include_printable)
    }

    /// 提交某个候选（记录原始简体词频后清空状态），返回上屏文本（按需简繁转换）。
    pub(crate) fn commit_candidate(&self, state: &mut State, text: &str) -> String {
        self.record_selection(&state.input_buffer, text);
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
    /// 规整短语/命令候选显示文本：换行/制表 → 空格（杜绝多行候选），超长截断加省略号。
    /// `max` 为最大字符数（`input.phrase.max_display_chars`），0 表示不限制。
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
        let _ = self.update_candidates(state);
        self.notify_ui_update(state);
        state.preedit.clone()
    }

    pub(crate) fn commit_selected(&self, state: &mut State, cand: &Candidate) -> KeyAction {
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
        self.record_selection(&code, &cand.text);
        if partial {
            state.committed_segs.push((code, cand.text.clone()));
            state.committed_text.push_str(&cand.text);
            state.input_buffer = state.input_buffer[consumed..].to_string();
            let _ = self.update_candidates(state); // preedit 已含前缀（update_candidates 内拼接）
            let display = state.preedit.clone();
            self.notify_ui_update(state);
            KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            }
        } else {
            state.committed_segs.push((code, cand.text.clone()));
            let final_simplified = format!("{}{}", state.committed_text, cand.text);
            self.learn_phrase_on_commit(state); // 自动造词（多段组成的词）
            let out = self.maybe_s2t(state, &final_simplified);
            self.reset_pinyin_composition(state);
            self.notify_ui_hide();
            Self::commit_action(out, true)
        }
    }

    /// $CC 命令候选选中：清理组合区、隐藏 UI，把命令源放独立线程异步执行。
    /// **异步是必须的**：控制器经 Weak 回调 handle_menu_command 等自锁方法，而此刻本线程
    /// 仍持 state 锁（std::sync::Mutex 非可重入），同线程重入即死锁——交独立线程待本次按键
    /// 处理释放锁后再跑（对齐 Go「不在 SearchCommand 持锁路径里再 Lock」的约束）。
    pub(crate) fn commit_command(&self, state: &mut State, cand: &Candidate) -> KeyAction {
        let src = cand.phrase_template.clone();
        // 命令 nav（从前缀列举选中）携完整码 group_code，用它作执行输入上下文
        // （让 code()/input() 等按完整码求值）；精确码命令 group_code 空 → 用当前缓冲。
        let input = if cand.group_code.is_empty() {
            state.input_buffer.clone()
        } else {
            cand.group_code.clone()
        };
        self.reset_pinyin_composition(state);
        self.notify_ui_hide();
        self.spawn_command(src, input);
        // ClearComposition 而非 Consumed：清掉应用里已输入的命令码（如 "coen"），
        // 否则 composition 残留（Consumed 仅吞键、不结束 composition）。type() 的上屏文本
        // 由命令线程经 push 管道单独提交。
        KeyAction::ClearComposition
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
        let word = state.candidates[idx].text.clone();
        let code = state.input_buffer.clone();
        let schema = self.engine_mgr.active_schema_id();

        // 单字无规则保护：避免把某个单字彻底锁死（在写规则前判定）
        if matches!(op, CandidateOp::Delete) && word.chars().count() <= 1 {
            debug!("candidate_op: 拒绝删除单字 '{}'", word);
            return;
        }
        let last = state.candidates.len().saturating_sub(1);
        if let Some(store) = &self.store {
            // None cand_id：码表静态词无动态短语 id。redb 事务持久，无需显式落盘。
            let r = match op {
                CandidateOp::MoveTop => store.pin_shadow(&schema, &code, &word, None, 0),
                CandidateOp::MoveUp => {
                    store.pin_shadow(&schema, &code, &word, None, idx.saturating_sub(1))
                }
                CandidateOp::MoveDown => {
                    store.pin_shadow(&schema, &code, &word, None, (idx + 1).min(last))
                }
                CandidateOp::Delete => store.delete_shadow(&schema, &code, &word),
                CandidateOp::Reset => store.remove_shadow_rule(&schema, &code, &word),
            };
            if let Err(e) = r {
                warn!("shadow op failed: {}", e);
            }
        }

        // 重新构建候选（会重新应用 Shadow）并重绘
        self.update_candidates(&mut state);
        self.notify_ui_update(&state);
    }

    /// 点击选词：提交页内第 N 个候选，经 push 管道异步上屏（对齐 Go PushCommitText）。
    pub(crate) fn mouse_select(&self, page_local: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() {
            return;
        }
        let (start, end) = self.page_range(&state);
        let idx = start + page_local;
        if idx >= end || idx >= state.candidates.len() {
            return;
        }
        // 前缀导航候选：补全输入到完整码并重查展开（二级选择，鼠标点击同键盘选中）。
        if state.candidates[idx].is_group {
            let code = state.candidates[idx].group_code.clone();
            self.complete_to_group_code(&mut state, &code);
            return;
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
            return;
        }
        let text = state.candidates[idx].text.clone();
        let chinese_mode = state.chinese_mode;
        let out = self.commit_candidate(&mut state, &text);
        // 鼠标提交后彻底复位各输入模式，避免遗留状态
        state.active = None;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_prefix.clear();
        state.quick_input_buffer.clear();
        state.quick_input_prefix.clear();
        state.temp_english_buffer.clear();
        drop(state);

        self.notify_ui_hide();
        let encoded = wind_ipc::codec::encode_commit_text(&out, None, false, chinese_mode, false);
        // 仅推给活动客户端，避免广播导致多个 TSF 端重复上屏
        self.push_server.push_commit_to_active(&encoded);
        debug!(
            "mouse_select: committed '{}' (page_local={})",
            out, page_local
        );
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
