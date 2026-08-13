//! 宿主服务注入点：协调器在逻辑中途需要**同步 pull** 的平台能力。
//!
//! 接口由消费者（协调器）定义，实现按运行形态注入：桌面默认 [`DesktopHostServices`]
//! 直通 `wind_ui::popup_menu`；headless / Android 在注入前落 trait 默认实现
//! （set/get 报错、cached 空串），Android FFI 后续由 Kotlin `ClipboardManager` 实现。
//!
//! 收录判据（三条全中才进 trait，缺一维持 cfg 兜底）：
//! ① 协调器在逻辑中途同步 pull；② cfg 兜底值在目标平台是语义错误而非可接受默认；
//! ③ 没有既存的 push 式注入通路（反例：宿主进程名走焦点事件喂 `pid_names`，不进这里）。
//! 方法一律带默认实现，未来追加对既有实现非破坏。

/// 平台能力的同步调用面。`Send + Sync`：按键线程与 cmdbar 异步执行线程都会调用。
pub trait HostServices: Send + Sync {
    /// 写系统剪贴板。失败要能传播（cmdbar `clip.copy` 经 run_actions 记 warn 弹错）。
    fn clipboard_set_text(&self, _text: &str) -> anyhow::Result<()> {
        anyhow::bail!("clip.copy: 宿主服务未注入")
    }

    /// 读系统剪贴板（阻塞版，允许重试）。仅在**执行动作**时使用；
    /// 按键线程的候选构建期禁止调用（见 [`Self::clipboard_get_text_cached`]）。
    fn clipboard_get_text(&self) -> anyhow::Result<String> {
        anyhow::bail!("clip.get: 宿主服务未注入")
    }

    /// 读系统剪贴板（缓存版，**绝不阻塞**，失败返回空串）。
    ///
    /// 与 [`Self::clipboard_get_text`] 的区分是行为契约而非实现细节：本方法在
    /// 每次按键的候选构建期被调用，只用于拼显示标签——阻塞版打不开剪贴板时会
    /// sleep 重试至 40ms，等于把最坏 40ms 摊到按键线程上。实现方必须保持
    /// 「宁陈旧/宁空，勿等待」。
    fn clipboard_get_text_cached(&self) -> String {
        String::new()
    }
}

/// 桌面实现：直通 `wind_ui::popup_menu` 的三平台剪贴板
/// （Windows CF_UNICODETEXT + 序列号缓存 / macOS pbcopy·pbpaste + Pasteboard 缓存）。
#[cfg(feature = "desktop-ui")]
pub struct DesktopHostServices;

/// headless 默认实现：全部落 trait 默认（set/get 报错、cached 空串）。
/// Android FFI 在首次使用前 `set_host_services` 注入 Kotlin 实现替代它。
#[cfg(not(feature = "desktop-ui"))]
pub struct NullHostServices;

#[cfg(not(feature = "desktop-ui"))]
impl HostServices for NullHostServices {}

#[cfg(feature = "desktop-ui")]
impl HostServices for DesktopHostServices {
    fn clipboard_set_text(&self, text: &str) -> anyhow::Result<()> {
        // try 版传播失败（OpenClipboard 被占用重试后仍失败等）；
        // 非 Windows/macOS 平台它自身即报「当前平台暂未支持」。
        wind_ui::popup_menu::try_set_clipboard_text(text)
    }

    fn clipboard_get_text(&self) -> anyhow::Result<String> {
        // cfg 分支保真自 SysClip::get_text：mock 平台的「空串」与「报错」是两种语义
        // ——读取失败必须让 clip.get 动作报错，而不是静默拿到空串。
        #[cfg(any(windows, target_os = "macos"))]
        {
            Ok(wind_ui::popup_menu::get_clipboard_text())
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            anyhow::bail!("clip.get: 当前平台暂未支持")
        }
    }

    fn clipboard_get_text_cached(&self) -> String {
        wind_ui::popup_menu::get_clipboard_text_cached()
    }
}
