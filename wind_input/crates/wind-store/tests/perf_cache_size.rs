//! redb 页缓存上限的标定：缩小它到底损失多少。
//!
//! redb 的默认上限是 **1 GiB**，对常驻输入法等于没有上限。但「该压下来」不等于
//! 「压到多少都行」——本文件量化代价，把 `DEFAULT_CACHE_SIZE_BYTES` 的取值从拍脑袋
//! 变成有数据支撑。
//!
//! ## 为什么代价可能比直觉小
//!
//! redb 2.x **不是 mmap**，而是自己在普通文件 IO 之上维护 `PagedCachedFile`。未命中
//! 走 `read` 系统调用，绝大多数由**操作系统页缓存**供数据——µs 级 syscall + memcpy，
//! 不是磁盘寻道。redb 的缓存下面还垫着一层 OS 页缓存，所以缩小它损失的是 syscall
//! 开销而非 IO 开销。这与「缩小数据库 buffer pool」的直觉不是一回事。
//!
//! ## 测哪几条路
//!
//! 覆盖四种真实访问形态，因为它们的局部性差别很大：
//!
//! | 形态 | 何时发生 | 特点 |
//! |---|---|---|
//! | 批量导入 | 用户导入大词库 | **写**密集；`set_cache_size` 的 10% 划给写缓存 |
//! | 简拼召回 | 每次按键（逐切点十几遍） | 索引小范围扫 + 主表**散点**回查 |
//! | 前缀补全 | 每次按键 | 主表小范围顺序扫 |
//! | 词库管理全量列举 | 打开设置页词库标签 | 主表**全表**顺序扫，最吃缓存 |
//!
//! ⚠️ 首版只测了读就想下结论。`set_cache_size` 同时决定写缓存（10%），而 19 万词
//! 批量导入正是真实工作流里最写密集的一步——只测读等于漏掉了唯一可能被压小的那一半。
//!
//! 手动跑：
//! ```text
//! cargo test --release -p wind-store --test perf_cache_size -- --ignored --nocapture
//! ```

use std::time::Instant;
use wind_store::{Store, wdict::WordIo};

const N: usize = 190_000;

/// 与 `perf_abbrev_index.rs` 同款夹具：2~4 音节轮换，声母串长度分布贴近真实词库。
fn rows() -> Vec<WordIo> {
    let l = b"abcdefghijklmnopqrstuvwxyz";
    (0..N)
        .map(|i| {
            let syl = 2 + (i % 3);
            let mut x = i;
            let mut segs: Vec<String> = Vec::with_capacity(syl);
            for _ in 0..syl {
                segs.push(format!("{}i", l[x % 26] as char));
                x /= 26;
            }
            WordIo {
                code: segs.join(" "),
                text: format!("词{i}"),
                weight: 100,
                count: 0,
            }
        })
        .collect()
}

fn ms(f: impl FnOnce()) -> f64 {
    let t = Instant::now();
    f();
    t.elapsed().as_secs_f64() * 1000.0
}

/// 简拼召回：索引小范围扫 + 主表散点回查。散点是缓存最不友好的形态。
fn abbrev_ms(s: &Store) -> f64 {
    let keys = ["ab", "cd", "xy", "mn", "pq"];
    ms(|| {
        for k in keys {
            std::hint::black_box(s.search_user_words_by_abbrev("py", k, 0).unwrap().len());
        }
    }) / keys.len() as f64
}

/// 前缀补全：主表小范围顺序扫。
fn prefix_ms(s: &Store) -> f64 {
    let keys = ["ai", "bi", "ci", "di", "ei"];
    ms(|| {
        for k in keys {
            std::hint::black_box(s.search_user_words_prefix("py", k, 30).unwrap().len());
        }
    }) / keys.len() as f64
}

/// 设置页列举：主表全表顺序扫，最吃缓存的那条路。
fn list_all_ms(s: &Store) -> f64 {
    ms(|| {
        std::hint::black_box(s.search_user_words_prefix("py", "", 0).unwrap().len());
    })
}

/// 测一档：返回 (导入 s, 简拼 ms, 前缀 ms, 列举 ms, 库大小 MB)。
///
/// **每档一个全新库**：导入必须在空库上测，否则第二档起走的是「已存在→unchanged」
/// 分支，一条都不落盘，测出来的是个假数。
fn measure(mib: usize, data: &[WordIo], tag: &str) -> (f64, f64, f64, f64, f64) {
    let p = std::env::temp_dir().join(format!("wind_perf_cache_{tag}_{mib}.redb"));
    let _ = std::fs::remove_file(&p);
    let s = Store::open_with_cache_size(&p, mib * 1024 * 1024).unwrap();

    let imp = ms(|| {
        std::hint::black_box(s.import_user_words("py", data).unwrap());
    });

    // 预热：让本档缓存按各自上限填充到稳态，否则测的是冷启动的一次性成本
    for _ in 0..3 {
        let _ = abbrev_ms(&s);
        let _ = prefix_ms(&s);
    }
    let a: f64 = (0..5).map(|_| abbrev_ms(&s)).sum::<f64>() / 5.0;
    let pf: f64 = (0..5).map(|_| prefix_ms(&s)).sum::<f64>() / 5.0;
    let l: f64 = (0..3).map(|_| list_all_ms(&s)).sum::<f64>() / 3.0;
    let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) as f64 / 1e6;

    drop(s);
    let _ = std::fs::remove_file(&p);
    (imp / 1000.0, a, pf, l, size)
}

#[test]
#[ignore = "每档都要重建 19 万条，手动跑：-- --ignored --nocapture"]
fn calibrate_cache_size() {
    let data = rows();
    let sizes = [1024usize, 256, 128, 64, 32, 16, 8, 2];

    // **正序 + 倒序各跑一遍**：磁盘状态/文件系统缓存会随测试推进而漂移，单向扫一遍
    // 分不清「断崖」和「越跑越慢」。两个方向都在同一处出现断崖，才算真的。
    for (tag, order) in [
        ("fwd", sizes.to_vec()),
        ("rev", {
            let mut v = sizes.to_vec();
            v.reverse();
            v
        }),
    ] {
        println!(
            "\n=== {} ===\n{:>9} | {:>13} | {:>12} | {:>12} | {:>12}",
            if tag == "fwd" {
                "正序（大→小）"
            } else {
                "倒序（小→大）"
            },
            "缓存",
            "批量导入",
            "简拼召回",
            "前缀补全",
            "全量列举"
        );
        println!(
            "{:->9}-+-{:->13}-+-{:->12}-+-{:->12}-+-{:->12}",
            "", "", "", "", ""
        );
        for mib in order {
            let (imp, a, pf, l, size) = measure(mib, &data, tag);
            println!(
                "{mib:>5} MiB | {imp:>10.2} s | {a:>9.4} ms | {pf:>9.4} ms | {l:>9.2} ms   (库 {size:.1} MB)"
            );
        }
    }
}
