//! Schema YAML 定义
//!
//! 与 Go 版本 `wind_input/internal/schema/schema.go` 对齐。

use serde::{Deserialize, Serialize};

/// 引擎类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineType {
    Pinyin,
    CodeTable,
    Mixed,
}

impl Default for EngineType {
    fn default() -> Self {
        Self::Pinyin
    }
}

/// 完整 Schema 定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    pub schema: SchemaInfo,
    #[serde(default)]
    pub engine: EngineSpec,
    #[serde(default)]
    pub dictionaries: Vec<DictSpec>,
    #[serde(default)]
    pub learning: LearningSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SchemaInfo {
    pub id: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EngineSpec {
    #[serde(rename = "type")]
    pub engine_type: EngineType,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DictSpec {
    pub id: String,
    pub label: String,
    pub path: String,
    #[serde(rename = "type")]
    pub dict_type: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LearningSpec {
    #[serde(default)]
    pub auto_learn: bool,
    #[serde(default)]
    pub auto_phrase: bool,
}
