//! 应用兼容性规则
//!
//! 与 Go 版本 `wind_input/pkg/config/compat.go` 对齐：按进程名为特定应用提供候选窗
//! 定位 / 光标获取等兼容修正。文件格式为 TOML 的 `[[apps]]` 数组表，加载顺序：
//! 系统预置（`{data_dir}/compat.toml`）→ 用户覆盖（`{user_config_dir}/compat.toml`），
//! 用户层同进程名规则覆盖系统层。

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// 默认兼容规则文件名。
pub const COMPAT_FILE_NAME: &str = "compat.toml";

/// 单个应用的兼容性规则。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AppCompatRule {
    /// 进程名（不区分大小写），如 "Weixin.exe"。
    #[serde(default)]
    pub process: String,
    /// 说明（仅文档用途）。
    #[serde(default)]
    pub comment: String,
    /// 使用 caret rect 的 top 而非 bottom 定位候选窗。
    /// 适用于 GetTextExt 返回的 height 不稳定的 WebView 应用（如微信 Qt 输入框，
    /// height 在 1↔20px 间跳变 → bottom 漂移 ~20px，但 top 始终稳定）。
    #[serde(default)]
    pub caret_use_top: bool,
    /// 跳过首次 composition 的 CARET_PENDING 等待（光标稳定的应用）：新组合首帧不等宿主
    /// reflow 后的权威坐标，立即显示候选窗。消费点 `Coordinator::notify_ui_update` 的首显闸门。
    #[serde(default)]
    pub skip_caret_pending: bool,
    /// 固定候选窗位置：拖动后位置持久化记忆，跨会话恢复。
    ///
    /// ⚠ **当前是死字段，无任何消费点**：该能力后来由 `ui.candidate.position_mode = "fixed"`
    /// 那一套（drag_pin > fixed > 跟随光标 三级优先级）实现，与本 per-app 开关无关。
    /// 保留字段只为兼容既有 compat.toml 不报错；要么接线，要么随下次 compat 格式调整删除。
    /// 故意不写进 `data/compat.toml` 的字段文档，避免承诺不存在的功能。
    #[serde(default)]
    pub pin_candidate_position: bool,
}

/// 所有应用兼容性规则 + 运行时查找表。
#[derive(Debug, Clone, Default)]
pub struct AppCompat {
    apps: Vec<AppCompatRule>,
    /// 小写进程名 → `apps` 下标。
    lookup: HashMap<String, usize>,
}

/// 反序列化中间体：仅承载 `[[apps]]` 数组，避免把 `lookup` 暴露给 TOML。
#[derive(Debug, Deserialize, Default)]
struct AppCompatFile {
    #[serde(default)]
    apps: Vec<AppCompatRule>,
}

impl AppCompat {
    /// 从一组规则构建（含查找表）。
    pub fn from_rules(apps: Vec<AppCompatRule>) -> Self {
        let mut c = AppCompat {
            apps,
            lookup: HashMap::new(),
        };
        c.build_lookup();
        c
    }

    /// 按进程名（不区分大小写）查规则，未匹配返回 None。
    pub fn get_rule(&self, process_name: &str) -> Option<&AppCompatRule> {
        self.lookup
            .get(&process_name.to_ascii_lowercase())
            .map(|&i| &self.apps[i])
    }

    fn build_lookup(&mut self) {
        self.lookup = self
            .apps
            .iter()
            .enumerate()
            .map(|(i, r)| (r.process.to_ascii_lowercase(), i))
            .collect();
    }

    /// 加载兼容规则：系统层（`{data_dir}/compat.toml`）+ 用户层覆盖
    /// （`{user_dir}/compat.toml`）。任一文件缺失/解析失败均静默跳过。
    pub fn load(data_dir: Option<&Path>, user_dir: Option<&Path>) -> Self {
        let mut apps: Vec<AppCompatRule> = Vec::new();
        if let Some(d) = data_dir {
            if let Some(sys) = load_file(&d.join(COMPAT_FILE_NAME)) {
                apps = sys;
            }
        }
        if let Some(u) = user_dir {
            if let Some(user) = load_file(&u.join(COMPAT_FILE_NAME)) {
                apps = merge_rules(apps, user);
            }
        }
        Self::from_rules(apps)
    }
}

/// 解析单个 compat.toml；文件不存在或解析失败返回 None。
fn load_file(path: &Path) -> Option<Vec<AppCompatRule>> {
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: AppCompatFile = toml::from_str(&text).ok()?;
    Some(parsed.apps)
}

/// 合并两组规则：user 中同名进程（不区分大小写）覆盖 base，其余 base 规则保留，
/// 末尾追加全部 user 规则（与 Go `mergeCompatRules` 对齐）。
fn merge_rules(base: Vec<AppCompatRule>, user: Vec<AppCompatRule>) -> Vec<AppCompatRule> {
    if user.is_empty() {
        return base;
    }
    let user_keys: std::collections::HashSet<String> = user
        .iter()
        .map(|r| r.process.to_ascii_lowercase())
        .collect();
    let mut merged: Vec<AppCompatRule> = base
        .into_iter()
        .filter(|r| !user_keys.contains(&r.process.to_ascii_lowercase()))
        .collect();
    merged.extend(user);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_apps_array_and_lookup_case_insensitive() {
        let toml = r#"
            [[apps]]
            process = "Weixin.exe"
            comment = "微信"
            caret_use_top = true
        "#;
        let file: AppCompatFile = toml::from_str(toml).unwrap();
        let compat = AppCompat::from_rules(file.apps);

        // 进程名匹配不区分大小写。
        let rule = compat
            .get_rule("weixin.exe")
            .expect("应命中 Weixin.exe 规则");
        assert!(rule.caret_use_top);
        assert!(compat.get_rule("WEIXIN.EXE").unwrap().caret_use_top);
        // 未配置的进程无规则。
        assert!(compat.get_rule("notepad.exe").is_none());
    }

    #[test]
    fn caret_use_top_defaults_false_when_absent() {
        let toml = r#"
            [[apps]]
            process = "Foo.exe"
        "#;
        let file: AppCompatFile = toml::from_str(toml).unwrap();
        let compat = AppCompat::from_rules(file.apps);
        let rule = compat.get_rule("foo.exe").unwrap();
        assert!(!rule.caret_use_top);
        assert!(!rule.skip_caret_pending);
        assert!(!rule.pin_candidate_position);
    }

    #[test]
    fn user_rules_override_system_by_process_name() {
        let base = vec![AppCompatRule {
            process: "Weixin.exe".into(),
            caret_use_top: true,
            ..Default::default()
        }];
        let user = vec![AppCompatRule {
            process: "weixin.exe".into(), // 大小写不同仍视为同进程
            caret_use_top: false,
            ..Default::default()
        }];
        let merged = AppCompat::from_rules(merge_rules(base, user));
        // 用户层关闭了 caret_use_top，应覆盖系统层。
        assert!(!merged.get_rule("Weixin.exe").unwrap().caret_use_top);
        // 合并后只剩一条（同进程去重）。
        assert_eq!(merged.apps.len(), 1);
    }

    #[test]
    fn empty_user_keeps_base() {
        let base = vec![AppCompatRule {
            process: "Weixin.exe".into(),
            caret_use_top: true,
            ..Default::default()
        }];
        let merged = merge_rules(base, vec![]);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].caret_use_top);
    }
}
