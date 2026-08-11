import Cocoa
import WindInputKit

// StatusBubblePanel — 模式切换状态提示气泡 (中英/标点/全半角)。
//
// 与 Win 端 StatusWindow 对齐: 模式切换时在 caret 附近弹出一个短文气泡 (如 "中 ，"),
// temp 模式到点自动消失, always 模式常驻。区别于菜单栏 NSStatusItem (常驻显示当前
// 方案/模式), 这是输入位置旁的瞬态反馈。
//
// Go 端 (forwarder) 据 config 合成文本 + 主题色 + 位置 + 时长, 经 push CmdStatusShow
// 下发; 本浮窗只负责渲染与定位。点击穿透, 不抢焦点。
/// 气泡背景视图: 承接拖动手势。
///
/// `hitTest` 恒返回自身, 让内部的 NSTextField 永远拿不到鼠标事件 —— 标签虽已设成
/// 不可编辑/不可选中, 但它仍在 hit-test 链上, 按在文字上就会漏掉一次起拖。
private final class BubbleBackgroundView: NSView {
    var onDragBegan: (() -> Void)?
    var onDragEnded: (() -> Void)?
    private var dragAnchor: NSPoint?
    private var dragOrigin: NSPoint?
    /// 本次按下是否已越过位移阈值 —— 没越过就只是一次点击, 不该被当成「摆放完成」上报
    /// (固定位置模式下每次点击都会白搭一次 IPC 往返 + 配置写盘)。同 CandidatePanel。
    private var dragMoved = false
    private static let dragThreshold: CGFloat = 3

    override func hitTest(_ point: NSPoint) -> NSView? {
        bounds.contains(convert(point, from: superview)) ? self : nil
    }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with event: NSEvent) {
        guard let win = window else { return }
        dragAnchor = NSEvent.mouseLocation
        dragOrigin = win.frame.origin
        dragMoved = false
    }

    override func mouseDragged(with event: NSEvent) {
        guard let anchor = dragAnchor, let origin = dragOrigin, let win = window else { return }
        let now = NSEvent.mouseLocation
        let (dx, dy) = (now.x - anchor.x, now.y - anchor.y)
        if !dragMoved {
            guard abs(dx) >= Self.dragThreshold || abs(dy) >= Self.dragThreshold else { return }
            dragMoved = true
            // 真开始拖了才撤自动隐藏定时器 —— 放在 mouseDown 里会让一次单纯的点击
            // 意外延长气泡寿命。
            onDragBegan?()
        }
        win.setFrameOrigin(NSPoint(x: origin.x + dx, y: origin.y + dy))
    }

    override func mouseUp(with event: NSEvent) {
        defer { dragAnchor = nil; dragOrigin = nil; dragMoved = false }
        guard dragAnchor != nil, dragMoved else { return }
        onDragEnded?()
    }
}

final class StatusBubblePanel: NSPanel {
    private let label = NSTextField(labelWithString: "")
    private let bgView = BubbleBackgroundView()
    private let hPad: CGFloat = 8
    private let vPad: CGFloat = 4
    private var hideTimer: Timer?
    /// 拖动落定回调, 参数为 wire 坐标 (top-left) 下的气泡左上角。
    /// 服务端只在「固定位置」模式下落盘; 跟随光标模式下拖动只是临时挪开。
    var onMoved: ((Int32, Int32) -> Void)?
    /// 最近一次 `show` 的自动隐藏时长 (ms, 0=常驻)。拖动期间要撤掉定时器、松手后按原时长
    /// 重新计时 —— 否则 temp 模式 (默认 800ms) 的气泡会在用户手还按着的时候消失。
    /// Windows 侧同一意图见 status_tip.rs 的 `should_stay_visible`(dragging || hover || menu)。
    private var lastDurationMs: Int32 = 0

    init() {
        super.init(contentRect: NSRect(x: 0, y: 0, width: 60, height: 24),
                   styleMask: [.borderless, .nonactivatingPanel],
                   backing: .buffered,
                   defer: false)
        isOpaque = false
        backgroundColor = .clear
        hasShadow = true
        level = .popUpMenu
        isFloatingPanel = true
        collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle]
        hidesOnDeactivate = false
        // 曾是 ignoresMouseEvents=true (点击穿透)。改为接收鼠标事件才能拖动 —— 与 Windows
        // 侧的状态气泡一致 (那边同样可拖, 见 status_tip.rs 的 StatusTipMoved)。气泡很小且
        // 多为瞬态 (temp 模式到点自动消失), 挡住点击的代价远小于「摆不动」。

        bgView.wantsLayer = true
        bgView.layer?.cornerRadius = 6
        bgView.layer?.masksToBounds = true

        label.isBezeled = false
        label.isEditable = false
        label.isSelectable = false
        label.drawsBackground = false
        label.alignment = .center
        label.lineBreakMode = .byClipping
        label.translatesAutoresizingMaskIntoConstraints = true

        bgView.addSubview(label)
        contentView = bgView
        bgView.onDragBegan = { [weak self] in self?.hideTimer?.invalidate() }
        bgView.onDragEnded = { [weak self] in self?.finishDrag() }
    }

    /// 拖动松手 → 回报 wire 落点, 并按原时长重新计时 (起拖时撤掉了定时器)。
    /// 服务端只在固定位置模式下落盘。
    private func finishDrag() {
        armHideTimer(lastDurationMs)
        guard let (x, y) = wireTopLeft() else { return }
        onMoved?(x, y)
    }

    private func armHideTimer(_ durationMs: Int32) {
        hideTimer?.invalidate()
        guard durationMs > 0 else { hideTimer = nil; return }
        hideTimer = Timer.scheduledTimer(withTimeInterval: Double(durationMs) / 1000.0,
                                         repeats: false) { [weak self] _ in
            self?.orderOut(nil)
        }
    }

    /// 气泡当前左上角的 wire 坐标; 不可见时返回 nil (没有"当前位置"可言)。
    /// 供服务端的 `pos.status_tip.query` 问询与拖动回报共用同一换算。
    func wireTopLeft() -> (Int32, Int32)? {
        guard isVisible, let p = PanelGeometry.wireTopLeft(of: frame) else { return nil }
        return (p.x, p.y)
    }

    /// 显示气泡。x/y 为 caret 屏幕坐标 (wire top-left, y 向下); durationMs>0 时到点自动隐藏。
    func show(text: String, bgHex: String, fgHex: String, wireX: Int32, wireY: Int32, durationMs: Int32) {
        guard !text.isEmpty else { hidePanel(); return }
        hideTimer?.invalidate()
        lastDurationMs = durationMs

        let bg = NSColor(windHex: bgHex) ?? NSColor(calibratedWhite: 0.235, alpha: 0.9)
        let fg = NSColor(windHex: fgHex) ?? .white
        bgView.layer?.backgroundColor = bg.cgColor

        let font = NSFont.systemFont(ofSize: 16)
        label.attributedStringValue = NSAttributedString(string: text, attributes: [
            .font: font, .foregroundColor: fg,
        ])
        label.sizeToFit()
        let textSize = label.frame.size
        let w = ceil(textSize.width) + hPad * 2
        let h = ceil(textSize.height) + vPad * 2
        label.frame = NSRect(x: hPad, y: vPad, width: ceil(textSize.width), height: ceil(textSize.height))
        setContentSize(NSSize(width: w, height: h))

        guard let screen = PanelGeometry.referenceScreen else {
            orderFrontRegardless(); return
        }
        let vf = screen.visibleFrame
        // wire top-left → Cocoa bottom-left: caret 点的 Cocoa y。气泡顶端贴 caret 点下方。
        // wireY 已是 caret 底部下方的锚点 (forwarder 加了 caretHeight+gap), 与候选窗口
        // 同位置; 固定位置模式下则直接是用户摆放的绝对坐标 (那边 fx/fy 就是 custom_x/y)。
        // 气泡顶边贴该锚点 (originY 为底边, 故 -h)。
        let caretLine = WireGeometry.flipY(CGFloat(wireY), screenHeight: screen.frame.height)
        var originX = CGFloat(wireX)
        var originY = caretLine - h

        if originX + w > vf.maxX { originX = vf.maxX - w }
        if originX < vf.minX { originX = vf.minX }
        if originY < vf.minY { originY = caretLine + 2 } // 下方放不下 → 翻到锚点上方
        if originY + h > vf.maxY { originY = vf.maxY - h }
        if originY < vf.minY { originY = vf.minY }

        setFrameOrigin(NSPoint(x: originX, y: originY))
        orderFrontRegardless()
        armHideTimer(durationMs)
    }

    func hidePanel() {
        hideTimer?.invalidate()
        hideTimer = nil
        orderOut(nil)
    }

}

private extension NSColor {
    /// 解析 #RGB / #RRGGBB / #RRGGBBAA。
    convenience init?(windHex: String) {
        var s = windHex.trimmingCharacters(in: .whitespaces)
        guard s.hasPrefix("#") else { return nil }
        s.removeFirst()
        guard let v = UInt64(s, radix: 16) else { return nil }
        let r, g, b, a: CGFloat
        switch s.count {
        case 6:
            r = CGFloat((v >> 16) & 0xFF) / 255
            g = CGFloat((v >> 8) & 0xFF) / 255
            b = CGFloat(v & 0xFF) / 255
            a = 1
        case 8:
            r = CGFloat((v >> 24) & 0xFF) / 255
            g = CGFloat((v >> 16) & 0xFF) / 255
            b = CGFloat((v >> 8) & 0xFF) / 255
            a = CGFloat(v & 0xFF) / 255
        default:
            return nil
        }
        self.init(srgbRed: r, green: g, blue: b, alpha: a)
    }
}
