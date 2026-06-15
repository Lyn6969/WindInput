//! AST 节点定义
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/ast/ast.go` 对齐。

/// Phrase 类型
#[derive(Debug, Clone)]
pub enum Phrase {
    Literal(String),
    Template(Expr),
    Command { display: Expr, actions: Vec<Expr> },
    Array { name: String, elements: Vec<Expr> },
}

/// 表达式类型
#[derive(Debug, Clone)]
pub enum Expr {
    StringLit(StringParts),
    NumberLit(f64),
    Ident(String),
    Call { name: String, args: Vec<Expr> },
}

/// 字符串部分（含插值）
#[derive(Debug, Clone)]
pub enum StringPart {
    Text(String),
    Interpolation(Expr),
}

pub type StringParts = Vec<StringPart>;
