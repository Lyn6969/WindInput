//! 缓存有效性：基于源文件**内容指纹**而非 mtime。
//!
//! 痛点：词库源 mtime 会被 scp/部署/版本控制刷新，导致 mtime 校验恒失效 → 每次重建
//! (300MB、耗时)。改用内容指纹后，只要源**内容**未变即复用缓存。
//!
//! 用法（`tag` 标明「这份缓存是按什么方式解析出来的」，读写必须一致）：
//!   - 加载前：`cache_is_fresh(cache, sources, tag)` 为 true 则直接用缓存；
//!   - 构建后：`write_cache_fp(cache, sources, tag)` 写指纹 sidecar 供下次校验。
//!
//! 指纹用 std SipHash（DefaultHasher）：仅做变更检测，非加密用途，足够且无额外依赖。

use std::hash::Hasher;
use std::path::{Path, PathBuf};

/// 指纹 sidecar 路径：`<cache>.fp`（紧贴缓存文件，随缓存一起增删）。
fn fp_sidecar(cache: &Path) -> PathBuf {
    let mut s = cache.as_os_str().to_os_string();
    s.push(".fp");
    PathBuf::from(s)
}

/// **解析语义版本**：改动会影响「同样的源文件解析出什么结果」时必须 +1。
///
/// 指纹原本只覆盖源数据，于是缓存回答的是「源文件变了吗」，而真正该回答的是
/// 「这份缓存和当前程序会产出的结果一致吗」。二者在解析器被修复时会分叉：源文件没变
/// → 指纹不变 → 复用旧缓存 → **解析器修复对存量用户静默失效**，且表现为「明明改了却
/// 没生效」这种最难排查的样子。把语义版本混进指纹，修复即自动重建。
///
/// 历史：
/// - 1 = 初始（列序逐行按 ASCII 猜）
/// - 2 = 列序改为文件级判定：读头部 `columns:` 声明，无声明则整文件投票探测列序、
///   权重仍按 librime 默认取第 3 列（纯 ASCII 词条如 `@`、`$CC(...)` 不再被误判成编码列）
/// - 3 = 只剥行尾空白（前导 U+3000 等不再被当缩进削掉）、空 text/code 跳过、
///   音节语义补上「code 列含空格」这条正面证据（编码在前的拼音库不再丢简拼与边界）、
///   `columns:` 支持流式写法且残缺声明改为整库跳过
/// - 4 = 词条文本的反转义对**命令栏语法条目**只还原换行/制表，反斜杠原样穿过
///   （`$CC(..., open("D:\\notes"))` 不再被本层与 cmdbar lexer 各吃一个反斜杠）
const PARSE_SEMANTICS_VERSION: u32 = 4;

/// 计算源文件集合的内容指纹：混入解析语义版本 + 调用方 tag + 每个源的 文件名/长度/内容。
///
/// `tag` 用于区分「同一份源文件、但解析方式不同」的缓存。**没有它就会出现这种静默错误**：
/// 把某词库的 `dict_type` 在 english ↔ 非 english 之间切换，只改变 `lowercase_code`
/// 而 `.yaml` 字节不变 → 指纹命中 → 永久复用大小写错误的缓存。
/// 同理，不同种类的缓存（词库 / 注释库）也应各自持 tag，免得共用一个语义版本号
/// 却各改各的、谁也没动机去 +1。
///
/// 任一源不可读 → None（视为需重建）。
fn fingerprint(sources: &[&Path], tag: &str) -> Option<String> {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write_u32(PARSE_SEMANTICS_VERSION);
    h.write(tag.as_bytes());
    h.write_u8(0xfe); // tag 与源内容之间的分隔
    for p in sources {
        let data = std::fs::read(p).ok()?;
        if let Some(name) = p.file_name() {
            h.write(name.to_string_lossy().as_bytes());
        }
        h.write_u64(data.len() as u64);
        h.write(&data);
        h.write_u8(0xff); // 分隔，避免相邻源内容拼接歧义
    }
    Some(format!("{:016x}", h.finish()))
}

/// 缓存是否可复用：缓存文件存在 且 指纹 sidecar 与当前源内容+tag 一致。
/// `tag` 见 [`fingerprint`]，必须与写入时一致。
pub fn cache_is_fresh(cache: &Path, sources: &[&Path], tag: &str) -> bool {
    if !cache.exists() {
        return false;
    }
    let Some(fp) = fingerprint(sources, tag) else {
        return false;
    };
    matches!(std::fs::read_to_string(fp_sidecar(cache)), Ok(s) if s.trim() == fp)
}

/// 缓存构建成功后调用：写入指纹 sidecar（best-effort，失败仅影响下次会多重建一次）。
pub fn write_cache_fp(cache: &Path, sources: &[&Path], tag: &str) {
    if let Some(fp) = fingerprint(sources, tag) {
        let _ = std::fs::write(fp_sidecar(cache), fp);
    }
}

/// 词库缓存的 tag：区分 code 列是否被小写化（`dict_type = english` 走小写）。
pub fn dict_tag(lowercase_code: bool) -> &'static str {
    if lowercase_code {
        "dict/lowercase"
    } else {
        "dict/raw"
    }
}

/// 注释库缓存的 tag。同理独立于词库解析：注释库用的是 `wind-reverse` 里那份精简解析器
/// （只取 text/comment/code 三列），与 `codetable` 的 rime 解析各自演进。
pub const COMMENT_TAG: &str = "comment/v1";

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, content: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("wind_fp_test_{name}"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content).unwrap();
        // 清掉上轮可能残留的指纹 sidecar，确保固定 temp 路径下测试可重复（否则
        // 上次 write_cache_fp 写的 `<p>.fp` 会让本轮「未写指纹应不新鲜」误判为新鲜）。
        let mut side = p.clone().into_os_string();
        side.push(".fp");
        let _ = std::fs::remove_file(side);
        p
    }

    /// 语义版本参与指纹：源内容不变、但解析语义版本变了，缓存必须失效。
    /// （否则解析器修复对存量用户静默不生效——本项目真实踩过。）
    #[test]
    fn parse_semantics_version_participates_in_fingerprint() {
        let src = tmp("semver_src.txt", b"same content");
        let fp_now = fingerprint(&[&src], "t").unwrap();
        // 复算一份「仅版本号不同」的指纹：与 fingerprint() 严格同构（含 tag 部分），
        // 只把版本 +1——否则差异可能来自别处，测试就名不副实了。
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write_u32(PARSE_SEMANTICS_VERSION + 1);
        h.write(b"t");
        h.write_u8(0xfe);
        let data = std::fs::read(&src).unwrap();
        h.write(src.file_name().unwrap().to_string_lossy().as_bytes());
        h.write_u64(data.len() as u64);
        h.write(&data);
        h.write_u8(0xff);
        let fp_other = format!("{:016x}", h.finish());
        assert_ne!(
            fp_now, fp_other,
            "同样的源内容，语义版本不同必须得到不同指纹"
        );
    }

    /// tag 参与指纹：同一份源、不同解析方式（如 english 词库的 code 小写化）
    /// 必须落到不同指纹，否则切换 dict_type 会永久复用大小写错误的缓存。
    #[test]
    fn tag_participates_in_fingerprint() {
        let src = tmp("tag_src.txt", b"same content");
        let raw = fingerprint(&[&src], dict_tag(false)).unwrap();
        let lower = fingerprint(&[&src], dict_tag(true)).unwrap();
        assert_ne!(raw, lower, "lowercase 与否必须得到不同指纹");
        assert_ne!(
            raw,
            fingerprint(&[&src], COMMENT_TAG).unwrap(),
            "不同种类缓存必须得到不同指纹"
        );

        // 端到端：用 raw tag 写的指纹，不该被 lowercase tag 判为新鲜
        let cache = tmp("tag_src.cache", b"<built>");
        write_cache_fp(&cache, &[&src], dict_tag(false));
        assert!(cache_is_fresh(&cache, &[&src], dict_tag(false)));
        assert!(
            !cache_is_fresh(&cache, &[&src], dict_tag(true)),
            "tag 不一致时必须判定为不新鲜"
        );
    }

    #[test]
    fn fresh_only_when_content_matches() {
        let src = tmp("src.txt", b"hello dict");
        let cache = tmp("src.cache", b"<built>");
        // 未写指纹 → 不新鲜
        assert!(!cache_is_fresh(&cache, &[&src], "t"));
        // 写指纹后 → 新鲜
        write_cache_fp(&cache, &[&src], "t");
        assert!(cache_is_fresh(&cache, &[&src], "t"));
    }

    #[test]
    fn mtime_change_keeps_fresh_content_change_invalidates() {
        let src = tmp("src2.txt", b"content A");
        let cache = tmp("src2.cache", b"<built>");
        write_cache_fp(&cache, &[&src], "t");
        // 仅改 mtime（重写相同内容）→ 仍新鲜（这正是修复点）
        std::fs::write(&src, b"content A").unwrap();
        assert!(cache_is_fresh(&cache, &[&src], "t"));
        // 改内容 → 失效
        std::fs::write(&src, b"content B").unwrap();
        assert!(!cache_is_fresh(&cache, &[&src], "t"));
    }

    #[test]
    fn missing_cache_not_fresh() {
        let src = tmp("src3.txt", b"x");
        let cache = std::env::temp_dir().join("wind_fp_test_nope.cache");
        let _ = std::fs::remove_file(&cache);
        assert!(!cache_is_fresh(&cache, &[&src], "t"));
    }
}
