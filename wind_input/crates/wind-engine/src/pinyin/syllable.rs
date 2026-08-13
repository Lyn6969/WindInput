//! 音节 Trie（~400 个合法拼音音节）
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/syllable_trie.go` 对齐。
//! 使用 HashMap Trie 实现高效音节边界检测。

use std::collections::HashMap;

/// 音节 Trie 节点
#[derive(Default)]
struct TrieNode {
    children: HashMap<u8, TrieNode>,
    is_end: bool,
}

/// 音节 Trie
pub struct SyllableTrie {
    root: TrieNode,
}

impl Default for SyllableTrie {
    fn default() -> Self {
        Self::new()
    }
}

impl SyllableTrie {
    pub fn new() -> Self {
        let mut trie = Self {
            root: TrieNode::default(),
        };
        trie.load_standard_syllables();
        trie
    }

    fn load_standard_syllables(&mut self) {
        for syl in STANDARD_SYLLABLES {
            self.insert(syl);
        }
    }

    fn insert(&mut self, syl: &str) {
        let mut node = &mut self.root;
        for byte in syl.bytes() {
            node = node.children.entry(byte).or_default();
        }
        node.is_end = true;
    }
}

/// 标准普通话音节全集（约 410 个，封闭集）。
/// 供 SyllableTrie 构建，以及造词时遍历查词典反推单字读音（generate::CharPinyinIndex）。
pub const STANDARD_SYLLABLES: &[&str] = &[
    "a", "ai", "an", "ang", "ao", "ba", "bai", "ban", "bang", "bao", "bei", "ben", "beng", "bi",
    "bian", "biao", "bie", "bin", "bing", "bo", "bu", "ca", "cai", "can", "cang", "cao", "ce",
    "cen", "ceng", "cha", "chai", "chan", "chang", "chao", "che", "chen", "cheng", "chi", "chong",
    "chou", "chu", "chua", "chuai", "chuan", "chuang", "chui", "chun", "chuo", "ci", "cong", "cou",
    "cu", "cuan", "cui", "cun", "cuo", "da", "dai", "dan", "dang", "dao", "de", "dei", "den",
    "deng", "di", "dian", "diao", "die", "ding", "diu", "dong", "dou", "du", "duan", "dui", "dun",
    "duo", "e", "ei", "en", "eng", "er", "fa", "fan", "fang", "fei", "fen", "feng", "fo", "fou",
    "fu", "ga", "gai", "gan", "gang", "gao", "ge", "gei", "gen", "geng", "gong", "gou", "gu",
    "gua", "guai", "guan", "guang", "gui", "gun", "guo", "ha", "hai", "han", "hang", "hao", "he",
    "hei", "hen", "heng", "hong", "hou", "hu", "hua", "huai", "huan", "huang", "hui", "hun", "huo",
    "ji", "jia", "jian", "jiang", "jiao", "jie", "jin", "jing", "jiong", "jiu", "ju", "juan",
    "jue", "jun", "ka", "kai", "kan", "kang", "kao", "ke", "ken", "keng", "kong", "kou", "ku",
    "kua", "kuai", "kuan", "kuang", "kui", "kun", "kuo", "la", "lai", "lan", "lang", "lao", "le",
    "lei", "leng", "li", "lia", "lian", "liang", "liao", "lie", "lin", "ling", "liu", "lo", "long",
    "lou", "lu", "luan", "lun", "luo", "lv", "lve", "ma", "mai", "man", "mang", "mao", "me", "mei",
    "men", "meng", "mi", "mian", "miao", "mie", "min", "ming", "miu", "mo", "mou", "mu", "na",
    "nai", "nan", "nang", "nao", "ne", "nei", "nen", "neng", "ni", "nian", "niang", "niao", "nie",
    "nin", "ning", "niu", "nong", "nou", "nu", "nuan", "nuo", "nv", "nve", "o", "ou", "pa", "pai",
    "pan", "pang", "pao", "pei", "pen", "peng", "pi", "pian", "piao", "pie", "pin", "ping", "po",
    "pou", "pu", "qi", "qia", "qian", "qiang", "qiao", "qie", "qin", "qing", "qiong", "qiu", "qu",
    "quan", "que", "qun", "ran", "rang", "rao", "re", "ren", "reng", "ri", "rong", "rou", "ru",
    "ruan", "rui", "run", "ruo", "sa", "sai", "san", "sang", "sao", "se", "sen", "seng", "sha",
    "shai", "shan", "shang", "shao", "she", "shen", "sheng", "shi", "shou", "shu", "shua", "shuai",
    "shuan", "shuang", "shui", "shun", "shuo", "si", "song", "sou", "su", "suan", "sui", "sun",
    "suo", "ta", "tai", "tan", "tang", "tao", "te", "teng", "ti", "tian", "tiao", "tie", "ting",
    "tong", "tou", "tu", "tuan", "tui", "tun", "tuo", "wa", "wai", "wan", "wang", "wei", "wen",
    "weng", "wo", "wu", "xi", "xia", "xian", "xiang", "xiao", "xie", "xin", "xing", "xiong", "xiu",
    "xu", "xuan", "xue", "xun", "ya", "yan", "yang", "yao", "ye", "yi", "yin", "ying", "yong",
    "you", "yu", "yuan", "yue", "yun", "za", "zai", "zan", "zang", "zao", "ze", "zei", "zen",
    "zeng", "zha", "zhai", "zhan", "zhang", "zhao", "zhe", "zhen", "zheng", "zhi", "zhong", "zhou",
    "zhu", "zhua", "zhuai", "zhuan", "zhuang", "zhui", "zhun", "zhuo", "zi", "zong", "zou", "zu",
    "zuan", "zui", "zun", "zuo",
    // 与 Go syllable_trie.go / shuangpin.validPinyinSyllables 对齐补全的稀有音节
    // （双拼转换真值依赖：紫光 ik→shei、ziguang 等；以及 kei/tei/zhei/nun/rua/yo）。
    "kei", "tei", "zhei", "shei", "nun", "rua", "yo",
];

impl SyllableTrie {
    /// 在指定位置匹配所有可能的音节（最长优先）
    pub fn match_at(&self, input: &str, pos: usize) -> Vec<String> {
        let bytes = input.as_bytes();
        let mut matches = Vec::new();
        let mut node = &self.root;

        for i in pos..bytes.len() {
            match node.children.get(&bytes[i]) {
                Some(child) => {
                    node = child;
                    if node.is_end {
                        matches.push(input[pos..=i].to_string());
                    }
                }
                None => break,
            }
        }

        matches.reverse(); // 最长优先
        matches
    }

    /// 检查是否为合法音节
    pub fn is_syllable(&self, s: &str) -> bool {
        let mut node = &self.root;
        for byte in s.bytes() {
            match node.children.get(&byte) {
                Some(child) => node = child,
                None => return false,
            }
        }
        node.is_end
    }

    /// 检查是否为合法音节的前缀
    pub fn is_prefix(&self, s: &str) -> bool {
        let mut node = &self.root;
        for byte in s.bytes() {
            match node.children.get(&byte) {
                Some(child) => node = child,
                None => return false,
            }
        }
        true
    }
}
