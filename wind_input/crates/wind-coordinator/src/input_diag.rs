//! 输入诊断纯数据类型 + InputScope 掩码判定（无 I/O，可单测）。

/// InputScope 位：与 C++ kScopeBitPassword / Go 端一致。
const IS_PASSWORD_BIT: u64 = 1 << 31;
const IS_NUMERIC_PASSWORD_BIT: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputDiagReason {
    None,
    CompartmentDisabled,
    InputScopePassword,
    NumericPassword,
}

impl Default for InputDiagReason {
    fn default() -> Self {
        InputDiagReason::None
    }
}

/// 判定禁用原因。compartment（DLL 已放行所有键）优先级最高。
pub fn reason_from(disabled: bool, mask: u64) -> InputDiagReason {
    if disabled {
        return InputDiagReason::CompartmentDisabled;
    }
    if mask & IS_NUMERIC_PASSWORD_BIT != 0 {
        return InputDiagReason::NumericPassword;
    }
    if mask & IS_PASSWORD_BIT != 0 {
        return InputDiagReason::InputScopePassword;
    }
    InputDiagReason::None
}

/// mask 是否命中密码/数字密码位（用于抑制策略）。
pub fn is_password_scope(mask: u64) -> bool {
    mask & (IS_PASSWORD_BIT | IS_NUMERIC_PASSWORD_BIT) != 0
}

pub fn reason_label(r: InputDiagReason) -> &'static str {
    match r {
        InputDiagReason::None => "无",
        InputDiagReason::CompartmentDisabled => "compartment",
        InputDiagReason::InputScopePassword => "密码",
        InputDiagReason::NumericPassword => "数字密码",
    }
}

#[derive(Clone, Debug, Default)]
pub struct InputDiagState {
    pub pid: u32,
    pub process_name: String,
    pub disabled: bool,
    pub reason: InputDiagReason,
    pub mask: u64,
}

/// 窗口 / TSF 上下文诊断快照的**存放位置在 wind-ui**（`input_diag_hud::WindowDiagView`）。
///
/// 依赖方向是 coordinator → ui，HUD 视图类型只能定义在 ui 侧；这里 re-export 一次，
/// 让本模块仍是"输入诊断数据类型"的单一入口。
pub use wind_ui::manager::WindowDiagView;

#[cfg(test)]
mod tests {
    use super::*;

    const IS_PASSWORD: u64 = 1 << 31;
    const IS_NUMERIC_PASSWORD: u64 = 1 << 63;

    #[test]
    fn reason_none_when_clean() {
        assert_eq!(reason_from(false, 0), InputDiagReason::None);
    }

    #[test]
    fn compartment_takes_precedence_over_mask() {
        // disabled=true 一律 CompartmentDisabled，即便 mask 有密码位
        assert_eq!(
            reason_from(true, IS_PASSWORD),
            InputDiagReason::CompartmentDisabled
        );
    }

    #[test]
    fn password_and_numeric_from_mask() {
        assert_eq!(
            reason_from(false, IS_PASSWORD),
            InputDiagReason::InputScopePassword
        );
        assert_eq!(
            reason_from(false, IS_NUMERIC_PASSWORD),
            InputDiagReason::NumericPassword
        );
    }

    #[test]
    fn is_password_scope_covers_both_bits() {
        assert!(is_password_scope(IS_PASSWORD));
        assert!(is_password_scope(IS_NUMERIC_PASSWORD));
        assert!(!is_password_scope(0));
    }
}
