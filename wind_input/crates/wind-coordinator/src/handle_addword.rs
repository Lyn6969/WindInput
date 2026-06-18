//! 造词 / 加词：选中后自动造词、命令栏 dict.add 加用户词。
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。

use crate::coordinator::{Coordinator, State, LEARN_ADD_WEIGHT, LEARN_WEIGHT_DELTA};
use tracing::{debug, warn};

impl Coordinator {
    /// 加词到用户层（code 为空时暂不支持自动推导编码）。
    pub(crate) fn cmd_dict_add(&self, text: &str, code: &str) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            anyhow::bail!("dict.add: 无 store");
        };
        if code.is_empty() {
            anyhow::bail!("dict.add: code 为空（Rust 端暂未支持自动推导编码）");
        }
        let schema = self.engine_mgr.active_schema_id();
        store.add_user_word(&schema, code, text, 100)?;
        Ok(())
    }

    /// 自动造词（L）：仅当用户**分步**组成（committed_segs ≥2 段、合并 ≥2 字）才学。
    /// 完整拼音码 = 各段码拼接；词 = 各段汉字拼接。写入临时层（需临时层，达阈值由 store 晋升路线处理）。
    pub(crate) fn learn_phrase_on_commit(&self, state: &State) {
        if state.committed_segs.len() < 2 {
            return;
        }
        let code: String = state.committed_segs.iter().map(|(c, _)| c.as_str()).collect();
        let text: String = state.committed_segs.iter().map(|(_, t)| t.as_str()).collect();
        if text.chars().count() < 2 || code.is_empty() {
            return;
        }
        let Some(store) = &self.store else { return };
        let schema = self.engine_mgr.active_schema_id();
        // add_weight/delta 取保守默认；晋升计数阈值由临时层累积达成（后续可接入 schema.learning 配置）。
        if let Err(e) = store.learn_temp_word(&schema, &code, &text, LEARN_ADD_WEIGHT, LEARN_WEIGHT_DELTA) {
            warn!("learn_temp_word failed: {}", e);
        } else {
            debug!("auto-learned phrase: {} -> {}", code, text);
        }
    }
}
