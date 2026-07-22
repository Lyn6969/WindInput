//! 命令栏服务依赖
//!
//! 对照 Go `wind_input/internal/cmdbar/services.go`。动作函数所需的宿主副作用能力，
//! 全部以 trait 注入；任一字段可为 `None`，动作函数在缺失时返回
//! [`CmdbarError::ServiceUnavailable`](crate::error::CmdbarError)，供宿主降级。

use std::sync::Arc;

/// 剪贴板服务：`clip.copy` / `clip.paste`。
pub trait ClipboardService: Send + Sync {
    fn set_text(&self, text: &str) -> anyhow::Result<()>;
    fn get_text(&self) -> anyhow::Result<String>;
    /// 把剪贴板内容送入当前输入框（Windows 合成 Ctrl+V）。
    fn paste(&self) -> anyhow::Result<()>;
}

/// 按键模拟：`key.tap` / `key.seq` / `key.hold` / `key.release` / `key.type`。
pub trait KeyInjector: Send + Sync {
    fn tap(&self, combo: &str) -> anyhow::Result<()>;
    fn sequence(&self, combos: &[String]) -> anyhow::Result<()>;
    fn hold(&self, combo: &str) -> anyhow::Result<()>;
    fn release(&self, combo: &str) -> anyhow::Result<()>;
    fn type_text(&self, text: &str) -> anyhow::Result<()>;
}

/// 打开 URL / 程序 / 文件：`open`（及默认的 `web.search`）。
pub trait UrlOpener: Send + Sync {
    fn open(&self, target: &str) -> anyhow::Result<()>;
}

/// 进程启动 / shell 执行：`proc.run` / `proc.shell` / `wind.cli`。
pub trait ProcessRunner: Send + Sync {
    fn run(&self, cmd: &str, args: &[String]) -> anyhow::Result<()>;
    fn shell(&self, cmdline: &str) -> anyhow::Result<()>;
    /// `proc.shell(cmd, "flagA,flagB")` 的扩展形式；不支持时可退化为 [`Self::shell`]。
    fn shell_ex(&self, cmdline: &str, flags: &[String]) -> anyhow::Result<()>;
    /// 以主程序自身 exe 执行 CLI 子命令（`wind.cli`）：宿主自取 exe 路径，
    /// 词条无需硬编码安装位置。默认未支持（测试/精简宿主）。
    fn run_self(&self, _args: &[String]) -> anyhow::Result<()> {
        anyhow::bail!("run_self: 宿主未支持")
    }
}

/// 词库：`dict.add`。`code` 为空时由实现按当前方案规则推导。
pub trait DictService: Send + Sync {
    fn add_word(&self, text: &str, code: &str) -> anyhow::Result<()>;
}

/// IME 状态控制：`ime.toggle` / `ime.schema` / `ime.theme_cycle` / `setting.open` / `setting.web`。
pub trait ImeController: Send + Sync {
    /// 切换 IME 状态（cn-en / fullshape / layout / candwin / s2t / preedit / toolbar）。
    fn toggle(&self, target: &str) -> anyhow::Result<()>;
    fn open_setting(&self, page: &str) -> anyhow::Result<()>;
    /// 以 --web 参数启动设置 Web 版。
    fn open_setting_web(&self, page: &str) -> anyhow::Result<()>;
    /// 切换输入方案（持久化）。
    fn set_schema(&self, id: &str) -> anyhow::Result<()>;
    /// 循环切换主题；dir="next"/"" 向后，"prev" 向前，返回新主题 ID。
    fn theme_cycle(&self, dir: &str) -> anyhow::Result<String>;
    /// 撤销最近一次上屏（`ime.undo_commit`）：按上屏历史删除对应字符数，
    /// 无历史时删 1 个。默认未支持（测试/精简宿主）。
    fn undo_commit(&self) -> anyhow::Result<()> {
        anyhow::bail!("undo_commit: 宿主未支持")
    }
}

/// 配置读写：`config.get` / `config.set` / `config.toggle`，key 为 YAML 路径。
pub trait ConfigService: Send + Sync {
    fn get(&self, key: &str) -> anyhow::Result<String>;
    fn set(&self, key: &str, value: &str) -> anyhow::Result<()>;
    /// 循环切换枚举或翻转 bool，返回新值。
    fn toggle(&self, key: &str) -> anyhow::Result<String>;
}

/// 可选搜索引擎定制：默认实现合成 URL 转发给 [`UrlOpener`]，仅在宿主需要不同语义时覆盖。
pub trait SearchEngine: Send + Sync {
    fn search(&self, engine: &str, query: &str) -> anyhow::Result<()>;
}

/// 注入到 [`EvalContext`](crate::context::EvalContext) 的副作用依赖束。每个字段可为 `None`。
#[derive(Default, Clone)]
pub struct Services {
    pub clip: Option<Arc<dyn ClipboardService>>,
    pub keys: Option<Arc<dyn KeyInjector>>,
    pub open: Option<Arc<dyn UrlOpener>>,
    pub proc: Option<Arc<dyn ProcessRunner>>,
    pub dict: Option<Arc<dyn DictService>>,
    pub ime: Option<Arc<dyn ImeController>>,
    pub config: Option<Arc<dyn ConfigService>>,
    pub search: Option<Arc<dyn SearchEngine>>,
}

impl Services {
    pub fn new() -> Self {
        Self::default()
    }
}
