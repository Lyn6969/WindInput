//! 主题定义
//!
//! 与 Go 版本 `wind_input/pkg/theme/theme.go` 对齐。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 主题定义
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Theme {
    #[serde(default)]
    pub meta: ThemeMeta,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub colors: Option<PaletteSchema>,
    #[serde(default)]
    pub views: Option<Views>,
    #[serde(default)]
    pub behavior: Option<Behavior>,
    #[serde(default)]
    pub resources: HashMap<String, ResourceRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThemeMeta {
    pub name: String,
    pub author: String,
    pub version: String,
}

pub type PaletteSchema = HashMap<String, ColorToken>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColorToken {
    Hex(String),
    LightDark { light: String, dark: String },
}

/// Views 定义
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Views {
    #[serde(default)]
    pub window: Option<ViewNode>,
    #[serde(default)]
    pub candidate_list: Option<ViewNode>,
    #[serde(default)]
    pub item: Option<ViewNode>,
    #[serde(default)]
    pub text: Option<ViewNode>,
}

/// View 节点
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ViewNode {
    #[serde(default)]
    pub margin: Option<[f64; 4]>,
    #[serde(default)]
    pub padding: Option<[f64; 4]>,
    #[serde(default)]
    pub background: Option<String>,
}

/// 行为配置
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Behavior {
    #[serde(default)]
    pub font_size: Option<f64>,
}

/// 资源引用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceRef {
    pub light: Option<String>,
    pub dark: Option<String>,
}
