import XCTest
@testable import muxel

/// Covers `LocalStore`'s failure behavior: a damaged store file must be preserved
/// (renamed, never deleted) and reported — not silently reset to empty.
final class LocalStoreTests: XCTestCase {
    private var dir: URL!

    override func setUp() {
        super.setUp()
        dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-store-tests-\(UUID().uuidString)")
        _ = LocalStore.takeCorruptNotice() // drain any leftover pending notice
    }

    override func tearDown() {
        try? FileManager.default.removeItem(at: dir)
        _ = LocalStore.takeCorruptNotice()
        super.tearDown()
    }

    func testMissingFileIsEmptyDocument() throws {
        let store = LocalStore(directory: dir)
        let doc = try store.load()
        XCTAssertEqual(doc, StoreDocument(), "first launch loads an empty document")
        XCTAssertNil(LocalStore.takeCorruptNotice())
    }

    func testSaveLoadRoundTrip() throws {
        let store = LocalStore(directory: dir)
        var doc = StoreDocument()
        doc.hosts.append(Host(name: "web", hostname: "example.com"))
        doc.projects.append(RemoteProject(name: "api", hostId: doc.hosts[0].id,
                                          remoteRoot: "/srv/api"))
        try store.save(doc)
        XCTAssertEqual(try store.load(), doc)
    }

    func testCorruptFileIsPreservedAndThrows() throws {
        let store = LocalStore(directory: dir)
        let garbage = Data("not json at all {{{".utf8)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try garbage.write(to: dir.appendingPathComponent("store.json"))

        XCTAssertThrowsError(try store.load()) { error in
            guard case StoreError.corrupt(let backup, _) = error else {
                return XCTFail("expected StoreError.corrupt, got \(error)")
            }
            XCTAssertEqual(backup, "store.json.corrupt")
        }

        // The damaged bytes were renamed, not deleted or overwritten.
        let backupURL = dir.appendingPathComponent("store.json.corrupt")
        XCTAssertEqual(try Data(contentsOf: backupURL), garbage)
        XCTAssertFalse(FileManager.default.fileExists(
            atPath: dir.appendingPathComponent("store.json").path))

        // The pending notice is recorded exactly once.
        XCTAssertNotNil(LocalStore.takeCorruptNotice())
        XCTAssertNil(LocalStore.takeCorruptNotice(), "notice is one-shot")

        // After the rename, the next load is a clean empty document.
        XCTAssertEqual(try store.load(), StoreDocument())
    }

    func testSaveIntoUnwritableDirectoryThrows() {
        // `/dev/null/...` can never be created, so the write must fail loudly.
        let store = LocalStore(directory: URL(fileURLWithPath: "/dev/null/nope"))
        XCTAssertThrowsError(try store.save(StoreDocument())) { error in
            guard case StoreError.saveFailed = error else {
                return XCTFail("expected StoreError.saveFailed, got \(error)")
            }
        }
    }
}
