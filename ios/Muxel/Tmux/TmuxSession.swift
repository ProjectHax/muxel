import Foundation

/// Faithful Swift port of `muxel-core`'s tmux session naming
/// (`crates/muxel-core/src/tmux.rs`). The session name is part of the interop
/// contract with desktop muxel — it MUST match byte-for-byte for `new-session -A`
/// to attach to (rather than duplicate) an existing session.
///
/// Name shape: `muxel_<slug>_<instance-uuid8>` where `<slug>` is the host display
/// name with every non-ASCII-alphanumeric character replaced by `_`, trimmed of
/// leading/trailing `_` (empty → `p`), and `<instance-uuid8>` is the first 8 hex
/// of the instance UUID (lowercase, no hyphens).
///
/// Interop note: the slug comes from the *host display name*. For desktop's
/// `new-session -A` to unify with a session this app created (and vice versa), the
/// host must be named the same on both. When merely *viewing*, prefer resolving a
/// session by its uuid8 suffix (`session(_:matchesInstance:)`) so we attach to
/// whatever session already exists for an instance regardless of slug.
enum TmuxSession {
    /// Compute the canonical session name for `(hostName, instanceId)`.
    static func name(hostName: String, instanceId: String) -> String {
        let slug = slugify(hostName)
        return "muxel_\(slug.isEmpty ? "p" : slug)_\(uuid8(instanceId))"
    }

    /// Whether a tmux session name belongs to `instanceId` (matches the uuid8
    /// suffix), ignoring the host-name slug. Used to find the session for an
    /// instance among `list-sessions` output.
    static func session(_ name: String, matchesInstanceId instanceId: String) -> Bool {
        name.hasPrefix("muxel_") && name.hasSuffix("_\(uuid8(instanceId))")
    }

    /// First 8 hex of a UUID string, lowercase, no hyphens — Rust's
    /// `uuid.simple()[..8]`.
    static func uuid8(_ idString: String) -> String {
        String(idString.replacingOccurrences(of: "-", with: "").lowercased().prefix(8))
    }

    // UUID convenience overloads (the interop models use String ids).
    static func name(hostName: String, instanceId: UUID) -> String {
        name(hostName: hostName, instanceId: instanceId.uuidString)
    }
    static func session(_ name: String, matchesInstance instanceId: UUID) -> Bool {
        session(name, matchesInstanceId: instanceId.uuidString)
    }
    static func uuid8(_ id: UUID) -> String { uuid8(id.uuidString) }

    /// Map each Unicode scalar to itself when it's ASCII `[0-9A-Za-z]`, else `_`,
    /// then trim leading/trailing `_` — exactly matching Rust's
    /// `chars().map(is_ascii_alphanumeric ? c : '_').trim_matches('_')`.
    static func slugify(_ s: String) -> String {
        var out = String.UnicodeScalarView()
        for scalar in s.unicodeScalars {
            let v = scalar.value
            let isAsciiAlnum =
                (v >= 0x30 && v <= 0x39) || // 0-9
                (v >= 0x41 && v <= 0x5A) || // A-Z
                (v >= 0x61 && v <= 0x7A)    // a-z
            out.append(isAsciiAlnum ? scalar : "_")
        }
        return String(out).trimmingCharacters(in: CharacterSet(charactersIn: "_"))
    }
}
