//! 注释词库加载耗时基准（**非断言测试**，`--ignored` 手动跑）。
//!
//! 量三条路径，回答「注释库大到什么程度会拖慢启动」：
//! 1. **冷启**：解析 yaml → 排序去重 → 写 `.wcmt`（只在源文件变更后发生一次）
//! 2. **热启**：指纹校验 + mmap 打开（每次启动的实际代价，与库大小基本无关）
//! 3. **查询**：二分，每页候选各查一次
//!
//! 跑法：
//! ```text
//! WIND_COMMENT_BENCH=/path/to/x.dict.yaml cargo test -p wind-reverse --test comment_load_bench -- --ignored --nocapture
//! ```

use std::time::Instant;

#[test]
#[ignore = "基准工具，需 WIND_COMMENT_BENCH 指定词库路径"]
fn bench_comment_dict_load() {
    let Ok(path) = std::env::var("WIND_COMMENT_BENCH") else {
        eprintln!("跳过：未设 WIND_COMMENT_BENCH");
        return;
    };
    let p = std::path::PathBuf::from(&path);
    let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    let cache = std::env::temp_dir().join(format!("wind_cmt_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);

    // 冷启：缓存目录为空，走解析 + 写盘
    let t0 = Instant::now();
    let mut rl = wind_reverse::ReverseLookup::default();
    rl.reload_comments(std::slice::from_ref(&p), Some(&cache));
    let cold = t0.elapsed();

    // 热启：缓存已在，走指纹校验 + mmap
    let t1 = Instant::now();
    let mut rl2 = wind_reverse::ReverseLookup::default();
    rl2.reload_comments(std::slice::from_ref(&p), Some(&cache));
    let warm = t1.elapsed();

    let wcmt: u64 = std::fs::read_dir(&cache)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".wcmt"))
                .filter_map(|e| e.metadata().ok().map(|m| m.len()))
                .sum()
        })
        .unwrap_or(0);

    // 查询耗时：命中与未命中各测一批（二分，应与条数呈对数关系）。
    let probes = ["的", "一个", "不存在的词xyz", "中国人"];
    let t2 = Instant::now();
    const ROUNDS: usize = 10_000;
    let mut hits = 0usize;
    for _ in 0..ROUNDS {
        for w in &probes {
            if !rl2.comment_of(w, None).is_empty() {
                hits += 1;
            }
        }
    }
    let q = t2.elapsed();

    println!(
        "\n注释库 {}\n  源文件 {:.1} MB → 缓存 {:.1} MB\n  冷启（解析+去重+排序+写 wcmt） {:?}\n  热启（指纹校验+mmap） {:?}\n  查询 {} 次 {:?}（{:.2} µs/次，命中 {}）\n",
        path,
        bytes as f64 / 1024.0 / 1024.0,
        wcmt as f64 / 1024.0 / 1024.0,
        cold,
        warm,
        ROUNDS * probes.len(),
        q,
        q.as_secs_f64() * 1e6 / (ROUNDS * probes.len()) as f64,
        hits,
    );
    drop(rl);
    drop(rl2);
    let _ = std::fs::remove_dir_all(&cache);
}
