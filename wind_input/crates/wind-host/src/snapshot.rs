//! 输入态快照：宿主按需拉取的完整状态。

/// 引擎模式真值。**是数据，不是界面**——画几个格、放哪、什么图标由宿主决定。
///
/// 宿主不得自记一份：方案切换会改写 [`icon_label`](Self::icon_label)，密码框会强制英文，
/// 都发生在核心内部；本地第二份真值必然漂移。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModeFlags {
    pub chinese_mode: bool,
    /// 模式主字，最多 2 个字符。中文态取方案的 `[schema] icon_label`（"五"/"拼"），
    /// 非中文态取 `[ui.labels]`（出厂 "英"/"A"，**用户可配**）。
    ///
    /// ⚠️ 宿主不得对取值做任何假设（"是不是英"、"长度是不是 1"）：它由核心单点
    /// 计算下发，判据和取值都在 `Coordinator::mode_icon_label`。
    pub icon_label: String,
    pub full_width: bool,
    pub chinese_punct: bool,
    pub s2t_enabled: bool,
    /// 是否该展示简繁开关（用户未启用简繁功能时为 false）
    pub s2t_shown: bool,
    /// 密码框强制英文生效中：仅影响**呈现**（显"英"且不高亮）
    pub password_suppress: bool,
}

/// 一次渲染所需的完整输入态。
///
/// # 为什么 preedit 在这里而不是只在推送里
///
/// 核心此前把 preedit 的下发**门控在渲染策略上**：`ui.candidate.preedit_display`
/// 取默认值 `app_inline` 时，协调器认为「组合区归宿主画」，于是**根本不下发 preedit**。
/// 那对 TSF 是成立的（宿主确实自己画组合区），但它把「状态是什么」和「谁来画」
/// 绑死了——自绘编码栏的宿主要拿数据，只能去改一个显示模式配置。
///
/// 快照里的字段一律是**状态**，与谁渲染无关。渲染策略降级为给宿主的建议。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputSnapshot {
    /// 编码区正文（五笔码 / 拼音串）。空 = 无组合。
    pub preedit: String,
    /// 编码区插入符位置：`preedit` 内的字节偏移（恒在字符边界）
    pub preedit_caret: usize,
    /// 模式指示短文本（五/拼/英/符…），空 = 不显示
    pub mode_label: String,
    /// 当前页候选文本
    pub candidates: Vec<String>,
    /// 键盘选中项（页内下标）
    pub selected: usize,
    /// 当前页（1 起）
    pub page: usize,
    pub total_pages: usize,
    pub mode: ModeFlags,
}
