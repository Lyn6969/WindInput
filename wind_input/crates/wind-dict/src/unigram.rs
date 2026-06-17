//! unigram.wdb：mmap 零拷贝的 unigram 语言模型
//!
//! 与 Go 版本 `wind_input/internal/dict/binformat`（WUNI 格式）对齐。
//! 词频模型很大（数十万条），全加载进 HashMap 占 ~40MB 常驻内存；
//! 改用 mmap + 二分查找后几乎不占常驻内存（页按需载入）。
//!
//! 文件布局：
//! - Header (24B)：magic "WUNI" + version + key_count + index_off + str_off + min_prob(f32 bits)
//! - KeyIndex[key_count] (12B 每条，按 key 字典序排序)：key_off u32 + key_len u16 + log_prob f32 + reserved u16
//! - StringPool：所有 key 的 UTF-8 字节

use memmap2::Mmap;
use std::fs::File;
use std::path::Path;
use tracing::info;

const MAGIC: [u8; 4] = [b'W', b'U', b'N', b'I'];
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 24;
const KEY_INDEX_SIZE: usize = 12;

/// unigram mmap 读取器
pub struct UnigramReader {
    mmap: Mmap,
    key_count: u32,
    index_off: u32,
    str_off: u32,
    min_prob: f32,
}

impl UnigramReader {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_SIZE {
            anyhow::bail!("unigram.wdb too short");
        }
        if mmap[0..4] != MAGIC {
            anyhow::bail!("invalid unigram magic");
        }
        let version = u32::from_le_bytes(mmap[4..8].try_into().unwrap());
        if version != VERSION {
            anyhow::bail!("unsupported unigram version: {}", version);
        }
        let key_count = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
        let index_off = u32::from_le_bytes(mmap[12..16].try_into().unwrap());
        let str_off = u32::from_le_bytes(mmap[16..20].try_into().unwrap());
        let min_prob = f32::from_le_bytes(mmap[20..24].try_into().unwrap());

        let index_end = index_off as usize + key_count as usize * KEY_INDEX_SIZE;
        if index_end > mmap.len() || str_off as usize > mmap.len() {
            anyhow::bail!("unigram.wdb offsets out of range");
        }
        info!(
            "Opened unigram.wdb: {} ({} keys)",
            path.display(),
            key_count
        );
        Ok(Self {
            mmap,
            key_count,
            index_off,
            str_off,
            min_prob,
        })
    }

    pub fn key_count(&self) -> u32 {
        self.key_count
    }

    pub fn min_prob(&self) -> f32 {
        self.min_prob
    }

    fn read_key(&self, i: u32) -> Option<(&str, f32)> {
        let off = self.index_off as usize + i as usize * KEY_INDEX_SIZE;
        if off + KEY_INDEX_SIZE > self.mmap.len() {
            return None;
        }
        let key_off = u32::from_le_bytes(self.mmap[off..off + 4].try_into().ok()?);
        let key_len = u16::from_le_bytes(self.mmap[off + 4..off + 6].try_into().ok()?);
        let log_prob = f32::from_le_bytes(self.mmap[off + 6..off + 10].try_into().ok()?);
        let start = self.str_off as usize + key_off as usize;
        let end = start + key_len as usize;
        if end > self.mmap.len() {
            return None;
        }
        let s = std::str::from_utf8(&self.mmap[start..end]).ok()?;
        Some((s, log_prob))
    }

    /// 二分查找 key 的 log_prob
    pub fn lookup(&self, key: &str) -> Option<f32> {
        let mut lo = 0u32;
        let mut hi = self.key_count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (mid_key, lp) = self.read_key(mid)?;
            match mid_key.cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(lp),
            }
        }
        None
    }

    pub fn contains(&self, key: &str) -> bool {
        self.lookup(key).is_some()
    }
}

/// 从 (词, 频次) 列表构建 unigram.wdb。
/// log_prob = ln(freq/total)；OOV min_prob = ln(0.5/total)。
pub fn write_unigram_wdb(path: impl AsRef<Path>, freqs: &[(String, f64)]) -> anyhow::Result<()> {
    use std::io::Write;

    let total: f64 = freqs.iter().map(|(_, f)| *f).sum();
    if total <= 0.0 {
        anyhow::bail!("unigram total freq is zero");
    }
    let min_prob = (0.5 / total).ln() as f32;

    // 排序 + 计算 logprob
    let mut entries: Vec<(&str, f32)> = freqs
        .iter()
        .filter(|(_, f)| *f > 0.0)
        .map(|(w, f)| (w.as_str(), (f / total).ln() as f32))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries.dedup_by(|a, b| a.0 == b.0);

    // 字符串池
    let mut pool: Vec<u8> = Vec::new();
    let mut index = Vec::with_capacity(entries.len() * KEY_INDEX_SIZE);
    for (key, lp) in &entries {
        let key_off = pool.len() as u32;
        pool.extend_from_slice(key.as_bytes());
        let key_len = key.len() as u16;
        index.extend_from_slice(&key_off.to_le_bytes());
        index.extend_from_slice(&key_len.to_le_bytes());
        index.extend_from_slice(&lp.to_le_bytes());
        index.extend_from_slice(&0u16.to_le_bytes()); // reserved
    }

    let index_off = HEADER_SIZE as u32;
    let str_off = index_off + index.len() as u32;

    let tmp = path.as_ref().with_extension("wdb.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&MAGIC)?;
        f.write_all(&VERSION.to_le_bytes())?;
        f.write_all(&(entries.len() as u32).to_le_bytes())?;
        f.write_all(&index_off.to_le_bytes())?;
        f.write_all(&str_off.to_le_bytes())?;
        f.write_all(&min_prob.to_le_bytes())?;
        f.write_all(&index)?;
        f.write_all(&pool)?;
    }
    std::fs::rename(&tmp, path.as_ref())?;
    info!("Wrote unigram.wdb: {} keys", entries.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unigram_wdb_roundtrip() {
        let tmp = std::env::temp_dir().join("wind_unigram_mmap_test.wdb");
        let freqs = vec![
            ("的".to_string(), 100.0),
            ("中国".to_string(), 40.0),
            ("爱".to_string(), 10.0),
        ];
        write_unigram_wdb(&tmp, &freqs).unwrap();

        let r = UnigramReader::open(&tmp).unwrap();
        assert_eq!(r.key_count(), 3);
        let de = r.lookup("的").unwrap();
        let zg = r.lookup("中国").unwrap();
        let ai = r.lookup("爱").unwrap();
        assert!(de > zg && zg > ai, "高频词 log_prob 更大");
        assert!(r.lookup("龘").is_none());
        assert!(r.min_prob() < ai, "OOV min_prob 应低于最低频词");
        let _ = std::fs::remove_file(&tmp);
    }
}
