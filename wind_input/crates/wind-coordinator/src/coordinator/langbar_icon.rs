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

/// 演示动画的代际。开/关各 +1，驱动线程每帧核对自己那一代是否仍是当前值，不是就退出。
///
/// 用代际而不是 `JoinHandle` + 停止标志：菜单可以被连点，两次开启之间那个线程还没退出，
/// 用标志位会让新旧两个线程都认为自己该跑（相位于是每帧被推进两次，动画快一倍）。
/// 代际让「谁是当前的驱动」有唯一答案，且无需持有句柄或等待线程结束。
#[cfg(all(feature = "desktop-ui", windows))]
static DEMO_ANIM_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 演示动画帧间隔。一圈 40 帧（`IconRenderer::DEMO_FRAMES_PER_CYCLE`），80ms/帧 ≈ 3.2 秒
/// 转一圈——足够看清转向，又不至于让每帧那套「重渲全部档位 + 跨进程推送 + 宿主重建图标」
/// 变成真实负担。它是 Dev 调试玩具，不为流畅度加码。
#[cfg(all(feature = "desktop-ui", windows))]
const DEMO_ANIM_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);

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

    /// 按当前状态立即重渲并发布图标，并让 DLL 重取一次。
    ///
    /// 这是「位图变了但状态没变」的专用入口——调试菜单改角标形状、演示动画推进相位都走它。
    /// 这类变化不构成状态变化，既有的状态推送不会发生，DLL 那边的 `UpdateFullStatus`
    /// 也会因 `needUpdate` 为假而不发 `OnUpdate`，所以必须自己补一条 [`CMD_REFRESH_ICON`]。
    ///
    /// 只在**确实写了新位图**时才推刷新：`publish` 内部对相同 spec 会跳过，此时 SHM 内容
    /// 没变，推了也只是让每个宿主白重绘一次。
    ///
    /// [`CMD_REFRESH_ICON`]: wind_ipc::protocol::CMD_REFRESH_ICON
    pub fn publish_langbar_icon_now(&self) {
        #[cfg(all(feature = "desktop-ui", windows))]
        if self.publish_langbar_icon(&self.build_status()) {
            self.push_refresh_icon();
        }
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

    /// 翻转演示动画（外圈跑马灯）开关，并起停驱动线程。
    ///
    /// **刻意不落盘**（对比形状 / 彩色 / 尺寸档三项）：它不是一个呈现偏好，而是一段持续
    /// 占用 CPU 与 IPC 的演示；服务重启后自己关掉才是对的默认，否则用户下次开机会看到一个
    /// 一直转圈的图标，还找不到它是什么。
    #[cfg(all(feature = "desktop-ui", windows))]
    pub(crate) fn toggle_icon_demo_animation(&self) {
        use std::sync::atomic::Ordering;

        let on = {
            let Ok(mut guard) = Self::icon_publisher().lock() else {
                return;
            };
            let Some(p) = guard.as_mut() else {
                return;
            };
            let next = !p.demo_animation();
            p.set_demo_animation(next);
            next
        };

        // 先改代际再发布：这一步让**上一个**驱动线程（若还在）作废，随后的发布才不会
        // 与它抢相位。关闭时这一发同时负责把跑马灯从图标上抹掉。
        let generation = DEMO_ANIM_GEN.fetch_add(1, Ordering::AcqRel) + 1;
        self.publish_langbar_icon_now();

        if !on {
            return;
        }
        let Some(weak) = self.self_weak.get().cloned() else {
            tracing::warn!("演示动画：拿不到 Coordinator 弱引用，动画不启动");
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("langbar-icon-demo".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(DEMO_ANIM_FRAME_INTERVAL);
                    // 代际核对放在最前：关掉开关后最多再多睡一帧就退出，不需要唤醒机制。
                    if DEMO_ANIM_GEN.load(Ordering::Acquire) != generation {
                        return;
                    }
                    // 服务正在退出 ⇒ 一并收摊。弱引用同时兼作生命周期闸门。
                    let Some(c) = weak.upgrade() else {
                        return;
                    };
                    // 推进相位与发布必须分两段取锁：publish_langbar_icon_now 内部还要取
                    // 同一把锁，握着它调过去就是自锁。
                    {
                        let Ok(mut guard) = Coordinator::icon_publisher().lock() else {
                            return;
                        };
                        let Some(p) = guard.as_mut() else {
                            return;
                        };
                        p.advance_demo_frame();
                    }
                    c.publish_langbar_icon_now();
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "演示动画驱动线程启动失败");
        }
    }

    /// 演示动画当前是否开着（菜单画勾用）。
    #[cfg(all(feature = "desktop-ui", windows))]
    pub(crate) fn icon_demo_animation(&self) -> bool {
        Self::icon_publisher()
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|p| p.demo_animation()))
            .unwrap_or(false)
    }

    /// 把当前状态渲染成语言栏图标并投送共享内存。
    ///
    /// ⚠ **不要直接调用**：状态推送路径一律走 [`Self::status_with_icon_published`]，
    /// 那里保证了发布先于推送。直接调用的只有初始补发与调试菜单——它们不伴随状态推送。
    ///
    /// 失败一律只记日志：DLL 侧在读不到 SHM 时会退回本地 DirectWrite 绘制，
    /// 图标不会消失，只是不跟随标点状态——不值得为此中断状态推送。
    ///
    /// 返回是否**确实写了新位图**（`false` = 状态与上次相同已跳过，或发布器不可用）。
    /// 调用方据此决定要不要补一条刷新推送。
    #[cfg(all(feature = "desktop-ui", windows))]
    pub(super) fn publish_langbar_icon(&self, s: &StatusUpdateData) -> bool {
        use wind_ui::langbar_icon::{IconSpec, PunctBadge};

        let cell = Self::icon_publisher();

        // 与工具栏同口径：CapsLock 开启时中文模式实际在打英文（见 build_status 的
        // effective_chinese），此时不该显示中文标点角标。
        let effective_chinese = s.chinese_mode && !s.caps_lock;

        let Ok(mut guard) = cell.lock() else {
            return false;
        };
        let Some(p) = guard.as_mut() else {
            return false;
        };

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
            // 相位取发布器持有的当前值，**不写死 0**：演示动画开着时，一次普通的状态推送
            // （切中英/切标点）也会走到这里，若在此归零，跑马灯每被状态变化打断一次就
            // 跳回起点。相位归发布器所有、只由动画定时器推进，是这两件事互不干扰的前提。
            frame: p.demo_frame(),
        };

        match p.publish(&spec) {
            // 记序号是排查「图标落后一帧」的唯一抓手：本行的时刻与 DLL 日志里
            // `GetIcon: from SHM` 的时刻一对，就能判断那次 GetIcon 取到的是第几版。
            // label / punct 都是模式状态（「中」「拼」这类短称），不含输入内容。
            Ok(Some(seq)) => {
                tracing::debug!(
                    seq,
                    label = %spec.label,
                    punct = ?spec.punct,
                    "语言栏图标已发布"
                );
                true
            }
            // 状态未变，SHM 里已经是这张图，跳过重渲。不记日志：状态推送远比状态
            // 变化频繁，每次都记会把这条日志变成噪声。
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "发布语言栏图标失败");
                false
            }
        }
    }
}
