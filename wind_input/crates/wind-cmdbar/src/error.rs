//! 命令栏错误类型
//!
//! 升级 Go 的字符串拼接错误为可分类枚举（便于宿主按种类降级：解析失败回退原短语、
//! 服务缺失静默、运行期错误记 WARN）。

use thiserror::Error;

/// 命令栏解析/求值错误。
#[derive(Debug, Error)]
pub enum CmdbarError {
    /// 词法/语法错误，附带源字节偏移。
    #[error("parse error at offset {offset}: {msg}")]
    Parse { offset: usize, msg: String },

    /// 引用了未注册的函数。
    #[error("unknown function {name:?}")]
    UnknownFunc { name: String },

    /// 参数个数不符。
    #[error("function {name:?} called with {got} args (min={min}, max={max})")]
    Arity {
        name: String,
        got: usize,
        min: usize,
        max: isize,
    },

    /// 在 `$CC` display 位调用了副作用函数（仅纯函数允许）。
    #[error("display: function {name:?} is not allowed (side-effecting)")]
    NotPure { name: String },

    /// 所需宿主服务未注入。
    #[error("{func}: service unavailable")]
    ServiceUnavailable { func: String },

    /// 函数执行期错误（类型转换失败、服务调用失败等）。
    #[error("{func}: {msg}")]
    Runtime { func: String, msg: String },

    /// 函数尚未实现（stub）。
    #[error("function not implemented: {name}")]
    NotImplemented { name: String },
}

impl CmdbarError {
    pub fn parse(offset: usize, msg: impl Into<String>) -> Self {
        CmdbarError::Parse {
            offset,
            msg: msg.into(),
        }
    }

    pub fn runtime(func: impl Into<String>, msg: impl Into<String>) -> Self {
        CmdbarError::Runtime {
            func: func.into(),
            msg: msg.into(),
        }
    }

    pub fn service(func: impl Into<String>) -> Self {
        CmdbarError::ServiceUnavailable { func: func.into() }
    }
}

pub type Result<T> = std::result::Result<T, CmdbarError>;
