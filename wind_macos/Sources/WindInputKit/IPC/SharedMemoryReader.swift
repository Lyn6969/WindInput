import Foundation

// SharedMemoryReader — Swift 端的 POSIX SHM 读端 (M3-5).
//
// 对位 Go 端 internal/bridge/host_render_darwin.go 的 writer 半边:
//   Go:    shm_open(O_CREAT|O_RDWR|O_EXCL) → ftruncate → mmap(PROT_RW) → 写 header+BGRA
//   Swift: shm_open(O_RDONLY) → mmap(PROT_READ) → 读 header.sequence + 取 BGRA
//
// SHM header 布局 (64 字节, 与 Go 端 ipc.SharedRenderHeader* 完全一致):
//   [00:04] magic     u32 LE  = 0x57494E44 "WIND"
//   [04:08] version   u32 LE  = 1
//   [08:12] sequence  u32 LE  (单调递增)
//   [12:16] flags     u32 LE  (bit0=Visible, bit1=ContentReady)
//   [16:20] screenX   i32 LE  (wire top-left)
//   [20:24] screenY   i32 LE
//   [24:28] width     u32 LE  (像素)
//   [28:32] height    u32 LE
//   [32:36] stride    u32 LE  (width * 4, BGRA)
//   [36:40] dataSize  u32 LE  (stride * height)
//   [40:64] reserved  zeros
//
// 用法:
//   let r = try SharedMemoryReader(name: "/WindInput_SHM", size: 4 * 1024 * 1024)
//   if let frame = r.snapshot() { /* frame.bgra (Data) + frame.width/height/x/y */ }
//   r.close()

public struct SharedFrame {
    public let sequence: UInt32
    public let flags: UInt32
    public let screenX: Int32
    public let screenY: Int32
    public let width: Int
    public let height: Int
    public let stride: Int
    public let bgra: Data    // 完整 BGRA 像素 (stride * height 字节, 已复制出 SHM)

    public var isVisible: Bool         { (flags & 0x1) != 0 }
    public var hasContent: Bool        { (flags & 0x2) != 0 }
    public var hasSoftwareShadow: Bool { (flags & 0x4) != 0 }
}

public enum SharedMemoryError: Error {
    case shmOpenFailed(errno: Int32)
    case mmapFailed(errno: Int32)
    case magicMismatch(got: UInt32)
    case sizeTooSmall
}

public final class SharedMemoryReader {
    public static let expectedMagic: UInt32 = 0x57494E44  // "WIND" LE bytes 44 4E 49 57
    public static let headerSize = 64

    private let name: String
    private let size: Int
    private var fd: Int32 = -1
    private var ptr: UnsafeMutableRawPointer?
    private var lastSeq: UInt32 = 0
    private let lock = NSLock()

    public init(name: String, size: Int) throws {
        self.name = name
        self.size = size

        // shm_open 是 C variadic 函数 (variadic for mode_t), Swift import 标为
        // unavailable, 改走 dlsym 拿真实函数指针, 转成固定参数 c convention 调用。
        typealias ShmOpenFn = @convention(c) (UnsafePointer<CChar>, Int32, mode_t) -> Int32
        guard let sym = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "shm_open") else {
            throw SharedMemoryError.shmOpenFailed(errno: ENOSYS)
        }
        let shmOpen = unsafeBitCast(sym, to: ShmOpenFn.self)
        let fd = name.withCString { shmOpen($0, O_RDONLY, 0) }
        if fd < 0 {
            throw SharedMemoryError.shmOpenFailed(errno: errno)
        }
        let mapped = mmap(nil, size, PROT_READ, MAP_SHARED, fd, 0)
        if mapped == nil || mapped == MAP_FAILED {
            let err = errno
            Darwin.close(fd)
            throw SharedMemoryError.mmapFailed(errno: err)
        }
        self.fd = fd
        self.ptr = mapped
    }

    deinit { closeReader() }

    public func closeReader() {
        lock.lock(); defer { lock.unlock() }
        if let p = ptr {
            munmap(p, size)
            ptr = nil
        }
        if fd >= 0 {
            Darwin.close(fd)
            fd = -1
        }
    }

    /// 读当前 header + BGRA, 返回 SharedFrame; magic 不对返 nil。
    /// 不做 seq 比较, 调用方自行决定是否相对上次变化才处理。
    public func snapshot() -> SharedFrame? {
        lock.lock(); defer { lock.unlock() }
        guard let p = ptr else { return nil }

        let hdr = UnsafeRawBufferPointer(start: p, count: Self.headerSize)
        let magic: UInt32 = hdr.load(fromByteOffset: 0, as: UInt32.self).littleEndian
        guard magic == Self.expectedMagic else { return nil }

        let sequence: UInt32 = hdr.load(fromByteOffset: 8, as: UInt32.self).littleEndian
        let flags: UInt32 = hdr.load(fromByteOffset: 12, as: UInt32.self).littleEndian
        let x = Int32(bitPattern: hdr.load(fromByteOffset: 16, as: UInt32.self).littleEndian)
        let y = Int32(bitPattern: hdr.load(fromByteOffset: 20, as: UInt32.self).littleEndian)
        let width: UInt32 = hdr.load(fromByteOffset: 24, as: UInt32.self).littleEndian
        let height: UInt32 = hdr.load(fromByteOffset: 28, as: UInt32.self).littleEndian
        let stride: UInt32 = hdr.load(fromByteOffset: 32, as: UInt32.self).littleEndian
        let dataSize: UInt32 = hdr.load(fromByteOffset: 36, as: UInt32.self).littleEndian

        guard Int(dataSize) > 0 else {
            return SharedFrame(sequence: sequence, flags: flags,
                               screenX: x, screenY: y,
                               width: 0, height: 0, stride: 0,
                               bgra: Data())
        }
        let pixOffset = Self.headerSize
        guard pixOffset + Int(dataSize) <= size else { return nil }

        let pixPtr = p.advanced(by: pixOffset)
        // 把 BGRA 复制出来, 避免后续 mmap 区被覆盖时 NSImage 拿到撕裂数据
        let bgra = Data(bytes: pixPtr, count: Int(dataSize))

        return SharedFrame(sequence: sequence, flags: flags,
                           screenX: x, screenY: y,
                           width: Int(width), height: Int(height),
                           stride: Int(stride),
                           bgra: bgra)
    }

    /// 仅当 sequence > 上次返回值时返回新 frame, 否则 nil。
    /// 上层 polling 调用方便。
    public func snapshotIfNew() -> SharedFrame? {
        guard let f = snapshot() else { return nil }
        lock.lock()
        let prev = lastSeq
        if f.sequence == prev { lock.unlock(); return nil }
        lastSeq = f.sequence
        lock.unlock()
        return f
    }
}
