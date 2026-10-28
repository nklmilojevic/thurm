import XCTest
@testable import Thurm

final class PaneKeyTests: XCTestCase {
    func testLocalPaneCopiesTheBareID() {
        XCTAssertEqual(PaneKey.local(7).copyText, "7")
    }

    /// `--remote` takes the configured host name, which is the pane's host id.
    func testRemotePaneCopiesTheRemoteFlags() {
        XCTAssertEqual(PaneKey("devbox", 7).copyText, "--remote=devbox --pane 7")
    }

    /// Remote names may start with `-`; a separate token would read as another option.
    func testLeadingHyphenRemoteStaysTheRemoteValue() {
        XCTAssertEqual(PaneKey("-dev", 7).copyText, "--remote=-dev --pane 7")
    }
}
