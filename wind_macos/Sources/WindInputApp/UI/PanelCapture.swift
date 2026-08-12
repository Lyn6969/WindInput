import Cocoa

// PanelCapture — 把 `.app` 自绘的浮窗截成 PNG 存盘 + 复制到剪贴板。
//
// 只用于状态气泡与悬停提示：这两者是本进程的原生 NSPanel，**像素不在服务进程**，
// 服务端只下发文本与配色，截图只能在这边做（候选窗相反，那是服务进程光栅化后经 SHM
// 推下来的，那边直接截自己的 buffer 就行）。
//
// **刻意不用 `CGWindowListCreateImage`**：那条路从 macOS 14 起要「屏幕录制」授权，
// 而本输入法申请的是「辅助功能」。为一个截图菜单项再要一项更敏感的授权不成比例，
// 何况用户拒授权后只会得到一张黑图 —— 那比功能不存在更糟。
// 自己的视图自己渲染，不需要任何授权。
enum PanelCapture {

    enum Failure: LocalizedError {
        case notVisible
        case noContentView
        case renderFailed
        case encodeFailed

        var errorDescription: String? {
            switch self {
            case .notVisible:     return "not_visible"
            case .noContentView:  return "no_content_view"
            case .renderFailed:   return "render_failed"
            case .encodeFailed:   return "encode_failed"
            }
        }
    }

    /// 截图 → 存盘 → 复制剪贴板。返回**剪贴板是否也成功**。
    ///
    /// 剪贴板失败不抛：与 Windows 侧同一取舍 —— 存盘是既成事实，剪贴板只是顺手，
    /// 失败只在 Toast 文案里说明，不该把整个操作判成失败。
    /// 须在主线程调用（渲染视图层级）。
    @discardableResult
    static func snapshot(_ panel: NSPanel, toPath path: String) throws -> Bool {
        guard panel.isVisible else { throw Failure.notVisible }
        guard let view = panel.contentView else { throw Failure.noContentView }
        let bounds = view.bounds
        guard bounds.width >= 1, bounds.height >= 1 else { throw Failure.renderFailed }

        let png = try renderPNG(view: view, bounds: bounds, scale: panel.backingScaleFactor)

        let url = URL(fileURLWithPath: path)
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(),
                                                withIntermediateDirectories: true)
        try png.write(to: url)

        let pb = NSPasteboard.general
        pb.clearContents()
        return pb.setData(png, forType: .png)
    }

    /// 按 backing 缩放（Retina=2）渲染视图层级为 PNG。
    ///
    /// 走 `layer.render(in:)` 而不是 `cacheDisplay(in:to:)`：这两个浮窗的背景是
    /// **图层属性**（`layer.backgroundColor` + `cornerRadius`）而不是 `draw(_:)` 画出来的，
    /// 而 `cacheDisplay` 只走视图的绘制路径 —— 对这种视图会截出一张只有文字、没有底色和
    /// 圆角的透明图。取不到 layer 时才回退 `cacheDisplay`。
    private static func renderPNG(view: NSView, bounds: NSRect, scale: CGFloat) throws -> Data {
        guard let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil,
            pixelsWide: Int((bounds.width * scale).rounded()),
            pixelsHigh: Int((bounds.height * scale).rounded()),
            bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
            colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)
        else { throw Failure.renderFailed }
        rep.size = bounds.size   // 逻辑尺寸：PNG 带上正确的 DPI，贴进文档不会是两倍大

        guard let ctx = NSGraphicsContext(bitmapImageRep: rep) else { throw Failure.renderFailed }
        NSGraphicsContext.saveGraphicsState()
        defer { NSGraphicsContext.restoreGraphicsState() }
        NSGraphicsContext.current = ctx
        ctx.cgContext.scaleBy(x: scale, y: scale)
        if let layer = view.layer {
            layer.render(in: ctx.cgContext)
        } else {
            view.displayIgnoringOpacity(bounds, in: ctx)
        }

        guard let png = rep.representation(using: .png, properties: [:]) else {
            throw Failure.encodeFailed
        }
        return png
    }
}
