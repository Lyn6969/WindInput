//! 真实词典端到端测试
//!
//! 加载仓库内的真实五笔/拼音词典，验证 CachedDict → DictWriter → DictReader (mmap)
//! 整条查询管道正确。这是 binformat entry_off 字节偏移 bug 的真实数据回归保护。
//!
//! 测试会在词典文件存在时运行；缺失时自动跳过（CI 无数据环境）。

use std::path::PathBuf;
use wind_dict::cached::CachedDict;

/// 仓库内 build_debug 数据目录（相对 crate manifest 向上两级）
fn data_schemas() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build_debug/data/schemas")
}

#[test]
fn test_real_wubi_candidates() {
    let path = data_schemas().join("wubi86/wubi86_jidian.dict.yaml");
    if !path.exists() {
        eprintln!("跳过：五笔词典不存在 {}", path.display());
        return;
    }

    // 清理可能存在的旧缓存，强制走 yaml→wdb→mmap 全路径
    let _ = std::fs::remove_file(path.with_extension("wdb"));

    let dict = CachedDict::load(&path).expect("加载五笔词典");
    assert!(dict.len() > 0, "五笔词典应非空");

    // 精确查找：a → 工/戈
    let a = dict.search("a");
    assert!(!a.is_empty(), "'a' 应有候选（mmap entry_off 回归）");
    assert!(a.iter().any(|(t, _, _)| t == "工"), "'a' 应包含 工");

    // 非首 key（验证 entry_off 字节偏移修复）
    let aaaa = dict.search("aaaa");
    assert!(!aaaa.is_empty(), "'aaaa' 应有候选");
    assert!(
        aaaa.iter().any(|(t, _, _)| t == "恭恭敬敬"),
        "'aaaa' 应包含 恭恭敬敬"
    );

    // 前缀查找
    let prefix = dict.search_prefix("aa", 20);
    assert!(!prefix.is_empty(), "'aa' 前缀应有候选");
}

#[test]
fn test_real_pinyin_candidates() {
    let path = data_schemas().join("pinyin/cn_dicts/base.dict.yaml");
    if !path.exists() {
        eprintln!("跳过：拼音词典不存在 {}", path.display());
        return;
    }

    let _ = std::fs::remove_file(path.with_extension("wdb"));

    let dict = CachedDict::load(&path).expect("加载拼音词典");
    assert!(dict.len() > 0, "拼音词典应非空");

    // 拼音 key 加载时去空格："a ba" → "aba"
    let aba = dict.search("aba");
    assert!(!aba.is_empty(), "'aba' 应有候选（去空格 key）");
    assert!(
        aba.iter().any(|(t, _, _)| t == "阿爸" || t == "阿巴"),
        "'aba' 应包含 阿爸/阿巴，实际: {:?}",
        aba.iter().map(|(t, _, _)| t.as_str()).collect::<Vec<_>>()
    );
}
