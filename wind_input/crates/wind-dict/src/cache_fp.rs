//! 缓存有效性：基于源文件**内容指纹**而非 mtime。
//!
//! 痛点：词库源 mtime 会被 scp/部署/版本控制刷新，导致 mtime 校验恒失效 → 每次重建
//! (300MB、耗时)。改用内容指纹后，只要源**内容**未变即复用缓存。
//!
//! 用法：
//!   - 加载前：`cache_is_fresh(cache, sources)` 为 true 则直接用缓存；
//!   - 构建后：`write_cache_fp(cache, sources)` 写指纹 sidecar 供下次校验。
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

/// 计算源文件集合的内容指纹：对每个源混入 文件名 + 长度 + 全部内容。
/// 任一源不可读 → None（视为需重建）。
fn fingerprint(sources: &[&Path]) -> Option<String> {
    let mut h = std::collections::hash_map::DefaultHasher::new();
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

/// 缓存是否可复用：缓存文件存在 且 指纹 sidecar 与当前源内容一致。
pub fn cache_is_fresh(cache: &Path, sources: &[&Path]) -> bool {
    if !cache.exists() {
        return false;
    }
    let Some(fp) = fingerprint(sources) else {
        return false;
    };
    matches!(std::fs::read_to_string(fp_sidecar(cache)), Ok(s) if s.trim() == fp)
}

/// 缓存构建成功后调用：写入指纹 sidecar（best-effort，失败仅影响下次会多重建一次）。
pub fn write_cache_fp(cache: &Path, sources: &[&Path]) {
    if let Some(fp) = fingerprint(sources) {
        let _ = std::fs::write(fp_sidecar(cache), fp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, content: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("wind_fp_test_{name}"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content).unwrap();
        p
    }

    #[test]
    fn fresh_only_when_content_matches() {
        let src = tmp("src.txt", b"hello dict");
        let cache = tmp("src.cache", b"<built>");
        // 未写指纹 → 不新鲜
        assert!(!cache_is_fresh(&cache, &[&src]));
        // 写指纹后 → 新鲜
        write_cache_fp(&cache, &[&src]);
        assert!(cache_is_fresh(&cache, &[&src]));
    }

    #[test]
    fn mtime_change_keeps_fresh_content_change_invalidates() {
        let src = tmp("src2.txt", b"content A");
        let cache = tmp("src2.cache", b"<built>");
        write_cache_fp(&cache, &[&src]);
        // 仅改 mtime（重写相同内容）→ 仍新鲜（这正是修复点）
        std::fs::write(&src, b"content A").unwrap();
        assert!(cache_is_fresh(&cache, &[&src]));
        // 改内容 → 失效
        std::fs::write(&src, b"content B").unwrap();
        assert!(!cache_is_fresh(&cache, &[&src]));
    }

    #[test]
    fn missing_cache_not_fresh() {
        let src = tmp("src3.txt", b"x");
        let cache = std::env::temp_dir().join("wind_fp_test_nope.cache");
        let _ = std::fs::remove_file(&cache);
        assert!(!cache_is_fresh(&cache, &[&src]));
    }
}
