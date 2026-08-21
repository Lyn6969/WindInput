//! 工具栏状态数据（由协调器推送，渲染端呈现）。

/// 工具栏状态（由协调器推送）
///
/// `PartialEq` 供协调器做**推送去重**（见 `notify_toolbar`）：宿主焦点抖动时同一份状态
/// 会被连推数次，全挤到 UI 线程上。用 derive 而不是手写比较——加字段时编译器自动带上，
/// 手写的那种漏一个字段就是「改了状态工具栏不更新」，且没有任何报错。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolbarState {
    pub chinese_mode: bool,
    /// 有效显示标签：中文模式取方案 icon_label（如 "拼"/"五"），无则 "中"；
    /// 英文小写为 "英"，大写锁定为 "A"（由协调器预计算后填入）。
    pub icon_label: String,
    pub caps_lock: bool,
    pub full_width: bool,
    pub chinese_punct: bool,
    /// 简繁转换当前是否启用（格内显示 "繁" 并高亮）
    pub s2t_enabled: bool,
    /// 是否显示简繁格（默认 false；用户开启简繁功能后显示）
    pub s2t_shown: bool,
    /// 当前打不出中文（密码框 / 焦点不在可编辑控件里 / 系统级禁用）：仅影响**呈现**
    /// （模式格显 "英" 且不高亮）。取值来自协调器的 `effective_input_block()`——
    /// 语言栏图标读的是**同一个**判定，两者不会再各说各话。
    ///
    /// 独立于 `icon_label` 而非直接改写它：后者是「当前方案标签」的单一语义，且会经
    /// StatusUpdate 下发写入 TSF 的 `_inputTypeLabel`（持久值）。把这种随焦点来去的
    /// 临时态烧进标签，离开时就得指望下一次状态推送把它改回来，漏一次图标即长期卡 "英"。
    pub input_blocked: bool,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            chinese_mode: true,
            icon_label: "中".to_string(),
            caps_lock: false,
            full_width: false,
            chinese_punct: true,
            s2t_enabled: false,
            s2t_shown: false,
            input_blocked: false,
        }
    }
}
