//! 测试/诊断支撑接口（debug_*）：仅测试与诊断代码消费，生产路径不调用。
//!
//! （自 coordinator.rs 平移，纯搬运。生产 tooltip 用的 DebugSchemaCtx 一族
//! 名字带 debug 但在真实路径上，不在此文件。）

use wind_ui_types::UiEvent;

use crate::coordinator::Coordinator;
use crate::pipeline::ModeKind;

impl Coordinator {
    /// 当前**已启用**的方案列表（`schema.available`，测试/诊断用）。
    ///
    /// 与 [`Self::active_schema_id`] 不同，这里回答的是「哪些方案会被启动预热覆盖」。
    /// 测试用它守住「目标方案确实未启用」这个前提——失去前提的回归用例会在已启用
    /// 方案上空跑一遍、永远绿。
    pub fn debug_available_schemas(&self) -> Vec<String> {
        self.engine_mgr.available_schemas()
    }

    /// 推给 TSF 的 key_up 热键白名单（测试/诊断用）。
    ///
    /// 这正是 `push_activation_status` 发出去的那份，不是另算一遍——修饰键类绑定
    /// 「能不能被触发」完全取决于它在不在这里面，用旁路重算的值断言等于没测。
    pub fn debug_key_up_hotkeys(&self) -> Vec<u32> {
        self.rt().compiled_hotkeys.key_up_tsf_hashes()
    }

    /// 直接装载短语层（仅测试用）：`(code, text, weight, position, is_system)`。
    ///
    /// ★ 补的是一个**结构性**测试缺口：真机短语层经 redb `store` 建立，而 headless 测试的
    /// `store` 是 `None` → 短语层恒空 → 所有依赖短语的判据（`has_code_prefix` 的前缀命中、
    /// z 的活码身份、夺取回路的触发条件）在测试里全都走不到。测试演示的是「z 是死码」那条
    /// 分支，真机跑的是「z 有 37 条 `zz*` 前缀」那条——两边结构性分叉，测试再绿也盖不住真机。
    ///
    /// 这个缺口让「让位判据与候选构建门槛不同源」整个漏到真机（见 `has_code_prefix` 文档）。
    pub fn debug_install_phrases(&self, records: Vec<(String, String, i32, i32, bool)>) {
        *self.phrases.write().unwrap_or_else(|e| e.into_inner()) =
            wind_phrase::PhraseLayer::from_records(records);
    }

    /// 是否还有更多候选未加载（测试/诊断用）
    /// 当前激活的 overlay 模式类别名；`None` = 普通输入。仅供测试断言。
    pub fn debug_active_mode(&self) -> Option<&'static str> {
        match self.state.lock().unwrap_or_else(|e| e.into_inner()).active {
            Some(ModeKind::TempPinyin) => Some("temp_pinyin"),
            Some(ModeKind::TempEnglish) => Some("temp_english"),
            Some(ModeKind::Url) => Some("url"),
            Some(ModeKind::Special(_)) => Some("special"),
            Some(ModeKind::Mix(_)) => Some("mix"),
            None => None,
        }
    }

    pub fn debug_has_more(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_more
    }

    /// 分页信息 (当前页0-based, 页内高亮0-based, 总页数)（测试/诊断用）
    pub fn debug_page_info(&self) -> (usize, usize, usize) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (s.current_page, s.selected_index, self.total_pages(&s))
    }

    /// 注入「候选窗当前是否反转排列」（测试/诊断用）。
    ///
    /// 刻意走 [`Coordinator::handle_ui_event`] 而非直接写字段——正式路径是 UI 线程发
    /// `UiEvent::CandidateFlipped`，测试入口跳过分发就测不到那条接线（同 `debug_candidate_op`）。
    pub fn debug_set_candidate_flipped(&self, flipped: bool) {
        self.handle_ui_event(UiEvent::CandidateFlipped(flipped));
    }

    /// 将统计采集器内存数据落库（测试/诊断用；生产由后台线程定时 flush）。
    pub fn debug_flush_stats(&self) {
        if let Some(c) = self.stat_collector.as_ref() {
            c.flush();
        }
    }

    /// 当前页候选文本列表（内部简体；测试/诊断用）
    pub fn debug_page_texts(&self) -> Vec<String> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (start, end) = self.page_range(&s);
        s.candidates[start..end]
            .iter()
            .map(|c| c.text.clone())
            .collect()
    }

    /// 当前页候选的"显示文本"（应用简繁后，与候选窗口一致；测试/诊断用）
    pub fn debug_page_display_texts(&self) -> Vec<String> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (start, end) = self.page_range(&s);
        s.candidates[start..end]
            .iter()
            .map(|c| self.cand_s2t_text(&s, c))
            .collect()
    }

    // ── webdata 契约测试(wind-webdata,crate 外)的白盒支撑 ──
    // 测试要验证「记账 → RPC 读出」的联动,记账入口是 pub(crate);经 debug_* 暴露,
    // 生产路径不调用。

    /// 上屏记账转发(仅测试)。
    pub fn debug_record_commit(
        &self,
        text: &str,
        code_len: u32,
        candidate_pos: i32,
        source: wind_store::stats::CommitSource,
    ) {
        self.record_commit(text, code_len, candidate_pos, source);
    }

    /// 顶层输入统计兜底转发(仅测试)。
    pub fn debug_record_input_stats(&self, action: &wind_bridge::handler::KeyAction) {
        self.record_input_stats(action);
    }

    /// 本次按键是否已被具体上屏路径记账(仅测试)。
    pub fn debug_stat_recorded(&self) -> bool {
        self.stat_recorded
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 短语层查询,返回命中文本(仅测试)。
    pub fn debug_phrase_texts(&self, code: &str) -> Vec<String> {
        self.phrases
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .lookup(code, &[], &|_| String::new())
            .into_iter()
            .map(|c| c.text)
            .collect()
    }
}
