//! 模式级候选布局（强制竖排 / 横排）的**唯一**决策点。
//!
//! 设计见 `docs/design/mode-candidate-layout.md`。
//!
//! 这里刻意**不**做「进入模式时保存旧布局、退出时回放」——那需要在 `state.active` 的
//! 8 个清空点各写一遍恢复（此前 quick / add_word 两个模式就已各写了三处），漏一处的表现是
//! 候选窗卡在竖排且没有任何日志。改为**声明式重算**：
//!
//! > 任何时刻的方向 = f(全局基线, 当前模式意图)
//!
//! 「恢复」于是不再是一个需要被执行的动作，而是模式退出后重算的自然结果。副作用是**自愈**：
//! 即使某条退出路径什么都没做（失焦、或将来新增一条谁都没想到的退出路径），
//! 下一次候选显示会自动算回基线。
//!
//! 决策（纯函数 [`intent_for`] / [`vertical_for`]）与取值（`impl Coordinator` 的包装）
//! 刻意分开：前者可直接用 `Config` + `ModeKind` 测出完整矩阵，不必构造协调器。

use crate::coordinator::{Coordinator, State};
use crate::pipeline::ModeKind;
use wind_config::{Config, LayoutIntent, OverlaySpec};
use wind_ui_types::UiCommand;

/// 「模式 → 布局意图」映射。**唯一一处**把这层对应关系写死的地方——新增模式只加一行。
///
/// 优先级：加词 > 独占模式 > 全局。加词面板是覆盖在任意输入态之上的临时面板，
/// 其显示需求（逐字确认）与底层模式无关，故优先。
///
/// 注意 `add_word` **不在** `state.active` 里（它是独立的 `add_word_active` 标志），
/// 所以「当前是什么模式」的判定必须把它一起收进来——这正是需要一个集中函数、
/// 而不是各模式内部各判各的理由。
///
/// `overlay` = 当前特殊模式的 `[overlay]` 段快照（`State::overlay_spec`）。特殊模式的
/// 配置住在方案文件而不是 `Config` 里，故它必须单独传入——保持本函数是纯函数，
/// 测试直接造 `OverlaySpec` 即可，不必构造 `EngineManager`。
pub(crate) fn intent_for(
    cfg: &Config,
    overlay: Option<&OverlaySpec>,
    active: Option<ModeKind>,
    add_word: bool,
) -> LayoutIntent {
    if add_word {
        return cfg.input.add_word.candidate_layout;
    }
    match active {
        Some(ModeKind::Mix(i)) => cfg
            .schema
            .mix_modes
            .get(i as usize)
            .map(|m| m.candidate_layout),
        Some(ModeKind::Special(_)) => overlay.map(|o| o.candidate_layout),
        Some(ModeKind::TempPinyin) => Some(cfg.input.temp_pinyin.candidate_layout),
        Some(ModeKind::TempEnglish) => Some(cfg.input.temp_english.candidate_layout),
        Some(ModeKind::Url) => Some(cfg.input.url.candidate_layout),
        // 辅助码：候选布局沿用主路径（筛选不改呈现形态）。
        Some(ModeKind::AuxCode) => None,
        None => None,
    }
    // 下标越界（热重载删掉了该实例）回落 Follow——跟随全局是安全的默认，不猜方向。
    .unwrap_or_default()
}

/// 意图叠加到基线上得出实际方向（true = 竖排）。
///
/// `Follow` 与 `Vertical` 只在 `baseline == true` 时才有区别——前者跟着基线变、后者恒定。
/// 这正是三态相对旧布尔 `force_vertical` 的全部增量，测试必须覆盖这一格。
pub(crate) fn vertical_for(intent: LayoutIntent, baseline: bool) -> bool {
    match intent {
        LayoutIntent::Vertical => true,
        LayoutIntent::Horizontal => false,
        LayoutIntent::Follow => baseline,
    }
}

impl Coordinator {
    /// 当前生效的布局意图（[`intent_for`] 的取值包装）。
    pub(crate) fn layout_intent(&self, state: &State) -> LayoutIntent {
        let rt = self.rt();
        intent_for(
            &rt.config,
            state.overlay_spec.as_ref(),
            state.active,
            state.add_word_active,
        )
    }

    /// 期望的候选方向（true = 竖排）。
    ///
    /// 基线取运行时镜像 `candidate_vertical`，**不读 `config.ui.candidate.layout`**：
    /// 命令栏 `ime.toggle("layout")` 改的是镜像，config 要等写盘 + 热重载回灌才跟上，
    /// 期间读 config 会按旧方向恢复。此前的 `force_vertical` 实现读的正是 config，
    /// 这是它的既存缺陷之一。
    pub(crate) fn desired_vertical(&self, state: &State) -> bool {
        let baseline = *self
            .candidate_vertical
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        vertical_for(self.layout_intent(state), baseline)
    }

    /// 当前期望的候选方向（测试/诊断用，对齐 `debug_in_temp_pinyin` 的既有形态）。
    pub fn debug_desired_vertical(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.desired_vertical(&state)
    }

    /// 把期望方向下发 UI，仅在与**上次真正下发的值**不同时发送。
    ///
    /// 去重不是性能优化：没有它每次按键都会发一条 `SetCandidateLayout`，UI 侧
    /// `set_vertical` 触发重排，在首显时序敏感的路径上会引入抖动。
    ///
    /// ⚠️ 它同时是测试的假绿来源——断言要落在 [`Self::desired_vertical`] 的返回值上，
    /// 不要断言「有没有发出 UiCommand」：值没变时本就不发，测试会拿不到信号却看起来通过。
    ///
    /// 调用点是 `UpdateCandidates` 的**两个**发送点之前：`notify_ui_update`（主路径）与
    /// `show_add_word_preview`（加词面板走独立绘制路径，不经 notify_ui_update）。
    /// 同 channel 按序处理，UI 先改方向再填候选，不会闪。隐藏路径无需调用——布局只在
    /// 显示时有意义，退出模式必然伴随「隐藏 + 下次显示」，恢复发生在显示之前。
    pub(crate) fn sync_candidate_layout(&self, state: &State) {
        let want = self.desired_vertical(state);
        let mut last = self
            .candidate_layout_sent
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *last != want {
            *last = want;
            let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(want));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_config::config::MixModeConfig;

    /// 造一份把各模式意图都设成指定值的配置。
    ///
    /// **不含特殊模式**——它的意图住在方案文件的 `[overlay]` 段，由 [`overlay_with`]
    /// 单独造出来经参数传入（见 `intent_for` 的 `overlay` 参数）。
    fn cfg_with(intent: LayoutIntent) -> Config {
        let mut c = Config::default();
        c.input.temp_pinyin.candidate_layout = intent;
        c.input.temp_english.candidate_layout = intent;
        c.input.url.candidate_layout = intent;
        c.input.add_word.candidate_layout = intent;
        c.schema.mix_modes = vec![MixModeConfig {
            candidate_layout: intent,
            ..Default::default()
        }];
        c
    }

    /// 造一份 `[overlay]` 段快照（= 特殊模式那一路的配置来源）。
    fn overlay_with(intent: LayoutIntent) -> OverlaySpec {
        OverlaySpec {
            candidate_layout: intent,
            ..Default::default()
        }
    }

    const MODES: &[ModeKind] = &[
        ModeKind::Mix(0),
        ModeKind::Special(0),
        ModeKind::TempPinyin,
        ModeKind::TempEnglish,
        ModeKind::Url,
    ];

    /// 全矩阵：每种模式 × 三种意图 × 两种基线。
    ///
    /// **`Follow` + 基线竖排是唯一能区分新旧语义的一格**（其余格三态与旧布尔表现相同）——
    /// 漏了它整个三态改造等于没测，故它在本用例里被显式断言而非顺带覆盖。
    #[test]
    fn every_mode_maps_intent_over_baseline() {
        for &mode in MODES {
            for (intent, baseline, want) in [
                (LayoutIntent::Follow, false, false),
                (LayoutIntent::Follow, true, true), // ← 三态相对布尔的全部增量
                (LayoutIntent::Vertical, false, true),
                (LayoutIntent::Vertical, true, true),
                (LayoutIntent::Horizontal, false, false),
                (LayoutIntent::Horizontal, true, false), // ← 旧布尔表达不了这一格
            ] {
                let cfg = cfg_with(intent);
                let ovs = overlay_with(intent);
                let got = vertical_for(intent_for(&cfg, Some(&ovs), Some(mode), false), baseline);
                assert_eq!(
                    got, want,
                    "mode={mode:?} intent={intent:?} baseline={baseline} 应得 {want}"
                );
            }
        }
    }

    /// 无模式时一律跟随基线，与任何模式配置无关。
    #[test]
    fn no_active_mode_follows_baseline() {
        let cfg = cfg_with(LayoutIntent::Vertical);
        let ovs = overlay_with(LayoutIntent::Vertical);
        let ov = Some(&ovs);
        assert_eq!(intent_for(&cfg, ov, None, false), LayoutIntent::Follow);
        assert!(!vertical_for(intent_for(&cfg, ov, None, false), false));
        assert!(vertical_for(intent_for(&cfg, ov, None, true), true));
    }

    /// 加词优先于底层模式：底层要横排，加词仍按加词的意图。
    #[test]
    fn add_word_outranks_active_mode() {
        let mut cfg = cfg_with(LayoutIntent::Horizontal);
        cfg.input.add_word.candidate_layout = LayoutIntent::Vertical;
        let ovs = overlay_with(LayoutIntent::Horizontal);
        let ov = Some(&ovs);
        for &mode in MODES {
            assert_eq!(
                intent_for(&cfg, ov, Some(mode), true),
                LayoutIntent::Vertical,
                "mode={mode:?} 下加词应优先"
            );
        }
        // 无底层模式时同样生效。
        assert_eq!(intent_for(&cfg, None, None, true), LayoutIntent::Vertical);
    }

    /// mix 下标越界（热重载删掉了该实例）回落 Follow，不猜方向、不 panic。
    /// 特殊模式侧的对应情形是**快照缺失**（该方案没有 `[overlay]` 段），同样回落。
    #[test]
    fn out_of_range_instance_falls_back_to_follow() {
        let cfg = cfg_with(LayoutIntent::Vertical);
        assert_eq!(
            intent_for(&cfg, None, Some(ModeKind::Mix(9)), false),
            LayoutIntent::Follow
        );
        assert_eq!(
            intent_for(&cfg, None, Some(ModeKind::Special(0)), false),
            LayoutIntent::Follow,
            "无 [overlay] 快照时回落跟随全局"
        );
        // 回落后仍跟随基线两个方向。
        assert!(vertical_for(
            intent_for(&cfg, None, Some(ModeKind::Mix(9)), false),
            true
        ));
        assert!(!vertical_for(
            intent_for(&cfg, None, Some(ModeKind::Mix(9)), false),
            false
        ));
    }

    /// 特殊模式的意图**只来自 `[overlay]` 快照**，与下标、与 `Config` 都无关。
    ///
    /// 这条钉住的是本次下沉的核心：配置从 config.toml 的数组搬到了方案文件，
    /// 若有人把取值改回读 `cfg`，这里会红。
    #[test]
    fn special_mode_intent_comes_from_overlay_spec() {
        // cfg 里所有模式都是 Horizontal，快照是 Vertical——取值必须听快照的。
        let cfg = cfg_with(LayoutIntent::Horizontal);
        let ovs = overlay_with(LayoutIntent::Vertical);
        for idx in [0u8, 9u8] {
            assert_eq!(
                intent_for(&cfg, Some(&ovs), Some(ModeKind::Special(idx)), false),
                LayoutIntent::Vertical,
                "下标 {idx} 不参与取值"
            );
        }
    }

    /// 每个模式只读自己的配置项，不串味（防止映射表复制粘贴写错字段）。
    #[test]
    fn each_mode_reads_its_own_key() {
        let mut cfg = cfg_with(LayoutIntent::Follow);
        cfg.input.temp_english.candidate_layout = LayoutIntent::Horizontal;
        let ovs = overlay_with(LayoutIntent::Follow);
        let ov = Some(&ovs);
        assert_eq!(
            intent_for(&cfg, ov, Some(ModeKind::TempEnglish), false),
            LayoutIntent::Horizontal
        );
        for &mode in MODES {
            if matches!(mode, ModeKind::TempEnglish) {
                continue;
            }
            assert_eq!(
                intent_for(&cfg, ov, Some(mode), false),
                LayoutIntent::Follow,
                "改临英不应影响 {mode:?}"
            );
        }
    }

    /// 内置 quick_mix 出厂强制竖排（等价于旧 `quick_input.force_vertical = true`）。
    /// 守的是「默认值只能落在 default_mix_modes()、预置文件不写 mix_modes」这条约束——
    /// 若有人把默认改回 Follow，全局横排的用户会突然发现快捷输入变横排了。
    #[test]
    fn builtin_quick_mix_defaults_to_vertical() {
        let cfg = Config::default();
        assert!(
            vertical_for(intent_for(&cfg, None, Some(ModeKind::Mix(0)), false), false),
            "内置 quick_mix 应出厂竖排"
        );
    }

    /// 加词出厂竖排（此前是硬编码强制竖排，迁成配置项后行为须不变）。
    #[test]
    fn add_word_defaults_to_vertical() {
        let cfg = Config::default();
        assert!(vertical_for(intent_for(&cfg, None, None, true), false));
    }
}
