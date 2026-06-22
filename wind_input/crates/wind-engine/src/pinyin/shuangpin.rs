//! 双拼转换器与布局
//!
//! 布局以 TOML 三表分区声明（data/schemas/shuangpin/<id>.toml），与 Go
//! `wind_input/internal/engine/pinyin/shuangpin/` 对齐，但方案数据外置不硬编码。

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// 双拼布局：键位 → 声母/韵母/零声母映射。
#[derive(Debug, Clone)]
pub struct Layout {
    pub id: String,
    pub name: String,
    initials: HashMap<u8, String>,
    finals: HashMap<u8, Vec<String>>,
    zero_initials: HashMap<u8, Vec<String>>,
}

#[derive(Deserialize)]
struct RawLayout {
    meta: RawMeta,
    #[serde(default)]
    initials: HashMap<String, String>,
    #[serde(default)]
    finals: HashMap<String, Vec<String>>,
    #[serde(default)]
    zero_initials: HashMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct RawMeta {
    id: String,
    name: String,
}

/// 单字节键转换（布局键均为单 ASCII 字符）。
fn key_byte(s: &str) -> anyhow::Result<u8> {
    let b = s.as_bytes();
    if b.len() != 1 {
        anyhow::bail!("布局键必须为单字符: {:?}", s);
    }
    Ok(b[0])
}

impl Layout {
    pub fn from_toml(path: &Path) -> anyhow::Result<Layout> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("读取双拼布局 {} 失败: {}", path.display(), e))?;
        Self::from_str(&text)
    }

    pub fn from_str(toml_text: &str) -> anyhow::Result<Layout> {
        let raw: RawLayout = toml::from_str(toml_text)?;
        let mut initials: HashMap<u8, String> = HashMap::new();
        // 声母自映射补全：26 字母默认映射自身
        for c in b'a'..=b'z' {
            initials.insert(c, (c as char).to_string());
        }
        // 显式声母覆盖
        for (k, v) in raw.initials {
            initials.insert(key_byte(&k)?, v);
        }
        let mut finals = HashMap::new();
        for (k, v) in raw.finals {
            finals.insert(key_byte(&k)?, v);
        }
        let mut zero_initials = HashMap::new();
        for (k, v) in raw.zero_initials {
            zero_initials.insert(key_byte(&k)?, v);
        }
        if finals.is_empty() {
            anyhow::bail!("双拼布局 {} 缺少 [finals]", raw.meta.id);
        }
        Ok(Layout {
            id: raw.meta.id,
            name: raw.meta.name,
            initials,
            finals,
            zero_initials,
        })
    }

    pub fn initial_of(&self, key: u8) -> Option<&str> {
        self.initials.get(&key).map(|s| s.as_str())
    }
    pub fn finals_of(&self, key: u8) -> &[String] {
        self.finals.get(&key).map(|v| v.as_slice()).unwrap_or(&[])
    }
    pub fn zero_of(&self, key: u8) -> &[String] {
        self.zero_initials.get(&key).map(|v| v.as_slice()).unwrap_or(&[])
    }
    pub fn is_final_key(&self, key: u8) -> bool {
        self.finals.contains_key(&key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XIAOHE: &str = r#"
[meta]
id = "xiaohe"
name = "小鹤双拼"
[initials]
v = "zh"
i = "ch"
u = "sh"
[finals]
o = ["uo", "o"]
k = ["uai", "ing"]
v = ["ui", "v"]
[zero_initials]
a = ["a", "ai", "an", "ang", "ao"]
"#;

    #[test]
    fn layout_parse_and_self_map() {
        let lay = Layout::from_str(XIAOHE).unwrap();
        assert_eq!(lay.id, "xiaohe");
        assert_eq!(lay.name, "小鹤双拼");
        // 显式声母
        assert_eq!(lay.initial_of(b'v'), Some("zh"));
        assert_eq!(lay.initial_of(b'i'), Some("ch"));
        // 自映射补全：未在 [initials] 列出的普通声母键映射自身
        assert_eq!(lay.initial_of(b'b'), Some("b"));
        assert_eq!(lay.initial_of(b'p'), Some("p"));
        // 韵母多值
        assert_eq!(lay.finals_of(b'o'), &["uo".to_string(), "o".to_string()]);
        assert!(lay.is_final_key(b'k'));
        assert!(!lay.is_final_key(b'q')); // q 不在 finals
        // 零声母
        assert_eq!(lay.zero_of(b'a').len(), 5);
    }

    #[test]
    fn layout_symbol_key_as_final() {
        let t = "[meta]\nid=\"x\"\nname=\"x\"\n[finals]\n\";\" = [\"ing\"]\n";
        let lay = Layout::from_str(t).unwrap();
        assert!(lay.is_final_key(b';'));
        assert_eq!(lay.finals_of(b';'), &["ing".to_string()]);
    }

    #[test]
    fn builtin_layouts_load() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        for id in ["xiaohe", "ziranma", "mspy", "sogou", "abc", "ziguang"] {
            let p = dir.join(format!("{id}.toml"));
            let lay = Layout::from_toml(&p)
                .unwrap_or_else(|e| panic!("加载 {id} 失败: {e}"));
            assert_eq!(lay.id, id, "{id} 的 meta.id 不匹配");
            assert!(lay.is_final_key(b'k'), "{id} 应有 finals['k']");
        }
        // 差异点对拍：小鹤 ao=c，ai=d
        let xh = Layout::from_toml(&dir.join("xiaohe.toml")).unwrap();
        assert_eq!(xh.finals_of(b'c'), &["ao".to_string()], "小鹤 finals[c] 应为 ao");
        assert_eq!(xh.finals_of(b'd'), &["ai".to_string()], "小鹤 finals[d] 应为 ai");
        assert_eq!(xh.initial_of(b'v'), Some("zh"), "小鹤 initials[v] 应为 zh");

        // 自然码 ao=k，ai=l（与小鹤不同）
        let zrm = Layout::from_toml(&dir.join("ziranma.toml")).unwrap();
        assert_eq!(zrm.finals_of(b'k'), &["ao".to_string()], "自然码 finals[k] 应为 ao");
        assert_eq!(zrm.finals_of(b'l'), &["ai".to_string()], "自然码 finals[l] 应为 ai");

        // 微软 ;=ing
        let ms = Layout::from_toml(&dir.join("mspy.toml")).unwrap();
        assert!(ms.is_final_key(b';'), "微软双拼应有 finals[;]");
        assert_eq!(ms.finals_of(b';'), &["ing".to_string()], "微软 finals[;] 应为 ing");

        // 智能ABC a=zh（声母），zero_initials 只有 o
        let abc = Layout::from_toml(&dir.join("abc.toml")).unwrap();
        assert_eq!(abc.initial_of(b'a'), Some("zh"), "智能ABC initials[a] 应为 zh");
        assert_eq!(abc.initial_of(b'e'), Some("ch"), "智能ABC initials[e] 应为 ch");
        assert_eq!(abc.initial_of(b'v'), Some("sh"), "智能ABC initials[v] 应为 sh");
        assert!(abc.zero_of(b'a').is_empty(), "智能ABC zero_initials 不应有 a 键（a=zh 冲突）");

        // 紫光 u=zh，i=sh，a=ch；zero_initials 无 a
        let zg = Layout::from_toml(&dir.join("ziguang.toml")).unwrap();
        assert_eq!(zg.initial_of(b'u'), Some("zh"), "紫光 initials[u] 应为 zh");
        assert_eq!(zg.initial_of(b'i'), Some("sh"), "紫光 initials[i] 应为 sh");
        assert_eq!(zg.initial_of(b'a'), Some("ch"), "紫光 initials[a] 应为 ch");
        assert!(zg.zero_of(b'a').is_empty(), "紫光 zero_initials 不应有 a 键（a=ch 冲突）");
        assert!(zg.is_final_key(b';'), "紫光应有 finals[;]");
        assert_eq!(zg.finals_of(b';'), &["ing".to_string()], "紫光 finals[;] 应为 ing");
    }
}
