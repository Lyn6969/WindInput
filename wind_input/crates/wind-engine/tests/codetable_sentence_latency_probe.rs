//! 探针：量「首次整句解码」落在按键线程上的延迟，并验证后台预热确实把它搬走了。
//!
//! 依赖 `build_dev/data` 真实词库，故 `#[ignore]`：
//!
//! ```text
//! cargo test -p wind-engine --test codetable_sentence_latency_probe -- --ignored --nocapture
//! ```
//!
//! ★ 判据是**两条路径的结果必须逐字相同**，不只是「预热那条快」。预热若改变了取值，
//!   那就不是预热而是另一套解码——快也没有意义。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use wind_dict::cached::CachedDict;
use wind_dict::{DictManager, SystemDictLayer};
use wind_engine::Engine;
use wind_engine::codetable::{CodeTableEngine, CommitOptions};

const MAX_CODE_LEN: usize = 4;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data/schemas")
}

/// 每次都新建一台引擎——`OnceLock` 是**按解码器实例**的，复用引擎就量不到冷启动了。
fn fresh_engine(with_freq: bool) -> Option<(CodeTableEngine, HashMap<String, String>)> {
    let base = data_dir().join("wubi86");
    let yaml = base.join("wubi86_jidian.dict.yaml");
    if !yaml.exists() {
        return None;
    }
    let wdat = base.join("wubi86_jidian.dict.wdat");
    let dict = CachedDict::load_at_with(&yaml, &wdat, false).ok()?;
    let mut full: HashMap<String, String> = HashMap::new();
    dict.for_each_entry(&mut |code, text, _w| {
        if !code.chars().all(|c| c.is_ascii_lowercase()) {
            return;
        }
        full.entry(text.to_string())
            .and_modify(|c| {
                if code.len() > c.len() {
                    *c = code.to_string();
                }
            })
            .or_insert_with(|| code.to_string());
    });
    let dm = DictManager::new();
    dm.register_layer(Box::new(SystemDictLayer::new(dict, "codetable-system")));
    let opts = CommitOptions {
        sentence_input: true,
        ..Default::default()
    };
    let mut e = CodeTableEngine::new(MAX_CODE_LEN, opts, Arc::new(dm));
    if with_freq {
        e = e.with_sentence_schemas_dir(data_dir());
    }
    Some((e, full))
}

fn top(e: &CodeTableEngine, code: &str) -> String {
    e.convert(code, 9)
        .unwrap()
        .candidates
        .first()
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

#[test]
#[ignore = "探针：依赖 build_dev 真实词库"]
fn prewarm_moves_first_decode_cost_off_the_key_thread() {
    let Some((e_cold, full)) = fresh_engine(true) else {
        eprintln!("跳过：无真实词库");
        return;
    };
    let code = format!(
        "{}{}{}{}",
        full["工作"], full["人员"], full["技术"], full["创新"]
    );

    // ① 不预热：首次解码在「按键线程」上现付两张表。
    let t0 = std::time::Instant::now();
    let cold_text = top(&e_cold, &code);
    let cold_ms = t0.elapsed().as_millis();

    let mut warm_us = 0u128;
    for _ in 0..20 {
        let t = std::time::Instant::now();
        let _ = top(&e_cold, &code);
        warm_us = warm_us.max(t.elapsed().as_micros());
    }

    // ② 只建简码索引（不接词频来源），拆出这一项占多少。
    let (e_idx, _) = fresh_engine(false).unwrap();
    let t = std::time::Instant::now();
    let _ = top(&e_idx, &code);
    let index_only_ms = t.elapsed().as_millis();

    // ③ 预热：构建完就把两张表推给后台线程。
    let (e_warm, _) = fresh_engine(true).unwrap();
    e_warm.prewarm_sentence();
    // 等预热线程落地。生产里不需要等——用户从切方案到敲出第 5 个码远不止这点时间。
    std::thread::sleep(std::time::Duration::from_secs(3));
    let t = std::time::Instant::now();
    let warm_text = top(&e_warm, &code);
    let prewarmed_ms = t.elapsed().as_millis();

    eprintln!("不预热·首次解码（按键线程现付）: {cold_ms} ms → {cold_text}");
    eprintln!("  其中·仅建简码索引:            {index_only_ms} ms");
    eprintln!("不预热·其后每次解码:            {warm_us} us");
    eprintln!("预热后·首次解码:                {prewarmed_ms} ms → {warm_text}");

    // ★ 结果必须一致：预热只搬运，不改变取值。
    assert_eq!(cold_text, warm_text, "预热改变了解码结果 —— 那就不是预热");
    assert!(
        !cold_text.is_empty(),
        "整句没出候选，探针本身失效（先查词库与 sentence_input）"
    );
    // ★ 反向对照：不预热那条必须**确实很慢**，否则「预热后很快」是假绿
    //   （比如词库没加载成功，两条都是空表都很快）。
    assert!(
        cold_ms >= 100,
        "不预热的首次解码只用了 {cold_ms} ms —— 说明根本没加载到词频表，本探针测的不是它该测的东西"
    );
    assert!(
        prewarmed_ms <= 20,
        "预热后首次解码仍要 {prewarmed_ms} ms —— 预热没生效"
    );
}
