//! 简拼索引 vs 全表枚举的量化对照（19 万词规模）。
//!
//! 立项现场：导入 19 万词后拼音明显卡顿、内存占用也很高，而**全拼输入完全不卡**。
//! 根因是简拼族召回按键枚举全部用户词，且前缀回退那条是逐切点循环、一次按键扫十几遍。
//!
//! ## 判据是什么
//!
//! **不是**「索引查询与词库规模无关」——那句话不成立，也不该断言。索引查询的代价
//! 正比于**共享同一声母串的词数**（组占用），而组占用确实会随词库增长，只是增长得
//! 远比词库慢：词有 2~4 个音节，声母串因此散布在 26² / 26³ / 26⁴ 个组上。
//!
//! 真正的判据是两条：
//! 1. 全表枚举随词库**线性**增长，索引查询不是 ⇒ 倍数差随规模拉开；
//! 2. 19 万词下**一次按键的实际开销**落在可感知阈值以下。
//!
//! 首版这里断言了「与规模无关」并且失败了——因为夹具把声母串固定成两字母，
//! 676 个组装 19 万词，组占用被迫随词库线性增长。那是夹具的性质，不是实现的。
//!
//! `#[ignore]`：19 万条记录要跑一会儿，不进常规 CI。手动跑：
//! ```text
//! cargo test --release -p wind-store --test perf_abbrev_index -- --ignored --nocapture
//! ```

use std::time::Instant;
use wind_store::{Store, wdict::WordIo};

/// step 6.2 的前缀回退是**逐切点循环**：一次按键最多走 `MAX_FALLBACK_CUTS` 个切点，
/// 每个切点还要查纯简拼键与若干混合模式投影键。用 16 作估算系数。
const LOOKUPS_PER_KEYSTROKE: f64 = 16.0;

/// 造 n 条用户词，**声母串长度分布贴近真实词库**（2~4 音节轮换）。
///
/// 每段形如 `{声母}i`，故段起始位是 0/2/4…，boundary 直接算得。
/// 走 `import_user_words` 单写事务批量导入——逐条 `add_user_word` 每条一个事务，
/// 19 万条要三分多钟。
fn seed(s: &Store, n: usize) {
    let l = b"abcdefghijklmnopqrstuvwxyz";
    let rows: Vec<WordIo> = (0..n)
        .map(|i| {
            let syl = 2 + (i % 3); // 2/3/4 音节轮换
            let mut x = i;
            let mut segs: Vec<String> = Vec::with_capacity(syl);
            for _ in 0..syl {
                segs.push(format!("{}i", l[x % 26] as char));
                x /= 26;
            }
            WordIo {
                // 带空格的音节码 → 导入侧拆出扁平码 + 真值边界
                code: segs.join(" "),
                text: format!("词{i}"),
                weight: 100,
                count: 0,
            }
        })
        .collect();
    s.import_user_words("py", &rows).unwrap();
}

/// 旧路径的对照组：枚举全部用户词。`search_user_words_prefix(schema, "", 0)`
/// 正是引擎侧 `search_prefix("", 0)` 落到存储层的样子。
fn full_scan_ms(s: &Store) -> f64 {
    let t = Instant::now();
    let v = s.search_user_words_prefix("py", "", 0).unwrap();
    std::hint::black_box(v.len());
    t.elapsed().as_secs_f64() * 1000.0
}

/// 返回 (平均耗时 ms, 命中条数)。取两字母键——**最拥挤的那一档**，即最坏情况。
fn index_lookup(s: &Store, keys: &[&str]) -> (f64, usize) {
    let mut hits = 0;
    let t = Instant::now();
    for k in keys {
        let v = s.search_user_words_by_abbrev("py", k, 0).unwrap();
        hits += v.len();
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0 / keys.len() as f64;
    (ms, hits / keys.len())
}

#[test]
#[ignore = "19 万条记录耗时较长，手动跑：-- --ignored --nocapture"]
fn index_beats_full_scan_and_the_gap_widens_with_size() {
    // 两字母键 = 最拥挤的一档（26² 个组），故这是索引侧的最坏情况。
    let dense = ["ab", "cd", "xy"];
    // 四字母键 = 最稀疏的一档（26⁴ 个组），用来展示代价确实跟着组占用走。
    let sparse = ["abcd", "efgh", "mnop"];

    let mut prev_ratio = 0.0f64;
    for n in [1_000usize, 10_000, 190_000] {
        let p = std::env::temp_dir().join(format!("wind_perf_abbrev_{n}.redb"));
        let _ = std::fs::remove_file(&p);
        let s = Store::open(&p).unwrap();

        let t = Instant::now();
        seed(&s, n);
        let build = t.elapsed().as_secs_f64();

        let _ = index_lookup(&s, &dense); // 预热，不把首次页缓存填充算进来
        let (d_ms, d_hits) = index_lookup(&s, &dense);
        let (s_ms, s_hits) = index_lookup(&s, &sparse);
        let scan = full_scan_ms(&s);
        let ratio = scan / d_ms.max(f64::EPSILON);

        println!(
            "n={n:>7} 建库{build:>5.1}s | 密集键 {d_ms:>7.4}ms({d_hits:>4}条) \
             稀疏键 {s_ms:>7.4}ms({s_hits}条) | 全表 {scan:>7.2}ms | 倍数 {ratio:>5.0}x \
             | 每次按键约 {:>6.2}ms",
            d_ms * LOOKUPS_PER_KEYSTROKE
        );

        assert!(
            s_ms <= d_ms,
            "稀疏键不该比密集键慢——代价应跟着组占用走（稀疏 {s_ms:.4}ms > 密集 {d_ms:.4}ms）"
        );

        if n == 190_000 {
            // 真正要回答的问题：19 万词下一次按键还卡不卡。
            let per_keystroke = d_ms * LOOKUPS_PER_KEYSTROKE;
            assert!(
                per_keystroke < 10.0,
                "19 万词下一次按键的简拼召回应远低于可感知阈值，实测 {per_keystroke:.2}ms"
            );
            assert!(ratio > 50.0, "对旧路径应有量级优势，实测仅 {ratio:.0}x");
        }

        // 全表枚举是线性的、索引不是 ⇒ 倍数差必须随规模拉开。
        assert!(
            ratio > prev_ratio,
            "规模变大而倍数没拉开（{prev_ratio:.0}x → {ratio:.0}x），说明索引侧也在线性增长"
        );
        prev_ratio = ratio;

        drop(s);
        let _ = std::fs::remove_file(&p);
    }
}
