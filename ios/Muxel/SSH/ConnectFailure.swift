import Foundation
import Network
import NIOCore
import NIOPosix

/// Why connecting to a host failed, read out of SwiftNIO's errors so the host
/// screen shows the reason instead of `NIOConnectionError error 1`. On cellular it
/// also names the likely cause of "works on Wi‑Fi, not on 5G": cellular data is off
/// for muxel, or the host is an address that only exists on the home network.
enum ConnectFailure: Equatable {
    /// The name doesn't resolve on this network.
    case lookupFailed
    /// The host answered and refused the port.
    case refused
    /// No route to the host from this network.
    case unreachable
    /// The TCP connect didn't complete in time.
    case timedOut
    /// TCP connected, but the SSH key exchange and login missed Citadel's deadline.
    case handshakeTimedOut
    /// The server closed the connection.
    case closed
    case other(String)

    /// Citadel's login deadline (`ClientHandshakeHandler`). Its expiry is also a
    /// `ChannelError.connectTimeout`, told apart from the TCP connect timeout
    /// (`SSHClientSettings.connectTimeout`, 30s) by this amount.
    static let loginTimeout = TimeAmount.seconds(10)

    init(_ error: Error) {
        switch error {
        case let e as ChannelError:
            switch e {
            case let .connectTimeout(amount):
                self = amount == Self.loginTimeout ? .handshakeTimedOut : .timedOut
            case .eof, .ioOnClosedChannel, .alreadyClosed, .inputClosed, .outputClosed:
                self = .closed
            default:
                self = .other("\(e)")
            }
        case let e as NIOConnectionError:
            // No connection attempts means no address came back from DNS.
            self = Self.classify(
                errnos: e.connectionErrors.compactMap { ($0.error as? IOError)?.errnoCode },
                lookupFailed: e.connectionErrors.isEmpty,
                fallback: "\(e)")
        case is SocketAddressError.UnknownHost:
            self = .lookupFailed
        case let e as IOError:
            self = Self.classify(errnos: [e.errnoCode], lookupFailed: false, fallback: "\(e)")
        default:
            self = .other(error.localizedDescription)
        }
    }

    /// One verdict from every address tried (Happy Eyeballs tries IPv6 and IPv4). A
    /// refusal wins, since the host is up and only the port isn't; then a timeout;
    /// then no route.
    static func classify(errnos: [CInt], lookupFailed: Bool, fallback: String) -> ConnectFailure {
        if lookupFailed { return .lookupFailed }
        if errnos.contains(ECONNREFUSED) { return .refused }
        if errnos.contains(ETIMEDOUT) { return .timedOut }
        let noRoute: [CInt] = [ENETUNREACH, EHOSTUNREACH, ENETDOWN, EHOSTDOWN, EADDRNOTAVAIL]
        if errnos.contains(where: noRoute.contains) { return .unreachable }
        return .other(fallback)
    }

    /// Whether the device's network can explain this failure. A refusal or a
    /// stalled login proves the host was reached.
    var dependsOnNetwork: Bool {
        switch self {
        case .lookupFailed, .unreachable, .timedOut, .other: return true
        case .refused, .handshakeTimedOut, .closed: return false
        }
    }

    /// The text the host screen shows. `network`, when known, turns a bare network
    /// failure into its likely cause.
    func message(host: String, port: Int, network: NetworkSnapshot?) -> String {
        if dependsOnNetwork, let blocker = network?.blocker { return blocker.message }
        var text: String
        switch self {
        case .lookupFailed:
            text = "Couldn't find \(host): the name doesn't resolve on this network."
        case .refused:
            text = "\(host) refused the connection on port \(port). Check the port and that sshd is running."
        case .unreachable:
            text = "\(host) isn't reachable from this network (no route to host)."
        case .timedOut:
            text = "Connecting to \(host) on port \(port) timed out."
        case .handshakeTimedOut:
            text = "\(host) accepted the connection, but the SSH login didn't finish within "
                + "\(Self.loginTimeout.nanoseconds / 1_000_000_000) seconds."
        case .closed:
            text = "The connection was closed by the server. Try again."
        case let .other(detail):
            text = "Couldn't connect to \(host) (\(detail))."
        }
        guard dependsOnNetwork else { return text }
        if network?.cellularOnly == true {
            text += Self.isLocalOnly(host)
                ? " It's a local-network address, so on cellular it's only reachable through a VPN such as Tailscale."
                : " You're on cellular: if \(host) is only reachable from your home or office network, connect through a VPN such as Tailscale."
        } else {
            text += " Check the hostname and port, and that this device can reach the host (VPN/network)."
        }
        return text
    }

    /// An address that only exists on a local network: a private, link-local or
    /// loopback IP, or a name in a LAN-only domain. Off that network it needs a VPN.
    static func isLocalOnly(_ rawHost: String) -> Bool {
        var host = rawHost.lowercased()
        if host.hasPrefix("["), host.hasSuffix("]") { host = String(host.dropFirst().dropLast()) }
        if let zone = host.firstIndex(of: "%") { host = String(host[..<zone]) }
        if let v4 = IPv4Address(host) {
            let b = [UInt8](v4.rawValue)
            return b[0] == 10 || b[0] == 127
                || (b[0] == 172 && (16...31).contains(b[1]))
                || (b[0] == 192 && b[1] == 168)
                || (b[0] == 169 && b[1] == 254)
        }
        if let v6 = IPv6Address(host) {
            let b = [UInt8](v6.rawValue)
            return v6.isLoopback
                || (b[0] & 0xfe) == 0xfc                  // fc00::/7 unique local
                || (b[0] == 0xfe && (b[1] & 0xc0) == 0x80) // fe80::/10 link local
        }
        if host.hasSuffix(".") { host.removeLast() }
        return ["local", "lan", "home", "home.arpa", "localdomain", "internal"]
            .contains { host == $0 || host.hasSuffix("." + $0) }
    }
}

/// The device's network path when a connect failed (`NWPathMonitor`).
struct NetworkSnapshot: Equatable, Sendable {
    enum Blocker: Equatable, Sendable {
        case offline, cellularDenied, wifiDenied, localNetworkDenied

        var message: String {
            switch self {
            case .offline:
                return "This device has no network connection."
            case .cellularDenied:
                return "Cellular data is turned off for muxel. Turn it on in Settings → Cellular → muxel, or join Wi‑Fi."
            case .wifiDenied:
                return "muxel isn't allowed to use WLAN data. Allow it in Settings → muxel → Wireless Data."
            case .localNetworkDenied:
                return "muxel isn't allowed to reach devices on your local network. "
                    + "Allow it in Settings → Privacy & Security → Local Network."
            }
        }
    }

    /// Why the path is down, when it is.
    var blocker: Blocker?
    /// Up over cellular, with no Wi‑Fi or wired link.
    var cellularOnly = false

    init(blocker: Blocker? = nil, cellularOnly: Bool = false) {
        self.blocker = blocker
        self.cellularOnly = cellularOnly
    }

    init(_ path: NWPath) {
        switch path.status {
        case .satisfied:
            self.init(cellularOnly: path.usesInterfaceType(.cellular)
                && !path.usesInterfaceType(.wifi) && !path.usesInterfaceType(.wiredEthernet))
        case .unsatisfied:
            switch path.unsatisfiedReason {
            case .cellularDenied: self.init(blocker: .cellularDenied)
            case .wifiDenied: self.init(blocker: .wifiDenied)
            case .localNetworkDenied: self.init(blocker: .localNetworkDenied)
            default: self.init(blocker: .offline)
            }
        default:
            // `.requiresConnection`: the path can come up on demand; nothing to blame.
            self.init()
        }
    }

    /// The current path, read once; nil if `NWPathMonitor` doesn't answer within a second.
    static func current() async -> NetworkSnapshot? {
        let monitor = NWPathMonitor()
        defer { monitor.cancel() }
        return await withCheckedContinuation { continuation in
            let reading = PathReading(continuation)
            monitor.pathUpdateHandler = { path in reading.finish(NetworkSnapshot(path)) }
            monitor.start(queue: .global(qos: .utility))
            DispatchQueue.global().asyncAfter(deadline: .now() + 1) { reading.finish(nil) }
        }
    }
}

/// Resumes `NetworkSnapshot.current()` once: on the first path update or the timeout.
private final class PathReading: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<NetworkSnapshot?, Never>?

    init(_ continuation: CheckedContinuation<NetworkSnapshot?, Never>) {
        self.continuation = continuation
    }

    func finish(_ value: NetworkSnapshot?) {
        let pending = lock.withLock { () -> CheckedContinuation<NetworkSnapshot?, Never>? in
            defer { continuation = nil }
            return continuation
        }
        pending?.resume(returning: value)
    }
}
