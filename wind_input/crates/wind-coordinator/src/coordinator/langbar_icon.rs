//! 语言栏图标发布（Windows 桌面形态）：共享内存单例 + 状态角标。
//!（coordinator 子模块，自 coordinator.rs 平移，纯搬运。）

use super::*;

/// 语言栏图标发布器（Windows 桌面形态）。
///
/// 做成进程级单例而非 [`Coordinator`] 字段，理由是它对应的资源本身就是进程级唯一的：
/// 共享内存名固定（`Local\WindInput_IconShm{_dev}`），一个进程开两份没有意义。
/// 附带好处是不必改动全部构造器。
///
/// 内层 `Option` 为 `None` = 创建失败。这不是致命错误——DLL 侧读不到 SHM 会退回
/// 本地 DirectWrite 绘制，图标照常显示，只是不跟随标点状态。
#[cfg(all(feature = "desktop-ui", windows))]
static ICON_PUBLISHER: std::sync::OnceLock<
    std::sync::Mutex<Option<wind_ui::langbar_icon::LangBarIconPublisher>>,
> = std::sync::OnceLock::new();

impl Coordinator {
    /// 服务启动后发布一次初始图标。
    ///
    /// [`Self::push_state_update`] 只在状态**变化**时调用。少了这一次补发，开机后到
    /// 用户第一次切换中英或标点之前，共享内存始终是空的，DLL 只能走本地绘制——
    /// 图标显示正常但没有角标，看起来像「功能根本没做」而不是「还没初始化」。
    ///
    /// 非 Windows 桌面形态下是空操作，故调用方无需自己加 cfg。
    pub fn publish_initial_langbar_icon(&self) {
        self.publish_langbar_icon_now();
    }

    /// 按当前状态立即重渲并发布图标。调试菜单改了呈现参数后靠它落地。
    ///
    /// ⚠ **只写共享内存，不通知 DLL。** `GetIcon` 是被动回调，DLL 要收到状态推送
    /// （`OnUpdate(TF_LBI_ICON)`）或焦点切换（`ForceRefresh`）才会重取。呈现参数变化
    /// 不构成状态变化，故调试菜单改完要等下一次焦点切换/模式切换才看得到——
    /// 关掉菜单时焦点回到宿主，通常正好触发一次。
    pub fn publish_langbar_icon_now(&self) {
        #[cfg(all(feature = "desktop-ui", windows))]
        self.publish_langbar_icon(&self.build_status());
    }

    /// 图标发布器单例。首次访问时创建；创建失败缓存为 `None`（DLL 会退回本地绘制）。
    ///
    /// 抽出来是因为调试菜单要在**发布之外**访问它（读当前形状画勾选、改形状），
    /// 而 `get_or_init` 的初始化逻辑只该有一份。
    #[cfg(all(feature = "desktop-ui", windows))]
    pub(crate) fn icon_publisher()
    -> &'static std::sync::Mutex<Option<wind_ui::langbar_icon::LangBarIconPublisher>> {
        use wind_ui::langbar_icon::{BadgeShape, LangBarIconPublisher};
        ICON_PUBLISHER.get_or_init(|| {
            let suffix = wind_config::variant::pipe_suffix();
            match LangBarIconPublisher::new(suffix, BadgeShape::default()) {
                Ok(mut p) => {
                    tracing::info!(shm = p.shm_name(), "语言栏图标共享内存已就绪");
                    // 恢复上次选定的呈现参数。`None`（从未设过）一律不动，保留构造函数
                    // 给的代码默认——state.toml 侧刻意不重复声明默认值，见其字段注释。
                    if let Some(dir) = Config::state_dir() {
                        let rs = wind_config::RuntimeState::load(&dir);
                        if let Some(id) = rs.langbar_icon_shape.as_deref() {
                            p.set_shape(BadgeShape::from_id(id));
                        }
                        if let Some(on) = rs.langbar_icon_colored {
                            p.set_colored(on);
                        }
                        if let Some(on) = rs.langbar_icon_size_marks {
                            p.set_size_marks(on);
                        }
                    }
                    std::sync::Mutex::new(Some(p))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "语言栏图标共享内存创建失败，DLL 将退回本地绘制");
                    std::sync::Mutex::new(None)
                }
            }
        })
    }

    /// 对发布器做一次改动，随后落盘并立即重发。发布器不可用时是空操作。
    ///
    /// 收成一个函数而不是每个调试项各写一遍「取锁 → 改 → 落盘 → 发布」：漏掉重发那步
    /// 的症状是「点了菜单毫无变化」、漏掉落盘那步是「重启就忘」，而调试菜单存在的意义
    /// 恰恰是反复比选——两种症状都直接毁掉它。
    ///
    /// 三项一起写回而不是各写各的：读回时也是三项一起读，写入侧按项分散的话，
    /// 某一项忘了落盘会表现为「另外两项记住了、这项没有」，比整体失效更难注意到。
    #[cfg(all(feature = "desktop-ui", windows))]
    pub(crate) fn tweak_langbar_icon(
        &self,
        f: impl FnOnce(&mut wind_ui::langbar_icon::LangBarIconPublisher),
    ) {
        if let Ok(mut guard) = Self::icon_publisher().lock()
            && let Some(p) = guard.as_mut()
        {
            f(p);
            // load-modify-save，与 toolbar_positions / record_last_state 同一模式：
            // state.toml 是多方共用的文件，整体覆盖会抹掉别人的字段。
            if let Some(dir) = Config::state_dir() {
                let mut rs = wind_config::RuntimeState::load(&dir);
                rs.langbar_icon_shape = Some(p.shape().as_id().to_string());
                rs.langbar_icon_colored = Some(p.colored());
                rs.langbar_icon_size_marks = Some(p.size_marks());
                if let Err(e) = rs.save(&dir) {
                    tracing::warn!(error = %e, "语言栏图标偏好落盘失败");
                }
            }
        }
        // 锁已在上面的块尾释放——发布内部还要再取一次同一把锁，留在块内会自锁。
        self.publish_langbar_icon_now();
    }

    /// 取当前状态，并在**返回之前**把对应的图标位图投进共享内存。
    ///
    /// 存在的唯一理由是强制两件事的先后：DLL 收到状态推送后会 `OnUpdate(TF_LBI_ICON)`，
    /// 系统随即回调 `GetIcon` 去读 SHM——那时新位图必须已经在里面。反过来（先推送、后发布）
    /// 是一个跨进程竞态：发布要重渲全部尺寸档 × 明暗两档，是毫秒级工作，而「推送 → DLL 读线程
    /// → PostMessage → OnUpdate → GetIcon」同样是毫秒级，谁先到取决于调度，表现为
    /// **切换偶尔不生效**（图标停在上一个状态，下次切换才追上）。
    ///
    /// 把发布藏进「取状态」这一步，是为了让调用方**拿不到**一个尚未发布的 status——
    /// 顺序由数据依赖保证，而不是靠每个推送函数各自记得先调一次发布。此前
    /// `push_state_update` 里的注释就写对了这条要求、代码却是反的，正是这个原因。
    ///
    /// 非 Windows 桌面形态下退化为纯粹的 [`Self::build_status`]，故调用方无需自己加 cfg。
    pub(crate) fn status_with_icon_published(&self) -> StatusUpdateData {
        let s = self.build_status();
        #[cfg(all(feature = "desktop-ui", windows))]
        self.publish_langbar_icon(&s);
        s
    }

    /// 把当前状态渲染成语言栏图标并投送共享内存。
    ///
    /// ⚠ **不要直接调用**：状态推送路径一律走 [`Self::status_with_icon_published`]，
    /// 那里保证了发布先于推送。直接调用的只有初始补发与调试菜单——它们不伴随状态推送。
    ///
    /// 失败一律只记日志：DLL 侧在读不到 SHM 时会退回本地 DirectWrite 绘制，
    /// 图标不会消失，只是不跟随标点状态——不值得为此中断状态推送。
    #[cfg(all(feature = "desktop-ui", windows))]
    pub(super) fn publish_langbar_icon(&self, s: &StatusUpdateData) {
        use wind_ui::langbar_icon::{IconSpec, PunctBadge};

        let cell = Self::icon_publisher();

        // 与工具栏同口径：CapsLock 开启时中文模式实际在打英文（见 build_status 的
        // effective_chinese），此时不该显示中文标点角标。
        let effective_chinese = s.chinese_mode && !s.caps_lock;
        let spec = IconSpec {
            label: s.icon_label.clone(),
            // 英文模式下标点恒为半角且不可切换（`toolbar.rs` 的渲染同样这么处理），
            // 角标此时没有信息量，故不画。
            punct: if !effective_chinese {
                PunctBadge::None
            } else if s.chinese_punct {
                PunctBadge::Chinese
            } else {
                PunctBadge::English
            },
            // 密码框 / 无编辑上下文 / 键盘禁用都是 **DLL 本地判定**的状态，服务端无从得知；
            // 那几种情况下 DLL 根本不读 SHM，直接本地绘制。故这里恒为 false。
            dimmed: false,
            // 状态驱动的发布恒用相位 0；演示动画由它自己的定时器推进相位。
            frame: 0,
        };

        if let Ok(mut guard) = cell.lock()
            && let Some(p) = guard.as_mut()
        {
            match p.publish(&spec) {
                // 记序号是排查「图标落后一帧」的唯一抓手：本行的时刻与 DLL 日志里
                // `GetIcon: from SHM` 的时刻一对，就能判断那次 GetIcon 取到的是第几版。
                // label / punct 都是模式状态（「中」「拼」这类短称），不含输入内容。
                Ok(Some(seq)) => tracing::debug!(
                    seq,
                    label = %spec.label,
                    punct = ?spec.punct,
                    "语言栏图标已发布"
                ),
                // 状态未变，SHM 里已经是这张图，跳过重渲。不记日志：状态推送远比状态
                // 变化频繁，每次都记会把这条日志变成噪声。
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "发布语言栏图标失败"),
            }
        }
    }
}
