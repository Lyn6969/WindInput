import XCTest
@testable import WindInputKit

/// CmdCandidateSelect 编码 + CmdCandidateRects 解码 (M5 鼠标点选协议).
final class CandidateSelectCodecTests: XCTestCase {

    func testEncodeCandidateSelectFrame() {
        let frame = BinaryCodec.encodeCandidateSelectFrame(index: 3)
        XCTAssertEqual(frame.count, 8 + 4)
        let (cmd, length, _) = try! BinaryCodec.decodeHeader(frame)
        XCTAssertEqual(cmd, UpstreamCmd.candidateSelect)
        XCTAssertEqual(length, 4)
        let payload = frame.subdata(in: WireProtocol.headerSize..<frame.count)
        XCTAssertEqual(payload.readUInt32LE(at: 0), 3)
    }

    func testEncodeCandidateSelectFrame_NegativeClampedToZero() {
        let frame = BinaryCodec.encodeCandidateSelectFrame(index: -5)
        let payload = frame.subdata(in: WireProtocol.headerSize..<frame.count)
        XCTAssertEqual(payload.readUInt32LE(at: 0), 0)
    }

    func testDecodeCandidateRectsPayload() throws {
        // count=2 + 2×(index,x,y,w,h)
        var buf = Data(count: 4 + 2 * 20)
        buf.writeUInt32LE(2, at: 0)
        // rect0: index=0, x=10,y=20,w=30,h=40
        buf.writeUInt32LE(0, at: 4)
        buf.writeUInt32LE(10, at: 8)
        buf.writeUInt32LE(20, at: 12)
        buf.writeUInt32LE(30, at: 16)
        buf.writeUInt32LE(40, at: 20)
        // rect1: index=1, x=50,y=20,w=30,h=40
        buf.writeUInt32LE(1, at: 24)
        buf.writeUInt32LE(50, at: 28)
        buf.writeUInt32LE(20, at: 32)
        buf.writeUInt32LE(30, at: 36)
        buf.writeUInt32LE(40, at: 40)

        let rects = try BinaryCodec.decodeCandidateRectsPayload(buf)
        XCTAssertEqual(rects.count, 2)
        XCTAssertEqual(rects[0], CandidateHitRect(index: 0, x: 10, y: 20, w: 30, h: 40))
        XCTAssertEqual(rects[1].x, 50)
        // hit-test
        XCTAssertTrue(rects[0].contains(px: 15, py: 25))
        XCTAssertFalse(rects[0].contains(px: 45, py: 25))
        XCTAssertTrue(rects[1].contains(px: 55, py: 25))
    }

    func testDecodeCandidateRectsPayload_Empty() throws {
        var buf = Data(count: 4)
        buf.writeUInt32LE(0, at: 0)
        let rects = try BinaryCodec.decodeCandidateRectsPayload(buf)
        XCTAssertEqual(rects.count, 0)
    }
}
