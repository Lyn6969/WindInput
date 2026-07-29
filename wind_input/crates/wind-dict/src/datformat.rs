//! DAT 格式 (wdat) — Double-Array Trie 词典
//!
//! 与 Go 版 `wind_input/internal/dict/datformat/` 对齐（主 DAT；暂不含简拼 AbbrevSection，
//! 与现 wdb 内容对等——全拼/全码）。前缀查询为「数组跳转 + 子树 DFS」，较 wdb 的键索引
//! 二分更省更快，适合拼音大词库逐键前缀检索。
//!
//! 文件布局（小端）：
//! ```text
//! [Header 48B]
//! [DAT Base: dat_size*4][DAT Check: dat_size*4]
//! [LeafTable: leaf_count*8]   每条 {entry_off u32, entry_len u16, _ u16}
//! [EntryRecords: entry_count*22]  每条 {text_off u32, text_len u16, weight i32, order u32, boundary u64}
//! [StringPool]
//! [CharMap 1028B]  {max_code i32, char_map[256] i32}
//! [Meta(可选) 4B len + bytes]
//! ```
//! DAT 查询：`base[s]+c=t`（状态 s 经紧凑码 c 转移到 t），`check[t]==s` 校验；
//! `base[t]<0` 表叶节点，`-base[t]-1` 为 LeafTable 索引；`c=0` 为终止符。

use crate::binformat::DictEntry;
use memmap2::Mmap;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::Path;
use tracing::info;

const MAGIC: [u8; 4] = [b'W', b'D', b'A', b'T'];
// v3：EntryRecord 增加 order u32（全局自然序，10→14B），跨编码等权时按词库出现顺序排序，
// 不再退化为叶内序号致编码字母序。旧 v2 缓存 mtime/指纹不匹配自动重建。
// v4：EntryRecord 增加 boundary u64（音节起始位 bitmask，14→22B）。源数据 rime `ni hao` 的
// 空格本就是音节真值边界，此前在解析期被 replace(' ',"") 丢弃，迫使查询侧用 DAG 重新猜切分
// （xi'an vs xian 无从分辨）、造词侧靠 410 音节暴力反推。key 仍为扁平串，故 DAT/前缀查询
// 语义完全不变——边界只作为 entry 侧元数据随查询结果返回。旧缓存同样靠指纹不匹配自动重建。
// v5：AbbrevSection 的条目文本从「词」改为「全拼码」——二级索引指向主键而非复制数据。
// 简拼查询因此变成「查索引拿码 → 走主表装配候选」，候选得到真实的 code 与 boundary，
// 词频不再因简拼/全拼分裂成两份计数。**二进制结构完全未变**，变的是那个字符串字段的
// 语义，故必须 bump：旧 v4 缓存若按新逻辑读，会把词当成码拿去查主表、简拼全数落空。
// 旧缓存靠内容指纹不匹配自动重建，无迁移代码。
const VERSION: u32 = 5;
const HEADER_SIZE: usize = 48;
const LEAF_SIZE: usize = 8;
const ENTRY_SIZE: usize = 22;
const CHARMAP_SIZE: usize = 4 + 256 * 4; // 1028

/// 原子写临时文件序号（同 binformat，进程内防 tmp 撞名）。
static ATOMIC_WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ======================= 构建：Double-Array（有序键直接构建，无中间 trie） =======================

/// 构建好的双数组。
struct Dat {
    base: Vec<i32>,
    check: Vec<i32>,
    char_map: [i32; 256],
    max_code: i32,
}

/// 从**已按字典序排好、唯一**的编码列表**直接构建 DAT**——不建中间 trie，峰值内存仅 base/check。
/// `data_index` = 编码在列表中的下标（= LeafTable 索引）。这是相对「先建几百万小节点 trie 再转双数组」
/// 的关键省内存改造：trie 那几百万个小分配会把堆撑高、碎片化、不还给系统（低配设备 OOM 风险）。
///
/// 原理：一个 trie 节点 ⇔ 一段**共享前缀**的连续编码区间 `[lo,hi)`（前缀 = codes[lo][..depth]）。
/// BFS 处理 (state, lo, hi, depth)：终止符 = 区间内 len==depth 的那个唯一编码（排序在最前）；
/// 其余编码按第 depth 字节连续分组即各子节点的子区间。
fn build_dat_from_sorted(codes: &[&str]) -> Dat {
    // 1) 字符映射：0 留给终止符，出现过的字节按序得紧凑码 1..=max_code。
    let mut seen = [false; 256];
    for c in codes {
        for &b in c.as_bytes() {
            seen[b as usize] = true;
        }
    }
    let mut char_map = [-1i32; 256];
    char_map[0] = 0;
    let mut max_code = 0i32;
    for b in 1..256 {
        if seen[b] {
            max_code += 1;
            char_map[b] = max_code;
        }
    }

    // 2) base/check 初始化（check=-1 表空闲），root 占位 0。
    let mut base = vec![0i32; 256];
    let mut check = vec![-1i32; 256];
    check[0] = 0;
    let mut search_start = 1i32;

    // 3) BFS：队列保层序（search_start 单调推进、packing 更好）。
    if !codes.is_empty() {
        let mut queue: std::collections::VecDeque<(i32, usize, usize, usize)> =
            std::collections::VecDeque::new();
        queue.push_back((0i32, 0, codes.len(), 0));
        while let Some((s, lo, hi, depth)) = queue.pop_front() {
            // 收集出边紧凑码 + 子区间分组。
            let mut child_codes: Vec<i32> = Vec::new();
            let mut terminal: Option<usize> = None;
            let mut i = lo;
            // 唯一编码 → 区间内至多一个 len==depth，且必为最短(排序在前)，即 codes[lo]。
            if codes[lo].len() == depth {
                terminal = Some(lo);
                child_codes.push(0);
                i = lo + 1;
            }
            // 余下编码 len>depth，按第 depth 字节连续分组。
            let mut groups: Vec<(u8, usize, usize)> = Vec::new();
            while i < hi {
                let b = codes[i].as_bytes()[depth];
                let glo = i;
                i += 1;
                while i < hi && codes[i].as_bytes()[depth] == b {
                    i += 1;
                }
                groups.push((b, glo, i));
                child_codes.push(char_map[b as usize]);
            }
            if child_codes.is_empty() {
                continue;
            }
            child_codes.sort_unstable();
            let bv = find_base(&child_codes, &mut base, &mut check, search_start);
            base[s as usize] = bv;

            if let Some(leaf) = terminal {
                let t = bv; // bv + 0
                grow(&mut base, &mut check, t as usize);
                check[t as usize] = s;
                base[t as usize] = -(leaf as i32) - 1;
            }
            for (b, glo, ghi) in groups {
                let c = char_map[b as usize];
                let t = bv + c;
                grow(&mut base, &mut check, t as usize);
                check[t as usize] = s;
                queue.push_back((t, glo, ghi, depth + 1));
            }

            while (search_start as usize) < check.len() && check[search_start as usize] != -1 {
                search_start += 1;
            }
        }
    }

    // 4) 裁剪尾部空闲。
    let mut size = base.len();
    while size > 1 && check[size - 1] == -1 {
        size -= 1;
    }
    base.truncate(size);
    check.truncate(size);

    Dat {
        base,
        check,
        char_map,
        max_code,
    }
}

/// 线性扫描 base 值，使所有 (base+c) 空闲。codes 已升序、非空。
fn find_base(codes: &[i32], base: &mut Vec<i32>, check: &mut Vec<i32>, search_start: i32) -> i32 {
    let min_code = codes[0];
    let mut b = search_start;
    loop {
        if b + min_code < 1 {
            b += 1;
            continue;
        }
        let mut conflict = false;
        for &c in codes {
            let pos = (b + c) as usize;
            grow(base, check, pos);
            if check[pos] != -1 {
                conflict = true;
                break;
            }
        }
        if !conflict {
            return b;
        }
        b += 1;
    }
}

/// 扩容 base/check 到能容纳索引 `need`（新增位置 check=-1）。
fn grow(base: &mut Vec<i32>, check: &mut Vec<i32>, need: usize) {
    if need < base.len() {
        return;
    }
    let mut new_cap = base.len();
    while need >= new_cap {
        new_cap *= 2;
    }
    base.resize(new_cap, 0);
    check.resize(new_cap, -1);
}

// ======================= 写入 =======================

/// 字符串池（去重）。
struct StringPool {
    buf: Vec<u8>,
    index: std::collections::HashMap<String, u32>,
}

impl StringPool {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            index: std::collections::HashMap::new(),
        }
    }
    fn add(&mut self, s: &str) -> u32 {
        if let Some(&off) = self.index.get(s) {
            return off;
        }
        let off = self.buf.len() as u32;
        self.buf.extend_from_slice(s.as_bytes());
        self.index.insert(s.to_string(), off);
        off
    }
}

/// 从排序后的 (code,entries) 构建一段独立 DAT：返回 (DAT, leaves, entries)，文本入共享池。
/// 主表与简拼表各调一次（共用同一 StringPool 去重）。
fn build_section(
    sorted: &[&(String, Vec<WriteEntry>)],
    pool: &mut StringPool,
) -> (Dat, Vec<(u32, u16)>, Vec<(u32, u16, i32, u32, u64)>) {
    let mut leaves: Vec<(u32, u16)> = Vec::with_capacity(sorted.len());
    let mut entries: Vec<(u32, u16, i32, u32, u64)> = Vec::new();
    let mut codes: Vec<&str> = Vec::with_capacity(sorted.len());
    let mut entry_byte_off = 0u32;
    for kv in sorted {
        let (code, ents) = (&kv.0, &kv.1);
        codes.push(code.as_str());
        leaves.push((entry_byte_off, ents.len() as u16));
        for (text, weight, order, boundary) in ents {
            let text_off = pool.add(text);
            entries.push((text_off, text.len() as u16, *weight, *order, *boundary));
        }
        entry_byte_off += (ents.len() * ENTRY_SIZE) as u32;
    }
    (build_dat_from_sorted(&codes), leaves, entries)
}

/// wdat 写入器：与 binformat::DictWriter 同样接口（add(code, entries)），输出 DAT 格式。
/// `add_abbrev` 追加简拼（声母缩写）表，写入独立 AbbrevSection（与全拼查询互不污染）。
/// 写入侧的一条候选：`(text, weight, order, boundary)`。
/// boundary 见 [`DictEntry::boundary`]（音节起始位 bitmask，0=无边界信息）。
type WriteEntry = (String, i32, u32, u64);

pub struct WdatWriter {
    keys: Vec<(String, Vec<WriteEntry>)>,
    abbrevs: Vec<(String, Vec<WriteEntry>)>,
    meta: Option<Vec<u8>>,
}

/// 把 `(text, weight)` 列表补上 order：order = 该 code 内的条目序号（0,1,2…）。
/// 复现 v2「叶内序号」语义，供未显式提供全局序的调用方（combined 合并、测试）向后兼容。
/// boundary 置 0（无边界信息）——本入口的调用方均非拼音全量构建路径。
fn with_local_order(entries: Vec<(String, i32)>) -> Vec<WriteEntry> {
    entries
        .into_iter()
        .enumerate()
        .map(|(i, (t, w))| (t, w, i as u32, 0u64))
        .collect()
}

impl WdatWriter {
    pub fn new() -> Self {
        Self {
            keys: Vec::new(),
            abbrevs: Vec::new(),
            meta: None,
        }
    }

    /// 追加一个 code 的候选（`(text, weight)`）。order 按 code 内序号自动补全（向后兼容旧语义）。
    /// 需要**跨编码全局自然序**（无权重按词库出现顺序）时改用 [`add_with_order`]。
    pub fn add(&mut self, code: String, entries: Vec<(String, i32)>) {
        if !entries.is_empty() {
            self.keys.push((code, with_local_order(entries)));
        }
    }

    /// 追加一个 code 的候选，携带**显式全局 order**（`(text, weight, order)`）。
    /// order 为词库文件内的全局出现序（跨编码单调），使等权候选跨编码按出现顺序排列。
    /// order 须 < `composite::PER_LAYER_NO_OFFSET`（1e7），否则会溢出到层序偏移带。
    /// boundary 置 0（无边界信息）；拼音全量构建请用 [`Self::add_with_boundary`]。
    pub fn add_with_order(&mut self, code: String, entries: Vec<(String, i32, u32)>) {
        if !entries.is_empty() {
            self.keys.push((
                code,
                entries
                    .into_iter()
                    .map(|(t, w, o)| (t, w, o, 0u64))
                    .collect(),
            ));
        }
    }

    /// 追加一个 code 的候选，携带 order **与音节边界**（`(text, weight, order, boundary)`）。
    /// 供拼音词典构建路径使用——边界取自 rime 源数据 `ni hao` 的空格（真值，非 DAG 猜测）。
    /// boundary 语义见 [`DictEntry::boundary`]。
    pub fn add_with_boundary(&mut self, code: String, entries: Vec<WriteEntry>) {
        if !entries.is_empty() {
            self.keys.push((code, entries));
        }
    }

    /// 追加简拼条目：`abbrev`=声母序列（`nh`），`entries` 每条 `(全拼码, weight)`
    /// —— **存的是码不是词**（v5，见文件头版本说明）。空条目忽略；order 按叶内序号补全。
    ///
    /// `weight` 只决定该简拼下截断时保留哪些码，候选自身的权重来自主表。
    pub fn add_abbrev(&mut self, abbrev: String, entries: Vec<(String, i32)>) {
        if !entries.is_empty() {
            self.abbrevs.push((abbrev, with_local_order(entries)));
        }
    }

    pub fn set_meta(&mut self, meta: Vec<u8>) {
        self.meta = Some(meta);
    }

    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// 原子写 .wdat（tmp+pid+seq → rename，与 binformat 一致，仅防读到半文件）。
    pub fn write(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        use std::io::Write;
        let path = path.as_ref();

        // 按 code 排序（确定性 + DAT key 唯一）。排序**引用**而非克隆全量数据，省一份大拷贝。
        let mut sorted: Vec<&(String, Vec<WriteEntry>)> = self.keys.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let mut sorted_ab: Vec<&(String, Vec<WriteEntry>)> = self.abbrevs.iter().collect();
        sorted_ab.sort_by(|a, b| a.0.cmp(&b.0));
        let has_abbrev = !sorted_ab.is_empty();

        // 共享字符串池：主表先入（简拼候选多与主表 text 重复 → 去重复用偏移）。
        let mut pool = StringPool::new();
        let (dat, leaves, entries) = build_section(&sorted, &mut pool);
        let (a_dat, a_leaves, a_entries) = if has_abbrev {
            let (d, l, e) = build_section(&sorted_ab, &mut pool);
            (Some(d), l, e)
        } else {
            (None, Vec::new(), Vec::new())
        };

        // 主区段偏移。
        let dat_size = dat.base.len() as u32;
        let dat_off = HEADER_SIZE as u32;
        let leaf_off = dat_off + dat_size * 4 * 2;
        let entry_off = leaf_off + (leaves.len() * LEAF_SIZE) as u32;
        let str_off = entry_off + (entries.len() * ENTRY_SIZE) as u32;
        let after_pool = str_off + pool.buf.len() as u32;

        // 简拼区段（AbbrevSection）：紧跟共享池之后。自描述头 24B（6×u32）：
        // {dat_size, leaf_count, dat_off, leaf_off, entry_off, char_map_off}。
        const ABBREV_HDR: u32 = 24;
        let (abbrev_off, a_dat_off, a_leaf_off, a_entry_off, a_charmap_off, after_abbrev) =
            if let Some(ad) = &a_dat {
                let abbrev_off = after_pool;
                let a_dat_off = abbrev_off + ABBREV_HDR;
                let a_dat_size = ad.base.len() as u32;
                let a_leaf_off = a_dat_off + a_dat_size * 4 * 2;
                let a_entry_off = a_leaf_off + (a_leaves.len() * LEAF_SIZE) as u32;
                let a_charmap_off = a_entry_off + (a_entries.len() * ENTRY_SIZE) as u32;
                let after = a_charmap_off + CHARMAP_SIZE as u32;
                (
                    abbrev_off,
                    a_dat_off,
                    a_leaf_off,
                    a_entry_off,
                    a_charmap_off,
                    after,
                )
            } else {
                (0, 0, 0, 0, 0, after_pool)
            };

        let char_map_off = after_abbrev;
        let meta_off = match &self.meta {
            Some(m) if !m.is_empty() => char_map_off + CHARMAP_SIZE as u32,
            _ => 0,
        };

        // 原子写。
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let seq = ATOMIC_WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tmp_os = path.as_os_str().to_os_string();
        tmp_os.push(format!(".tmp.{}.{seq}", std::process::id()));
        let tmp = std::path::PathBuf::from(tmp_os);
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);

        // Header (48B, LE)。
        f.write_all(&MAGIC)?;
        f.write_all(&VERSION.to_le_bytes())?;
        f.write_all(&dat_size.to_le_bytes())?;
        f.write_all(&(leaves.len() as u32).to_le_bytes())?;
        f.write_all(&dat_off.to_le_bytes())?;
        f.write_all(&leaf_off.to_le_bytes())?;
        f.write_all(&entry_off.to_le_bytes())?;
        f.write_all(&str_off.to_le_bytes())?;
        f.write_all(&abbrev_off.to_le_bytes())?;
        f.write_all(&meta_off.to_le_bytes())?;
        f.write_all(&(entries.len() as u32).to_le_bytes())?;
        f.write_all(&char_map_off.to_le_bytes())?;

        let write_dat_section = |f: &mut std::io::BufWriter<std::fs::File>,
                                 dat: &Dat,
                                 leaves: &[(u32, u16)],
                                 entries: &[(u32, u16, i32, u32, u64)]|
         -> std::io::Result<()> {
            for v in &dat.base {
                f.write_all(&v.to_le_bytes())?;
            }
            for v in &dat.check {
                f.write_all(&v.to_le_bytes())?;
            }
            for (eoff, elen) in leaves {
                f.write_all(&eoff.to_le_bytes())?;
                f.write_all(&elen.to_le_bytes())?;
                f.write_all(&0u16.to_le_bytes())?;
            }
            for (toff, tlen, w, order, boundary) in entries {
                f.write_all(&toff.to_le_bytes())?;
                f.write_all(&tlen.to_le_bytes())?;
                f.write_all(&w.to_le_bytes())?;
                f.write_all(&order.to_le_bytes())?;
                f.write_all(&boundary.to_le_bytes())?; // v4：音节边界（22B 中的末 8B）
            }
            Ok(())
        };
        let write_charmap =
            |f: &mut std::io::BufWriter<std::fs::File>, dat: &Dat| -> std::io::Result<()> {
                f.write_all(&dat.max_code.to_le_bytes())?;
                for c in &dat.char_map {
                    f.write_all(&c.to_le_bytes())?;
                }
                Ok(())
            };

        // 主区段 + 共享池。
        write_dat_section(&mut f, &dat, &leaves, &entries)?;
        f.write_all(&pool.buf)?;

        // 简拼区段：自描述头 + DAT/leaf/entry + 简拼 CharMap。
        if let Some(ad) = &a_dat {
            f.write_all(&(ad.base.len() as u32).to_le_bytes())?;
            f.write_all(&(a_leaves.len() as u32).to_le_bytes())?;
            f.write_all(&a_dat_off.to_le_bytes())?;
            f.write_all(&a_leaf_off.to_le_bytes())?;
            f.write_all(&a_entry_off.to_le_bytes())?;
            f.write_all(&a_charmap_off.to_le_bytes())?;
            write_dat_section(&mut f, ad, &a_leaves, &a_entries)?;
            write_charmap(&mut f, ad)?;
        }

        // 主 CharMap。
        write_charmap(&mut f, &dat)?;

        // Meta。
        if let Some(m) = &self.meta {
            if !m.is_empty() {
                f.write_all(&(m.len() as u32).to_le_bytes())?;
                f.write_all(m)?;
            }
        }

        f.flush()?;
        drop(f);
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        info!(
            "Wrote wdat: {} keys, {} abbrevs, {} entries, dat_size={}",
            leaves.len(),
            a_leaves.len(),
            entries.len(),
            dat_size
        );
        Ok(())
    }
}

impl Default for WdatWriter {
    fn default() -> Self {
        Self::new()
    }
}

// ======================= 读取（mmap 零拷贝） =======================

/// 一段 DAT 的视图（主表 / 简拼表各一份），供查询方法复用同一套 walk/DFS 逻辑。
struct DatView {
    dat_off: usize,
    check_off: usize,
    dat_size: u32,
    leaf_off: usize,
    entry_off: usize,
    char_map: [i32; 256],
    rev_map: Vec<u8>, // 紧凑码 → 原始字节（1..=max_code）
    max_code: i32,
}

pub struct WdatReader {
    mmap: Mmap,
    leaf_count: u32,
    str_off: usize, // 共享字符串池（主表与简拼共用）
    main: DatView,
    abbrev: Option<DatView>, // 简拼区段（声母缩写，独立 DAT，不污染全拼前缀查询）
}

impl WdatReader {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_SIZE {
            anyhow::bail!("wdat too short");
        }
        if mmap[0..4] != MAGIC {
            anyhow::bail!("invalid wdat magic: {:?}", &mmap[0..4]);
        }
        let file_len = mmap.len();
        let rd = |off: usize| u32::from_le_bytes(mmap[off..off + 4].try_into().unwrap());
        // 版本必须匹配：v2→v3 EntryRecord 由 10→14B，旧缓存若按新步长解析会读到错乱候选。
        // 校验失败即 bail，调用方（cached/load_merged_dicts）捕获 Err 后回退重建，自动升级缓存。
        let version = rd(4);
        if version != VERSION {
            anyhow::bail!("wdat version mismatch: file={version}, expected={VERSION} (需重建缓存)");
        }
        let dat_size = rd(8);
        let leaf_count = rd(12);
        let dat_off = rd(16) as usize;
        let leaf_off = rd(20) as usize;
        let entry_off = rd(24) as usize;
        let str_off = rd(28) as usize;
        let abbrev_off = rd(32) as usize;
        let char_map_off = rd(44) as usize;

        // 从 char_map_off 读 CharMap → (char_map, rev_map, max_code)。
        let read_charmap = |off: usize| -> ([i32; 256], Vec<u8>, i32) {
            let max_code = i32::from_le_bytes(mmap[off..off + 4].try_into().unwrap());
            let mut cm = [-1i32; 256];
            for b in 0..256 {
                let o = off + 4 + b * 4;
                cm[b] = i32::from_le_bytes(mmap[o..o + 4].try_into().unwrap());
            }
            let mut rm = vec![0u8; (max_code.max(0) as usize) + 1];
            for b in 0..256 {
                let c = cm[b];
                if c > 0 && (c as usize) < rm.len() {
                    rm[c as usize] = b as u8;
                }
            }
            (cm, rm, max_code)
        };

        // 主区段越界校验。
        let check_off = dat_off + dat_size as usize * 4;
        if check_off + dat_size as usize * 4 > file_len
            || char_map_off + CHARMAP_SIZE > file_len
            || leaf_off > file_len
            || str_off > file_len
        {
            anyhow::bail!("wdat offsets out of range");
        }
        let (char_map, rev_map, max_code) = read_charmap(char_map_off);
        let main = DatView {
            dat_off,
            check_off,
            dat_size,
            leaf_off,
            entry_off,
            char_map,
            rev_map,
            max_code,
        };

        // 简拼区段（自描述头 24B：dat_size, leaf_count, dat_off, leaf_off, entry_off, char_map_off）。
        let abbrev = if abbrev_off != 0 && abbrev_off + 24 <= file_len {
            let a_dat_size = rd(abbrev_off);
            let a_dat_off = rd(abbrev_off + 8) as usize;
            let a_leaf_off = rd(abbrev_off + 12) as usize;
            let a_entry_off = rd(abbrev_off + 16) as usize;
            let a_charmap_off = rd(abbrev_off + 20) as usize;
            if a_charmap_off + CHARMAP_SIZE <= file_len
                && a_dat_off + a_dat_size as usize * 8 <= file_len
            {
                let (cm, rm, mc) = read_charmap(a_charmap_off);
                Some(DatView {
                    dat_off: a_dat_off,
                    check_off: a_dat_off + a_dat_size as usize * 4,
                    dat_size: a_dat_size,
                    leaf_off: a_leaf_off,
                    entry_off: a_entry_off,
                    char_map: cm,
                    rev_map: rm,
                    max_code: mc,
                })
            } else {
                None
            }
        } else {
            None
        };

        info!(
            "Opened wdat: {} ({} keys, dat_size={}, abbrev={})",
            path.display(),
            leaf_count,
            dat_size,
            abbrev.is_some()
        );
        Ok(Self {
            mmap,
            leaf_count,
            str_off,
            main,
            abbrev,
        })
    }

    pub fn key_count(&self) -> u32 {
        self.leaf_count
    }

    #[inline]
    fn base(&self, v: &DatView, i: i32) -> i32 {
        let o = v.dat_off + (i as usize) * 4;
        i32::from_le_bytes(self.mmap[o..o + 4].try_into().unwrap())
    }
    #[inline]
    fn check(&self, v: &DatView, i: i32) -> i32 {
        let o = v.check_off + (i as usize) * 4;
        i32::from_le_bytes(self.mmap[o..o + 4].try_into().unwrap())
    }
    #[inline]
    fn in_range(v: &DatView, t: i32) -> bool {
        t >= 0 && (t as u32) < v.dat_size
    }

    /// 沿 code 走到状态（不含终止符）。失败返回 None。
    fn walk(&self, v: &DatView, code: &str) -> Option<i32> {
        let mut s = 0i32;
        for &b in code.as_bytes() {
            let c = v.char_map[b as usize];
            if c < 0 {
                return None;
            }
            let t = self.base(v, s) + c;
            if !Self::in_range(v, t) || self.check(v, t) != s {
                return None;
            }
            s = t;
        }
        Some(s)
    }

    /// 状态 s 的终止符叶（若有）→ LeafTable 索引。
    fn terminal_leaf(&self, v: &DatView, s: i32) -> Option<u32> {
        let t = self.base(v, s); // + 0
        if !Self::in_range(v, t) || self.check(v, t) != s {
            return None;
        }
        let bt = self.base(v, t);
        if bt >= 0 {
            return None; // 非叶
        }
        Some((-bt - 1) as u32)
    }

    fn read_leaf(&self, v: &DatView, leaf_idx: u32) -> (u32, u16) {
        let o = v.leaf_off + leaf_idx as usize * LEAF_SIZE;
        let eoff = u32::from_le_bytes(self.mmap[o..o + 4].try_into().unwrap());
        let elen = u16::from_le_bytes(self.mmap[o + 4..o + 6].try_into().unwrap());
        (eoff, elen)
    }

    fn read_string(&self, off: u32, len: u16) -> &str {
        let start = self.str_off + off as usize;
        let end = start + len as usize;
        if end > self.mmap.len() {
            return "";
        }
        std::str::from_utf8(&self.mmap[start..end]).unwrap_or("")
    }

    /// 流式读某叶的所有候选：逐条回调 f(text, weight, order, boundary)。order=写入时携带的
    /// 全局自然序（v3；无权重时跨编码按词库出现顺序排列）；boundary=音节起始位 bitmask
    /// （v4，见 [`DictEntry::boundary`]，0=无边界信息）。不分配中间 Vec，供全量遍历流式使用。
    fn read_leaf_entries(
        &self,
        v: &DatView,
        leaf_idx: u32,
        f: &mut dyn FnMut(&str, i32, i32, u64),
    ) {
        let (eoff, elen) = self.read_leaf(v, leaf_idx);
        let base = v.entry_off + eoff as usize;
        for i in 0..elen as usize {
            let o = base + i * ENTRY_SIZE;
            if o + ENTRY_SIZE > self.mmap.len() {
                break;
            }
            let text_off = u32::from_le_bytes(self.mmap[o..o + 4].try_into().unwrap());
            let text_len = u16::from_le_bytes(self.mmap[o + 4..o + 6].try_into().unwrap());
            let weight = i32::from_le_bytes(self.mmap[o + 6..o + 10].try_into().unwrap());
            let order = u32::from_le_bytes(self.mmap[o + 10..o + 14].try_into().unwrap());
            let boundary = u64::from_le_bytes(self.mmap[o + 14..o + 22].try_into().unwrap());
            f(
                self.read_string(text_off, text_len),
                weight,
                order as i32,
                boundary,
            );
        }
    }

    /// 读某叶候选到 out（精确/前缀查找用）。
    fn read_entries(&self, v: &DatView, leaf_idx: u32, code: &str, out: &mut Vec<DictEntry>) {
        self.read_leaf_entries(v, leaf_idx, &mut |text, weight, order, boundary| {
            out.push(DictEntry {
                code: code.to_string(),
                text: text.to_string(),
                weight,
                order,
                boundary,
            });
        });
    }

    fn exact(&self, v: &DatView, code: &str) -> Vec<DictEntry> {
        if v.dat_size == 0 {
            return Vec::new();
        }
        let Some(s) = self.walk(v, code) else {
            return Vec::new();
        };
        let Some(leaf) = self.terminal_leaf(v, s) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        self.read_entries(v, leaf, code, &mut out);
        out
    }

    /// 精确查找（全拼/全码）。
    pub fn search(&self, code: &str) -> Vec<DictEntry> {
        self.exact(&self.main, code)
    }

    /// 简拼查找（声母缩写，如 "nh"→你好）：查独立简拼 DAT，按权重降序、截断。
    /// 无简拼区段或未命中返回空。
    pub fn search_abbrev(&self, code: &str, limit: usize) -> Vec<DictEntry> {
        let Some(v) = &self.abbrev else {
            return Vec::new();
        };
        let mut out = self.exact(v, code);
        out.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.order.cmp(&b.order)));
        if limit > 0 {
            out.truncate(limit);
        }
        out
    }

    pub fn has_abbrev(&self) -> bool {
        self.abbrev.is_some()
    }

    /// 是否存在**严格长于** `prefix` 的编码（trie 上即：prefix 状态还有非终止后继）。
    ///
    /// 成本 O(max_code)（≤ 出现过的字节种数，码表约 26）次数组读，**不触碰 LeafTable /
    /// StringPool**。这是给「更长后继」这类**存在性**判据用的——此前调用方一律走
    /// `search_prefix(input, 64)` 再 `.any(code 更长)`，等于为一个 bool 遍历整棵子树，
    /// 在 `ok` 拼字这类单前缀 8.8 万条的词库上单次即 20ms 级。
    ///
    /// 注意语义比「search_prefix 后 any」**更严格也更正确**：后者的结果先经权重排序截断
    /// （长码候选权重低时会被挤出而漏判），跨层合并时还会因「同 text 取最短码」抹掉长码；
    /// 本函数直接问 trie，不受二者影响。
    pub fn has_longer_code(&self, prefix: &str) -> bool {
        let v = &self.main;
        if v.dat_size == 0 {
            return false;
        }
        let Some(s) = self.walk(v, prefix) else {
            return false;
        };
        // c=0 是终止符（prefix 自身成词），不算「更长」；c>=1 的任一有效转移都意味着
        // 该子树下必有更长编码（trie 每条路径终归通向叶）。
        let b = self.base(v, s);
        (1..=v.max_code).any(|c| {
            let t = b + c;
            Self::in_range(v, t) && self.check(v, t) == s
        })
    }

    /// 前缀查找：走到前缀状态，DFS 子树遍历叶（重建每个候选的完整 code），
    /// 按权重降序、order 升序取前 `limit` 条（与 binformat::DictReader 对齐）。
    ///
    /// **实现为 top-N 堆而非「全收集 + 全排序 + 截断」**：DFS 仍遍历整棵子树（不早停，
    /// 故结果不漏高权重项），但只保留最优的 `limit` 条，且**够不上当前第 N 名的条目
    /// 不做任何 String 分配**。对 `ok` 拼字这类「单前缀下 8.8 万条」的词库，旧写法要
    /// 物化 8.8 万个 `DictEntry`（17.6 万次堆分配）再全排序，最后扔掉 99.9%——单次
    /// 前缀查询 80ms 级。
    ///
    /// 排序键含 `seq`（DFS 遍历序）作第三级 tie-break，以复现 `sort_by` 的**稳定排序**
    /// 语义：`(weight, order)` 打平时保持 DFS 顺序。这不是可选项——`with_local_order`
    /// 写入的词库（`export_to_writer` / combined 合并路径）order 是「code 内序号 0,1,2」，
    /// 等权时会大量打平，缺了 seq 堆将任意取舍，候选列表随之抖动。
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<DictEntry> {
        if self.main.dat_size == 0 || limit == 0 {
            return Vec::new();
        }
        let Some(start) = self.walk(&self.main, prefix) else {
            return Vec::new();
        };

        /// 排序键。`Ord` 定义为「**越差越大**」，故堆顶恒为当前最差的入选者，
        /// 且 `into_sorted_vec()`（升序）直接给出最优→最差的最终顺序。
        #[derive(PartialEq, Eq)]
        struct RankKey {
            weight: i32,
            order: i32,
            seq: u64,
        }
        impl Ord for RankKey {
            fn cmp(&self, other: &Self) -> Ordering {
                // weight 小 → 差；order 大 → 差；seq 大 → 差。
                other
                    .weight
                    .cmp(&self.weight)
                    .then(self.order.cmp(&other.order))
                    .then(self.seq.cmp(&other.seq))
            }
        }
        impl PartialOrd for RankKey {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }
        struct Ranked {
            key: RankKey,
            entry: DictEntry,
        }
        impl PartialEq for Ranked {
            fn eq(&self, other: &Self) -> bool {
                self.key == other.key
            }
        }
        impl Eq for Ranked {}
        impl Ord for Ranked {
            fn cmp(&self, other: &Self) -> Ordering {
                self.key.cmp(&other.key)
            }
        }
        impl PartialOrd for Ranked {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        let v = &self.main;
        let mut heap: BinaryHeap<Ranked> = BinaryHeap::with_capacity(limit + 1);
        let mut seq: u64 = 0;
        let mut path: Vec<u8> = prefix.as_bytes().to_vec();
        self.for_each_leaf(v, start, &mut path, &mut |code, leaf| {
            self.read_leaf_entries(v, leaf, &mut |text, weight, order, boundary| {
                let key = RankKey { weight, order, seq };
                seq += 1;
                if heap.len() >= limit {
                    // 堆顶是最差的入选者：不严格优于它就丢弃——注意此处**先比较后分配**，
                    // 绝大多数条目走到这里即返回，不触碰 code/text 的 to_string()。
                    match heap.peek() {
                        Some(worst) if key >= worst.key => return,
                        _ => {}
                    }
                    heap.pop();
                }
                heap.push(Ranked {
                    key,
                    entry: DictEntry {
                        code: code.to_string(),
                        text: text.to_string(),
                        weight,
                        order,
                        boundary,
                    },
                });
            });
        });
        heap.into_sorted_vec()
            .into_iter()
            .map(|r| r.entry)
            .collect()
    }

    /// 遍历全部条目（供反查索引构建）：DFS 全树**流式**回调 (code,text,weight)，
    /// 不累积全量 Vec——避免在大词库反查时堆起数十万 DictEntry（私有堆峰值/碎片）。
    pub fn for_each_entry(&self, f: &mut dyn FnMut(&str, &str, i32)) {
        if self.main.dat_size == 0 {
            return;
        }
        let v = &self.main;
        let mut path: Vec<u8> = Vec::new();
        self.for_each_leaf(v, 0, &mut path, &mut |code, leaf| {
            self.read_leaf_entries(v, leaf, &mut |text, weight, _order, _boundary| {
                f(code, text, weight);
            });
        });
    }

    /// DFS 子树：对每个叶**流式**调用 on_leaf(完整code, leaf_idx)，不累积候选。
    /// 用显式栈避免深递归；path 随进出栈增删。
    fn for_each_leaf(
        &self,
        v: &DatView,
        start: i32,
        path: &mut Vec<u8>,
        on_leaf: &mut dyn FnMut(&str, u32),
    ) {
        let mut stack: Vec<(i32, usize, i32)> = vec![(start, path.len(), 1)];
        if let Some(leaf) = self.terminal_leaf(v, start) {
            on_leaf(std::str::from_utf8(path).unwrap_or(""), leaf);
        }
        while let Some(&mut (s, plen, ref mut next_c)) = stack.last_mut() {
            path.truncate(plen);
            let mut descended = false;
            while *next_c <= v.max_code {
                let c = *next_c;
                *next_c += 1;
                let t = self.base(v, s) + c;
                if !Self::in_range(v, t) || self.check(v, t) != s {
                    continue;
                }
                path.push(v.rev_map[c as usize]);
                if let Some(leaf) = self.terminal_leaf(v, t) {
                    on_leaf(std::str::from_utf8(path).unwrap_or(""), leaf);
                }
                stack.push((t, path.len(), 1));
                descended = true;
                break;
            }
            if !descended {
                stack.pop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(tmp_name: &str, data: &[(&str, &[(&str, i32)])]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(tmp_name);
        let mut w = WdatWriter::new();
        for (code, ents) in data {
            let v: Vec<(String, i32)> = ents.iter().map(|(t, wt)| (t.to_string(), *wt)).collect();
            w.add(code.to_string(), v);
        }
        w.write(&p).expect("write wdat");
        p
    }

    /// 按 (code, entries) 构建 wdat（owned 版，供批量生成用例）。走 `WdatWriter::add`
    /// → `with_local_order`，即 order = code 内序号——与 `export_to_writer` / combined
    /// 合并路径同语义，单条 code 时 order 恒 0（等权即全打平，最坏 tie-break 场景）。
    fn build_owned(tmp_name: &str, data: &[(String, Vec<(String, i32)>)]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(tmp_name);
        let mut w = WdatWriter::new();
        for (code, ents) in data {
            w.add(code.clone(), ents.clone());
        }
        w.write(&p).expect("write wdat");
        p
    }

    /// 参考实现 = 改造前的「DFS 全量收集 → 稳定排序 → 截断」。作为差分对比的黄金标准：
    /// top-N 堆的输出必须与它**逐条相同**（含顺序），否则即为正确性回归。
    fn reference_prefix(r: &WdatReader, prefix: &str, limit: usize) -> Vec<DictEntry> {
        let v = &r.main;
        let Some(start) = r.walk(v, prefix) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut path: Vec<u8> = prefix.as_bytes().to_vec();
        r.for_each_leaf(v, start, &mut path, &mut |code, leaf| {
            r.read_leaf_entries(v, leaf, &mut |text, weight, order, boundary| {
                out.push(DictEntry {
                    code: code.to_string(),
                    text: text.to_string(),
                    weight,
                    order,
                    boundary,
                });
            });
        });
        out.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.order.cmp(&b.order)));
        out.truncate(limit);
        out
    }

    fn assert_same(actual: &[DictEntry], expect: &[DictEntry], ctx: &str) {
        let a: Vec<_> = actual
            .iter()
            .map(|e| (&e.code, &e.text, e.weight))
            .collect();
        let b: Vec<_> = expect
            .iter()
            .map(|e| (&e.code, &e.text, e.weight))
            .collect();
        assert_eq!(a, b, "{ctx}");
    }

    /// top-N 堆 == 全量排序取前 N，在**权重与 order 全部打平**时也成立。
    ///
    /// 这是 `ok` 拼字词库的真实形状：88020 条无 weight 列（全取 default_weight），
    /// 每 code 单条（order 恒 0）。此时两级排序键完全失效，顺序只由稳定排序的
    /// 「保持 DFS 序」兜底——正是 top-N 堆最容易与全排序分歧之处（堆本身不稳定，
    /// 靠 `RankKey::seq` 复现）。
    #[test]
    fn topn_prefix_matches_full_sort_when_all_keys_tie() {
        let data: Vec<(String, Vec<(String, i32)>)> = (0..500u32)
            .map(|i| {
                let c1 = (b'a' + (i / 26) as u8) as char;
                let c2 = (b'a' + (i % 26) as u8) as char;
                (format!("ok{c1}{c2}"), vec![(format!("字{i}"), 0)])
            })
            .collect();
        let p = build_owned("wdat_topn_all_tie.wdat", &data);
        let r = WdatReader::open(&p).unwrap();

        for limit in [1usize, 5, 90, 499, 500, 501, 1000] {
            assert_same(
                &r.search_prefix("ok", limit),
                &reference_prefix(&r, "ok", limit),
                &format!("全打平 limit={limit}"),
            );
        }
        let _ = std::fs::remove_file(&p);
    }

    /// top-N 堆 == 全量排序取前 N，混合权重 + 同 code 多条（order 0,1,2）时也成立。
    #[test]
    fn topn_prefix_matches_full_sort_mixed_weights() {
        let data: Vec<(String, Vec<(String, i32)>)> = (0..200u32)
            .map(|i| {
                let c1 = (b'a' + (i / 26) as u8) as char;
                let c2 = (b'a' + (i % 26) as u8) as char;
                // 权重故意大量重复（i % 7），并让部分 code 带 2 条（order 0/1）。
                let mut ents = vec![(format!("甲{i}"), (i % 7) as i32 * 100)];
                if i % 3 == 0 {
                    ents.push((format!("乙{i}"), (i % 7) as i32 * 100));
                }
                (format!("ok{c1}{c2}"), ents)
            })
            .collect();
        let p = build_owned("wdat_topn_mixed.wdat", &data);
        let r = WdatReader::open(&p).unwrap();

        for limit in [1usize, 3, 50, 200, 999] {
            assert_same(
                &r.search_prefix("ok", limit),
                &reference_prefix(&r, "ok", limit),
                &format!("混合权重 limit={limit}"),
            );
        }
        let _ = std::fs::remove_file(&p);
    }

    /// **护栏**：DFS 必须遍历完整棵子树，不得早停。
    ///
    /// 最高权重的词放在字典序最末。若把本函数「优化」成 `codetable.rs` / `binformat.rs`
    /// 那种「攒够 limit(*2) 就 break」，DFS 按字典序推进，这条永远进不了结果集——
    /// 那正是本次修复**刻意没有采用**早停方案的原因（8.8 万条的库上偏差极显著）。
    #[test]
    fn topn_prefix_keeps_high_weight_entry_at_lexical_tail() {
        let mut data: Vec<(String, Vec<(String, i32)>)> = (0..400u32)
            .map(|i| {
                let c1 = (b'a' + (i / 26) as u8) as char;
                let c2 = (b'a' + (i % 26) as u8) as char;
                (format!("ok{c1}{c2}"), vec![(format!("庸{i}"), 10)])
            })
            .collect();
        data.push(("okzz".to_string(), vec![("压轴".to_string(), 9999)]));
        let p = build_owned("wdat_topn_tail.wdat", &data);
        let r = WdatReader::open(&p).unwrap();

        let out = r.search_prefix("ok", 5);
        assert_eq!(out.len(), 5);
        assert_eq!(
            out[0].text, "压轴",
            "字典序最末的高权重词被漏掉 ⇒ DFS 提前中止了"
        );
        assert_eq!(out[0].code, "okzz");
        let _ = std::fs::remove_file(&p);
    }

    /// `has_longer_code` 的基本语义：只认**严格更长**的后继，前缀自身成词不算。
    #[test]
    fn has_longer_code_semantics() {
        let p = build(
            "wdat_has_longer.wdat",
            &[
                ("ok", &[("好", 10)]),     // 自身成词，且下面还有更长的
                ("okz", &[("仄", 10)]),    // 中间节点，自身也成词
                ("okzz", &[("最长", 10)]), // 该支最长
                ("om", &[("某", 10)]),     // 自身成词，无更长后继
            ],
        );
        let r = WdatReader::open(&p).unwrap();

        assert!(r.has_longer_code(""), "空前缀：词库非空即有更长码");
        assert!(r.has_longer_code("o"), "o 下有 ok/om");
        assert!(r.has_longer_code("ok"), "ok 自身成词，但下面还有 okz/okzz");
        assert!(r.has_longer_code("okz"), "okz 自身成词，但下面还有 okzz");
        assert!(!r.has_longer_code("okzz"), "okzz 是该支最长，无更长后继");
        assert!(!r.has_longer_code("om"), "om 自身成词且无后继");
        assert!(!r.has_longer_code("xyz"), "前缀不存在");
        assert!(!r.has_longer_code("okzzz"), "越过最长码");
        let _ = std::fs::remove_file(&p);
    }

    /// `has_longer_code` 与旧判据（`search_prefix(_, 64)` 再 `any(code 更长)`）在
    /// **未触发截断**时逐一致——覆盖「前缀自身成词 / 中间节点 / 叶子 / 不存在」四类位置。
    ///
    /// 二者并非全等：旧判据经权重排序截断，长码候选权重偏低被挤出前 64 名时会漏判成
    /// false。新实现直接问 trie，故 **新 ⊇ 旧**（只会更保守，不会更激进）——这正是
    /// 「更长后继」作为上屏安全阀所需的方向。本测试锁住未截断场景下二者不得分歧。
    #[test]
    fn has_longer_code_agrees_with_legacy_predicate_when_untruncated() {
        let data: Vec<(String, Vec<(String, i32)>)> = (0..300u32)
            .map(|i| {
                let c1 = (b'a' + (i / 26) as u8) as char;
                let c2 = (b'a' + (i % 26) as u8) as char;
                // 一半是 3 码（叶），一半再挂 4 码后继 → 覆盖有/无更长后继两类。
                if i % 2 == 0 {
                    (format!("z{c1}{c2}"), vec![(format!("甲{i}"), 10)])
                } else {
                    (format!("z{c1}{c2}q"), vec![(format!("乙{i}"), 10)])
                }
            })
            .collect();
        let p = build_owned("wdat_has_longer_agree.wdat", &data);
        let r = WdatReader::open(&p).unwrap();

        let legacy = |prefix: &str| -> bool {
            let n = prefix.chars().count();
            r.search_prefix(prefix, 64)
                .iter()
                .any(|e| e.code.chars().count() > n)
        };
        // 枚举全部编码的全部前缀 + 空串 + 若干不存在的码。
        let mut prefixes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        prefixes.insert(String::new());
        for extra in ["zzzz", "q", "z"] {
            prefixes.insert(extra.to_string());
        }
        r.for_each_entry(&mut |code, _t, _w| {
            for i in 1..=code.len() {
                prefixes.insert(code[..i].to_string());
            }
        });
        for pre in &prefixes {
            assert_eq!(
                r.has_longer_code(pre),
                legacy(pre),
                "前缀 {pre:?} 上新旧判据分歧"
            );
        }
        let _ = std::fs::remove_file(&p);
    }

    /// limit=0 → 空（与改造前 `truncate(0)` 语义一致，且不做无谓的 DFS）。
    #[test]
    fn topn_prefix_limit_zero_is_empty() {
        let data = vec![("oka".to_string(), vec![("甲".to_string(), 10)])];
        let p = build_owned("wdat_topn_zero.wdat", &data);
        let r = WdatReader::open(&p).unwrap();
        assert!(r.search_prefix("ok", 0).is_empty());
        assert_eq!(r.search_prefix("ok", 1).len(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn exact_match_multi_key() {
        let p = build(
            "wdat_exact_test.wdat",
            &[
                ("a", &[("工", 9999), ("戈", 100)]),
                ("ni", &[("你", 800), ("尼", 50)]),
                ("nihao", &[("你好", 1200)]),
                ("zhongguo", &[("中国", 2000)]),
            ],
        );
        let r = WdatReader::open(&p).unwrap();
        assert_eq!(r.key_count(), 4);

        let a = r.search("a");
        assert_eq!(a.len(), 2);
        assert!(a.iter().any(|e| e.text == "工" && e.weight == 9999));
        assert!(a.iter().any(|e| e.text == "戈"));

        let nihao = r.search("nihao");
        assert_eq!(nihao.len(), 1);
        assert_eq!(nihao[0].text, "你好");
        assert_eq!(nihao[0].weight, 1200);
        assert_eq!(nihao[0].code, "nihao");

        let zg = r.search("zhongguo");
        assert_eq!(zg.len(), 1);
        assert_eq!(zg[0].text, "中国");

        // 不存在的 code。
        assert!(r.search("xyz").is_empty());
        assert!(r.search("n").is_empty()); // "n" 非终止（是 ni/nihao 前缀但无 n 自身词）
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn prefix_collects_subtree_with_codes() {
        let p = build(
            "wdat_prefix_test.wdat",
            &[
                ("ni", &[("你", 800)]),
                ("nihao", &[("你好", 1200)]),
                ("nihaoma", &[("你好吗", 300)]),
                ("nin", &[("您", 600)]),
                ("zhong", &[("中", 500)]),
            ],
        );
        let r = WdatReader::open(&p).unwrap();
        // 前缀 "ni" → ni/nihao/nihaoma/nin（不含 zhong）。
        let res = r.search_prefix("ni", 10);
        let texts: Vec<&str> = res.iter().map(|e| e.text.as_str()).collect();
        assert!(texts.contains(&"你"));
        assert!(texts.contains(&"你好"));
        assert!(texts.contains(&"你好吗"));
        assert!(texts.contains(&"您"));
        assert!(!texts.contains(&"中"), "前缀 ni 不应含 zhong: {texts:?}");
        // 重建的 code 正确。
        let nihao = res.iter().find(|e| e.text == "你好").unwrap();
        assert_eq!(nihao.code, "nihao");
        let nihaoma = res.iter().find(|e| e.text == "你好吗").unwrap();
        assert_eq!(nihaoma.code, "nihaoma");
        // 按权重降序。
        assert_eq!(res[0].text, "你好", "最高权重 1200 应排首: {texts:?}");
        // limit 截断。
        assert_eq!(r.search_prefix("ni", 2).len(), 2);
        let _ = std::fs::remove_file(&p);
    }

    /// 回归（wdat v3）：无权重（全同权）时，跨编码前缀查询应按**全局 order（词库出现顺序）**
    /// 排列，而非退化为编码字母序。构造 order 与字母序相反以区分两者。
    #[test]
    fn prefix_no_weight_sorts_by_global_order_not_code() {
        let p = std::env::temp_dir().join("wdat_global_order_test.wdat");
        let mut w = WdatWriter::new();
        // 出现顺序 za → ma → aa（order 0,1,2），编码字母序则相反 aa < ma < za；权重全 0。
        w.add_with_order("za".to_string(), vec![("Z".to_string(), 0, 0)]);
        w.add_with_order("ma".to_string(), vec![("M".to_string(), 0, 1)]);
        w.add_with_order("aa".to_string(), vec![("A".to_string(), 0, 2)]);
        w.write(&p).expect("write wdat");
        let r = WdatReader::open(&p).unwrap();
        let res = r.search_prefix("", 10);
        let texts: Vec<&str> = res.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["Z", "M", "A"],
            "无权重应按全局 order（出现序）排序而非编码字母序，实际: {texts:?}"
        );
        // 单编码多条同权：前缀查询（会按 weight desc、order asc 排序）亦按 order 升序恢复出现序。
        // （精确 search 不排序、返回叶内存储序——生产路径写入前已预排序，故存储序即展示序。）
        let mut w2 = WdatWriter::new();
        w2.add_with_order(
            "aa".to_string(),
            vec![
                ("三".to_string(), 0, 2),
                ("一".to_string(), 0, 0),
                ("二".to_string(), 0, 1),
            ],
        );
        let p2 = std::env::temp_dir().join("wdat_global_order_test2.wdat");
        w2.write(&p2).expect("write wdat");
        let r2 = WdatReader::open(&p2).unwrap();
        let t2: Vec<String> = r2
            .search_prefix("aa", 10)
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(
            t2,
            vec!["一", "二", "三"],
            "同码同权前缀查询应按 order 升序"
        );
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn for_each_enumerates_all() {
        let p = build(
            "wdat_foreach_test.wdat",
            &[("a", &[("工", 9999), ("戈", 100)]), ("aaaa", &[("叕", 50)])],
        );
        let r = WdatReader::open(&p).unwrap();
        let mut got: Vec<(String, String, i32)> = Vec::new();
        r.for_each_entry(&mut |c, t, w| got.push((c.to_string(), t.to_string(), w)));
        assert_eq!(got.len(), 3, "应枚举 3 条: {got:?}");
        assert!(got.contains(&("a".to_string(), "工".to_string(), 9999)));
        assert!(got.contains(&("a".to_string(), "戈".to_string(), 100)));
        assert!(got.contains(&("aaaa".to_string(), "叕".to_string(), 50)));
        let _ = std::fs::remove_file(&p);
    }

    /// **对拍**：同一份数据分别建 wdb(binformat) 与 wdat，比对精确/前缀/全枚举查询结果一致
    /// （按 (code,text,weight) 集合比较，忽略 natural_order 这一格式差异）。这是 wdb→wdat
    /// 迁移的核心正确性保证。
    #[test]
    fn parity_with_wdb() {
        use crate::binformat::{DictReader, DictWriter};
        let data: Vec<(&str, Vec<(&str, i32)>)> = vec![
            ("a", vec![("工", 9999), ("戈", 100)]),
            ("ni", vec![("你", 800), ("尼", 50)]),
            ("nihao", vec![("你好", 1200)]),
            ("nihaoma", vec![("你好吗", 300)]),
            ("nin", vec![("您", 600)]),
            ("zhong", vec![("中", 500)]),
            ("zhongguo", vec![("中国", 2000)]),
            ("zhi", vec![("之", 700), ("知", 690)]),
        ];
        let to_owned = |e: &Vec<(&str, i32)>| -> Vec<(String, i32)> {
            e.iter().map(|(t, w)| (t.to_string(), *w)).collect()
        };

        let wdb_path = std::env::temp_dir().join("wdat_parity.wdb");
        let mut dw = DictWriter::new();
        for (c, e) in &data {
            dw.add(c.to_string(), to_owned(e));
        }
        dw.write(&wdb_path).unwrap();
        let wdb = DictReader::open(&wdb_path).unwrap();

        let wdat_path = std::env::temp_dir().join("wdat_parity.wdat");
        let mut ww = WdatWriter::new();
        for (c, e) in &data {
            ww.add(c.to_string(), to_owned(e));
        }
        ww.write(&wdat_path).unwrap();
        let wdat = WdatReader::open(&wdat_path).unwrap();

        assert_eq!(wdb.key_count(), wdat.key_count(), "key_count 应一致");

        // 精确：每个 code 的 (text,weight) 集合一致。
        for (c, _) in &data {
            let mut a: Vec<(String, i32)> = wdb
                .search(c)
                .into_iter()
                .map(|e| (e.text, e.weight))
                .collect();
            let mut b: Vec<(String, i32)> = wdat
                .search(c)
                .into_iter()
                .map(|e| (e.text, e.weight))
                .collect();
            a.sort();
            b.sort();
            assert_eq!(a, b, "精确查询 '{c}' 不一致");
        }
        // 不存在 code 两者都空。
        assert!(wdb.search("xyz").is_empty() && wdat.search("xyz").is_empty());

        // 前缀：(code,text,weight) 集合一致（含空前缀=全量、单字母、整码）。
        for pre in ["ni", "n", "nih", "zhong", "z", "a", "zh", ""] {
            let mut a: Vec<(String, String, i32)> = wdb
                .search_prefix(pre, 100000)
                .into_iter()
                .map(|e| (e.code, e.text, e.weight))
                .collect();
            let mut b: Vec<(String, String, i32)> = wdat
                .search_prefix(pre, 100000)
                .into_iter()
                .map(|e| (e.code, e.text, e.weight))
                .collect();
            a.sort();
            b.sort();
            assert_eq!(a, b, "前缀查询 '{pre}' 不一致");
        }

        // for_each_entry：全量 (code,text,weight) 集合一致。
        let mut ea = Vec::new();
        wdb.for_each_entry(&mut |c, t, w| ea.push((c.to_string(), t.to_string(), w)));
        let mut eb = Vec::new();
        wdat.for_each_entry(&mut |c, t, w| eb.push((c.to_string(), t.to_string(), w)));
        ea.sort();
        eb.sort();
        assert_eq!(ea, eb, "全枚举不一致");

        let _ = std::fs::remove_file(&wdb_path);
        let _ = std::fs::remove_file(&wdat_path);
    }

    /// 简拼 AbbrevSection 往返：简拼查得到、按权重排序，且**不污染全拼**精确/前缀查询。
    ///
    /// ⚠️ **条目内容已随 v5 从「词」改为「全拼码」**（二级索引指向主键）。取出的
    /// `DictEntry::text` 现在装的是码，调用方拿它去主表装配候选。
    #[test]
    fn abbrev_section_roundtrip() {
        let p = std::env::temp_dir().join("wdat_abbrev_test.wdat");
        let mut w = WdatWriter::new();
        w.add("nihao".into(), vec![("你好".into(), 1200)]);
        w.add("beijing".into(), vec![("北京".into(), 2000)]);
        // 存码不存词；同一简拼下多个码按权重降序（权重只用于截断时取舍）。
        w.add_abbrev(
            "nh".into(),
            vec![("nihao".into(), 1200), ("nihuo".into(), 5)],
        );
        w.add_abbrev("bj".into(), vec![("beijing".into(), 2000)]);
        w.add_abbrev("nhm".into(), vec![("nihaoma".into(), 300)]);
        w.write(&p).unwrap();

        let r = WdatReader::open(&p).unwrap();
        assert!(r.has_abbrev());
        // 简拼命中 + 权重降序；取出的是**全拼码**。
        let nh = r.search_abbrev("nh", 10);
        assert_eq!(nh.len(), 2);
        assert_eq!(nh[0].text, "nihao", "按权重降序，且存的是码: {nh:?}");
        assert_eq!(nh[0].code, "nh");
        assert_eq!(r.search_abbrev("bj", 10)[0].text, "beijing");
        assert_eq!(r.search_abbrev("nhm", 10)[0].text, "nihaoma");
        assert!(r.search_abbrev("zzz", 10).is_empty());
        // **不污染全拼**：全拼 search/search_prefix 不应命中简拼码。
        assert!(r.search("nh").is_empty(), "全拼精确不应命中简拼码 nh");
        assert_eq!(r.search("nihao")[0].text, "你好");
        let pre = r.search_prefix("n", 100);
        assert!(pre.iter().any(|e| e.text == "你好"));
        assert!(
            !pre.iter().any(|e| e.code == "nh" || e.code == "nhm"),
            "前缀查询不应含简拼码: {:?}",
            pre.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 无简拼区段时 search_abbrev 返回空、has_abbrev=false。
    #[test]
    fn no_abbrev_section() {
        let p = build("wdat_noabbrev_test.wdat", &[("a", &[("工", 1)])]);
        let r = WdatReader::open(&p).unwrap();
        assert!(!r.has_abbrev());
        assert!(r.search_abbrev("nh", 10).is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn shared_prefix_and_branching() {
        // 强分支：同前缀大量分叉，验证 base/check 冲突解决正确。
        let data: Vec<(String, Vec<(String, i32)>)> = (0..200)
            .map(|i| (format!("code{i:03}"), vec![(format!("词{i}"), i)]))
            .collect();
        let p = std::env::temp_dir().join("wdat_branch_test.wdat");
        let mut w = WdatWriter::new();
        for (c, e) in &data {
            w.add(c.clone(), e.clone());
        }
        w.write(&p).unwrap();
        let r = WdatReader::open(&p).unwrap();
        assert_eq!(r.key_count(), 200);
        for i in 0..200 {
            let res = r.search(&format!("code{i:03}"));
            assert_eq!(res.len(), 1, "code{i:03} 应命中");
            assert_eq!(res[0].text, format!("词{i}"));
            assert_eq!(res[0].weight, i);
        }
        // 前缀 code0 → code000..code099（100 条）。
        let pre = r.search_prefix("code0", 1000);
        assert_eq!(pre.len(), 100, "code0 前缀应 100 条");
        let _ = std::fs::remove_file(&p);
    }
}
