import Darwin
import Foundation

enum SocketError: LocalizedError {
    case pathTooLong(String)
    case system(String, Int32)
    case closed
    case lineTooLong

    var errorDescription: String? {
        switch self {
        case let .pathTooLong(path): "Daemon socket path is too long: \(path)"
        case let .system(operation, code):
            "\(operation) failed: \(String(cString: strerror(code)))"
        case .closed: "The daemon closed the connection"
        case .lineTooLong: "The daemon sent an oversized message"
        }
    }
}

final class UnixSocket: @unchecked Sendable {
    private let descriptorLock = NSLock()
    private var descriptor: Int32
    private var readBuffer = Data()
    private let maximumLineBytes = 8 * 1024 * 1024

    init(path: String) throws {
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw SocketError.system("socket", errno) }
        descriptor = fd
        guard Darwin.fcntl(fd, F_SETFD, FD_CLOEXEC) == 0 else {
            let code = errno
            Darwin.close(fd)
            descriptor = -1
            throw SocketError.system("fcntl(FD_CLOEXEC)", code)
        }
        var noSigPipe: Int32 = 1
        guard Darwin.setsockopt(
            fd,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSigPipe,
            socklen_t(MemoryLayout<Int32>.size)
        ) == 0 else {
            let code = errno
            Darwin.close(fd)
            descriptor = -1
            throw SocketError.system("setsockopt(SO_NOSIGPIPE)", code)
        }

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8) + [0]
        let pathCapacity = MemoryLayout.size(ofValue: address.sun_path)
        guard pathBytes.count <= pathCapacity else {
            Darwin.close(fd)
            descriptor = -1
            throw SocketError.pathTooLong(path)
        }

        withUnsafeMutableBytes(of: &address.sun_path) { destination in
            destination.copyBytes(from: pathBytes)
        }
        let pathOffset = MemoryLayout<sockaddr_un>.offset(of: \sockaddr_un.sun_path) ?? 2
        let addressLength = socklen_t(pathOffset + pathBytes.count)
        address.sun_len = UInt8(min(Int(addressLength), Int(UInt8.max)))
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, addressLength)
            }
        }
        guard result == 0 else {
            let code = errno
            Darwin.close(fd)
            descriptor = -1
            throw SocketError.system("connect", code)
        }
        try setReadTimeout(seconds: 20)
        try setWriteTimeout(seconds: 20)
    }

    deinit { close() }

    func close() {
        let fd = descriptorLock.withLock {
            guard descriptor >= 0 else { return Int32(-1) }
            let fd = descriptor
            descriptor = -1
            return fd
        }
        guard fd >= 0 else { return }
        Darwin.shutdown(fd, SHUT_RDWR)
        Darwin.close(fd)
    }

    func setReadTimeout(seconds: Int?) throws {
        try setTimeout(option: SO_RCVTIMEO, seconds: seconds)
    }

    func setWriteTimeout(seconds: Int?) throws {
        try setTimeout(option: SO_SNDTIMEO, seconds: seconds)
    }

    private func setTimeout(option: Int32, seconds: Int?) throws {
        let interval: __darwin_time_t = numericCast(seconds ?? 0)
        var timeout = timeval(tv_sec: interval, tv_usec: 0)
        let fd = try duplicateDescriptor()
        defer { Darwin.close(fd) }
        let result = withUnsafePointer(to: &timeout) { pointer in
            Darwin.setsockopt(
                fd,
                SOL_SOCKET,
                option,
                pointer,
                socklen_t(MemoryLayout<timeval>.size)
            )
        }
        guard result == 0 else { throw SocketError.system("setsockopt", errno) }
    }

    func write(_ data: Data) throws {
        let fd = try duplicateDescriptor()
        defer { Darwin.close(fd) }
        var sent = 0
        try data.withUnsafeBytes { rawBuffer in
            guard let base = rawBuffer.baseAddress else { return }
            while sent < data.count {
                let count = Darwin.send(
                    fd,
                    base.advanced(by: sent),
                    data.count - sent,
                    0
                )
                if count < 0 {
                    if errno == EINTR { continue }
                    throw SocketError.system("send", errno)
                }
                if count == 0 { throw SocketError.closed }
                sent += count
            }
        }
    }

    func writeLine(_ data: Data) throws {
        var line = data
        line.append(0x0a)
        try write(line)
    }

    func readLine() throws -> Data {
        let fd = try duplicateDescriptor()
        defer { Darwin.close(fd) }
        while true {
            if let newline = readBuffer.firstIndex(of: 0x0a) {
                guard newline <= maximumLineBytes else { throw SocketError.lineTooLong }
                let line = readBuffer[..<newline]
                readBuffer.removeSubrange(...newline)
                return Data(line)
            }
            guard readBuffer.count < maximumLineBytes else { throw SocketError.lineTooLong }
            var bytes = [UInt8](repeating: 0, count: 16 * 1024)
            let count = Darwin.recv(fd, &bytes, bytes.count, 0)
            if count < 0 {
                if errno == EINTR { continue }
                throw SocketError.system("receive", errno)
            }
            if count == 0 { throw SocketError.closed }
            readBuffer.append(contentsOf: bytes.prefix(count))
        }
    }

    private func duplicateDescriptor() throws -> Int32 {
        // Pin the underlying socket before releasing the lock. Otherwise close()
        // can recycle the descriptor between lookup and send/recv, redirecting
        // IPC to an unrelated socket. shutdown() still wakes the duplicated fd.
        try descriptorLock.withLock {
            guard descriptor >= 0 else { throw SocketError.closed }
            let fd = Darwin.fcntl(descriptor, F_DUPFD_CLOEXEC, 0)
            guard fd >= 0 else { throw SocketError.system("fcntl(F_DUPFD_CLOEXEC)", errno) }
            return fd
        }
    }
}
