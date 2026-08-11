import Foundation
import CoreGraphics

// WireGeometry — 浮窗落位的 wire ↔ Cocoa 坐标换算。
//
// 与 [CaretCoords] 同一件事的另一半：那边管「caret 矩形 → wire」(只有一个方向)，
// 这边管**窗口矩形**的双向换算。分开是因为参照点不同——caret 传的是光标左上角，窗口
// 传的是内容左上角，且窗口这边要能反着算回去。
//
// 两套坐标系的差别只有一处但很致命：
//   - wire (服务进程 / Windows 侧)：原点在**主屏左上角**，y 轴**向下**。
//   - Cocoa 全局坐标：原点在**主屏左下角**，y 轴**向上**。
//
// 以前只有「服务端算好位置 → 摆窗」一个方向，换算散在各 panel 的 show() 里。拖动上报
// 引入了**反方向**换算，两个方向一旦用了不同的参照屏或差一个窗口高度，每轮「拖动 → 落盘
// → 重新显示」就会累计一次偏移，窗口逐次漂走。故收口到这里，并用 round-trip 测试钉死。
//
// **参照屏必须是带菜单栏的主屏 (`NSScreen.screens.first`)，不是 `NSScreen.main`。**
// `NSScreen.main` 是「当前 key window 所在的屏」，多显示器下会随用户操作变，而 wire 原点
// 钉死在主屏左上角。两者在单屏下恰好相同，所以这个错误在单屏机器上永远显不出来。
public enum WireGeometry {

    /// wire y（自主屏顶边向下）↔ Cocoa y（自主屏底边向上）。同一公式自逆。
    public static func flipY(_ y: CGFloat, screenHeight: CGFloat) -> CGFloat {
        screenHeight - y
    }

    /// 窗口矩形 → wire 下的**左上角**（即候选窗/气泡落盘用的 custom_x/custom_y）。
    public static func wireTopLeft(of frame: CGRect, screenHeight: CGFloat) -> (x: Int32, y: Int32) {
        (Int32(frame.origin.x.rounded()),
         Int32(flipY(frame.maxY, screenHeight: screenHeight).rounded()))
    }

    /// wire 左上角 + 窗口尺寸 → Cocoa 的 `setFrameOrigin` 入参（左**下**角）。
    /// `wireTopLeft(of:screenHeight:)` 的逆。
    public static func cocoaOrigin(wireX: CGFloat, wireY: CGFloat, size: CGSize,
                                   screenHeight: CGFloat) -> CGPoint {
        CGPoint(x: wireX, y: flipY(wireY, screenHeight: screenHeight) - size.height)
    }

    /// 把窗口矩形钳进可见区 `visibleFrame`（避开菜单栏/Dock）；返回钳后的左下角。
    /// 内容比屏幕还大时保证**左上角**可见（先按右/下回拉，再按左/上兜底）。
    public static func clamp(origin: CGPoint, size: CGSize, visibleFrame vf: CGRect) -> CGPoint {
        var x = origin.x
        var y = origin.y
        if x + size.width > vf.maxX { x = vf.maxX - size.width }
        if x < vf.minX { x = vf.minX }
        if y + size.height > vf.maxY { y = vf.maxY - size.height }
        if y < vf.minY { y = vf.minY }
        return CGPoint(x: x, y: y)
    }
}
