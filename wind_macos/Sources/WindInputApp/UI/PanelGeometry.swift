import Cocoa
import WindInputKit

// PanelGeometry — 把 [WireGeometry] 的纯几何接到真实 NSScreen 上。
//
// 拆成两层是为了可测：换算规则（含往返互逆性）在 WindInputKit 里用注入的 screenHeight
// 测；这里只负责「哪块屏」这两个决定，那部分离开真机测不了。
//
// **两个决定是不同的问题，用错屏的后果也不同：**
//
// 1. **翻 y 轴用哪块屏** → 只能是带菜单栏的主屏。wire 原点钉死在主屏左上角，这是协议
//    定义的一部分，与窗口实际落在哪块屏无关。
// 2. **钳边界用哪块屏** → 必须是**目标点所在**的那块屏。用主屏去钳会把副屏上的窗口一把
//    拽回主屏：在主屏右侧的副屏上打字，wire x 约 2400，按主屏 `visibleFrame` 一钳就成了
//    `主屏右边缘 − 窗口宽`，候选窗直接飞到另一块屏上；固定位置存在副屏更是永远还原不回去。
//
// 这两件事以前混在一起（都用 `NSScreen.main`），错误恰好互相抵消，所以在单屏机器上、
// 以及多屏但只有一个方向被用到时，都看不出来。
enum PanelGeometry {

    /// wire 原点所在的参照屏 = 带菜单栏的主屏。无屏幕（理论上不会发生）时返回 nil。
    ///
    /// 不可用 `NSScreen.main`——那是「当前 key window 所在的屏」，多显示器下随用户操作变。
    /// **caret 上报也必须用同一块屏**（见 `InputController.sendCaretUpdate`）：两个方向用
    /// 不同参照时，两屏高度差就直接变成候选窗相对光标的垂直偏移。
    static var referenceScreen: NSScreen? { NSScreen.screens.first }

    /// 参照屏高度；取不到屏时 0（调用方应放弃换算而不是拿 0 硬算）。
    static var referenceHeight: CGFloat { referenceScreen?.frame.height ?? 0 }

    /// 与 `rect` 有交集的屏；都没交集（点在屏外/刚拔掉副屏）时回退参照屏。
    static func screen(containing rect: NSRect) -> NSScreen? {
        for s in NSScreen.screens where s.frame.intersects(rect) { return s }
        return referenceScreen
    }

    /// 窗口矩形 → wire 左上角；取不到屏幕时返回 nil。
    static func wireTopLeft(of frame: NSRect) -> (x: Int32, y: Int32)? {
        guard let s = referenceScreen else { return nil }
        return WireGeometry.wireTopLeft(of: frame, screenHeight: s.frame.height)
    }

    /// wire 左上角 → 已钳进**所在屏**可见区的 Cocoa 左下角；取不到屏幕时返回 nil。
    static func cocoaOrigin(wireX: CGFloat, wireY: CGFloat, size: NSSize) -> NSPoint? {
        guard let ref = referenceScreen else { return nil }
        let o = WireGeometry.cocoaOrigin(wireX: wireX, wireY: wireY, size: size,
                                         screenHeight: ref.frame.height)
        return clamped(o, size: size)
    }

    /// 把已有落点钳进**所在屏**可见区（内容尺寸变化后原落点可能已越界）。
    static func clamped(_ origin: NSPoint, size: NSSize) -> NSPoint {
        guard let s = screen(containing: NSRect(origin: origin, size: size)) else { return origin }
        return WireGeometry.clamp(origin: origin, size: size, visibleFrame: s.visibleFrame)
    }
}
