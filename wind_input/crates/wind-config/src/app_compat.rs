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
    ///
    /// 2026-08-03 起不再是默认档。它的「准」有很大一部分是**碰巧**的：Excel 那类
    /// 慢宿主上它靠 `caret_pending` 的 600ms 延长兜住，宿主再慢 50ms 一样会错位
    /// （实测 Excel 需要 808ms 的那次它就没兜住）。真正解决错位的是首帧信任门，
    /// 而那条判据 `fast` 同样享有。
    Wait,
    /// 仍等坐标，但等到「可信」即放行：DLL 在首帧 reflow 期间连发几条试探坐标，
    /// 取第一条「与上一轮权威坐标不同」的采用（宿主未 reflow 时返回的正是上一轮那个
    /// 位置，一旦变化即说明新位置已就绪）。连续快速输入时更进一步——直接采信首条。
    /// 实测 EverEdit ~3ms、WPS ~11ms 出候选窗。
    ///
    /// **默认档**（2026-08-03 起）。此前不敢作默认，是因为它在焦点切换/鼠标移动光标
    /// 之后的首帧会拿一份属于别处的旧坐标去定位；首帧信任门补上这个洞之后
    /// （`caret_cache_verified`，见 `docs/redesign/candidate-window-positioning.md`
    /// 第 6 层），它在「坐标不可信」的那一刻会自动退回去等真值，其余时候保持 25ms
    /// 短兜底。实测常规连打首帧中位 7ms，焦点后首帧中位 105ms 且位置正确。
    #[default]
    Fast,
    /// 完全不等，首帧直接沿用上一次的坐标。最快，但只要光标位置变动过
    /// （手动移动、换行、文本重排）那个位置就是错的，会先错位显示再跳回。
    Instant,
}

impl FirstShowMode {
    /// 配置串 → 枚举。无法识别时回落**默认档**——「写了个认不出的值」与「没写」
    /// 得到同样的行为，最不意外。
    ///
    /// ⚠ 刻意写 `Self::default()` 而非硬编码某一档：这里曾硬编码 `Wait`，与
    /// `#[default]` 是两处独立事实，2026-08-03 调默认档时若不是顺手查了一遍就会漏改，
    /// 而漏改**不会有任何编译或测试信号**（生产走 serde，本函数只有测试在调）。
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "wait" => Self::Wait,
            "fast" => Self::Fast,
            "instant" => Self::Instant,
            _ => Self::default(),
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

/// 应用独立的初始中英状态取值。
///
/// 语义是**初始值而非锁定**：进入该应用时套用，用户随后可自由手动切换，
/// 停留在该应用期间不再被改写（详见 `Coordinator::initial_chinese_mode_for`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InitialMode {
    English,
    Chinese,
}

impl InitialMode {
    /// 配置串 → 枚举。无法识别返回 `None`（＝不干预），不 panic 也不回落到某一档：
    /// 「用户拼错了」与「用户想要英文」是两回事，后者必须是显式写对才成立。
    pub fn from_config(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "english" | "en" => Some(Self::English),
            "chinese" | "zh" => Some(Self::Chinese),
            _ => None,
        }
    }
    /// 枚举 → 配置串（写回 compat.toml 用）。
    pub fn as_config(self) -> &'static str {
        match self {
            Self::English => "english",
            Self::Chinese => "chinese",
        }
    }
    /// 落到 `chinese_mode` / `chinese_punct` 这类布尔状态。
    pub fn is_chinese(self) -> bool {
        matches!(self, Self::Chinese)
    }
    /// 布尔 → 枚举（菜单写盘时把当前状态反写成规则用）。
    pub fn from_chinese(chinese: bool) -> Self {
        if chinese {
            Self::Chinese
        } else {
            Self::English
        }
    }
}

/// 容错反序列化 `Option<InitialMode>`：无法识别的值退化为 `None`（＝不干预）。
///
/// ⚠ 不能直接 `#[derive(Deserialize)]` 让 serde 自己认字符串：`load_file` 解析失败时
/// 返回 `None` 会**整份 compat.toml 静默跳过**，于是一个字段拼错就让该文件里所有应用的
/// 所有规则一起失效，且日志里毫无痕迹。单字段容错把爆炸半径限制在这一个字段内。
fn de_initial_mode<'de, D>(d: D) -> Result<Option<InitialMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(d)?;
    Ok(raw.as_deref().and_then(InitialMode::from_config))
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
    /// 进入本应用时的初始中英状态；`None` = 不干预，沿用全局逻辑。
    ///
    /// **必须是 `Option` 不能是 `bool`**：`#[serde(default)]` 下的 bool 会让所有未配置
    /// 规则的应用都拿到 `false`，等于给全世界配了「初始英文」。
    #[serde(
        default,
        deserialize_with = "de_initial_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub initial_mode: Option<InitialMode>,
    /// 进入本应用时的初始中英标点；`None` = 不干预。
    ///
    /// 显式值**压过** `input.punct.follow_mode` 的推导，否则用户配了它却恰好开着
    /// follow_mode 时会完全无效且无痕迹。
    #[serde(
        default,
        deserialize_with = "de_initial_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub initial_punct: Option<InitialMode>,
    /// 该进程加入 HostRender 白名单（受限宿主如 Win11 开始菜单 SearchHost.exe，候选窗由
    /// 服务进程渲染后经共享内存转交宿主进程内的 DLL 上屏，绕开普通窗口盖不过的 Band 层级）。
    ///
    /// 原为独立的 `config.toml` 全局列表 `compat.host_render_processes`，现并入按进程名
    /// 匹配的兼容规则表——与 `caret_use_top` 等字段同一套查找路径，不再是第二个真相源。
    /// 消费点须按**事件源 PID 直查** `AppCompat::host_render_processes()` 现算的白名单
    /// （`HostRenderManager::is_process_whitelisted`），不得经 `ActiveCompat` 全局焦点槽缓存
    /// ——开始菜单弹出会连带激活兄弟进程，焦点槽会被污染，详见
    /// `docs/redesign/host-render-windows-port.md` §11.2。
    #[serde(default, skip_serializing_if = "is_false")]
    pub host_render: bool,
}

/// 在一组规则上就地修改指定进程的**某一个**字段。
///
/// 纯函数（不碰文件系统），故可直接单测——本仓凡涉 `%APPDATA%` 落盘的逻辑都要这样
/// 抽出来，否则端到端测试会真写用户配置目录（见 project_dict_override_sparse_merge 的教训）。
///
/// 进程名不区分大小写匹配；命中则只改 `edit` 触碰的字段、其余保持不动；未命中则**追加**
/// 一条只带该字段的新规则（不是整表快照，避免把系统层的其它字段冻结进用户层）。
pub fn upsert_rule(
    rules: &mut Vec<AppCompatRule>,
    process: &str,
    edit: impl FnOnce(&mut AppCompatRule),
) {
    let key = process.to_ascii_lowercase();
    for r in rules.iter_mut() {
        if r.process.to_ascii_lowercase() == key {
            edit(r);
            return;
        }
    }
    let mut fresh = AppCompatRule {
        process: process.to_string(),
        ..Default::default()
    };
    edit(&mut fresh);
    rules.push(fresh);
}

/// 在一组规则上设置指定进程的首显策略。语义见 [`upsert_rule`]。
pub fn set_first_show_mode(rules: &mut Vec<AppCompatRule>, process: &str, mode: FirstShowMode) {
    upsert_rule(rules, process, |r| r.first_show_mode = mode);
}

/// 在一组规则上设置指定进程的初始中英状态（`None` = 清除规则，回到跟随全局）。
pub fn set_initial_mode(rules: &mut Vec<AppCompatRule>, process: &str, mode: Option<InitialMode>) {
    upsert_rule(rules, process, |r| r.initial_mode = mode);
}

/// 在一组规则上设置指定进程的初始中英标点（`None` = 清除规则，回到跟随全局）。
pub fn set_initial_punct(rules: &mut Vec<AppCompatRule>, process: &str, mode: Option<InitialMode>) {
    upsert_rule(rules, process, |r| r.initial_punct = mode);
}

/// 在一组规则上设置指定进程是否加入 HostRender 白名单。语义见 [`upsert_rule`]。
pub fn set_host_render(rules: &mut Vec<AppCompatRule>, process: &str, enabled: bool) {
    upsert_rule(rules, process, |r| r.host_render = enabled);
}

/// 把规则集渲染成用户层 compat.toml 全文（含固定文件头）。纯函数，便于单测断言产物。
pub fn render_user_compat(rules: &[AppCompatRule]) -> Result<String, toml::ser::Error> {
    let file = AppCompatFile {
        apps: rules.to_vec(),
    };
    Ok(format!("{USER_COMPAT_HEADER}{}", toml::to_string(&file)?))
}

/// 就地修改用户层 compat.toml 中指定进程的规则（load-modify-save）。
///
/// 只读写**用户层**：系统层 `data/compat.toml` 不受影响（合并时用户层同名进程整条覆盖它）。
/// 文件或目录不存在时自动创建；解析失败按空规则集处理（宁可重建也不要把菜单卡死，
/// 用户手改坏了 TOML 时仍能通过菜单恢复到可用状态）。
pub fn update_user_rule(
    user_dir: &Path,
    process: &str,
    edit: impl FnOnce(&mut AppCompatRule),
) -> Result<(), std::io::Error> {
    let path = user_dir.join(COMPAT_FILE_NAME);
    let mut rules = load_file(&path).unwrap_or_default();
    upsert_rule(&mut rules, process, edit);
    let text = render_user_compat(&rules)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::create_dir_all(user_dir)?;
    std::fs::write(&path, text)?;
    Ok(())
}

/// 设置用户层 compat.toml 中指定进程的首显策略。语义见 [`update_user_rule`]。
pub fn set_user_first_show_mode(
    user_dir: &Path,
    process: &str,
    mode: FirstShowMode,
) -> Result<(), std::io::Error> {
    update_user_rule(user_dir, process, |r| r.first_show_mode = mode)
}

/// 设置用户层 compat.toml 中指定进程的初始中英状态（`None` = 清除规则）。
pub fn set_user_initial_mode(
    user_dir: &Path,
    process: &str,
    mode: Option<InitialMode>,
) -> Result<(), std::io::Error> {
    update_user_rule(user_dir, process, |r| r.initial_mode = mode)
}

/// 设置用户层 compat.toml 中指定进程的初始中英标点（`None` = 清除规则）。
pub fn set_user_initial_punct(
    user_dir: &Path,
    process: &str,
    mode: Option<InitialMode>,
) -> Result<(), std::io::Error> {
    update_user_rule(user_dir, process, |r| r.initial_punct = mode)
}

/// 设置用户层 compat.toml 中指定进程是否加入 HostRender 白名单。
pub fn set_user_host_render(
    user_dir: &Path,
    process: &str,
    enabled: bool,
) -> Result<(), std::io::Error> {
    update_user_rule(user_dir, process, |r| r.host_render = enabled)
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

    /// 现算 HostRender 白名单：所有 `host_render = true` 的进程名（原始大小写）。
    ///
    /// 供 `HostRenderManager::set_whitelist` 消费；调用方须按事件源 PID 直查，
    /// 不得经 `ActiveCompat` 全局焦点槽缓存，理由见 [`AppCompatRule::host_render`]。
    pub fn host_render_processes(&self) -> Vec<String> {
        self.apps
            .iter()
            .filter(|r| r.host_render)
            .map(|r| r.process.clone())
            .collect()
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
        // 缺字段 = 取默认档。与 default() 比而非硬编码某一档：本条测的是「serde 有没有
        // 走 #[serde(default)]」，不是「默认档是哪一个」——后者由 default_mode_is_fast 钉。
        assert_eq!(rule.first_show_mode, FirstShowMode::default());
        // 缺字段 = 不干预。若这两个退化成 Some(English)，等于给所有未配置的应用
        // 都配上了「初始英文」——这正是字段必须用 Option 而非 bool 的原因。
        assert_eq!(rule.initial_mode, None);
        assert_eq!(rule.initial_punct, None);
    }

    #[test]
    fn initial_mode_parses_both_values() {
        let toml = r#"
            [[apps]]
            process = "Everything.exe"
            initial_mode = "english"
            initial_punct = "chinese"
        "#;
        let file: AppCompatFile = toml::from_str(toml).unwrap();
        let compat = AppCompat::from_rules(file.apps);
        let rule = compat.get_rule("everything.exe").unwrap();
        assert_eq!(rule.initial_mode, Some(InitialMode::English));
        assert_eq!(rule.initial_punct, Some(InitialMode::Chinese));
        assert!(!rule.initial_mode.unwrap().is_chinese());
        assert!(rule.initial_punct.unwrap().is_chinese());
    }

    /// 单字段拼错只让**该字段**退化为「不干预」，不得连累同规则的其它字段、
    /// 也不得让整份 compat.toml 解析失败（`load_file` 失败会静默跳过整个文件，
    /// 于是一个错别字就让所有应用的所有规则一起失效且毫无痕迹）。
    #[test]
    fn unknown_initial_mode_degrades_to_none_without_killing_the_file() {
        let toml = r#"
            [[apps]]
            process = "Everything.exe"
            initial_mode = "englsh"
            caret_use_top = true

            [[apps]]
            process = "Weixin.exe"
            initial_mode = "chinese"
        "#;
        let file: AppCompatFile = toml::from_str(toml).expect("拼错的值不得让整份文件解析失败");
        let compat = AppCompat::from_rules(file.apps);
        let bad = compat.get_rule("everything.exe").unwrap();
        assert_eq!(bad.initial_mode, None, "无法识别 → 不干预");
        assert!(bad.caret_use_top, "同规则的其它字段不受牵连");
        // 后续规则完整存活。
        assert_eq!(
            compat.get_rule("weixin.exe").unwrap().initial_mode,
            Some(InitialMode::Chinese)
        );
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
    fn mode_parses_from_config_and_falls_back_to_default() {
        assert_eq!(FirstShowMode::from_config("fast"), FirstShowMode::Fast);
        assert_eq!(
            FirstShowMode::from_config(" INSTANT "),
            FirstShowMode::Instant
        );
        assert_eq!(FirstShowMode::from_config("wait"), FirstShowMode::Wait);
        // 未知值回落**默认档**：写错了和没写得到同样的行为，最不意外。
        // ⚠ 断言写成与 `default()` 比而非硬编码某一档——回落值与 `#[default]` 是两处
        // 独立事实，各自硬编码就是漏改的温床（且漏改毫无编译信号，生产走 serde、本函数
        // 只有测试在调）。这样写等于把「两处必须一致」本身钉成不变量。
        assert_eq!(
            FirstShowMode::from_config("turbo"),
            FirstShowMode::default()
        );
        assert_eq!(FirstShowMode::Fast.as_config(), "fast");
    }

    /// 默认档位是产品决策，单独钉一条，改动时必须显式过这一关。
    ///
    /// 2026-08-03 由 `wait` 改为 `fast`：`fast` 此前不敢作默认，是因为焦点切换/鼠标移动
    /// 光标后的首帧会拿一份属于别处的旧坐标定位；首帧信任门补上该洞后，它在坐标不可信时
    /// 会自动退回去等真值。实测常规连打首帧中位 7ms，焦点后首帧中位 105ms 且位置正确。
    #[test]
    fn default_mode_is_fast() {
        assert_eq!(FirstShowMode::default(), FirstShowMode::Fast);
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

    #[test]
    fn set_initial_mode_upserts_and_clears() {
        let mut rules = vec![AppCompatRule {
            process: "Everything.exe".into(),
            comment: "搜索框默认英文".into(),
            caret_use_top: true,
            ..Default::default()
        }];
        // 命中：只改 initial_mode，其它字段不得被连带修改。
        set_initial_mode(&mut rules, "everything.exe", Some(InitialMode::English));
        assert_eq!(rules.len(), 1, "命中时不得追加新规则");
        assert_eq!(rules[0].initial_mode, Some(InitialMode::English));
        assert!(rules[0].caret_use_top, "其它字段不得被连带修改");
        assert_eq!(rules[0].comment, "搜索框默认英文");

        // 标点是独立维度，设置它不得动中英。
        set_initial_punct(&mut rules, "Everything.EXE", Some(InitialMode::English));
        assert_eq!(rules[0].initial_punct, Some(InitialMode::English));
        assert_eq!(rules[0].initial_mode, Some(InitialMode::English));

        // None = 清除规则，回到跟随全局（菜单的「跟随全局」档走这条）。
        set_initial_mode(&mut rules, "Everything.exe", None);
        assert_eq!(rules[0].initial_mode, None);
        assert_eq!(
            rules[0].initial_punct,
            Some(InitialMode::English),
            "只清中英"
        );

        // 未命中：追加只带该字段的最小规则。
        set_initial_mode(&mut rules, "cmd.exe", Some(InitialMode::English));
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1].process, "cmd.exe", "应保留原始大小写");
        assert_eq!(rules[1].initial_mode, Some(InitialMode::English));
        assert!(!rules[1].caret_use_top);
    }

    #[test]
    fn render_omits_none_initial_mode_and_roundtrips() {
        let rules = vec![AppCompatRule {
            process: "Everything.exe".into(),
            initial_mode: Some(InitialMode::English),
            ..Default::default()
        }];
        let text = render_user_compat(&rules).expect("渲染失败");
        assert!(text.contains(r#"initial_mode = "english""#), "产物: {text}");
        assert!(!text.contains("initial_punct"), "None 字段不应写出: {text}");

        let parsed: AppCompatFile = toml::from_str(&text).expect("产物应可解析");
        assert_eq!(parsed.apps[0].initial_mode, Some(InitialMode::English));
        assert_eq!(parsed.apps[0].initial_punct, None);
    }

    #[test]
    fn host_render_defaults_false_and_omitted_from_render() {
        let toml = r#"
            [[apps]]
            process = "Foo.exe"
        "#;
        let file: AppCompatFile = toml::from_str(toml).unwrap();
        let compat = AppCompat::from_rules(file.apps);
        assert!(!compat.get_rule("foo.exe").unwrap().host_render);

        let rules = vec![AppCompatRule {
            process: "Foo.exe".into(),
            ..Default::default()
        }];
        let text = render_user_compat(&rules).expect("渲染失败");
        assert!(!text.contains("host_render"), "false 开关不应写出: {text}");
    }

    #[test]
    fn set_host_render_upserts_and_host_render_processes_collects_only_enabled() {
        let mut rules = vec![
            AppCompatRule {
                process: "Weixin.exe".into(),
                caret_use_top: true,
                ..Default::default()
            },
            AppCompatRule {
                process: "SearchHost.exe".into(),
                ..Default::default()
            },
        ];
        set_host_render(&mut rules, "searchhost.exe", true); // 大小写无关命中
        assert_eq!(rules.len(), 2, "命中时不得追加新规则");
        assert!(rules[1].host_render);
        assert!(rules[0].caret_use_top, "其它规则不受牵连");

        let compat = AppCompat::from_rules(rules);
        assert_eq!(
            compat.host_render_processes(),
            vec!["SearchHost.exe".to_string()],
            "只收集 host_render=true 的进程，且保留原始大小写"
        );
    }

    #[test]
    fn render_omits_false_host_render_but_keeps_true() {
        let rules = vec![
            AppCompatRule {
                process: "A.exe".into(),
                host_render: true,
                ..Default::default()
            },
            AppCompatRule {
                process: "B.exe".into(),
                host_render: false,
                ..Default::default()
            },
        ];
        let text = render_user_compat(&rules).expect("渲染失败");
        assert!(text.contains("host_render = true"), "产物: {text}");

        let parsed: AppCompatFile = toml::from_str(&text).expect("产物应可解析");
        let compat = AppCompat::from_rules(parsed.apps);
        assert_eq!(compat.host_render_processes(), vec!["A.exe".to_string()]);
    }
}
