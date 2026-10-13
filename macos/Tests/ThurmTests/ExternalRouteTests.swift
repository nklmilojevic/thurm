import XCTest
@testable import Thurm

final class ExternalRouteTests: XCTestCase {
    private let items: [String: ItemKind] = [
        "/Users/me/code": .directory,
        "/Users/me/code/notes.txt": .file,
        "/Users/me/Project Files": .directory,
    ]

    private func kind(_ path: String) -> ItemKind? { items[path] }

    private func parse(_ string: String) -> ExternalRequest? {
        guard let url = URL(string: string) else { return nil }
        return ExternalRoute.parse(url, kind: kind)
    }

    func testOpenRoutes() {
        XCTAssertEqual(parse("thurm://open?path=/Users/me/code"), .tab(cwd: "/Users/me/code"))
        XCTAssertEqual(parse("thurm://open"), .tab(cwd: nil))
        XCTAssertEqual(parse("thurm://new-workspace?path=/Users/me/code"), .newWorkspace(cwd: "/Users/me/code"))
        XCTAssertEqual(parse("thurm://new-workspace"), .newWorkspace(cwd: nil))
        XCTAssertNil(parse("thurm://new-window?path=/Users/me/code"))
        XCTAssertNil(parse("thurm://new-workspace?path=/Users/me/missing"))
    }

    func testSplitRoutes() {
        XCTAssertEqual(parse("thurm://split?path=/Users/me/code&direction=down"),
                       .split(cwd: "/Users/me/code", direction: .down))
        XCTAssertEqual(parse("thurm://split?path=/Users/me/code"),
                       .split(cwd: "/Users/me/code", direction: .right))
        XCTAssertEqual(parse("thurm://split?direction=right"), .split(cwd: nil, direction: .right))
        XCTAssertNil(parse("thurm://split?direction=left"))
        XCTAssertNil(parse("thurm://split?direction=up"))
    }

    func testSettingsAndWorkspaceRoutes() {
        XCTAssertEqual(parse("thurm://settings"), .settings)
        XCTAssertNil(parse("thurm://settings?path=/Users/me/code"))
        XCTAssertEqual(parse("thurm://workspace/Project%20Files"), .workspace(name: "Project Files"))
        XCTAssertNil(parse("thurm://workspace"))
        XCTAssertNil(parse("thurm://workspace/"))
        XCTAssertNil(parse("thurm://workspace/a/b"))
    }

    func testPathMustBeAbsoluteExistingDirectory() {
        XCTAssertNil(parse("thurm://open?path=Users/me/code"))
        XCTAssertNil(parse("thurm://open?path="))
        XCTAssertNil(parse("thurm://open?path=/Users/me/missing"))
        XCTAssertNil(parse("thurm://open?path=/Users/me/code/notes.txt"))
        XCTAssertNil(parse("thurm://split?path=/Users/me/missing&direction=down"))
    }

    func testCommandsAndUnknownPartsAreRefused() {
        XCTAssertNil(parse("thurm://open?path=/Users/me/code&cmd=rm%20-rf%20~"))
        XCTAssertNil(parse("thurm://open?cmd=ls"))
        XCTAssertNil(parse("thurm://new-workspace?path=/Users/me/code&cmd=ls"))
        XCTAssertNil(parse("thurm://split?path=/Users/me/code&input=ls"))
        XCTAssertNil(parse("thurm://open?path=/Users/me/code&path=/Users/me/code"))
        XCTAssertNil(parse("thurm://open/extra?path=/Users/me/code"))
        XCTAssertNil(parse("thurm://open#fragment"))
        XCTAssertNil(parse("thurm://run?cmd=ls"))
        XCTAssertNil(parse("thurm://"))
    }

    func testOtherSchemesAreRefused() {
        XCTAssertNil(parse("https://open?path=/Users/me/code"))
        XCTAssertNil(parse("rex://open?path=/Users/me/code"))
    }

    func testSchemeIsCaseInsensitive() {
        XCTAssertEqual(parse("THURM://settings"), .settings)
    }

    func testRemoteFocusNeverReceivesLocalPath() {
        XCTAssertEqual(placement(.tab(cwd: "/Users/me/code"), remote: true),
                       .tab(cwd: "/Users/me/code", local: true))
        XCTAssertEqual(placement(.tab(cwd: nil), remote: true), .tab(cwd: nil, local: false))
        XCTAssertEqual(placement(.split(cwd: "/Users/me/code", direction: .down), remote: true),
                       .tab(cwd: "/Users/me/code", local: true))
        XCTAssertEqual(placement(.split(cwd: nil, direction: .right), remote: true),
                       .split(cwd: nil, direction: .right))
    }

    func testLocalFocusSplitsAndTabs() {
        XCTAssertEqual(placement(.tab(cwd: "/Users/me/code"), remote: false),
                       .tab(cwd: "/Users/me/code", local: true))
        XCTAssertEqual(placement(.split(cwd: "/Users/me/code", direction: .right), remote: false),
                       .split(cwd: "/Users/me/code", direction: .right))
    }

    func testNoWindowOpensATab() {
        XCTAssertEqual(ExternalRoute.placement(for: .split(cwd: "/Users/me/code", direction: .down),
                                               focusedIsRemote: false, hasWindow: false),
                       .tab(cwd: "/Users/me/code", local: true))
    }

    func testNewWorkspaceAndSettingsPlacement() {
        XCTAssertEqual(placement(.newWorkspace(cwd: "/Users/me/code"), remote: true),
                       .newWorkspace(cwd: "/Users/me/code"))
        XCTAssertEqual(placement(.settings, remote: true), .settings)
        XCTAssertEqual(placement(.workspace(name: "Work"), remote: false), .workspace(name: "Work"))
    }

    private func placement(_ request: ExternalRequest, remote: Bool) -> Placement {
        ExternalRoute.placement(for: request, focusedIsRemote: remote, hasWindow: true)
    }

    func testDirectoryForOpenedItems() {
        XCTAssertEqual(ExternalRoute.directory(forOpened: URL(fileURLWithPath: "/Users/me/code"), kind: kind),
                       "/Users/me/code")
        XCTAssertEqual(ExternalRoute.directory(forOpened: URL(fileURLWithPath: "/Users/me/code/notes.txt"),
                                               kind: kind),
                       "/Users/me/code")
        XCTAssertNil(ExternalRoute.directory(forOpened: URL(fileURLWithPath: "/Users/me/missing"), kind: kind))
        XCTAssertNil(ExternalRoute.directory(forOpened: URL(string: "https://example.com/code")!, kind: kind))
    }
}
