//! 反查索引（`.wridx`）规模基准（**非断言测试**，`--ignored` 手动跑）。
//!
//! 回答三个问题，它们共同决定「落盘 + mmap」这条路划不划算：
//! 1. **索引体积** vs 旧的常驻堆量级——磁盘若比内存还大，方案就不成立
//!    （text 为键的 `.wdat` 方案正是这么被否掉的：264 MB 磁盘换 95 MB 内存）
//! 2. **冷启**：遍历全部词库 → 排序去重 → 序列化 → 写盘
//! 3. **热启**：打开已有 `.wridx`（每次启动的实际代价）+ 常驻字节数
//!
//! 跑法（目录里放该方案的全部 `.wdat`）：
//! ```text
//! WIND_REVERSE_BENCH='C:\Users\me\AppData\Local\WindInputDev\cache\feihuzj2' \
//!   cargo test -p wind-dict --test reverse_index_bench -- --ignored --nocapture
//! ```

use std::time::Instant;
use wind_dict::cached::CachedDict;
use wind_dict::reverseidx::ReverseIndex;

#[test]
#[ignore = "基准工具，需 WIND_REVERSE_BENCH 指定含 .wdat 的目录"]
fn bench_reverse_index_build_and_open() {
    let Ok(dir) = std::env::var("WIND_REVERSE_BENCH") else {
        eprintln!("跳过：未设 WIND_REVERSE_BENCH");
        return;
    };
    let dir = std::path::PathBuf::from(&dir);
    let mut wdats: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("目录读不到")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "wdat"))
        .collect();
    wdats.sort();
    assert!(!wdats.is_empty(), "{} 下没有 .wdat", dir.display());

    let t0 = Instant::now();
    let dicts: Vec<CachedDict> = wdats
        .iter()
        .filter_map(|p| {
            wind_dict::reader_pool::open_wdat(p)
                .ok()
                .map(CachedDict::Mmap)
        })
        .collect();
    let open_dicts = t0.elapsed();
    let entries: usize = dicts.iter().map(|d| d.len()).sum();
    let wdat_bytes: u64 = wdats
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
        .sum();

    // 冷启：遍历 + 排序去重 + 序列化
    let t1 = Instant::now();
    let image = wind_dict::cached::serialize_reverse_index_from(&dicts);
    let build = t1.elapsed();

    let out = std::env::temp_dir().join(format!("wind_wridx_bench_{}.wridx", std::process::id()));
    let t2 = Instant::now();
    wind_dict::reverseidx::write_wridx(&out, &image).unwrap();
    let write = t2.elapsed();

    // 热启：两种打开方式各测一遍（阈值给足=常驻，给 0=mmap）
    let t3 = Instant::now();
    let mapped = ReverseIndex::open(&out, 0).unwrap();
    let open_mapped = t3.elapsed();
    let t4 = Instant::now();
    let resident = ReverseIndex::open(&out, usize::MAX).unwrap();
    let open_resident = t4.elapsed();

    // 查询：命中与未命中各一批（二分，应与词数呈对数关系）
    let probes = ["的", "一个", "不存在的词xyz", "中国人"];
    const ROUNDS: usize = 10_000;
    let mut q = Vec::new();
    for (name, idx) in [("mmap", &mapped), ("常驻", &resident)] {
        let t = Instant::now();
        let mut hits = 0usize;
        for _ in 0..ROUNDS {
            for w in &probes {
                if idx.codes_of(w).is_some() {
                    hits += 1;
                }
            }
        }
        q.push((name, t.elapsed(), hits));
    }
    // 前缀扫描（词语联想的取数口，比点查贵得多）
    let t5 = Instant::now();
    let assoc = mapped.texts_with_prefix("中", 9).len();
    let prefix_mapped = t5.elapsed();
    let t6 = Instant::now();
    let _ = resident.texts_with_prefix("中", 9);
    let prefix_resident = t6.elapsed();

    println!(
        "\n反查索引基准 {}\n\
         \x20 词库 {} 个 / {} 条 / wdat {:.1} MB\n\
         \x20 索引 {} 词 → {:.1} MB（相对 wdat {:.0}%）\n\
         \x20 打开词库 {open_dicts:?} | 构建 {build:?} | 写盘 {write:?}\n\
         \x20 重开 mmap {open_mapped:?}（常驻 {:.1} MB）| 重开常驻 {open_resident:?}（常驻 {:.1} MB）\n",
        dir.display(),
        dicts.len(),
        entries,
        wdat_bytes as f64 / 1024.0 / 1024.0,
        mapped.len(),
        mapped.data_bytes() as f64 / 1024.0 / 1024.0,
        mapped.data_bytes() as f64 * 100.0 / wdat_bytes.max(1) as f64,
        mapped.resident_bytes() as f64 / 1024.0 / 1024.0,
        resident.resident_bytes() as f64 / 1024.0 / 1024.0,
    );
    for (name, dur, hits) in q {
        println!(
            "\x20 点查 {name}：{} 次 {dur:?}（{:.2} µs/次，命中 {hits}）",
            ROUNDS * probes.len(),
            dur.as_secs_f64() * 1e6 / (ROUNDS * probes.len()) as f64,
        );
    }
    println!(
        "\x20 前缀扫描「中」→ {assoc} 条：mmap {prefix_mapped:?} / 常驻 {prefix_resident:?}\n"
    );

    drop((mapped, resident));
    let _ = std::fs::remove_file(&out);
}
