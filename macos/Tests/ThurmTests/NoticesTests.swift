import XCTest
@testable import Thurm

final class NoticesTests: XCTestCase {
    private let ours = "0.12.1+412.3fa1c2e"

    private func remote(_ name: String, phase: String = "connected", build: String?, upgrade: Bool) -> RemoteStatus {
        var json: [String: Any] = ["name": name, "phase": phase, "upgrade_available": upgrade]
        if let build { json["remote_build"] = build }
        return RemoteStatus(json: json)!
    }

    func testComparesBuilds() {
        XCTAssertTrue(Notices.isNewer("0.13.0+450.aaaaaaa", than: ours))
        XCTAssertTrue(Notices.isNewer("0.12.1+413.aaaaaaa", than: ours))
        XCTAssertTrue(Notices.isNewer("0.12.10", than: "0.12.9"))
        // Release and tip builds: the commit count decides.
        XCTAssertTrue(Notices.isNewer("0.12.1-tip.3fa1c2e+831", than: "0.12.1+830"))
        XCTAssertFalse(Notices.isNewer("0.12.1+830", than: "0.12.1-tip.3fa1c2e+831"))
        XCTAssertTrue(Notices.isNewer("0.12.1+830", than: "0.12.0-tip.fb01231+820"))
        XCTAssertTrue(Notices.isNewer("0.13.0+nix.3fa1c2e", than: "0.12.1+830"))
        XCTAssertFalse(Notices.isNewer("0.12.0+400.aaaaaaa", than: ours))
        XCTAssertFalse(Notices.isNewer(ours, than: ours))
        // Nix builds carry no commit count: same version is not known to be newer.
        XCTAssertFalse(Notices.isNewer("0.12.1+nix.3fa1c2e", than: ours))
        XCTAssertFalse(Notices.isNewer(ours, than: "0.12.1+nix.3fa1c2e"))
    }

    func testNothingWhenAllIsCurrent() {
        let n = Notices.notices(update: nil, localDaemon: nil,
                                remotes: [remote("a", build: ours, upgrade: false)], ours: ours, dismissed: [])
        XCTAssertEqual(n, [])
    }

    func testUpdateComesFirstAndCannotBeDismissed() {
        let n = Notices.notices(update: "Update ready: 0.13.0", localDaemon: "0.12.0+400.aaaaaaa",
                                remotes: [], ours: ours, dismissed: [])
        XCTAssertEqual(n.map(\.kind), [.update, .localDaemon])
        XCTAssertFalse(n[0].dismissible)
        XCTAssertTrue(n[1].dismissible)
    }

    func testDismissedLocalDaemonReturnsForAnotherBuild() {
        let first = Notices.notices(update: nil, localDaemon: "0.12.0+400.aaaaaaa", remotes: [], ours: ours,
                                    dismissed: [])
        let dismissed = Set(first[0].keys)
        XCTAssertEqual(Notices.notices(update: nil, localDaemon: "0.12.0+400.aaaaaaa", remotes: [], ours: ours,
                                       dismissed: dismissed), [])
        XCTAssertEqual(Notices.notices(update: nil, localDaemon: "0.12.0+401.bbbbbbb", remotes: [], ours: ours,
                                       dismissed: dismissed).map(\.kind), [.localDaemon])
    }

    func testOlderRemotesShareOneNotice() {
        let remotes = [
            remote("devbox", build: "0.12.0+400.aaaaaaa", upgrade: true),
            remote("current", build: ours, upgrade: false),
            remote("ahead", build: "0.13.0+450.ccccccc", upgrade: true),
            remote("old", phase: "upgrade_needed", build: nil, upgrade: false),
        ]
        let n = Notices.notices(update: nil, localDaemon: nil, remotes: remotes, ours: ours, dismissed: [])
        XCTAssertEqual(n.map(\.kind), [.remotes(["devbox", "old"])])
        XCTAssertEqual(n[0].text, "Update Thurm on 2 hosts")

        let one = Notices.notices(update: nil, localDaemon: nil, remotes: Array(remotes.prefix(1)), ours: ours,
                                  dismissed: [])
        XCTAssertEqual(one[0].text, "Update Thurm on devbox")
        XCTAssertEqual(one[0].tooltip, "devbox runs Thurm 0.12.0+400.aaaaaaa, this Mac \(ours).")
    }

    func testDismissedRemoteReturnsAfterItsBuildChanges() {
        let before = [remote("devbox", build: "0.12.0+400.aaaaaaa", upgrade: true)]
        let dismissed = Set(Notices.notices(update: nil, localDaemon: nil, remotes: before, ours: ours,
                                            dismissed: [])[0].keys)
        XCTAssertEqual(Notices.notices(update: nil, localDaemon: nil, remotes: before, ours: ours,
                                       dismissed: dismissed), [])
        let after = [remote("devbox", build: "0.12.0+405.ddddddd", upgrade: true)]
        XCTAssertEqual(Notices.notices(update: nil, localDaemon: nil, remotes: after, ours: ours,
                                       dismissed: dismissed).count, 1)
    }
}
