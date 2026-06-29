import Foundation

/// How an instance's system prompt is delivered. Port of `InjectionMode`
/// (`crates/muxel-core/src/agent.rs`), tagged by `"mode"` (snake_case values).
enum InjectionMode: Equatable {
    case none
    case cliFlag(flag: String)
    case typeIn
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
        case let other:
            throw DecodingError.dataCorruptedError(
                forKey: .mode, in: c, debugDescription: "unknown injection mode \(other)")
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
        }
    }
}
