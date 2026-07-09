import XCTest
@testable import muxel

/// Mirrors the ported subset of `crates/muxel-core/src/pane.rs`'s unit tests, so the
/// Swift pane-tree mutations stay behaviourally identical to desktop (same active-index
/// fixup, no-duplicate guard, split collapse, same-direction flatten).
final class PaneMutationTests: XCTestCase {
    private func tabsLeaf(_ tabs: [String], _ active: Int = 0) -> PaneNode {
        .leaf(tabs: tabs, active: active)
    }
    private func single(_ id: String) -> PaneNode { .leaf(tabs: [id], active: 0) }

    // MARK: split / flatten / nest

    func testSplitALeafCreatesTwoChildSplit() {
        var tree: PaneNode? = single("a")
        XCTAssertTrue(PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b"))
        guard case let .split(direction, sizes, children)? = tree else { return XCTFail("expected split") }
        XCTAssertEqual(direction, .horizontal)
        XCTAssertEqual(children.count, 2)
        XCTAssertEqual(sizes.count, 2)
    }

    func testSameDirectionSplitsFlattenToNWay() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        PaneTree.split(&tree, target: "b", direction: .horizontal, newInstance: "c")
        XCTAssertEqual(tree?.allTabs, ["a", "b", "c"])
        guard case let .split(_, sizes, children)? = tree else { return XCTFail("expected split") }
        XCTAssertEqual(children.count, 3, "should flatten into one 3-way split")
        XCTAssertEqual(sizes.count, 3)
    }

    func testCrossDirectionSplitsNest() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        PaneTree.split(&tree, target: "b", direction: .vertical, newInstance: "c")
        guard case let .split(direction, _, children)? = tree, direction == .horizontal else {
            return XCTFail("expected horizontal split")
        }
        XCTAssertEqual(children.count, 2)
        guard case .split(.vertical, _, _) = children[1] else { return XCTFail("expected nested vertical split") }
    }

    func testSplitBesideInsertsOnTheRequestedSide() {
        var tree: PaneNode? = single("a")
        XCTAssertTrue(PaneTree.splitBeside(&tree, target: "a", direction: .horizontal, newInstance: "b", before: true))
        guard case let .split(_, _, children)? = tree else { return XCTFail("expected split") }
        XCTAssertEqual(children[0].allTabs, ["b"], "before: true puts the new pane first")
        XCTAssertEqual(children[1].allTabs, ["a"])
    }

    // MARK: remove / collapse

    func testRemoveCollapsesTwoChildSplit() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        XCTAssertTrue(PaneTree.remove(&tree, target: "a"))
        XCTAssertEqual(tree, single("b"))
    }

    func testRemoveLastPaneEmptiesTree() {
        var tree: PaneNode? = single("a")
        XCTAssertTrue(PaneTree.remove(&tree, target: "a"))
        XCTAssertNil(tree)
    }

    func testRemoveMiddleOfThreeKeepsTwo() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        PaneTree.split(&tree, target: "b", direction: .horizontal, newInstance: "c")
        XCTAssertTrue(PaneTree.remove(&tree, target: "b"))
        XCTAssertEqual(tree?.allTabs, ["a", "c"])
    }

    func testRemoveAbsentIsNoop() {
        var tree: PaneNode? = single("a")
        XCTAssertFalse(PaneTree.remove(&tree, target: "zzz"))
        XCTAssertEqual(tree, single("a"))
    }

    // MARK: tab operations

    func testRemoveTabKeepsGroup() {
        var tree: PaneNode? = tabsLeaf(["a", "b"], 0)
        XCTAssertTrue(PaneTree.remove(&tree, target: "a"))
        XCTAssertEqual(tree?.leafTabs?.tabs, ["b"])
        XCTAssertEqual(tree?.leafTabs?.active, 0)
    }

    func testRemoveActiveTabClampsActive() {
        var tree: PaneNode? = tabsLeaf(["a", "b", "c"], 2)
        XCTAssertTrue(PaneTree.remove(&tree, target: "c"))
        XCTAssertEqual(tree?.leafTabs?.tabs, ["a", "b"])
        XCTAssertEqual(tree?.leafTabs?.active, 1)
    }

    func testRemoveMiddleTabShiftsActiveDown() {
        var tree: PaneNode? = tabsLeaf(["a", "b", "c"], 2)
        XCTAssertTrue(PaneTree.remove(&tree, target: "b"))
        XCTAssertEqual(tree?.leafTabs?.tabs, ["a", "c"])
        XCTAssertEqual(tree?.leafTabs?.active, 1)
    }

    func testRemoveLastTabCollapsesPane() {
        var tree: PaneNode? = .split(direction: .horizontal, sizes: [1, 1],
                                     children: [tabsLeaf(["a", "b"], 0), single("c")])
        XCTAssertTrue(PaneTree.remove(&tree, target: "a"))
        XCTAssertTrue(PaneTree.remove(&tree, target: "b"))
        XCTAssertEqual(tree, single("c"))
    }

    func testAddTabAppendsAndActivates() {
        var tree: PaneNode? = single("a")
        XCTAssertTrue(PaneTree.addTab(&tree, target: "a", newInstance: "b"))
        XCTAssertEqual(tree?.leafTabs?.tabs, ["a", "b"])
        XCTAssertEqual(tree?.leafTabs?.active, 1)
    }

    func testAddTabTargetNotFoundIsFalse() {
        var tree: PaneNode? = single("a")
        XCTAssertFalse(PaneTree.addTab(&tree, target: "zzz", newInstance: "b"))
    }

    func testAddTabIntoAPaneOfASplit() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        XCTAssertTrue(PaneTree.addTab(&tree, target: "b", newInstance: "c"))
        let path = tree?.findPath("c")
        XCTAssertEqual(path.flatMap { tree?.node(atPath: $0)?.leafTabs?.tabs }, ["b", "c"])
        XCTAssertEqual(tree?.allTabs, ["a", "b", "c"])
    }

    func testAddTabDuplicateIsNoop() {
        var tree: PaneNode? = single("a")
        XCTAssertFalse(PaneTree.addTab(&tree, target: "a", newInstance: "a"))
    }

    func testAddTabAtPrependMiddleAppend() {
        var tree: PaneNode? = tabsLeaf(["a", "b", "c"], 0)
        XCTAssertTrue(PaneTree.addTabAt(&tree, target: "a", newInstance: "d", index: 1))
        XCTAssertEqual(tree?.leafTabs?.tabs, ["a", "d", "b", "c"])
        XCTAssertEqual(tree?.leafTabs?.active, 1)
        XCTAssertTrue(PaneTree.addTabAt(&tree, target: "a", newInstance: "e", index: 0))
        XCTAssertEqual(tree?.leafTabs?.tabs, ["e", "a", "d", "b", "c"])
        XCTAssertEqual(tree?.leafTabs?.active, 0)
        XCTAssertTrue(PaneTree.addTabAt(&tree, target: "a", newInstance: "f", index: Int.max))
        XCTAssertEqual(tree?.leafTabs?.active, 5)
    }

    func testAddTabAtRejectsDuplicateAndMissing() {
        var tree: PaneNode? = tabsLeaf(["a", "b"], 0)
        XCTAssertFalse(PaneTree.addTabAt(&tree, target: "a", newInstance: "b", index: 0))
        XCTAssertFalse(PaneTree.addTabAt(&tree, target: "zzz", newInstance: "x", index: 0))
    }

    func testSetActiveTabUpdatesIndex() {
        var tree: PaneNode? = tabsLeaf(["a", "b", "c"], 0)
        XCTAssertTrue(PaneTree.setActiveTab(&tree, instance: "b"))
        XCTAssertEqual(tree?.leafTabs?.active, 1)
        XCTAssertFalse(PaneTree.setActiveTab(&tree, instance: "zzz"))
    }

    func testSurvivingActiveAfterRemovePicksNeighbor() {
        let root = tabsLeaf(["a", "b", "c"], 1) // active = b
        XCTAssertEqual(root.survivingActiveAfterRemove("b"), "c")
        XCTAssertNil(single("a").survivingActiveAfterRemove("a"))
    }

    // MARK: move_into_split

    func testMoveIntoSplitPullsTabFromTwoTabLeaf() {
        var tree: PaneNode? = tabsLeaf(["a", "b"], 0)
        XCTAssertTrue(PaneTree.moveIntoSplit(&tree, dragged: "b", target: "a", direction: .horizontal, before: false))
        guard case let .split(direction, _, children)? = tree else { return XCTFail("expected split") }
        XCTAssertEqual(direction, .horizontal)
        XCTAssertEqual(children.count, 2)
        XCTAssertEqual(children[0].allTabs, ["a"])
        XCTAssertEqual(children[1].allTabs, ["b"])
    }

    func testMoveIntoSplitFromThreeTabLeafKeepsGroup() {
        var tree: PaneNode? = tabsLeaf(["a", "b", "c"], 0)
        XCTAssertTrue(PaneTree.moveIntoSplit(&tree, dragged: "c", target: "a", direction: .vertical, before: true))
        XCTAssertNotEqual(tree?.findPath("a"), tree?.findPath("c"))
        XCTAssertEqual(tree?.findPath("a").flatMap { tree?.node(atPath: $0)?.leafTabs?.tabs }, ["a", "b"])
        XCTAssertEqual(tree?.findPath("c").flatMap { tree?.node(atPath: $0)?.leafTabs?.tabs }, ["c"])
    }

    func testMoveIntoSplitFromOtherPane() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        XCTAssertTrue(PaneTree.moveIntoSplit(&tree, dragged: "a", target: "b", direction: .horizontal, before: true))
        XCTAssertEqual(tree?.allTabs, ["a", "b"])
        XCTAssertNotEqual(tree?.findPath("a"), tree?.findPath("b"))
    }

    func testMoveIntoSplitSameInstanceNoop() {
        var tree: PaneNode? = single("a")
        XCTAssertFalse(PaneTree.moveIntoSplit(&tree, dragged: "a", target: "a", direction: .horizontal, before: false))
        XCTAssertEqual(tree, single("a"))
    }

    func testMoveIntoSplitPathRevalidationThreePane() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        PaneTree.split(&tree, target: "b", direction: .horizontal, newInstance: "c")
        // [a | b | c]. Pull a to the left of c → [b, a, c].
        XCTAssertTrue(PaneTree.moveIntoSplit(&tree, dragged: "a", target: "c", direction: .horizontal, before: true))
        XCTAssertEqual(tree?.allTabs, ["b", "a", "c"])
        XCTAssertNotEqual(tree?.findPath("a"), tree?.findPath("c"))
    }

    func testMoveIntoTabsMergesTwoPanes() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        XCTAssertTrue(PaneTree.moveIntoTabs(&tree, dragged: "a", target: "b"))
        // Collapsed to a single tabbed leaf [b, a] with a active.
        XCTAssertEqual(tree?.leafTabs?.tabs, ["b", "a"])
        XCTAssertEqual(tree?.leafTabs?.active, 1)
    }

    func testMoveIntoTabsSamePaneNoop() {
        var tree: PaneNode? = tabsLeaf(["a", "b"], 0)
        XCTAssertFalse(PaneTree.moveIntoTabs(&tree, dragged: "a", target: "b"))
        XCTAssertEqual(tree?.leafTabs?.tabs, ["a", "b"])
    }

    func testMoveTabToSameLeafReorder() {
        var tree: PaneNode? = tabsLeaf(["a", "b", "c"], 0)
        XCTAssertTrue(PaneTree.moveTabTo(&tree, dragged: "a", targetAnchor: "a", index: 2))
        XCTAssertEqual(tree?.leafTabs?.tabs, ["b", "c", "a"])
        XCTAssertEqual(tree?.leafTabs?.active, 2)
    }

    func testMoveTabToSameSlotNoop() {
        var tree: PaneNode? = tabsLeaf(["a", "b", "c"], 0)
        XCTAssertFalse(PaneTree.moveTabTo(&tree, dragged: "a", targetAnchor: "a", index: 0))
    }

    func testMoveTabToCrossLeafInsertsAtIndex() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        XCTAssertTrue(PaneTree.moveTabTo(&tree, dragged: "a", targetAnchor: "b", index: 0))
        XCTAssertEqual(tree?.leafTabs?.tabs, ["a", "b"])
    }

    func testSetSplitSizesByKey() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        let key = tree!.stableKey
        XCTAssertTrue(PaneTree.setSplitSizes(&tree, key: key, sizes: [0.7, 0.3]))
        if case let .split(_, s, _)? = tree { XCTAssertEqual(s, [0.7, 0.3]) } else { XCTFail() }
        // Wrong count → match found (true) but sizes unchanged.
        XCTAssertTrue(PaneTree.setSplitSizes(&tree, key: key, sizes: [1, 2, 3]))
        if case let .split(_, s, _)? = tree { XCTAssertEqual(s, [0.7, 0.3]) } else { XCTFail() }
        // Unknown key → false.
        XCTAssertFalse(PaneTree.setSplitSizes(&tree, key: "zzz", sizes: [0.5, 0.5]))
    }

    func testDropZoneClassification() {
        let r = CGRect(x: 0, y: 0, width: 400, height: 300)
        // Tab strip (top band) and body center → join as a tab.
        XCTAssertEqual(PaneDropZone.classify(point: CGPoint(x: 200, y: 20), in: r), .tabs)
        XCTAssertEqual(PaneDropZone.classify(point: CGPoint(x: 200, y: 150), in: r), .tabs)
        // Body edges → split on that side.
        XCTAssertEqual(PaneDropZone.classify(point: CGPoint(x: 8, y: 150), in: r), .split(.horizontal, before: true))
        XCTAssertEqual(PaneDropZone.classify(point: CGPoint(x: 392, y: 150), in: r), .split(.horizontal, before: false))
        XCTAssertEqual(PaneDropZone.classify(point: CGPoint(x: 200, y: 292), in: r), .split(.vertical, before: false))
    }

    func testLeafCount() {
        var tree: PaneNode? = single("a")
        XCTAssertEqual(tree?.leafCount, 1)
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        PaneTree.split(&tree, target: "b", direction: .vertical, newInstance: "c")
        XCTAssertEqual(tree?.leafCount, 3)
        // Extra tabs in a leaf don't add panes.
        PaneTree.addTab(&tree, target: "c", newInstance: "d")
        XCTAssertEqual(tree?.leafCount, 3)
    }

    // MARK: find_path + serde round-trip of a mutated tree

    func testFindPathAndGet() {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        PaneTree.split(&tree, target: "b", direction: .vertical, newInstance: "c")
        let path = tree!.findPath("c")!
        XCTAssertEqual(tree!.node(atPath: path), single("c"))
    }

    func testMutatedTreeRoundTrips() throws {
        var tree: PaneNode? = single("a")
        PaneTree.split(&tree, target: "a", direction: .horizontal, newInstance: "b")
        PaneTree.moveIntoSplit(&tree, dragged: "b", target: "a", direction: .vertical, before: false)
        let data = try MuxelJSON.encoder.encode(tree)
        let back = try MuxelJSON.decoder.decode(PaneNode?.self, from: data)
        XCTAssertEqual(back, tree)
    }
}
