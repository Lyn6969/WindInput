//! 合并引擎:导入/还原的策略与结果类型。逐表 dry-run 计算在各功能任务里接线到 store。
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    #[default]
    Merge,
    Replace,
}

impl Strategy {
    /// 从 RPC 参数字符串解析;未知值回退 Merge(默认合并)。
    pub fn from_param(s: &str) -> Strategy {
        match s {
            "replace" => Strategy::Replace,
            _ => Strategy::Merge,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportOutcome {
    pub added: usize,
    pub updated: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    pub will_add: usize,
    pub will_update: usize,
    pub will_conflict: usize,
    pub unchanged: usize,
    pub samples: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_from_param() {
        assert_eq!(Strategy::from_param("replace"), Strategy::Replace);
        assert_eq!(Strategy::from_param("merge"), Strategy::Merge);
        assert_eq!(
            Strategy::from_param("garbage"),
            Strategy::Merge,
            "未知回退 Merge"
        );
        assert_eq!(Strategy::default(), Strategy::Merge);
    }

    #[test]
    fn preview_serializes_camel_case() {
        let p = ImportPreview {
            will_add: 3,
            will_update: 1,
            will_conflict: 0,
            unchanged: 2,
            samples: vec!["工".into()],
        };
        let j = serde_json::to_string(&p).unwrap();
        assert!(j.contains("willAdd"), "字段应为 camelCase 供前端");
        assert!(j.contains("\"willAdd\":3"));
    }
}
