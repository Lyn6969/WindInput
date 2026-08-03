//! 「检索范围」过滤在**前缀输入与全码输入下的一致性**（端到端）。
//!
//! 回归现场：五笔「桜」(sivg) 打 `siv` 能出、打全 `sivg` 反而消失。
//! 根因不在过滤规则，而在它上游的**按 text 去重**：`sivg` 码位下另有常用字「档」，而「档」
//! 还有简码 `siv`——打 `siv` 时「档」以 code="siv" 入列，它在 `sivg` 的那条被去重丢弃，于是
//! `sivg` 组只剩生僻的「桜」成了孤儿码而放行；打全 `sivg` 时两者同组，「桜」才被滤掉。
//! **同一个字，打得越全反而越不出**。修法见 `Candidate::merged_codes`：去重时把被弃条目
//! 所占的码位并进幸存者，使过滤看到的分组与词库真相一致。
//!
//! ## ⚠️ 三条用例必须合看，缺一即可能假绿
//!
//! - `rare_char_hidden_consistently`：主用例，`siv` 与 `sivg` 下「桜」**同样不出现**；
//! - `gb18030_reveals_rare_char_in_both_inputs`：**反向对照**，同样两次输入、只把检索范围放开
//!   → 「桜」两次都出现。它排除了「桜 压根不在词库/检索不到」这种假绿——没有它，主用例
//!   连词库为空都能通过；
//! - `orphan_rare_char_survives_smart_filter`：**边界对照**，`sivs` 的「樑」无同码常用字，
//!   智能档下须保留。证明修复只还原真实存在的同码位遮蔽，而非无差别多滤生僻字。
//!
//! 词典缺失时自动跳过 —— ⚠️ `build_dev/data` 不存在时**整族静默跳过而计数照绿**，
//! 判据是耗时（正常秒级 vs 跳过 0.0x s）。

use std::path::PathBuf;
use wind_bridge::handler::{KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::EVENT_KEY_DOWN;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn dict_ready(d: &std::path::Path) -> bool {
    d.join("schemas/wubi86/wubi86_jidian.dict.yaml").exists()
}

fn key_event(key_code: u32) -> KeyEventData {
    KeyEventData {
        key_code,
        scan_code: 0,
        modifiers: 0,
        event_type: EVENT_KEY_DOWN,
        toggles: 0,
        event_seq: 0,
        prev_char: 0,
    }
}

fn wubi_config(filter_mode: &str) -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into()];
    cfg.schema.active = "wubi86".into();
    cfg.input.default.chinese_mode = true;
    cfg.input.filter_mode = filter_mode.into();
    cfg
}

/// 在指定检索范围下敲入 `code`，返回候选文本列表。
///
/// 每次新建 Coordinator：满码自动上屏默认关（`auto_commit_at_full=false`），故 4 码输入
/// 仍停在候选态可供检查。
fn candidates_for(filter_mode: &str, code: &str) -> Vec<String> {
    let coord = Coordinator::new_headless(wubi_config(filter_mode), Some(&data_dir()));
    for c in code.chars() {
        coord.handle_key_event(&key_event((c.to_ascii_uppercase() as u32) & 0xFF));
    }
    coord.debug_all_candidate_texts()
}

/// 主用例：生僻字在前缀输入与全码输入下的可见性必须一致。
#[test]
fn rare_char_hidden_consistently() {
    if !dict_ready(&data_dir()) {
        eprintln!("跳过：五笔词库不存在");
        return;
    }
    let siv = candidates_for("smart", "siv");
    let sivg = candidates_for("smart", "sivg");

    // 前置：常用字「档」两次都在，确认输入链路正常（否则下面的「不含桜」毫无意义）
    assert!(
        siv.contains(&"档".to_string()),
        "打 siv 应出常用字「档」，实际: {:?}",
        &siv[..siv.len().min(8)]
    );
    assert!(
        sivg.contains(&"档".to_string()),
        "打 sivg 应出常用字「档」，实际: {:?}",
        &sivg[..sivg.len().min(8)]
    );

    // 核心：sivg 码位有常用字「档」占位，「桜」在两种输入下都应被智能档滤掉
    assert!(
        !siv.contains(&"桜".to_string()),
        "打 siv 时 sivg 码位的生僻字「桜」不该露出——此前因去重丢失遮蔽关系而露出，\
         导致打全 sivg 反而消失。实际: {:?}",
        &siv[..siv.len().min(12)]
    );
    assert!(
        !sivg.contains(&"桜".to_string()),
        "打全 sivg 时「桜」应被同码常用字「档」遮蔽。实际: {:?}",
        &sivg[..sivg.len().min(12)]
    );
}

/// ★ 反向对照：放开检索范围后「桜」必须两次都出现。
///
/// 没有这条，主用例在「词库里根本没有桜」「siv/sivg 检索不到任何东西」时同样会绿。
#[test]
fn gb18030_reveals_rare_char_in_both_inputs() {
    if !dict_ready(&data_dir()) {
        eprintln!("跳过：五笔词库不存在");
        return;
    }
    let siv = candidates_for("gb18030", "siv");
    let sivg = candidates_for("gb18030", "sivg");
    assert!(
        siv.contains(&"桜".to_string()),
        "全部字符档下打 siv 应能前缀命中「桜」，实际: {:?}",
        &siv[..siv.len().min(12)]
    );
    assert!(
        sivg.contains(&"桜".to_string()),
        "全部字符档下打全 sivg 必须出「桜」（它正是该码位的字），实际: {:?}",
        &sivg[..sivg.len().min(12)]
    );
}

/// ★ 边界对照：无同码常用字的孤儿码生僻字，智能档下须保留。
///
/// 「樑」(sivs) 与「档」(siv/sivg) 不共码位，不该被本次修复牵连滤掉——
/// 若实现退化成「有常用字就滤掉所有生僻字」，这条会红。
#[test]
fn orphan_rare_char_survives_smart_filter() {
    if !dict_ready(&data_dir()) {
        eprintln!("跳过：五笔词库不存在");
        return;
    }
    let sivs = candidates_for("smart", "sivs");
    assert!(
        sivs.contains(&"樑".to_string()),
        "sivs 码位下无常用字，孤儿码生僻字「樑」应保留，实际: {:?}",
        &sivs[..sivs.len().min(12)]
    );
}
