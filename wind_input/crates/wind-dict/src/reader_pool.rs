//! wdat / unigram mmap reader 的进程级共享池。
//!
//! 同一个缓存文件常被多个方案引用：`pinyin.schema.toml` 与 `shuangpin.schema.toml` 都指向
//! `pinyin/rime_frost.dict.yaml`，混输方案（`wubi86_pinyin`）还会再递归建一套子引擎。而
//! `EngineManager::cache_path` 用**源文件父目录名**做命名空间，三者最终都解析到同一个
//! `<cache>/pinyin/rime_frost.merged.wdat` —— 实测该 62MB 文件被 mmap 三份、`unigram.wdb`
//! 三份、`wubi86_jidian.wdat` 两份。本池按缓存文件路径复用同一个 reader。
//!
//! # 为什么池里存 `Weak` 而不是 `Arc`
//!
//! 池**不持有**强引用：最后一个引擎释放后 `Arc` 计数归零，mmap 随即解除。这在 Windows 上
//! 是必需的 —— 文件被 mmap 期间 `rename`/删除会 Access Denied，而词库重建全部要 rename
//! 覆盖（`CachedDict::write_cache`、combined/merged 重写、`write_unigram_wdb`）。若池持强
//! 引用，reader 将永久驻留，重建会从「偶发失败」恶化成「永久失败」。
//!
//! 存 `Weak` 则天然保住既有的释放语义：`EngineManager::reload_from_config` 的
//! `engines.clear()` 与 `invalidate_schema` 的 `engines.remove()` 依旧是有效释放点，
//! 无需在池上再叠一层手工引用计数或强制关闭通道。
//!
//! # key 的选取
//!
//! key 是缓存文件路径本身，**不含大小/mtime** —— 新鲜度判定归 `cache_fp`（内容指纹），
//! 本池只负责「同一路径只 mmap 一份」，两者职责分离。这也遵循本 crate 既有共识：用内容
//! 指纹而非 mtime，以免部署刷新 mtime 导致恒重建。
//!
//! 路径直接做 key 而不 `canonicalize`：缓存路径统一由 `cache_path`（源路径的纯函数）生成，
//! 同一文件必然得到同一字符串。万一将来出现不同写法，后果也只是退化成各开一份
//! （即本池引入前的行为），不会取到错误的 reader。

use crate::datformat::WdatReader;
use crate::unigram::UnigramReader;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

type Pool<T> = OnceLock<Mutex<HashMap<PathBuf, Weak<T>>>>;

static WDAT_POOL: Pool<WdatReader> = OnceLock::new();
static UNIGRAM_POOL: Pool<UnigramReader> = OnceLock::new();

/// 打开 wdat；同一路径已有存活 reader 时复用，不再新建映射。
pub fn open_wdat(path: &Path) -> anyhow::Result<Arc<WdatReader>> {
    get_or_open(WDAT_POOL.get_or_init(Default::default), path, |p| {
        WdatReader::open(p)
    })
}

/// 打开 unigram.wdb；同一路径已有存活 reader 时复用，不再新建映射。
pub fn open_unigram(path: &Path) -> anyhow::Result<Arc<UnigramReader>> {
    get_or_open(UNIGRAM_POOL.get_or_init(Default::default), path, |p| {
        UnigramReader::open(p)
    })
}

fn get_or_open<T>(
    pool: &Mutex<HashMap<PathBuf, Weak<T>>>,
    path: &Path,
    open: impl FnOnce(&Path) -> anyhow::Result<T>,
) -> anyhow::Result<Arc<T>> {
    let mut guard = pool.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(alive) = guard.get(path).and_then(Weak::upgrade) {
        return Ok(alive);
    }
    // 未命中，或条目已失效（上一批持有者已释放，文件可能已被重建过）：重新打开。
    //
    // open 放在锁内：mmap 只是建立映射不读盘（按需分页），耗时以微秒计；且引擎构建本就
    // 被 `EngineManager::build_locks` 串行化过，不值得为此引入「锁外构建 + 双检」的两段式。
    let reader = Arc::new(open(path)?);
    guard.insert(path.to_path_buf(), Arc::downgrade(&reader));
    // 顺带清掉失效条目，避免 map 随方案增删单调增长。
    guard.retain(|_, w| w.strong_count() > 0);
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codetable::CodetableDict;
    use crate::datformat::WdatWriter;

    /// 造一个最小可用的 wdat，返回其路径。
    fn make_wdat(dir: &Path, name: &str, code: &str, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut d = CodetableDict::empty();
        d.merge_single(code.into(), text.into(), 1, 0);
        let mut w = WdatWriter::new();
        d.export_to_wdat(&mut w);
        w.write(&path).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wind-reader-pool-{}-{}", std::process::id(), tag))
    }

    #[test]
    fn same_path_shares_one_reader() {
        let dir = temp_dir("share");
        let p = make_wdat(&dir, "a.wdat", "a", "啊");

        let r1 = open_wdat(&p).unwrap();
        let r2 = open_wdat(&p).unwrap();
        assert!(
            Arc::ptr_eq(&r1, &r2),
            "同一路径必须复用同一个 reader（这正是本池的目的）"
        );
        assert_eq!(Arc::strong_count(&r1), 2, "两个持有者");
        // 复用的 reader 功能正常
        assert_eq!(r2.search("a").len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn different_paths_do_not_share() {
        let dir = temp_dir("distinct");
        let p1 = make_wdat(&dir, "b.wdat", "b", "波");
        let p2 = make_wdat(&dir, "c.wdat", "c", "此");

        let r1 = open_wdat(&p1).unwrap();
        let r2 = open_wdat(&p2).unwrap();
        assert!(!Arc::ptr_eq(&r1, &r2));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 池存 Weak 的核心契约：持有者全部释放后 mmap 必须解除，否则 Windows 上词库重建
    /// 的 rename 会永久失败。用「释放后能否覆写该文件」来验证映射确实断开了。
    #[test]
    fn dropping_all_holders_releases_the_mapping() {
        let dir = temp_dir("release");
        let p = make_wdat(&dir, "d.wdat", "d", "的");

        let r = open_wdat(&p).unwrap();
        assert_eq!(Arc::strong_count(&r), 1);
        drop(r);

        // 全部持有者已释放 → 文件不再被映射 → 可覆写（Windows 上映射未解除时这里会失败）
        let mut d = CodetableDict::empty();
        d.merge_single("dd".into(), "地".into(), 1, 0);
        let mut w = WdatWriter::new();
        d.export_to_wdat(&mut w);
        w.write(&p).expect("持有者释放后必须能覆写词库文件");

        // 失效条目不会被复用：重新打开应拿到覆写后的新内容
        let r2 = open_wdat(&p).unwrap();
        assert_eq!(r2.search("dd").len(), 1, "应读到覆写后的新内容");
        assert_eq!(r2.search("d").len(), 0, "旧内容不应再出现");

        drop(r2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
