//! 命令栏（cmdbar）宿主集成
//!
//! 对照 Go `wind_input/internal/coordinator/cmdbar_context.go` + `cmdbar_services.go`。
//! 负责三件事：
//! 1. [`Coordinator::init_cmdbar`]：构造后装配 [`Services`] 与自身 Weak 引用；
//! 2. [`CmdbarCtx`]：把 coordinator 运行时状态适配为 [`EvalContext`]；
//! 3. 控制器（[`CoordIme`] / [`CoordDict`]）：把 cmdbar 动作映射到 coordinator 能力。
//!
//! **平台缺口**：key/clip/proc/url/search/config/setting 等服务在 Rust 平台层尚缺，
//! 对应字段留 `None`，相关动作调用返回 ServiceUnavailable（宿主侧记 WARN 降级）；
//! 现已接通 ime.toggle(cn-en/fullshape/s2t)、ime.schema、dict.add。
//!
//! **线程/锁**：动作经独立线程执行（见 `Coordinator::spawn_command`），故控制器回调
//! 自锁的 coordinator 方法是安全的（此刻按键处理已释放 state 锁）。

use crate::coordinator::Coordinator;
use chrono::{DateTime, Local};
use std::sync::{Arc, Weak};
use tracing::warn;
use wind_cmdbar::{DictService, EvalContext, ImeController, Services};

impl Coordinator {
    /// 构造后装配 cmdbar：自身 Weak 引用 + Services（ime/dict 后端）。一次性，幂等。
    pub(crate) fn init_cmdbar(self: &Arc<Self>) {
        let _ = self.self_weak.set(Arc::downgrade(self));
        let weak = Arc::downgrade(self);
        let mut svc = Services::new();
        svc.ime = Some(Arc::new(CoordIme(weak.clone())));
        svc.dict = Some(Arc::new(CoordDict(weak)));
        // config/key/clip/proc/url/search：平台/配置能力待补，留 None。
        let _ = self.cmdbar_services.set(svc);
    }

    /// 执行一个 `$CC` 命令源：解析 → 求值 → 跑动作链。返回待上屏文本（多数命令为空）。
    /// **必须在独立线程、未持 state 锁时调用**（控制器会回调自锁的 coordinator 方法）。
    pub(crate) fn run_command_candidate(&self, src: &str, input: &str) -> String {
        let Some(services) = self.cmdbar_services.get() else {
            return String::new();
        };
        let ctx = CmdbarCtx {
            input: input.to_string(),
            now: Local::now(),
            services,
        };
        let reg = wind_cmdbar::default_registry();
        match wind_cmdbar::evaluate_phrase(src, &ctx, reg) {
            Ok(wind_cmdbar::PhraseEval::Single { actions, .. }) => {
                let (insert, err) = wind_cmdbar::run_actions(&actions, &ctx, reg);
                if let Some(e) = err {
                    warn!("cmdbar 命令动作失败: {}", e);
                }
                insert
            }
            // $SS 数组的动作在各元素自身选中时执行，整组选中不跑动作。
            Ok(wind_cmdbar::PhraseEval::Array(_)) => String::new(),
            Err(e) => {
                warn!("cmdbar 命令求值失败 ({:?}): {}", src, e);
                String::new()
            }
        }
    }
}

/// 命令栏求值上下文（coordinator 适配）。当前提供 input/now/env + services；
/// 交互态（last/clip/sel/app/title）待平台层补齐后接入（与 Go 早期实现一致先留空）。
struct CmdbarCtx<'a> {
    input: String,
    now: DateTime<Local>,
    services: &'a Services,
}

impl EvalContext for CmdbarCtx<'_> {
    fn input(&self) -> String {
        self.input.clone()
    }
    fn last(&self, _n: i64) -> String {
        String::new()
    }
    fn clip(&self, _n: i64) -> String {
        String::new()
    }
    fn sel(&self) -> String {
        String::new()
    }
    fn app(&self) -> String {
        String::new()
    }
    fn title(&self) -> String {
        String::new()
    }
    fn env(&self, name: &str) -> String {
        std::env::var(name).unwrap_or_default()
    }
    fn now(&self) -> DateTime<Local> {
        self.now
    }
    fn services(&self) -> Option<&Services> {
        Some(self.services)
    }
}

/// IME 控制器：ime.toggle / ime.schema 接通；setting.* / theme_cycle 待平台能力补齐。
struct CoordIme(Weak<Coordinator>);

impl ImeController for CoordIme {
    fn toggle(&self, target: &str) -> anyhow::Result<()> {
        if let Some(c) = self.0.upgrade() {
            c.cmd_ime_toggle(target);
        }
        Ok(())
    }
    fn open_setting(&self, _page: &str) -> anyhow::Result<()> {
        warn!("setting.open: Rust 端设置应用待补");
        Ok(())
    }
    fn open_setting_web(&self, _page: &str) -> anyhow::Result<()> {
        warn!("setting.web: Rust 端设置应用待补");
        Ok(())
    }
    fn set_schema(&self, id: &str) -> anyhow::Result<()> {
        if let Some(c) = self.0.upgrade() {
            c.cmd_set_schema(id);
        }
        Ok(())
    }
    fn theme_cycle(&self, _dir: &str) -> anyhow::Result<String> {
        warn!("ime.theme_cycle: Rust 端主题循环待补");
        Ok(String::new())
    }
}

/// 词库控制器：dict.add 接通用户词层。
struct CoordDict(Weak<Coordinator>);

impl DictService for CoordDict {
    fn add_word(&self, text: &str, code: &str) -> anyhow::Result<()> {
        if let Some(c) = self.0.upgrade() {
            c.cmd_dict_add(text, code)?;
        }
        Ok(())
    }
}
