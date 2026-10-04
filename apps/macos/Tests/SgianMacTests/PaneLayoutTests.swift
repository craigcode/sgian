import Foundation
import Testing
@testable import SgianMac

@Test func nativeLayoutRestoresRatiosAndRepairsPaneMembership() throws {
    let data = Data(#"{"type":"split","id":"split-1","direction":"column","ratio":0.3,"first":{"type":"leaf","id":"one"},"second":{"type":"leaf","id":"two"}}"#.utf8)
    let tree = try #require(PaneLayout.parse(JSONDecoder.ipc.decode(JSONValue.self, from: data)))
    #expect(tree.paneIDs == ["one", "two"])
    #expect(PaneLayout.parse(tree.json) == tree)
    let resized = tree.resizing("split-1", ratio: 0.7)
    #expect(resized.json["ratio"]?.numberValue == 0.7)
    let repaired = try #require(PaneLayout.reconcile(tree, paneIDs: ["two", "three"]))
    #expect(repaired.paneIDs == ["two", "three"])
    #expect(repaired.removing("three") == .leaf("two"))
    #expect(PaneLayout.reconcile(tree, paneIDs: []) == nil)
}

@Test func nativeLayoutSplitUsesSelectedPaneAndRejectsMalformedTrees() throws {
    let tree = PaneLayout.joined(.leaf("one"), .leaf("two"), direction: "row")
    let next = tree.inserting("three", beside: "one", direction: "column")
    #expect(next.paneIDs == ["one", "three", "two"])
    #expect(next.json["first"]?["direction"]?.stringValue == "column")
    let duplicate = PaneLayout.joined(.leaf("one"), .leaf("one"), direction: "row")
    #expect(PaneLayout.parse(duplicate.json) == nil)
    #expect(PaneLayout.parse(.object(["type": .string("split"), "id": .string("bad")] )) == nil)
    #expect(PaneLayout.clamp(.nan) == 0.5)
    #expect(tree.resizing(tree.json["id"]!.stringValue!, ratio: 100).json["ratio"]?.numberValue == 0.82)
}

@Test func panesPlacedByOthersShareTheWidthEvenly() throws {
    // Four panes created outside the client (ctl, Kranz): each column must
    // end up a quarter of the width, not 1/8, 1/8, 1/4, 1/2.
    let tree = try #require(PaneLayout.reconcile(nil, paneIDs: ["a", "b", "c", "d"]))
    func widths(_ node: PaneLayout, _ share: Double) -> [String: Double] {
        switch node {
        case let .leaf(id): return [id: share]
        case let .split(_, _, ratio, first, second):
            return widths(first, share * ratio).merging(widths(second, share * (1 - ratio))) { a, _ in a }
        }
    }
    let shares = widths(tree, 1)
    #expect(shares.count == 4)
    for (_, share) in shares { #expect(abs(share - 0.25) < 0.001) }
    // One more pane arriving later joins at a fifth.
    let five = try #require(PaneLayout.reconcile(tree, paneIDs: ["a", "b", "c", "d", "e"]))
    #expect(abs((widths(five, 1)["e"] ?? 0) - 0.2) < 0.001)
    // A pane the person split by hand keeps the half it was given.
    if case let .split(_, _, ratio, _, _) = PaneLayout.joined(.leaf("x"), .leaf("y"), direction: "column") { #expect(ratio == 0.5) }
}
