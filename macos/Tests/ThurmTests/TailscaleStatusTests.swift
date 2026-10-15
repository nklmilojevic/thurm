import XCTest
@testable import Thurm

final class TailscaleStatusTests: XCTestCase {
    private func json(_ v: String?) -> Any { v.map { $0 as Any } ?? NSNull() }

    private func status(_ state: String, https: String = "unavailable", httpsError: String? = nil,
                        error: String? = nil, authURL: String? = nil) throws -> TailscaleStatus {
        try XCTUnwrap(TailscaleStatus(json: [
            "state": state, "hostname": "thurm", "dns_name": "thurm.tail1234.ts.net",
            "url": "wss://thurm.tail1234.ts.net/v1/connect", "allow_tailnet_members": false,
            "account": ["login_name": "me@example.com"],
            "https": ["state": https, "error": json(httpsError)],
            "error": json(error), "auth_url": json(authURL),
        ] as [String: Any]))
    }

    func testServingOnlyOnceHTTPSIsReady() throws {
        XCTAssertEqual(try status("running", https: "ready").label, "Serving")
        XCTAssertEqual(try status("running", https: "ready").detail, "Only me@example.com may connect.")
        for https in ["magic_dns_off", "certs_off", "provisioning", "unavailable"] {
            let s = try status("running", https: https)
            XCTAssertNotEqual(s.label, "Serving", https)
            XCTAssertNil(s.pairingURL, https)
        }
        XCTAssertEqual(try status("running", https: "magic_dns_off").label, "Needs MagicDNS")
        XCTAssertEqual(try status("running", https: "certs_off").label, "Needs HTTPS certificates")
        XCTAssertEqual(try status("running", https: "certs_off", httpsError: "certs are off").detail,
                       "certs are off")
        XCTAssertNotNil(try status("running", https: "magic_dns_off").detail)
    }

    func testSignOutOfferedWheneverTheNodeMayHoldASignIn() throws {
        for state in ["running", "needs_login", "needs_machine_auth", "off", "stopped", "error"] {
            XCTAssertTrue(try status(state).canSignOut, state)
        }
        XCTAssertFalse(try status("starting").canSignOut)
    }

    func testNotificationKeys() throws {
        XCTAssertNil(try status("needs_login").notificationKey, "no link to send yet")
        XCTAssertEqual(try status("needs_login", authURL: "https://login.tailscale.com/a/x").notificationKey,
                       "needs_login")
        XCTAssertEqual(try status("needs_machine_auth").notificationKey, "needs_machine_auth")
        XCTAssertEqual(try status("error", error: "boom").notificationKey, "error:boom")
        XCTAssertEqual(try status("stopped", error: "the Tailscale helper stopped; restarting it").notificationKey,
                       "stopped:the Tailscale helper stopped; restarting it")
        for state in ["off", "starting", "running", "stopped"] {
            XCTAssertNil(try status(state).notificationKey, state)
        }
    }

    func testNoticeHeldWhileNotificationsAreOffPostsOnceTheyAreOn() throws {
        let login = try status("needs_login", authURL: "https://login.tailscale.com/a/x")
        var notices = TailscaleNotices()
        XCTAssertFalse(notices.shouldPost(login, enabled: false))
        XCTAssertFalse(notices.shouldPost(login, enabled: false))
        XCTAssertTrue(notices.shouldPost(login, enabled: true))
        XCTAssertFalse(notices.shouldPost(login, enabled: true), "once per state")
    }

    func testNoticePostsAgainAfterTheStateChanges() throws {
        var notices = TailscaleNotices()
        XCTAssertTrue(notices.shouldPost(try status("error", error: "boom"), enabled: true))
        XCTAssertTrue(notices.shouldPost(try status("error", error: "bang"), enabled: true))
        XCTAssertFalse(notices.shouldPost(try status("running", https: "ready"), enabled: true))
        XCTAssertNil(notices.notified)
        XCTAssertTrue(notices.shouldPost(try status("error", error: "bang"), enabled: true))
    }
}
