//! 编码区（preedit）光标：缓冲区内的编辑位置，及「buffer 位置 → 显示串位置 → TSF caret」换算。
//!
//! 对齐 Go `internal/coordinator/pinyin_mode_shared.go` 与 `handle_candidates.go` 的光标模型：
//!
//! - 光标是 **buffer 内的字节偏移**，定义域 `[0, buf.len()]`。缓冲恒为 ASCII（拼音/五笔字母 +
//!   `'` 分隔符），故字节 == 字符 == UTF-16 单元；但本模块不假设这点，一律走 char boundary，
//!   避免将来缓冲混入非 ASCII 时 `String` 切片 panic（Go 切 `string` 越界只是运行时错，Rust
//!   切非边界直接 panic，这是移植中唯一必须收紧的地方）。
//! - 光标**不参与引擎查询**：`update_candidates` 恒以整串 buffer 查询，移动光标不重算候选。
//!   中间插入字符后按新的完整 buffer 全量重算，与在末尾输入无差别。
//! - 已转换前缀（`committed_text`）是只读前缀，光标进不去：Home 只到 buffer 开头。

/// 向下取到最近的字符边界（`i` 已在边界或越界时原样/截断返回）。
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// 把 buffer 内的字节位置映射到 display 串内的字节位置。
///
/// 对齐 Go `mapBufferPosToDisplayPos`：逐字节贪心同步扫描——display 中与 buffer 对不上的字符
/// （引擎插入的音节分隔空格 / `'`）只推进 display 指针。不依赖音节边界表，故对「引擎怎么分词」
/// 无假设：只要 display 是 buffer 按序插入若干分隔符的产物，映射即成立。
pub(crate) fn map_buf_pos_to_display_pos(buffer: &str, display: &str, buf_pos: usize) -> usize {
    let (b, d) = (buffer.as_bytes(), display.as_bytes());
    let (mut bi, mut di) = (0usize, 0usize);
    while bi < buf_pos && di < d.len() {
        if bi < b.len() && d[di] == b[bi] {
            bi += 1;
        }
        di += 1;
    }
    di
}

/// 组合区光标的 TSF 单位（UTF-16 code unit）。
///
/// 组合区显示串 = `prefix`（已转换前缀/模式引导符，只读）+ `body`（buffer 的显示形态）。
/// `cursor` 是 `buffer` 内字节位，先映射到 body 内位置，再连同 prefix 一起折算为 UTF-16。
///
/// 单位取 UTF-16 而非字符数：C++ 侧 `TextService::_UpdateComposition` 用 `ITfRange::ShiftEnd`
/// 定位，其偏移单位是 UTF-16 code unit。Go 用 rune 数是因为汉字都在 BMP（见 Go
/// `displayCursorPos` 注释），扩展 B 区汉字（生僻字候选）会错位；Rust 直接按 UTF-16 算。
pub(crate) fn caret_utf16(prefix: &str, buffer: &str, body: &str, cursor: usize) -> u32 {
    let dpos = floor_boundary(body, map_buf_pos_to_display_pos(buffer, body, cursor));
    (prefix.encode_utf16().count() + body[..dpos].encode_utf16().count()) as u32
}

/// 借用 buffer + cursor 的编辑视图，提供边界安全的编辑原语。
///
/// 所有 cursor 运算封死在此，调用方不做裸 `usize` 加减——这是 5 个 overlay 模式各写各的
/// `push`/`pop` 之外，唯一需要共享的东西。构造时把 cursor 夹到合法边界，故即便外部字段被
/// 别处直接改坏（如 buffer 整体替换后忘了重置 cursor），也不会 panic，只会退化到边界。
pub(crate) struct BufEdit<'a> {
    buf: &'a mut String,
    cursor: &'a mut usize,
}

impl<'a> BufEdit<'a> {
    pub(crate) fn new(buf: &'a mut String, cursor: &'a mut usize) -> Self {
        *cursor = floor_boundary(buf, *cursor);
        Self { buf, cursor }
    }

    #[cfg(test)]
    pub(crate) fn pos(&self) -> usize {
        *self.cursor
    }

    /// 在光标处插入字符，光标随之后移（光标在末尾时等价于原来的 `push`）。
    pub(crate) fn insert(&mut self, ch: char) {
        self.buf.insert(*self.cursor, ch);
        *self.cursor += ch.len_utf8();
    }

    /// 删除光标前一个字符（Backspace）。返回是否真的删了（光标在 0 → false）。
    pub(crate) fn backspace(&mut self) -> bool {
        if *self.cursor == 0 {
            return false;
        }
        let prev = floor_boundary(self.buf, *self.cursor - 1);
        self.buf.replace_range(prev..*self.cursor, "");
        *self.cursor = prev;
        true
    }

    /// 删除光标后一个字符（Delete），光标不动。返回是否真的删了（光标在末尾 → false）。
    pub(crate) fn delete(&mut self) -> bool {
        if *self.cursor >= self.buf.len() {
            return false;
        }
        let next = self.buf[*self.cursor..]
            .chars()
            .next()
            .map(|c| *self.cursor + c.len_utf8())
            .unwrap_or(self.buf.len());
        self.buf.replace_range(*self.cursor..next, "");
        true
    }

    /// 左移一个字符。返回是否移动了（已在最左 → false，调用方据此决定吃键还是透传）。
    pub(crate) fn move_left(&mut self) -> bool {
        if *self.cursor == 0 {
            return false;
        }
        *self.cursor = floor_boundary(self.buf, *self.cursor - 1);
        true
    }

    /// 右移一个字符。返回是否移动了（已在最右 → false）。
    pub(crate) fn move_right(&mut self) -> bool {
        if *self.cursor >= self.buf.len() {
            return false;
        }
        *self.cursor = self.buf[*self.cursor..]
            .chars()
            .next()
            .map(|c| *self.cursor + c.len_utf8())
            .unwrap_or(self.buf.len());
        true
    }

    /// 跳到最左。返回是否移动了。
    pub(crate) fn home(&mut self) -> bool {
        let moved = *self.cursor != 0;
        *self.cursor = 0;
        moved
    }

    /// 跳到最右。返回是否移动了。
    pub(crate) fn end(&mut self) -> bool {
        let moved = *self.cursor != self.buf.len();
        *self.cursor = self.buf.len();
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_pos_skips_inserted_separators() {
        // 引擎给的 display 在 buffer 基础上插入分词空格：ni|hao → "ni hao"
        assert_eq!(map_buf_pos_to_display_pos("nihao", "ni hao", 0), 0);
        assert_eq!(map_buf_pos_to_display_pos("nihao", "ni hao", 2), 2); // ni|hao → "ni| hao"
        assert_eq!(map_buf_pos_to_display_pos("nihao", "ni hao", 3), 4); // nih|ao → "ni h|ao"
        assert_eq!(map_buf_pos_to_display_pos("nihao", "ni hao", 5), 6); // 末尾
    }

    #[test]
    fn map_pos_handles_apostrophe_display() {
        // 手动分隔符已在 buffer 里时，display 与 buffer 同形
        assert_eq!(map_buf_pos_to_display_pos("ni'hao", "ni'hao", 3), 3);
    }

    #[test]
    fn map_pos_degenerate_display_falls_back() {
        // display 与 buffer 完全对不上（引擎回落/异常）：不 panic，退化为 display 长度内推进
        let got = map_buf_pos_to_display_pos("nihao", "", 3);
        assert_eq!(got, 0);
    }

    #[test]
    fn caret_counts_prefix_in_utf16() {
        // 已转换前缀「你」= 1 个 UTF-16 单元；剩余 buffer "hao" 光标在 1 → display "h|ao"
        assert_eq!(caret_utf16("你", "hao", "hao", 1), 2);
        // 前缀为空
        assert_eq!(caret_utf16("", "nihao", "ni hao", 3), 4);
    }

    #[test]
    fn caret_prefix_outside_bmp_counts_two_units() {
        // 扩展 B 区汉字（U+20000）占 2 个 UTF-16 单元——Go 按 rune 数会算成 1，这里必须是 2
        let ext = "\u{20000}";
        assert_eq!(ext.chars().count(), 1);
        assert_eq!(caret_utf16(ext, "hao", "hao", 0), 2);
    }

    #[test]
    fn bufedit_insert_at_cursor() {
        let (mut b, mut c) = (String::from("niao"), 2usize);
        let mut e = BufEdit::new(&mut b, &mut c);
        e.insert('h');
        assert_eq!((b.as_str(), c), ("nihao", 3));
    }

    #[test]
    fn bufedit_insert_at_end_equals_push() {
        let (mut b, mut c) = (String::from("ni"), 2usize);
        BufEdit::new(&mut b, &mut c).insert('h');
        assert_eq!((b.as_str(), c), ("nih", 3));
    }

    #[test]
    fn bufedit_backspace_and_delete() {
        let (mut b, mut c) = (String::from("nihao"), 3usize);
        let mut e = BufEdit::new(&mut b, &mut c);
        assert!(e.backspace()); // 删光标前 'h'
        assert_eq!((b.as_str(), c), ("niao", 2));

        let mut e = BufEdit::new(&mut b, &mut c);
        assert!(e.delete()); // 删光标后 'a'，光标不动
        assert_eq!((b.as_str(), c), ("nio", 2));
    }

    #[test]
    fn bufedit_boundaries_report_no_move() {
        let (mut b, mut c) = (String::from("ni"), 0usize);
        let mut e = BufEdit::new(&mut b, &mut c);
        assert!(!e.move_left()); // 已在最左
        assert!(!e.backspace()); // 光标在 0，删不动
        assert!(e.move_right());
        assert!(e.move_right());
        assert!(!e.move_right()); // 已在最右
        assert!(!e.delete()); // 光标在末尾，前删无物
        assert_eq!(c, 2);
    }

    #[test]
    fn bufedit_home_end() {
        let (mut b, mut c) = (String::from("nihao"), 2usize);
        let mut e = BufEdit::new(&mut b, &mut c);
        assert!(e.home());
        assert_eq!(e.pos(), 0);
        assert!(!e.home()); // 已在最左
        assert!(e.end());
        assert_eq!(e.pos(), 5);
        assert!(!e.end());
    }

    #[test]
    fn bufedit_clamps_stale_cursor() {
        // buffer 被别处整体替换、cursor 未同步（越界）：夹到边界而非 panic
        let (mut b, mut c) = (String::from("ni"), 99usize);
        let mut e = BufEdit::new(&mut b, &mut c);
        assert_eq!(e.pos(), 2);
        assert!(e.backspace());
        assert_eq!(b.as_str(), "n");
    }

    #[test]
    fn bufedit_non_ascii_never_panics() {
        // 缓冲混入非 ASCII（当前不会发生，但保证不 panic 且按字符步进）
        let (mut b, mut c) = (String::from("中a"), 0usize);
        let mut e = BufEdit::new(&mut b, &mut c);
        assert!(e.move_right());
        assert_eq!(e.pos(), 3); // 「中」占 3 字节
        assert!(e.backspace());
        assert_eq!((b.as_str(), c), ("a", 0));
    }
}
