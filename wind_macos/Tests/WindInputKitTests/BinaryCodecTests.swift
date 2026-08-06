import XCTest
@testable import WindInputKit

final class BinaryCodecTests: XCTestCase {

    func testHeaderRoundtrip_KeyEvent() throws {
        let h = BinaryCodec.encodeHeader(cmd: UpstreamCmd.keyEvent, payloadLen: 18)
        XCTAssertEqual(h.count, WireProtocol.headerSize)
        let (cmd, len, isAsync) = try BinaryCodec.decodeHeader(h)
        XCTAssertEqual(cmd, UpstreamCmd.keyEvent)
        XCTAssertEqual(len, 18)
        XCTAssertFalse(isAsync)
    }

    func testHeaderAsyncFlag() throws {
        let h = BinaryCodec.encodeHeader(cmd: UpstreamCmd.modeNotify, payloadLen: 0, async: true)
        let (cmd, len, isAsync) = try BinaryCodec.decodeHeader(h)
        XCTAssertEqual(cmd, UpstreamCmd.modeNotify)
        XCTAssertEqual(len, 0)
        XCTAssertTrue(isAsync)
    }

    func testHeaderVersionMismatch() {
        // 构造一个 v2 帧 (major != 0x1)
        var h = BinaryCodec.encodeHeader(cmd: UpstreamCmd.keyEvent, payloadLen: 0)
        h[0] = 0x01
        h[1] = 0x20  // version = 0x2001
        do {
            _ = try BinaryCodec.decodeHeader(h)
            XCTFail("expected versionMismatch")
        } catch IPCError.versionMismatch(let v) {
            XCTAssertEqual(v, 0x2001)
        } catch {
            XCTFail("wrong error: \(error)")
        }
    }

    func testHeaderPayloadTooLarge() {
        var h = BinaryCodec.encodeHeader(cmd: UpstreamCmd.keyEvent, payloadLen: 0)
        // length = MaxPayloadSize + 1
        let bad: UInt32 = WireProtocol.maxPayloadSize + 1
        h.writeUInt32LE(bad, at: 4)
        do {
            _ = try BinaryCodec.decodeHeader(h)
            XCTFail("expected payloadTooLarge")
        } catch IPCError.payloadTooLarge(let n) {
            XCTAssertEqual(n, bad)
        } catch {
            XCTFail("wrong error: \(error)")
        }
    }

    func testKeyEventFrameRoundtrip() throws {
        let original = KeyEventPayload(
            keyCode: 0x41,
            scanCode: 0x1E,
            modifiers: 0x0001,
            eventType: .down,
            toggles: 0x01,
            eventSeq: 42,
            prevChar: 0x4E2D  // '中'
        )
        let frame = BinaryCodec.encodeKeyEventFrame(original)
        XCTAssertEqual(frame.count, WireProtocol.headerSize + 18)

        // 验帧头
        let header = frame.prefix(WireProtocol.headerSize)
        let (cmd, len, _) = try BinaryCodec.decodeHeader(header)
        XCTAssertEqual(cmd, UpstreamCmd.keyEvent)
        XCTAssertEqual(len, 18)

        // 验 payload
        let payload = frame.subdata(in: WireProtocol.headerSize..<frame.count)
        let decoded = try BinaryCodec.decodeKeyEventPayload(payload)
        XCTAssertEqual(decoded, original)
    }

    func testEmptyFrame_AckHeader() throws {
        let f = BinaryCodec.encodeEmptyFrame(cmd: DownstreamCmd.ack)
        XCTAssertEqual(f.count, WireProtocol.headerSize)
        let (cmd, len, _) = try BinaryCodec.decodeHeader(f)
        XCTAssertEqual(cmd, DownstreamCmd.ack)
        XCTAssertEqual(len, 0)
    }

    func testFocusGainedFrame_InputScopeMask() throws {
        // 密码框: IS_PASSWORD 位 (bit31)。载荷与 Win 端同构 (39B) + bundleID 段。
        let mask: UInt64 = UInt64(1) << 31
        let token: UInt64 = (UInt64(4321) << 32) | 7
        let f = BinaryCodec.encodeFocusGainedFrame(
            clientToken: token, inputScopeMask: mask, bundleID: "com.apple.TextEdit")
        let (cmd, len, _) = try BinaryCodec.decodeHeader(f)
        XCTAssertEqual(cmd, UpstreamCmd.focusGained)
        XCTAssertEqual(len, UInt32(43 + "com.apple.TextEdit".utf8.count))
        let payload = f.subdata(in: WireProtocol.headerSize ..< f.count)
        // caret 段全 0: 服务端 apply_focus_caret 见 height==0 即返回, 不污染坐标缓存。
        for i in 0..<20 { XCTAssertEqual(payload[payload.startIndex + i], 0) }
        XCTAssertEqual(payload.readUInt64LE(at: 20), token)
        XCTAssertEqual(payload.readUInt64LE(at: 28), mask)
        XCTAssertEqual(payload[payload.startIndex + 38], 0) // caretSource = UNKNOWN
        let n = Int(payload.readUInt32LE(at: 39))
        XCTAssertEqual(
            String(data: payload.subdata(in: (payload.startIndex + 43)..<(payload.startIndex + 43 + n)),
                   encoding: .utf8),
            "com.apple.TextEdit")
    }

    func testFocusGainedFrame_MeetsRustMinimumLength() throws {
        // 回归: 曾只发 12 字节, 而 Rust FocusGainedPayload::from_bytes 下限是 36 —— 解码恒
        // 失败, FOCUS_GAINED 重型段 (按应用初始模式 / compat 规则 / 密码框抑制) 从未执行。
        let f = BinaryCodec.encodeFocusGainedFrame(
            clientToken: 0, inputScopeMask: 0, bundleID: "")
        let (_, len, _) = try BinaryCodec.decodeHeader(f)
        XCTAssertGreaterThanOrEqual(len, 39)
        let payload = f.subdata(in: WireProtocol.headerSize ..< f.count)
        XCTAssertEqual(payload.readUInt32LE(at: 39), 0) // bundleID 为空也要有长度字段
    }

    func testDecodeKeyEventPayloadTooShort() {
        do {
            _ = try BinaryCodec.decodeKeyEventPayload(Data(repeating: 0, count: 8))
            XCTFail("expected payloadTooShort")
        } catch IPCError.payloadTooShort {
            // ok
        } catch {
            XCTFail("wrong error: \(error)")
        }
    }

    func testDecodeKeyEventPayload_16Bytes_NoPrevChar() throws {
        // Win/老 TSF 端可能只发 16 字节 (无 prevChar). codec 应回退 prevChar=0.
        var p = Data(count: 16)
        p.writeUInt32LE(0x41, at: 0)
        p.writeUInt32LE(0x1E, at: 4)
        p.writeUInt32LE(0x0001, at: 8)
        p[12] = 0
        p[13] = 0
        p.writeUInt16LE(7, at: 14)

        let decoded = try BinaryCodec.decodeKeyEventPayload(p)
        XCTAssertEqual(decoded.keyCode, 0x41)
        XCTAssertEqual(decoded.eventSeq, 7)
        XCTAssertEqual(decoded.prevChar, 0)
    }

    // MARK: - CmdOpenSettings (0x0507) 深链切词

    func testOpenSettingsArguments_PageOnly() {
        XCTAssertEqual(
            BinaryCodec.decodeOpenSettingsArguments(Data("dict".utf8)), ["--page=dict"])
    }

    func testOpenSettingsArguments_PageWithExtras() {
        // 回归: 整串曾被塞进单个 --page=, 设置端 parse_target 解析不出页 id, 只开默认页。
        let a = BinaryCodec.decodeOpenSettingsArguments(
            Data("dict --schema=wubi86 --type=shadow".utf8))
        XCTAssertEqual(a, ["--page=dict", "--schema=wubi86", "--type=shadow"])
    }

    func testOpenSettingsArguments_QuotedValueWithSpace() {
        // build_settings_args 对含空白的值加双引号; 切词后引号须去掉、值保持完整。
        let a = BinaryCodec.decodeOpenSettingsArguments(
            Data("add-word --text=\"你 好\" --code=nihao".utf8))
        XCTAssertEqual(a, ["--page=add-word", "--text=你 好", "--code=nihao"])
    }

    func testOpenSettingsArguments_NoPageOnlyFlags() {
        // page=None + extra 非空 (如 --dark): 首词已是选项, 不得再包一层 --page=。
        XCTAssertEqual(
            BinaryCodec.decodeOpenSettingsArguments(Data("--dark --soft".utf8)),
            ["--dark", "--soft"])
    }

    func testOpenSettingsArguments_Empty() {
        XCTAssertEqual(BinaryCodec.decodeOpenSettingsArguments(Data()), [])
        XCTAssertEqual(BinaryCodec.decodeOpenSettingsArguments(Data("   ".utf8)), [])
    }
}
