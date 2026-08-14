//! 按键探针：吃键判定与按键处理的共同输入。

/// 修饰键位（与 `wind_ipc::protocol` 的 KEYMOD_* 同布局，避免宿主两头换算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(pub u32);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: u32 = 0x0001;
    pub const CTRL: u32 = 0x0002;
    pub const ALT: u32 = 0x0004;

    pub fn has_shift(self) -> bool {
        self.0 & Self::SHIFT != 0
    }

    pub fn has_ctrl(self) -> bool {
        self.0 & Self::CTRL != 0
    }

    pub fn has_alt(self) -> bool {
        self.0 & Self::ALT != 0
    }

    /// 是否带 Ctrl 或 Alt。这两个键在多数判据里同进同退——它们的组合归宿主快捷键，
    /// 输入法不该染指（吃掉 Ctrl+= 会让宿主的放大失效）。
    pub fn has_ctrl_or_alt(self) -> bool {
        self.has_ctrl() || self.has_alt()
    }
}

/// 一次按键的描述。**吃键判定与实际处理必须喂同一个探针**——
/// 两者判据分岔正是「吃了再吐」或丢键的来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyProbe {
    /// 虚拟键码（wind-keys keymap 的取值）
    pub vk: u32,
    pub modifiers: Modifiers,
    /// 宿主是否处于**只读/不可编辑**上下文（浏览器非编辑区、密码框强制英文等）。
    ///
    /// 由宿主给出而非核心推断：只有宿主看得见文本上下文。为 `true` 时一律不吃键。
    pub host_readonly: bool,
}

impl KeyProbe {
    pub fn new(vk: u32) -> Self {
        Self {
            vk,
            modifiers: Modifiers::NONE,
            host_readonly: false,
        }
    }

    pub fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = modifiers;
        self
    }

    pub fn readonly(mut self, readonly: bool) -> Self {
        self.host_readonly = readonly;
        self
    }
}
