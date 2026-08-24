//! 常用字表的**用户覆盖**：运行时镜像装载 + 候选右键「设为生僻字 / 设为常用字」。
//!
//! 真相在 store（`wind_store::common_chars`，key = 单个字，不带方案），这里维护
//! `Coordinator::common_chars` 这份内存镜像，并在写库后立刻重灌——「设了没反应、
//! 重启才生效」正是镜像没回灌造成的，本仓已在别处栽过。
//!
//! ## 与候选调整（shadow）的分界
//!
//! | | 作用域 | 用户看到的 |
//! |---|---|---|
//! | shadow | 这个方案、这个码 | 「隐藏此候选」只在这个码下没了 |
//! | 本模块 | **全局**，所有方案所有码 | 「设为生僻字」在哪儿打它都降级 |
//!
//! 两者在右键菜单里挨着，文案必须把作用域说出来，否则用户会两个都试一遍再困惑于
//! 表现为何不同。

use tracing::{debug, warn};

/// 设置页列表的一行：**只列用户改过的字**（稀疏存储的直接好处——列表天然是
/// 「我的调整」，而不是让人在 8104 个字里翻页找）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommonCharRow {
    pub ch: char,
    /// 用户设定的方向。
    pub common: bool,
    /// 出厂判定。界面靠它显示「出厂：常用 → 现在：生僻」这层对照，
    /// 没有它，用户看到一行「的 · 生僻」根本不知道自己改的是什么。
    pub base_common: bool,
}

/// 某个字的当前状态（设置页「添加」时的预览与校验）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommonCharState {
    /// 是否落在常用字表的管辖域内（[`wind_candidate::is_common_scope`]）。
    /// `false` 时界面必须拒绝添加——域外字符读端根本不查表，存了也永不生效。
    pub governed: bool,
    /// 出厂判定。
    pub base_common: bool,
    /// 用户覆盖方向；`None` = 跟随出厂。
    pub over: Option<bool>,
}

/// 设置页对一个字的编辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommonCharEdit {
    /// 设为常用（`true`）/ 生僻（`false`）。与出厂同向时等价于 [`Self::Reset`]。
    Set(bool),
    /// 撤销覆盖，回到出厂判定。
    Reset,
    /// 清空全部覆盖（整表恢复出厂）。此时 `ch` 参数被忽略。
    ClearAll,
}

/// 候选文本作为「常用字标记」对象时的状态。
///
/// 刻意不带「是否已有覆盖」：菜单只有一项，点回出厂方向即等于恢复
/// （见 [`crate::Coordinator::toggle_common_char`]），没有第二个菜单项需要靠它灰显。
pub(crate) struct CommonCharMark {
    /// 目标字。
    pub ch: char,
    /// 当前判定（含用户覆盖）。菜单据此二选一：判常用就给「设为生僻字」，反之亦然。
    pub common: bool,
}

/// 候选文本能不能被标记，能则返回那个字。
///
/// 两条准入，缺一不可：
/// 1. **恰好一个字符**——「常用」是字级属性，词组没有；
/// 2. **落在常用字表的管辖域内**（[`wind_candidate::is_common_scope`]）——域外字符
///    （中文标点、emoji、字母数字）读端根本不查表，放行就会存下一条永不生效的记录。
///
/// ⚠️ 第 2 条必须调 `is_common_scope` 而不是自己再写一份「是不是汉字」：两份判据一旦
/// 漂移，用户设了却完全静默地不生效（`wind-candidate` 侧有
/// `common_scope_matches_string_judgement` 钉着同源性）。
pub(crate) fn common_char_of(text: &str) -> Option<char> {
    let mut it = text.chars();
    let ch = it.next()?;
    if it.next().is_some() {
        return None;
    }
    wind_candidate::is_common_scope(ch).then_some(ch)
}

impl crate::Coordinator {
    /// 从 store 装载用户覆盖到运行时镜像。启动时一次；每次写库后也走它。
    ///
    /// headless（无 store）时保持空覆盖 = 纯出厂判定。
    pub(crate) fn reload_common_chars(&self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let rows = match store.list_common_char_overrides() {
            Ok(v) => v,
            Err(e) => {
                warn!("常用字覆盖: 读取失败，本次按出厂判定: {e}");
                return;
            }
        };
        let n = rows.len();
        self.common_chars
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .set_overrides(rows.into_iter().map(|o| (o.ch, o.common)));
        debug!("常用字覆盖: 装载 {n} 条");
    }

    /// 取某个候选文本的标记状态；不可标记时 `None`（菜单据此不给这两项）。
    pub(crate) fn common_char_mark(&self, text: &str) -> Option<CommonCharMark> {
        // 无 store 就没有落点：菜单给了入口而写端无处可写，是那种「点得动却毫无反应」
        // 的静默错配。headless 与未初始化存储一律不给。
        self.store.as_ref()?;
        let ch = common_char_of(text)?;
        let cc = self.common_chars.read().unwrap_or_else(|e| e.into_inner());
        Some(CommonCharMark {
            ch,
            common: cc.is_char_common(ch),
        })
    }

    /// 设置页列表：**只含用户改过的字**，按码位升序（store 的键序）。
    pub(crate) fn common_char_rows(&self) -> Vec<CommonCharRow> {
        let Some(store) = self.store.as_ref() else {
            return Vec::new();
        };
        let rows = match store.list_common_char_overrides() {
            Ok(v) => v,
            Err(e) => {
                warn!("常用字覆盖: 列举失败: {e}");
                return Vec::new();
            }
        };
        // 出厂判定从**内存镜像**取，不再读一次文件：镜像与过滤层用的是同一份数据，
        // 分头取值会让界面显示的「出厂判定」与实际生效的那份悄悄错开。
        let cc = self.common_chars.read().unwrap_or_else(|e| e.into_inner());
        rows.into_iter()
            .map(|o| CommonCharRow {
                ch: o.ch,
                common: o.common,
                base_common: cc.is_base_common(o.ch),
            })
            .collect()
    }

    /// 某个字的当前状态：设置页「添加」时用来预览与**校验**。
    pub(crate) fn common_char_state(&self, ch: char) -> CommonCharState {
        let cc = self.common_chars.read().unwrap_or_else(|e| e.into_inner());
        CommonCharState {
            governed: wind_candidate::is_common_scope(ch),
            base_common: cc.is_base_common(ch),
            over: cc.override_of(ch),
        }
    }

    /// 设置页对一个字的编辑：写库 + 回灌镜像一并完成。
    ///
    /// ⚠️ 拒绝管辖域外的字符（中文标点、emoji、字母数字）。读端 `is_string_common` 对它们
    /// 直接跳过，放行只会在库里留下一条用户以为生效的死记录，且**全程无报错**。
    /// 这里返回 Err 而不是静默忽略，界面才能告诉用户「这个字符不受常用字表管辖」。
    pub(crate) fn common_char_edit(&self, ch: char, edit: CommonCharEdit) -> anyhow::Result<()> {
        let Some(store) = self.store.as_ref() else {
            anyhow::bail!("无持久化存储");
        };
        match edit {
            CommonCharEdit::ClearAll => {
                let n = store.clear_common_char_overrides()?;
                debug!("常用字覆盖: 清空 {n} 条");
                self.reload_common_chars();
            }
            CommonCharEdit::Reset => {
                self.clear_common_char(ch);
            }
            CommonCharEdit::Set(common) => {
                if !wind_candidate::is_common_scope(ch) {
                    anyhow::bail!("「{ch}」不受常用字表管辖（只有汉字才有常用/生僻之分）");
                }
                self.apply_common_target(ch, common);
            }
        }
        Ok(())
    }

    /// 页内第 `page_local` 个候选的标记状态（测试/诊断用）：`(字, 当前是否判常用)`；
    /// `None` = 右键菜单不给「设为生僻字 / 设为常用字」这一项。
    ///
    /// 菜单可用性与写端准入共用 [`Self::common_char_mark`]，故断言本函数
    /// **等于同时锁住两条通路**——它们错配的表现是「点得动却毫无反应」，没有日志。
    pub fn debug_common_char_mark(&self, page_local: usize) -> Option<(char, bool)> {
        let text = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (start, end) = self.page_range(&state);
            let idx = start + page_local;
            if idx >= end || idx >= state.candidates.len() {
                return None;
            }
            state.candidates[idx].text.clone()
        };
        self.common_char_mark(&text).map(|m| (m.ch, m.common))
    }

    /// 写一条覆盖并立刻重灌镜像。`common` = 设为常用字 / 设为生僻字。
    ///
    /// 返回是否写成功——调用方据此决定要不要重建候选（写失败还重建纯属白跑一轮）。
    pub(crate) fn set_common_char(&self, ch: char, common: bool) -> bool {
        let Some(store) = self.store.as_ref() else {
            return false;
        };
        if let Err(e) = store.set_common_char_override(ch, common) {
            warn!("常用字覆盖: 写入失败 ch={ch} common={common}: {e}");
            return false;
        }
        debug!(
            "常用字覆盖: 设 {ch} → {}",
            if common { "常用" } else { "生僻" }
        );
        self.reload_common_chars();
        true
    }

    /// 撤销某字的覆盖，回到出厂判定。
    ///
    /// 与「设为常用字」**不是**一回事：出厂判生僻的字撤销后仍是生僻。
    pub(crate) fn clear_common_char(&self, ch: char) -> bool {
        let Some(store) = self.store.as_ref() else {
            return false;
        };
        match store.remove_common_char_override(ch) {
            Ok(existed) => {
                debug!("常用字覆盖: 撤销 {ch}（原本有覆盖={existed}）");
                self.reload_common_chars();
                existed
            }
            Err(e) => {
                warn!("常用字覆盖: 撤销失败 ch={ch}: {e}");
                false
            }
        }
    }

    /// 把某个字的判定设成 `common`，返回是否真的有变化。
    ///
    /// ## 切到出厂方向时**删覆盖**，而不是写一条同向记录
    ///
    /// 用户把「的」设成生僻（存 `false`，与出厂相反），过一会儿又设回常用：目标方向
    /// 恰好等于出厂判定，此时删掉那条覆盖让它重新跟随出厂，而不是存一条 `true`。
    /// 两个好处：
    /// - 库里永远只有「与出厂不同」的字，设置页列出来的就是一份干净的「我改过的」；
    /// - 出厂表将来升版时这个字自动跟随，不会被一条冗余记录钉死在旧判定上。
    ///
    /// 由此也**不需要单独的「恢复出厂」菜单项**——同一项点回去就是恢复。
    ///
    /// ★ 右键与设置页共用本函数。两条入口各写一份「要不要删」的判断，迟早会漂移成
    /// 「右键点回去干净、设置页改回去留一条冗余」这种没人看得出的差别。
    pub(crate) fn apply_common_target(&self, ch: char, common: bool) -> bool {
        let base = self
            .common_chars
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_base_common(ch);
        if common == base {
            self.clear_common_char(ch)
        } else {
            self.set_common_char(ch, common)
        }
    }

    /// 候选右键「设为生僻字 / 设为常用字」的写端。
    pub(crate) fn toggle_common_char(&self, state: &mut crate::coordinator::State, text: &str) {
        let Some(mark) = self.common_char_mark(text) else {
            return;
        };
        // 目标 = 当前判定取反。菜单文案正是按 `mark.common` 二选一的，两边同源。
        if !self.apply_common_target(mark.ch, !mark.common) {
            return;
        }
        // 重建候选：`is_common` 一变，过滤（智能 / 常用字档）与**排序**都会跟着变——
        // 后者容易被忘：混输的拼音精确档拿 `is_common` 当提档准入（`is_pinyin_exact_tier`），
        // 只重绘不重建的话，用户会看到「标记了，但候选顺序还是老样子」。
        //
        // ⚠️ 必须按模式分派：主路径的 `update_candidates` 读 `input_buffer`，特殊模式下它
        // 恒为空——走错分支的后果不是「不刷新」而是候选窗当场清空。
        if matches!(state.active, Some(crate::pipeline::ModeKind::Special(_))) {
            // 返回值是「全码策略请求自动上屏」的意向，此处刻意丢弃：编码一个字没变，
            // 用户只是在标记字的常用性，凭空上屏是错的。
            let _ = self.update_special_candidates(state);
        } else {
            self.update_candidates(state);
        }
        self.notify_ui_update(state);
    }
}

#[cfg(test)]
mod tests {
    use super::common_char_of;

    #[test]
    fn accepts_single_han_and_pua() {
        assert_eq!(common_char_of("我"), Some('我'));
        assert_eq!(common_char_of("鬱"), Some('鬱'));
        assert_eq!(
            common_char_of("\u{E831}"),
            Some('\u{E831}'),
            "PUA 被码表当汉字用"
        );
        assert_eq!(common_char_of("\u{20000}"), Some('\u{20000}'), "扩展 B");
    }

    #[test]
    fn rejects_phrases() {
        // 「常用」是字级属性，词组没有——给词组存覆盖，读端逐字判定时永远看不到它。
        assert_eq!(common_char_of("我们"), None);
        assert_eq!(common_char_of(""), None);
    }

    /// 域外字符一律拒绝：读端 `is_string_common` 直接忽略它们，放行等于存一条死记录。
    #[test]
    fn rejects_out_of_scope_chars() {
        for s in ["、", "，", "①", "℃", "あ", "😀", "A", "7", " "] {
            assert_eq!(common_char_of(s), None, "{s} 不该放行");
        }
    }
}
