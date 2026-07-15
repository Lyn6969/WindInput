//! 词库多段导入导出编排（用户词库 / 临时词库 / 词频 / 候选调整）。
//!
//! 把 store 各数据域原语组合成「一个 .wdict 文件多段、按类型可选」的导入导出，对齐旧 Go
//! wind_dict 一文件多段的形态。段与引擎类型的适用关系（码表四段 / 混输仅候选调整 /
//! 拼音三段）由调用方（RPC / UI）决定，本模块只按传入的 sections 处理。

use crate::store::Store;
use crate::user_words::WordsImportCounts;
use crate::wdict::{self, DictWdict, FreqIo, WordIo};

/// 词库数据段类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictSection {
    /// 用户词库
    UserWords,
    /// 临时词库
    TempWords,
    /// 词频
    Freq,
    /// 候选调整（shadow）
    Shadow,
}

impl DictSection {
    /// RPC/UI 稳定标识（camelCase）。
    pub fn key(&self) -> &'static str {
        match self {
            DictSection::UserWords => "userWords",
            DictSection::TempWords => "tempWords",
            DictSection::Freq => "freq",
            DictSection::Shadow => "shadow",
        }
    }

    /// wdict 段标签（`--- !<tag>`）。
    pub fn tag(&self) -> &'static str {
        match self {
            DictSection::UserWords => "words",
            DictSection::TempWords => "temp_words",
            DictSection::Freq => "freq",
            DictSection::Shadow => "shadow",
        }
    }

    /// 从 RPC key / wdict 段名解析（宽松：兼容多种写法）。
    pub fn from_key(k: &str) -> Option<Self> {
        match k {
            "userWords" | "words" | "user_words" => Some(DictSection::UserWords),
            "tempWords" | "temp_words" => Some(DictSection::TempWords),
            "freq" => Some(DictSection::Freq),
            "shadow" => Some(DictSection::Shadow),
            _ => None,
        }
    }
}

/// 单段导入结果（供 RPC 序列化）。
#[derive(Debug, Clone, Default)]
pub struct SectionImport {
    pub key: &'static str,
    /// 用户词库分类计数（仅 UserWords）。
    pub words: Option<WordsImportCounts>,
    /// 其余段的导入条数。
    pub imported: usize,
    pub skipped: usize,
}

/// 多段导入结果。
#[derive(Debug, Clone, Default)]
pub struct DictImportReport {
    pub sections: Vec<SectionImport>,
}

impl Store {
    /// 导出所选数据段为单个多段 wdict 文本。缺数据的段仍会写出（空段），保证文件自描述。
    /// `engine_type` 写入头部供导入时校验（防跨引擎类型误导）；调用方（RPC）解析后传入。
    pub fn export_dict_sections_wdict(
        &self,
        schema: &str,
        sections: &[DictSection],
        exported_at: &str,
        engine_type: &str,
    ) -> anyhow::Result<String> {
        let mut d = DictWdict::default();
        for sec in sections {
            match sec {
                DictSection::UserWords => d.words = Some(self.collect_user_word_rows(schema)?),
                DictSection::TempWords => {
                    let recs = self.search_temp_words_prefix(schema, "", 0)?;
                    d.temp_words = Some(
                        recs.into_iter()
                            .map(|r| WordIo {
                                code: r.code,
                                text: r.text,
                                weight: r.weight,
                                count: r.count,
                            })
                            .collect(),
                    );
                }
                DictSection::Freq => {
                    let (rows, _total) = self.list_freq_paged(schema, "", 0, 0)?;
                    d.freq = Some(
                        rows.into_iter()
                            .map(|(code, text, rec)| FreqIo {
                                code,
                                text,
                                count: rec.count,
                                last_used: rec.last_used,
                            })
                            .collect(),
                    );
                }
                DictSection::Shadow => d.shadow = Some(self.export_shadow_actions(schema)?),
            }
        }
        Ok(wdict::export_dict_sections(
            &d,
            exported_at,
            schema,
            engine_type,
        ))
    }

    /// 从多段 wdict 文本导入所选数据段。**只处理文件中实际存在的段**（防 replace 误清空
    /// 文件未携带的段）。replace=先清该段再导入；否则 Merge。
    pub fn import_dict_sections_wdict(
        &self,
        schema: &str,
        text: &str,
        sections: &[DictSection],
        replace: bool,
    ) -> anyhow::Result<DictImportReport> {
        let present = wdict::sections_present(text);
        let has = |tag: &str| present.iter().any(|t| t == tag);
        let mut rep = DictImportReport::default();
        for sec in sections {
            if !has(sec.tag()) {
                continue; // 文件无此段，跳过（不清空、不计入）
            }
            match sec {
                DictSection::UserWords => {
                    let (rows, skipped) =
                        wdict::parse_words_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
                    if replace {
                        self.clear_user_words(schema)?;
                    }
                    let counts = self.import_user_words(schema, &rows)?;
                    rep.sections.push(SectionImport {
                        key: sec.key(),
                        words: Some(counts),
                        imported: rows.len(),
                        skipped,
                    });
                }
                DictSection::TempWords => {
                    let (rows, skipped) =
                        wdict::parse_temp_words_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
                    if replace {
                        self.clear_temp_words(schema)?;
                    }
                    let n = self.import_temp_word_rows(schema, &rows)?;
                    rep.sections.push(SectionImport {
                        key: sec.key(),
                        words: None,
                        imported: n,
                        skipped,
                    });
                }
                DictSection::Freq => {
                    let (rows, skipped) =
                        wdict::parse_freq_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
                    if replace {
                        self.clear_freq(schema)?;
                    }
                    let n = self.import_freq_rows(schema, &rows)?;
                    rep.sections.push(SectionImport {
                        key: sec.key(),
                        words: None,
                        imported: n,
                        skipped,
                    });
                }
                DictSection::Shadow => {
                    let (actions, skipped) =
                        wdict::parse_shadow_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
                    if replace {
                        self.clear_shadow(schema)?;
                    }
                    let n = self.import_shadow_actions(schema, &actions)?;
                    rep.sections.push(SectionImport {
                        key: sec.key(),
                        words: None,
                        imported: n,
                        skipped,
                    });
                }
            }
        }
        Ok(rep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn all_sections_roundtrip() {
        let path = tmp("wind_dict_sections_io.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 100).unwrap();
        s.on_word_selected("wb", "a", "工", 0, 0).unwrap(); // user count -> 1
        s.learn_temp_word("wb", "ab", "临时", 50).unwrap();
        s.learn_temp_word("wb", "ab", "临时", 50).unwrap(); // temp count -> 2
        s.record_freq("wb", "a", "工").unwrap();
        s.record_freq("wb", "a", "工").unwrap(); // freq count -> 2
        s.pin_shadow("wb", "aaaa", "恭", None, 0).unwrap();
        s.delete_shadow("wb", "bbbb", "见").unwrap();

        let all = [
            DictSection::UserWords,
            DictSection::TempWords,
            DictSection::Freq,
            DictSection::Shadow,
        ];
        let text = s
            .export_dict_sections_wdict("wb", &all, "2026-07-15T00:00:00+08:00", "codetable")
            .unwrap();
        for tag in ["--- !words", "--- !temp_words", "--- !freq", "--- !shadow"] {
            assert!(text.contains(tag), "缺段 {tag}");
        }

        // 导入新库，全段还原
        let path2 = tmp("wind_dict_sections_io2.redb");
        let s2 = Store::open(&path2).unwrap();
        let rep = s2
            .import_dict_sections_wdict("wb", &text, &all, false)
            .unwrap();
        assert_eq!(rep.sections.len(), 4);

        let uw = s2.get_user_words("wb", "a").unwrap();
        assert_eq!(uw[0].weight, 100);
        assert_eq!(uw[0].count, 1, "用户词 count 流转");
        let tw = s2.get_temp_words("wb", "ab").unwrap();
        assert_eq!(tw[0].count, 2, "临时词 count 保真");
        assert_eq!(
            s2.get_freq("wb", "a", "工").unwrap().unwrap().count,
            2,
            "词频 count 流转"
        );
        assert!(
            s2.get_shadow_rules("wb", "aaaa").unwrap().is_some(),
            "pin 还原"
        );
        assert_eq!(
            s2.get_shadow_rules("wb", "bbbb").unwrap().unwrap().deleted,
            vec!["见".to_string()],
            "del 还原"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn selective_export_and_present_guard() {
        let path = tmp("wind_dict_sel.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 100).unwrap();
        s.record_freq("wb", "a", "工").unwrap();

        // 只导词频
        let text = s
            .export_dict_sections_wdict("wb", &[DictSection::Freq], "t", "codetable")
            .unwrap();
        assert!(text.contains("--- !freq"));
        assert!(!text.contains("--- !words"), "未选用户词库不应写出");

        // 目标库已有用户词；即便选了 UserWords，但文件无 words 段 → replace 也不清空
        let path2 = tmp("wind_dict_sel2.redb");
        let s2 = Store::open(&path2).unwrap();
        s2.add_user_word("wb", "z", "旧", 9).unwrap();
        let all = [DictSection::UserWords, DictSection::Freq];
        let rep = s2
            .import_dict_sections_wdict("wb", &text, &all, true)
            .unwrap();
        assert_eq!(rep.sections.len(), 1, "只处理文件存在的 freq 段");
        assert_eq!(rep.sections[0].key, "freq");
        assert!(
            !s2.get_user_words("wb", "z").unwrap().is_empty(),
            "文件无 words 段，replace 不应清空既有用户词"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }
}
