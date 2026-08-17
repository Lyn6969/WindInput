//! 出厂剪贴板反查短语（`cofc`）的守门。
//!
//! **系统短语的语法错误用户零感知**：写错一个函数名或括号，`evaluate_phrase` 返回 Err，
//! 短语层只记一条 warn 就把整条丢掉——用户侧表现为「打了 cofc 什么都没有」，
//! 与「剪贴板是空的」完全同形，靠人是分不出来的。
//!
//! ⚠️ 读的是**仓库** `data/system.phrases.toml` 而不是 `build_dev/data/`：
//! 那份是部署产物，且在 worktree 里是指向主仓的符号链接（改动不会体现），
//! 拿它跑等于验主仓的旧文件。出厂词条的语法是源文件的属性，与部署无关。

use std::path::PathBuf;

fn repo_phrases() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../data/system.phrases.toml")
}

fn layer() -> wind_phrase::PhraseLayer {
    let p = repo_phrases();
    assert!(p.is_file(), "找不到出厂短语文件：{}", p.display());
    wind_phrase::PhraseLayer::load(&p)
}

/// ★ `cofc` 必须能解析并求值出「剪贴板那个字的反查」。
///
/// 断言的是**渲染结果原样等于宿主返回值**，因为出厂词条是纯模板 `{dict.rev(clip())}`——
/// display 不做任何加工，直接就是上屏文本。若有人把它改成 `$CC(...)` 包起来，
/// 这条会因为多出 `command_src` 而变红，提醒他「无动作短语的 display 即上屏文本」
/// 这个前提被打破了。
#[test]
fn factory_cofc_parses_and_renders_clipboard_lookup() {
    let clip = |_n: i64| "好人".to_string();
    // 只认第 1 个字，模拟真实反查（第 2 个字不该被这条词条查到）。
    let reverse = |text: &str, fmt: &str| {
        assert_eq!(text, "好", "出厂词条应只查剪贴板第 1 个字");
        assert!(
            fmt.contains("${code}") && fmt.contains("${pinyin}"),
            "默认版式应含编码与拼音，实际收到 {fmt:?}"
        );
        "好: vbg hǎo".to_string()
    };
    let host = wind_phrase::PhraseHost {
        clip: &clip,
        reverse: &reverse,
    };

    let hits = layer().lookup("cofc", &[], &host);
    assert_eq!(
        hits.len(),
        1,
        "cofc 应恰好产出一条候选（解析失败会得到 0 条）"
    );
    assert_eq!(hits[0].text, "好: vbg hǎo");
    assert!(
        hits[0].command_src.is_none(),
        "出厂 cofc 是纯模板短语：display 即上屏文本，不该是命令候选"
    );
}

/// 剪贴板为空 / 查不到时**不出候选**，而不是出一条点了没反应的空白候选。
#[test]
fn factory_cofc_yields_nothing_when_lookup_is_empty() {
    let reverse = |_t: &str, _f: &str| String::new();

    // 剪贴板为空：没有字可查。
    let empty = |_n: i64| String::new();
    let host = wind_phrase::PhraseHost {
        clip: &empty,
        reverse: &reverse,
    };
    assert!(
        layer().lookup("cofc", &[], &host).is_empty(),
        "剪贴板为空时不得产出空白候选"
    );

    // 剪贴板有内容但反查查不到（英文、标点、生僻字）。
    let latin = |_n: i64| "Abc".to_string();
    let host = wind_phrase::PhraseHost {
        clip: &latin,
        reverse: &reverse,
    };
    assert!(
        layer().lookup("cofc", &[], &host).is_empty(),
        "查不到时不得产出只有标签的候选（如孤零零的 `A:`）"
    );
}
