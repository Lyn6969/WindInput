//! 内置函数实现
//!
//! 对照 Go `wind_input/internal/cmdbar/funcs/`。每组函数在各自文件里以 `specs()` 暴露
//! [`FuncSpec`](crate::registry::FuncSpec) 列表，由 [`Registry`](crate::registry::Registry) 装配。

pub mod action;
pub mod calc;
pub mod config;
pub mod dict_ime;
pub mod help;
pub mod text;
pub mod util;
pub mod value;

/// 声明式构造 [`FuncSpec`](crate::registry::FuncSpec) 列表，压缩 ~50 个函数的样板。
///
/// 用法（kind 为单关键字）：
/// ```ignore
/// func_specs! {
///     "len" : Text (1, 1) det => fn_len, "字符数", "len(x)";
///     "code": Value (0, 1) pure => fn_code, "输入编码", "code()";
///     "open": Action (1, 1) effect => fn_open, "打开", "open(\"u\")";
/// }
/// ```
/// `det` = 纯且确定；`pure` = 纯但非确定（依赖外部状态）；`effect` = 副作用函数。
macro_rules! func_specs {
    ( $( $name:literal : $cat:ident ( $min:expr, $max:expr ) $kind:ident => $eval:path , $desc:literal , $ex:literal );+ $(;)? ) => {
        vec![ $(
            $crate::registry::FuncSpec {
                name: $name,
                category: $crate::registry::Category::$cat,
                min_args: $min,
                max_args: $max,
                pure: func_specs!(@pure $kind),
                deterministic: func_specs!(@det $kind),
                deprecated: false,
                alias_of: "",
                description: $desc,
                example: $ex,
                eval: $eval,
            }
        ),+ ]
    };
    (@pure pure) => { true };
    (@pure det) => { true };
    (@pure effect) => { false };
    (@det pure) => { false };
    (@det det) => { true };
    (@det effect) => { false };
}

pub(crate) use func_specs;
