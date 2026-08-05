//! Store 桥接层：把 wind-store 的用户词 / 临时词包成 `DictLayer`，挂进 CompositeDict。
//!
//! 见 docs/redesign/dict.md §3。词频/shadow 不在此（词频是排序独立维度 frequency.md；
//! shadow 是 ShadowProvider，引擎排序后应用）——本文件只负责"用户词/临时词作为查询层"。

use crate::layer::{DictLayer, LayerType};
use std::sync::Arc;
use wind_candidate::{Candidate, better};
use wind_store::Store;
use wind_store::user_words::UserWordRecord;

/// 把用户/临时词记录映射为候选；`is_temp` 决定 meta 标记，`is_prefix` 标记前缀补全。
///
/// ⚠️ **`boundary` 必须显式带上**。P2a 贯通边界时改的是 `SystemDictLayer`，但有两条旁路
/// 绕开它：`PinyinEngine` 直接持有 `CachedDict`（P2b 已补），以及本层——记录里
/// （`user_words.rs` 的 24B value）明明存着边界、`search_user_words_prefix` 也读了出来，
/// 到这里被 `..Default::default()` 吃掉，于是**用户词候选的 boundary 恒为 0**，
/// 双拼边界校验、长词上浮判据、自动造词沿用边界四处一并失效。
/// 见 docs/design/pinyin-code-domains.md §3 L2。
fn record_to_candidate(r: UserWordRecord, is_temp: bool, is_prefix: bool) -> Candidate {
    let mut c = Candidate {
        // 库里存的就是真实文本（含真换行），**此处不做任何转义处理**。
        //
        // 转义只发生在系统边界上：文本文件（`.dict.yaml`/导入/导出）与设置页 UI
        // 各自进出时转换，数据库与内存中一律是真实文本。若在这里反转义，等于要求
        // 每个写入端都先转义过——而写入端有 8 条（设置页 ×3、快捷加词、自动造词、
        // 导入、临时词、备份还原），漏一条，该路径写入的 `C:\note` 就会被展开成
        // `C:` + 换行 + `ote`，且静默无感。
        text: r.text,
        code: r.code,
        weight: r.weight,
        boundary: r.boundary,
        is_prefix,
        ..Default::default()
    };
    c.meta.raw_weight = r.weight;
    if is_temp {
        c.meta.is_temp_dict = true;
    } else {
        c.meta.is_user_dict = true;
    }
    c
}

fn sort_trunc(mut v: Vec<Candidate>, limit: usize) -> Vec<Candidate> {
    v.sort_by(better);
    if limit > 0 {
        v.truncate(limit);
    }
    v
}

/// 用户造词层（redb 后端，可变；写经 Store 的 add/remove/update）。
pub struct StoreUserLayer {
    store: Arc<Store>,
    schema_id: String,
    name: String,
}

impl StoreUserLayer {
    pub fn new(store: Arc<Store>, schema_id: impl Into<String>) -> Self {
        let schema_id = schema_id.into();
        let name = format!("user:{schema_id}");
        Self {
            store,
            schema_id,
            name,
        }
    }
}

impl DictLayer for StoreUserLayer {
    fn name(&self) -> &str {
        &self.name
    }

    fn layer_type(&self) -> LayerType {
        LayerType::User
    }

    fn search(&self, code: &str, limit: usize) -> Vec<Candidate> {
        let recs = self
            .store
            .get_user_words(&self.schema_id, code)
            .unwrap_or_default();
        let cands = recs
            .into_iter()
            .map(|r| record_to_candidate(r, false, false))
            .collect();
        sort_trunc(cands, limit)
    }

    fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<Candidate> {
        let recs = self
            .store
            .search_user_words_prefix(&self.schema_id, prefix, limit)
            .unwrap_or_default();
        let cands = recs
            .into_iter()
            .map(|r| record_to_candidate(r, false, true))
            .collect();
        sort_trunc(cands, limit)
    }
}

/// 临时学习词层（redb 后端，可变）。
pub struct StoreTempLayer {
    store: Arc<Store>,
    schema_id: String,
    name: String,
}

impl StoreTempLayer {
    pub fn new(store: Arc<Store>, schema_id: impl Into<String>) -> Self {
        let schema_id = schema_id.into();
        let name = format!("temp:{schema_id}");
        Self {
            store,
            schema_id,
            name,
        }
    }
}

impl DictLayer for StoreTempLayer {
    fn name(&self) -> &str {
        &self.name
    }

    fn layer_type(&self) -> LayerType {
        LayerType::Temp
    }

    fn search(&self, code: &str, limit: usize) -> Vec<Candidate> {
        let recs = self
            .store
            .get_temp_words(&self.schema_id, code)
            .unwrap_or_default();
        let cands = recs
            .into_iter()
            .map(|r| record_to_candidate(r, true, false))
            .collect();
        sort_trunc(cands, limit)
    }

    fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<Candidate> {
        let recs = self
            .store
            .search_temp_words_prefix(&self.schema_id, prefix, limit)
            .unwrap_or_default();
        let cands = recs
            .into_iter()
            .map(|r| record_to_candidate(r, true, true))
            .collect();
        sort_trunc(cands, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite::CompositeDict;

    fn store(name: &str) -> Arc<Store> {
        let p = std::env::temp_dir().join(format!("wind_storelayer_{name}.redb"));
        let _ = std::fs::remove_file(&p);
        Arc::new(Store::open(&p).unwrap())
    }

    /// **边界必须穿过「记录 → 候选」这一层**（P2a 漏掉的第二条旁路，见 record_to_candidate 注释）。
    ///
    /// 存储层一直存着边界（24B value），`search_user_words_prefix` 也一直读得出来，
    /// 但转候选时被 `..Default::default()` 吃掉 ⇒ 用户词候选 boundary 恒 0，
    /// 双拼校验 / 长词上浮 / 自动造词沿用边界一并静默失效。
    ///
    /// 三条取候选的路径都要守（精确、前缀、临时层），漏一条就等于没修。
    #[test]
    fn boundary_reaches_candidate_from_store() {
        let s = store("boundary_passthrough");
        // ni|hao → 起始 {0,2}；xi|an|ning → {0,2,4}
        s.add_user_word("pinyin", "nihao", "你好", 500, 0b101)
            .unwrap();
        s.learn_temp_word("pinyin", "xianning", "西安宁", 800, 0b10101)
            .unwrap();

        let ul = StoreUserLayer::new(s.clone(), "pinyin");
        assert_eq!(ul.search("nihao", 10)[0].boundary, 0b101, "精确查询");
        assert_eq!(
            ul.search_prefix("ni", 10)[0].boundary,
            0b101,
            "前缀补全（用户长词上浮判据吃这个值）"
        );

        let tl = StoreTempLayer::new(s.clone(), "pinyin");
        assert_eq!(tl.search("xianning", 10)[0].boundary, 0b10101, "临时层");

        // 无边界记录仍是 0（旧数据 / 手输码 → 消费方降级回 DAG），不得凭空造出边界
        s.add_user_word("pinyin", "abcd", "工作", 100, 0).unwrap();
        assert_eq!(ul.search("abcd", 10)[0].boundary, 0);
    }

    #[test]
    fn test_user_layer_search() {
        let s = store("user_search");
        s.add_user_word("wb", "a", "工", 100, 0).unwrap();
        s.add_user_word("wb", "abc", "啊吧次", 50, 0).unwrap();
        let layer = StoreUserLayer::new(s.clone(), "wb");
        assert_eq!(layer.layer_type(), LayerType::User);
        let exact = layer.search("a", 10);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].text, "工");
        assert!(exact[0].meta.is_user_dict);
        // 前缀 "a" 命中 a / abc
        assert_eq!(layer.search_prefix("a", 10).len(), 2);
    }

    #[test]
    fn test_is_prefix_flag_search_vs_search_prefix() {
        // TDD: 验证 search 返回 is_prefix=false，search_prefix 返回 is_prefix=true
        let s = store("is_prefix_flag");
        s.add_user_word("wb", "abc", "啊吧次", 50, 0).unwrap();
        let layer = StoreUserLayer::new(s.clone(), "wb");

        // 精确匹配：is_prefix 应为 false
        let exact = layer.search("abc", 10);
        assert_eq!(exact.len(), 1);
        assert!(
            !exact[0].is_prefix,
            "search() 返回的候选 is_prefix 应为 false"
        );

        // 前缀匹配：is_prefix 应为 true
        let prefix = layer.search_prefix("ab", 10);
        assert_eq!(prefix.len(), 1);
        assert!(
            prefix[0].is_prefix,
            "search_prefix() 返回的候选 is_prefix 应为 true"
        );
    }

    #[test]
    fn test_temp_layer_is_prefix_flag() {
        // StoreTempLayer 的 search_prefix 同样应标记 is_prefix=true
        let s = store("temp_is_prefix_flag");
        s.learn_temp_word("wb", "xyz", "某词", 100, 0).unwrap();
        let layer = StoreTempLayer::new(s.clone(), "wb");

        let exact = layer.search("xyz", 10);
        assert_eq!(exact.len(), 1);
        assert!(!exact[0].is_prefix, "临时层 search() is_prefix 应为 false");

        let prefix = layer.search_prefix("xy", 10);
        assert_eq!(prefix.len(), 1);
        assert!(
            prefix[0].is_prefix,
            "临时层 search_prefix() is_prefix 应为 true"
        );
    }

    #[test]
    fn test_temp_layer_and_composite() {
        let s = store("temp_composite");
        s.add_user_word("wb", "ni", "你", 100, 0).unwrap();
        s.learn_temp_word("wb", "ni", "拟", 800, 0).unwrap();
        let composite = CompositeDict::new();
        composite.register_layer(Box::new(StoreUserLayer::new(s.clone(), "wb")));
        composite.register_layer(Box::new(StoreTempLayer::new(s.clone(), "wb")));
        // composite 跨层查 "ni" → 你(user) + 拟(temp)
        let got = composite.search("ni", 10);
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|c| c.text == "你" && c.meta.is_user_dict));
        assert!(got.iter().any(|c| c.text == "拟" && c.meta.is_temp_dict));
    }
}
