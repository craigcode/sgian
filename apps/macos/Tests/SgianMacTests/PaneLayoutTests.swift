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
