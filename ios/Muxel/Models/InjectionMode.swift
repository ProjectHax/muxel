import Foundation

/// How an instance's system prompt is delivered. Port of `InjectionMode`
/// (`crates/muxel-core/src/agent.rs`), tagged by `"mode"` (snake_case values).
enum InjectionMode: Equatable, Hashable {
    case none
    case cliFlag(flag: String)
    case typeIn
    /// Context added by Muse's SessionStart hook. Only desktop muxel's local
    /// launches install the hook, so the phone delivers nothing.
    case sessionHook
    /// A mode from a newer desktop muxel. One pane no longer fails the shared
    /// layout, and its name is written back, though not any fields a newer
    /// mode adds. The phone delivers nothing for it.
    case unknown(mode: String)
}

extension InjectionMode: Codable {
    private enum CodingKeys: String, CodingKey {
        case mode, flag
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .mode) {
        case "none": self = .none
        case "cli_flag": self = .cliFlag(flag: try c.decode(String.self, forKey: .flag))
        case "type_in": self = .typeIn
        case "session_hook": self = .sessionHook
        case let other: self = .unknown(mode: other)
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .none:
            try c.encode("none", forKey: .mode)
        case let .cliFlag(flag):
            try c.encode("cli_flag", forKey: .mode)
            try c.encode(flag, forKey: .flag)
        case .typeIn:
            try c.encode("type_in", forKey: .mode)
        case .sessionHook:
            try c.encode("session_hook", forKey: .mode)
        case let .unknown(mode):
            try c.encode(mode, forKey: .mode)
        }
    }
}
