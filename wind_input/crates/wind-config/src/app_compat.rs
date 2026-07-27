//! 应用兼容性规则
//!
//! 与 Go 版本 `wind_input/pkg/config/compat.go` 对齐：按进程名为特定应用提供候选窗
//! 定位 / 光标获取等兼容修正。文件格式为 TOML 的 `[[apps]]` 数组表，加载顺序：
//! 系统预置（`{data_dir}/compat.toml`）→ 用户覆盖（`{user_config_dir}/compat.toml`），
//! 用户层同进程名规则覆盖系统层。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// 默认兼容规则文件名。
pub const COMPAT_FILE_NAME: &str = "compat.toml";

/// 写回用户层 compat.toml 时的固定文件头。
///
/// 用户层由右键菜单自动管理，每次切换都会**整份重写**（TOML 序列化不保留注释），
/// 故必须在文件里就把这件事讲明白，否则用户手写的说明被吞掉时无从得知原因。
/// 完整的字段文档留在系统层 `data/compat.toml`——那份不会被程序改写。
const USER_COMPAT_HEADER: &str = "\
# 用户层应用兼容规则（覆盖 / 追加系统层 data/compat.toml）
#
# ⚠ 本文件由输入法右键菜单自动管理，每次通过菜单切换开关都会整份重写，
#   手写的注释与排版不会保留。需要长期留存的说明请写在系统层 compat.toml。
#
# 合并语义：同名进程（不区分大小写）整条覆盖系统层，系统层其余规则保留。
# 字段说明见系统层 data/compat.toml 顶部注释。

";

/// `skip_serializing_if` 用：省略默认为 false 的开关，避免写回时铺满一堆 `= false`。
fn is_false(b: &bool) -> bool {
    !*b
}

/// 候选窗首显策略：新组合的候选窗**何时**显示。
///
/// 背景：宿主插入组合内容后要 reflow 才能给出正确的光标坐标，而 reflow 需要时间
/// （实测首帧 GetTextExt 到稳定值要 85~95ms）。这三档是「快」与「准」之间的取舍。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FirstShowMode {
    /// 等宿主 reflow 后的权威坐标才显示。最准，代价是 85~95ms 首显延迟，
    /// 快速连打时候选窗只来得及显示几毫秒，观感「迟钝」。
    #[default]
    Wait,
    /// 仍等坐标，但等到「可信」即放行：DLL 在首帧 reflow 期间连发几条试探坐标，
    /// 取第一条「与上一轮权威坐标不同」的采用（宿主未 reflow 时返回的正是上一轮那个
    /// 位置，一旦变化即说明新位置已就绪）。连续快速输入时更进一步——直接采信首条。
    /// 实测 EverEdit ~3ms、WPS ~11ms 出候选窗。
    Fast,
    /// 完全不等，首帧直接沿用上一次的坐标。最快，但只要光标位置变动过
    /// （手动移动、换行、文本重排）那个位置就是错的，会先错位显示再跳回。
    Instant,
}

impl FirstShowMode {
    /// 配置串 → 枚举。无法识别时回落 `Wait`（最保守的一档）。
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "fast" => Self::Fast,
            "instant" => Self::Instant,
            _ => Self::Wait,
        }
    }
    /// 枚举 → 配置串（写回 compat.toml 用）。
    pub fn as_config(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Fast => "fast",
            Self::Instant => "instant",
        }
    }
}

/// 单个应用的兼容性规则。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AppCompatRule {
    /// 进程名（不区分大小写），如 "Weixin.exe"。
    #[serde(default)]
    pub process: String,
    /// 说明（仅文档用途）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
    /// 使用 caret rect 的 top 而非 bottom 定位候选窗。
    /// 适用于 GetTextExt 返回的 height 不稳定的 WebView 应用（如微信 Qt 输入框，
    /// height 在 1↔20px 间跳变 → bottom 漂移 ~20px，但 top 始终稳定）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub caret_use_top: bool,
    /// 候选窗首显策略。三档互斥——做成枚举而不是几个 bool：布尔开关可以同时打开，
    /// 实测就因此出过一次「fast 配了却从未生效」（instant 优先、抢先放行，fast 的判据
    /// 根本没机会跑），日志里 630 条试探坐标一条没被消费。互斥语义要由类型保证。
    #[serde(default)]
    pub first_show_mode: FirstShowMode,
    /// 固定候选窗位置：拖动后位置持久化记忆，跨会话恢复。
    ///
    /// ⚠ **当前是死字段，无任何消费点**：该能力后来由 `ui.candidate.position_mode = "fixed"`
    /// 那一套（drag_pin > fixed > 跟随光标 三级优先级）实现，与本 per-app 开关无关。
    /// 保留字段只为兼容既有 compat.toml 不报错；要么接线，要么随下次 compat 格式调整删除。
    /// 故意不写进 `data/compat.toml` 的字段文档，避免承诺不存在的功能。
    #[serde(default, skip_serializing_if = "is_false")]
    pub pin_candidate_position: bool,
}

/// 在一组规则上设置指定进程的首显策略。
///
/// 纯函数（不碰文件系统），故可直接单测——本仓凡涉 `%APPDATA%` 落盘的逻辑都要这样
/// 抽出来，否则端到端测试会真写用户配置目录（见 project_dict_override_sparse_merge 的教训）。
///
/// 进程名不区分大小写匹配；命中则只改这一个字段、其余字段保持不动；未命中则**追加**
/// 一条只带该字段的新规则（不是整表快照，避免把系统层的其它字段冻结进用户层）。
pub fn set_first_show_mode(rules: &mut Vec<AppCompatRule>, process: &str, mode: FirstShowMode) {
    let key = process.to_ascii_lowercase();
    for r in rules.iter_mut() {
        if r.process.to_ascii_lowercase() == key {
            r.first_show_mode = mode;
            return;
        }
    }
    rules.push(AppCompatRule {
        process: process.to_string(),
        first_show_mode: mode,
        ..Default::default()
    });
}

/// 把规则集渲染成用户层 compat.toml 全文（含固定文件头）。纯函数，便于单测断言产物。
pub fn render_user_compat(rules: &[AppCompatRule]) -> Result<String, toml::ser::Error> {
    let file = AppCompatFile {
        apps: rules.to_vec(),
    };
    Ok(format!("{USER_COMPAT_HEADER}{}", toml::to_string(&file)?))
}

/// 设置用户层 compat.toml 中指定进程的首显策略。
///
/// 只读写**用户层**：系统层 `data/compat.toml` 不受影响（合并时用户层同名进程整条覆盖它）。
/// 文件或目录不存在时自动创建；解析失败按空规则集处理（宁可重建也不要把菜单卡死，
/// 用户手改坏了 TOML 时仍能通过菜单恢复到可用状态）。
pub fn set_user_first_show_mode(
    user_dir: &Path,
    process: &str,
    mode: FirstShowMode,
) -> Result<(), std::io::Error> {
    let path = user_dir.join(COMPAT_FILE_NAME);
    let mut rules = load_file(&path).unwrap_or_default();
    set_first_show_mode(&mut rules, process, mode);
    let text = render_user_compat(&rules)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::create_dir_all(user_dir)?;
    std::fs::write(&path, text)?;
    Ok(())
}

/// 所有应用兼容性规则 + 运行时查找表。
#[derive(Debug, Clone, Default)]
pub struct AppCompat {
    apps: Vec<AppCompatRule>,
    /// 小写进程名 → `apps` 下标。
    lookup: HashMap<String, usize>,
}

/// 序列化中间体：仅承载 `[[apps]]` 数组，避免把 `lookup` 暴露给 TOML。
#[derive(Debug, Deserialize, Serialize, Default)]
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
        assert_eq!(rule.first_show_mode, FirstShowMode::Wait);
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

    #[test]
    fn set_mode_on_existing_rule_keeps_other_fields() {
        // 命中已有规则：只改 first_show_mode，caret_use_top / comment 不得被动。
        let mut rules = vec![AppCompatRule {
            process: "Weixin.exe".into(),
            comment: "微信".into(),
            caret_use_top: true,
            ..Default::default()
        }];
        set_first_show_mode(&mut rules, "weixin.exe", FirstShowMode::Fast); // 大小写无关
        assert_eq!(rules.len(), 1, "命中时不得追加新规则");
        assert_eq!(rules[0].first_show_mode, FirstShowMode::Fast);
        assert!(rules[0].caret_use_top, "其它字段不得被连带修改");
        assert_eq!(rules[0].comment, "微信");
        // 三档互斥：再设一次直接覆盖，不存在「两档同时生效」的中间态
        // ——正是布尔开关时代那个「fast 配了却从未生效」的成因。
        set_first_show_mode(&mut rules, "Weixin.EXE", FirstShowMode::Instant);
        assert_eq!(rules[0].first_show_mode, FirstShowMode::Instant);
    }

    #[test]
    fn set_mode_appends_minimal_rule_when_absent() {
        // 未命中：追加**只带该字段**的最小规则，不做整表快照（否则会把系统层其它字段
        // 冻结进用户层，正是 project_dict_override_sparse_merge 记录过的坑）。
        let mut rules: Vec<AppCompatRule> = Vec::new();
        set_first_show_mode(&mut rules, "EverEdit.exe", FirstShowMode::Fast);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].process, "EverEdit.exe", "应保留原始大小写");
        assert_eq!(rules[0].first_show_mode, FirstShowMode::Fast);
        assert!(!rules[0].caret_use_top);
    }

    #[test]
    fn mode_parses_from_config_and_falls_back_to_wait() {
        assert_eq!(FirstShowMode::from_config("fast"), FirstShowMode::Fast);
        assert_eq!(
            FirstShowMode::from_config(" INSTANT "),
            FirstShowMode::Instant
        );
        assert_eq!(FirstShowMode::from_config("wait"), FirstShowMode::Wait);
        // 未知值回落最保守的一档，而不是 panic 或取激进档——用户手改错了不该变成抖动。
        assert_eq!(FirstShowMode::from_config("turbo"), FirstShowMode::Wait);
        assert_eq!(FirstShowMode::default(), FirstShowMode::Wait);
        assert_eq!(FirstShowMode::Fast.as_config(), "fast");
    }

    #[test]
    fn render_omits_false_flags_and_roundtrips() {
        // 渲染产物：false 开关与空 comment 全部省略（不铺 `= false`），且能被自己解析回来。
        let rules = vec![AppCompatRule {
            process: "EverEdit.exe".into(),
            first_show_mode: FirstShowMode::Fast,
            ..Default::default()
        }];
        let text = render_user_compat(&rules).expect("渲染失败");
        assert!(text.contains(r#"first_show_mode = "fast""#), "产物: {text}");
        assert!(
            !text.contains("caret_use_top"),
            "false 开关不应写出: {text}"
        );
        assert!(!text.contains("comment"), "空 comment 不应写出: {text}");
        assert!(text.starts_with("# 用户层应用兼容规则"), "缺少文件头警示");

        let parsed: AppCompatFile = toml::from_str(&text).expect("产物应可解析");
        assert_eq!(parsed.apps.len(), 1);
        assert_eq!(parsed.apps[0].first_show_mode, FirstShowMode::Fast);
        assert_eq!(parsed.apps[0].process, "EverEdit.exe");
    }
}
