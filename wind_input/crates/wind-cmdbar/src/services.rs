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

/// 一次 `proc.run` 启动请求。
///
/// **刻意用结构体而不是继续加形参**：这些选项都来自 `proc.run` 的具名参数，会随
/// 需求增长。加字段时每个实现点都会编译失败、被迫面对新选项；而多加一个形参
/// 很容易被某个实现原样忽略掉，表现为「参数写了不生效」且毫无痕迹。
///
/// 空串一律表示"未指定，用默认"。各字段的取值已在 cmdbar 层校验过白名单，
/// 宿主收到的一定是合法值（或空串）。
#[derive(Debug, Clone, Copy)]
pub struct ProcSpawn<'a> {
    /// 目标程序 / 文件 / URL。
    pub cmd: &'a str,
    /// 命令行参数，引号处理由宿主负责。
    pub args: &'a [String],
    /// 工作目录；空串 = 宿主按默认策略决定（**不是**"继承调用方当前目录"）。
    pub cwd: &'a str,
    /// ShellExecute 动词：`open`(默认) / `runas` / `edit` / `print` / `explore` / `properties`。
    /// 仅 Windows 有效，其它平台由宿主记 WARN 并忽略。
    pub verb: &'a str,
    /// 初始窗口状态：`normal`(默认) / `min` / `max` / `hidden`。
    /// 仅 Windows 有效，其它平台由宿主记 WARN 并忽略。
    pub show: &'a str,
}

impl<'a> ProcSpawn<'a> {
    /// 只给目标与参数的最简形式（其余走默认），供测试与内部调用。
    pub fn new(cmd: &'a str, args: &'a [String]) -> Self {
        ProcSpawn {
            cmd,
            args,
            cwd: "",
            verb: "",
            show: "",
        }
    }
}

/// 进程启动 / shell 执行：`proc.run` / `proc.shell` / `wind.cli`。
///
/// `cwd` 不是可选形参：「忘了接工作目录」的宿主会静默把 CWD 继承给子进程
/// （在 Windows 上就是前台应用的当前目录，且会随文件对话框漂移），没有任何
/// 报错——这类半接线只能靠签名本身杜绝。
pub trait ProcessRunner: Send + Sync {
    fn run(&self, spec: &ProcSpawn<'_>) -> anyhow::Result<()>;
    /// `flags` 为 `proc.shell(cmd, "flagA,flagB")` 拆出的标志集（可空）。
    /// 无 verb/show：命令行交给 shell 执行，那两个是 ShellExecute 的概念。
    fn shell(&self, cmdline: &str, flags: &[String], cwd: &str) -> anyhow::Result<()>;
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
    /// 打开设置窗口的指定页面。`page` 为规范页 id（schema/input/keys/ui/dict/
    /// advanced/about），空串打开默认页。未知 id 由设置端忽略并落到默认页。
    ///
    /// `args` 是**原样直通**给设置程序的附加命令行参数（如
    /// `--schema=wubi86 --type=shadow` 定位到五笔的候选调整），空串=无附加参数。
    /// 宿主刻意不解析、不校验其内容：设置端每加一个新参数都要改一遍这里，
    /// 才是真正难维护的地方。含空白的值请自行用引号包裹。
    fn open_setting(&self, page: &str, args: &str) -> anyhow::Result<()>;
    /// 以 --web 参数启动设置 Web 版。`args` 语义同 [`Self::open_setting`]。
    fn open_setting_web(&self, page: &str, args: &str) -> anyhow::Result<()>;
    /// 切换输入方案（持久化）。
    fn set_schema(&self, id: &str) -> anyhow::Result<()>;
    /// 循环切换主题；dir="next"/"" 向后，"prev" 向前，返回新主题 ID。
    fn theme_cycle(&self, dir: &str) -> anyhow::Result<String>;
    /// 撤销最近一次上屏（`ime.undo_commit`）：按上屏历史删除对应字符数，
    /// 无历史时删 1 个。默认未支持（测试/精简宿主）。
    fn undo_commit(&self) -> anyhow::Result<()> {
        anyhow::bail!("undo_commit: 宿主未支持")
    }
    /// 上屏配对文本并激活配对状态（`ime.pair`）：插入 `left + right`、光标落在两段之间，
    /// 同时把这一层压入配对栈，使跳出键（Tab/Enter）能越过 `right`。
    ///
    /// `jump_steps` = 跳出时光标右移的格数。
    ///
    /// 与自动配对的分工：自动配对由标点按键触发、右段恒为单字符；本方法由词条显式调用，
    /// 右段可以是任意文本。**受 `input.auto_pair` 总开关约束**——关闭时由宿主退化为纯上屏
    /// （整串上屏、光标落末尾、不压栈），判定在宿主侧，本层不做。
    fn pair(&self, left: &str, right: &str, jump_steps: u32) -> anyhow::Result<()> {
        let _ = (left, right, jump_steps);
        anyhow::bail!("pair: 宿主未支持")
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
