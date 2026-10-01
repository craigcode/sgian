import Foundation

/// The same persisted binary tree used by the daemon and the Tauri client.
indirect enum PaneLayout: Equatable {
    case leaf(String)
    case split(id: String, direction: String, ratio: Double, first: PaneLayout, second: PaneLayout)

    var paneIDs: [String] {
        switch self {
        case let .leaf(id): [id]
        case let .split(_, _, _, first, second): first.paneIDs + second.paneIDs
        }
    }

    var json: JSONValue {
        switch self {
        case let .leaf(id): .object(["type": .string("leaf"), "id": .string(id)])
        case let .split(id, direction, ratio, first, second):
            .object(["type": .string("split"), "id": .string(id), "direction": .string(direction),
                     "ratio": .number(ratio), "first": first.json, "second": second.json])
        }
    }

    static func parse(_ value: JSONValue?, depth: Int = 0) -> PaneLayout? {
        guard depth < 32, let value, let id = value["id"]?.stringValue, !id.isEmpty else { return nil }
        if value["type"]?.stringValue == "leaf" { return .leaf(id) }
        guard value["type"]?.stringValue == "split",
              let direction = value["direction"]?.stringValue, ["row", "column"].contains(direction),
              let ratio = value["ratio"]?.numberValue, ratio.isFinite, ratio > 0, ratio < 1,
              let first = parse(value["first"], depth: depth + 1),
              let second = parse(value["second"], depth: depth + 1)
        else { return nil }
        let ids = first.paneIDs + second.paneIDs
        guard Set(ids).count == ids.count else { return nil }
        return .split(id: id, direction: direction, ratio: clamp(ratio), first: first, second: second)
    }

    static func clamp(_ ratio: Double) -> Double { ratio.isFinite ? min(0.82, max(0.18, ratio)) : 0.5 }

    static func reconcile(_ layout: PaneLayout?, paneIDs: [String]) -> PaneLayout? {
        let live = Set(paneIDs)
        var tree = layout
        for id in layout?.paneIDs ?? [] where !live.contains(id) { tree = tree?.removing(id) }
        for id in paneIDs where !(tree?.paneIDs.contains(id) ?? false) {
            tree = tree.map { joined($0, .leaf(id), direction: "row") } ?? .leaf(id)
        }
        return tree
    }

    static func joined(_ first: PaneLayout, _ second: PaneLayout, direction: String) -> PaneLayout {
        .split(id: "split-\(UUID().uuidString)", direction: direction, ratio: 0.5, first: first, second: second)
    }

    func inserting(_ paneID: String, beside anchor: String?, direction: String) -> PaneLayout {
        let tree = removing(paneID)
        guard let tree else { return .leaf(paneID) }
        guard let anchor, tree.paneIDs.contains(anchor) else { return Self.joined(tree, .leaf(paneID), direction: direction) }
        return tree.replacing(anchor, with: Self.joined(.leaf(anchor), .leaf(paneID), direction: direction))
    }

    func replacing(_ paneID: String, with replacement: PaneLayout) -> PaneLayout {
        switch self {
        case let .leaf(id): id == paneID ? replacement : self
        case let .split(id, direction, ratio, first, second):
            .split(id: id, direction: direction, ratio: ratio,
                   first: first.replacing(paneID, with: replacement), second: second.replacing(paneID, with: replacement))
        }
    }

    func removing(_ paneID: String) -> PaneLayout? {
        switch self {
        case let .leaf(id): return id == paneID ? nil : self
        case let .split(id, direction, ratio, first, second):
            let a = first.removing(paneID), b = second.removing(paneID)
            guard let a else { return b }
            guard let b else { return a }
            return .split(id: id, direction: direction, ratio: ratio, first: a, second: b)
        }
    }

    func resizing(_ splitID: String, ratio: Double) -> PaneLayout {
        switch self {
        case .leaf: self
        case let .split(id, direction, oldRatio, first, second):
            .split(id: id, direction: direction, ratio: id == splitID ? Self.clamp(ratio) : oldRatio,
                   first: first.resizing(splitID, ratio: ratio), second: second.resizing(splitID, ratio: ratio))
        }
    }
}
