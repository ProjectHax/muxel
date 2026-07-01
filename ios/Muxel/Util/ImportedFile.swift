import Foundation

/// Reading a user-picked file (SwiftUI `fileImporter`) under its security scope —
/// shared by the host and identity editors' private-key import.
enum ImportedFile {
    /// Read a fileImporter result's bytes under its security scope. nil when the
    /// pick failed or the file couldn't be read.
    static func read(_ result: Result<URL, Error>) -> (data: Data, name: String)? {
        guard case let .success(url) = result else { return nil }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        guard let data = try? Data(contentsOf: url) else { return nil }
        return (data, url.lastPathComponent)
    }
}
