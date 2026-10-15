import XCTest
@testable import Thurm

final class PairPhoneTests: XCTestCase {
    private let deviceKey = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBCX3gCbaYfg5u9YLs2Wl/JueENke93ZHXviQRq40gOlkHjaEa1MveakIBTyg0/VVXe/fFWxV3pteMd9Pe1sUf8g= thurm-ios:iPhone"

    func testFingerprintMatchesSshKeygen() {
        // `ssh-keygen -lf` of this key.
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINx8GUwWGlzynTf2livkowYPZiyb97ww3S3Lhj7csFN+ host"
        XCTAssertEqual(PhonePairing.fingerprint(publicKeyLine: line),
                       "SHA256:YU6LvFHdTg1dnOOH+NqrecmSEjtnOCbFmqP7ct+qH0o")
    }

    func testAcceptsOnlyThurmDeviceKeys() {
        XCTAssertEqual(PhonePairing.validDeviceKey(deviceKey)?.device, "iPhone")
        XCTAssertNil(PhonePairing.validDeviceKey(deviceKey.replacingOccurrences(of: "thurm-ios:", with: "")))
        XCTAssertNil(PhonePairing.validDeviceKey("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINx8GUwWGlzynTf2livkowYPZiyb97ww3S3Lhj7csFN+ thurm-ios:x"))
        XCTAssertNil(PhonePairing.validDeviceKey("command=\"x\" " + deviceKey))
    }

    func testRejectsALineBreakSmugglingASecondKey() {
        let second = "ssh-ed25519\tAAAAC3NzaC1lZDI1NTE5AAAAINx8GUwWGlzynTf2livkowYPZiyb97ww3S3Lhj7csFN+\tx"
        XCTAssertNil(PhonePairing.validDeviceKey(deviceKey + "\n" + second))
        XCTAssertNil(PhonePairing.validDeviceKey(deviceKey.replacingOccurrences(of: "thurm-ios:", with: "thurm-ios:a\nb")))
        XCTAssertNil(PhonePairing.validDeviceKey(deviceKey.replacingOccurrences(of: "iPhone", with: "i\tPhone")))
    }

    func testRejectsAKeyBlobOfTheRightLengthThatIsNoKey() {
        let parts = deviceKey.split(separator: " ")
        let garbage = Data(repeating: 0x41, count: 104).base64EncodedString()
        XCTAssertNil(PhonePairing.validDeviceKey("\(parts[0]) \(garbage) \(parts[2])"))
        // The right header, but a point that is not on the curve.
        var blob = Data(base64Encoded: String(parts[1]))!
        blob[blob.count - 1] ^= 0xFF
        XCTAssertNil(PhonePairing.validDeviceKey("\(parts[0]) \(blob.base64EncodedString()) \(parts[2])"))
    }

    func testAnUnreadableFileIsNotReplaced() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let file = dir.appendingPathComponent(".ssh/authorized_keys")
        try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        // Not UTF-8: unreadable as text.
        try Data([0x73, 0x73, 0x68, 0xFF, 0xFE, 0x0A]).write(to: file)
        let (key, device) = try XCTUnwrap(PhonePairing.validDeviceKey(deviceKey))
        XCTAssertThrowsError(try PhonePairing.add(key: key, device: device, to: file))
        XCTAssertThrowsError(try PhonePairing.revoke(.init(name: "x", line: "x"), in: file))
        XCTAssertEqual(try Data(contentsOf: file), Data([0x73, 0x73, 0x68, 0xFF, 0xFE, 0x0A]))
        // A missing file is an empty one.
        try FileManager.default.removeItem(at: file)
        try PhonePairing.add(key: key, device: device, to: file)
        XCTAssertEqual(PhonePairing.devices(in: file).map(\.name), ["iPhone"])
    }

    func testAddReplaceAndRevokeKeepOtherKeys() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let file = dir.appendingPathComponent(".ssh/authorized_keys")
        try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        try "ssh-ed25519 AAAA other@laptop\n".write(to: file, atomically: true, encoding: .utf8)
        defer { try? FileManager.default.removeItem(at: dir) }

        let (key, device) = try XCTUnwrap(PhonePairing.validDeviceKey(deviceKey))
        try PhonePairing.add(key: key, device: device, to: file)
        try PhonePairing.add(key: key, device: device, to: file)
        var devices = PhonePairing.devices(in: file)
        XCTAssertEqual(devices.map(\.name), ["iPhone"], "pairing again replaces the key")
        XCTAssertTrue(devices[0].line.hasPrefix("restrict,port-forwarding ecdsa-sha2-nistp256 "))
        let mode = try FileManager.default.attributesOfItem(atPath: file.path)[.posixPermissions] as? Int
        XCTAssertEqual(mode, 0o600)

        try PhonePairing.revoke(devices[0], in: file)
        devices = PhonePairing.devices(in: file)
        XCTAssertTrue(devices.isEmpty)
        XCTAssertEqual(try String(contentsOf: file, encoding: .utf8), "ssh-ed25519 AAAA other@laptop\n")
    }

    func testOnlyLinesThurmWroteAreDevices() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let file = dir.appendingPathComponent(".ssh/authorized_keys")
        try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let (key, device) = try XCTUnwrap(PhonePairing.validDeviceKey(deviceKey))
        let lookalikes = """
        # retired: thurm-ios:iPhone
        ssh-ed25519 AAAAC3Nz admin thurm-ios:iPhone
        ssh-ed25519 AAAAC3Nz thurm-ios:iPhone
        restrict,port-forwarding \(key) extra thurm-ios:iPhone
        no-pty \(key) thurm-ios:iPhone

        """
        try lookalikes.write(to: file, atomically: true, encoding: .utf8)
        XCTAssertTrue(PhonePairing.devices(in: file).isEmpty)
        try PhonePairing.add(key: key, device: device, to: file)
        XCTAssertEqual(try String(contentsOf: file, encoding: .utf8),
                       lookalikes + "restrict,port-forwarding \(key) thurm-ios:iPhone\n",
                       "pairing again replaces only the line Thurm wrote")
        let devices = PhonePairing.devices(in: file)
        XCTAssertEqual(devices.map(\.name), ["iPhone"])
        try PhonePairing.revoke(devices[0], in: file)
        XCTAssertEqual(try String(contentsOf: file, encoding: .utf8), lookalikes)
    }

    func testPairingLinkRoundTrips() throws {
        let link = PhonePairing.payload(name: "studio", host: "studio.tail.ts.net", port: 22, user: "me",
                                        hostKeys: ["SHA256:abc"], relay: nil, code: nil)
        XCTAssertTrue(link.hasPrefix("thurm://pair?d="))
        var b64 = String(link.dropFirst("thurm://pair?d=".count))
            .replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        b64 += String(repeating: "=", count: (4 - b64.count % 4) % 4)
        let d = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(Data(base64Encoded: b64))) as? [String: Any])
        XCTAssertEqual(d["v"] as? Int, 1)
        XCTAssertEqual(d["host_keys"] as? [String], ["SHA256:abc"])
        XCTAssertNil(d["relay"])
        XCTAssertNil(d["remotes"], "no remote hosts, no key")
    }

    private func decode(_ link: String) throws -> [String: Any] {
        var b64 = String(link.dropFirst("thurm://pair?d=".count))
            .replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        b64 += String(repeating: "=", count: (4 - b64.count % 4) % 4)
        return try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(Data(base64Encoded: b64))) as? [String: Any])
    }

    func testRemoteHostsParsedFromPairingHosts() throws {
        let ok = try XCTUnwrap(PhonePairing.RemoteHost(json: [
            "name": "devbox", "host": "10.0.0.5", "port": 2222, "user": "me",
            "host_keys": ["SHA256:a", "SHA256:b"], "socket": NSNull(), "proxy": "bastion",
        ] as [String: Any]))
        XCTAssertTrue(ok.included, "on by default with host keys")
        XCTAssertEqual(ok.port, 2222)
        XCTAssertEqual(ok.proxy, "bastion")
        XCTAssertNil(ok.socket)
        let unknown = try XCTUnwrap(PhonePairing.RemoteHost(json: [
            "name": "pi", "host": "pi", "port": 22, "user": "me", "host_keys": [] as [String],
            "error": "no known host key for pi: connect to it from this Mac once",
        ] as [String: Any]))
        XCTAssertFalse(unknown.included)
        XCTAssertFalse(unknown.inCode)
        XCTAssertNil(PhonePairing.RemoteHost(json: ["error": "x"]))
    }

    func testPairingLinkWithRemotesRoundTrips() throws {
        var devbox = PhonePairing.RemoteHost(name: "devbox", address: "devbox.tail.ts.net", port: 2222, user: "me",
                                             hostKeys: ["SHA256:dev"], socket: "/run/user/1000/thurm/thurmd.sock")
        devbox.address = " devbox.tail.ts.net "
        var unchecked = PhonePairing.RemoteHost(name: "nas", address: "nas", user: "admin", hostKeys: ["SHA256:nas"])
        unchecked.included = false
        let noKeys = PhonePairing.RemoteHost(name: "pi", address: "pi", user: "me", hostKeys: [],
                                             error: "no known host key for pi")
        var noAddress = PhonePairing.RemoteHost(name: "vm", address: "vm", user: "me", hostKeys: ["SHA256:vm"])
        noAddress.address = "  "
        let link = PhonePairing.payload(name: "studio", host: "studio.tail.ts.net", port: 22, user: "me",
                                        hostKeys: ["SHA256:abc"], relay: nil, code: nil,
                                        remotes: [devbox, unchecked, noKeys, noAddress])
        let d = try decode(link)
        let remotes = try XCTUnwrap(d["remotes"] as? [[String: Any]])
        XCTAssertEqual(remotes.count, 1, "unchecked hosts, and hosts without keys or an address, are left out")
        let r = remotes[0]
        XCTAssertEqual(r["name"] as? String, "devbox")
        XCTAssertEqual(r["host"] as? String, "devbox.tail.ts.net")
        XCTAssertEqual(r["port"] as? Int, 2222)
        XCTAssertEqual(r["user"] as? String, "me")
        XCTAssertEqual(r["host_keys"] as? [String], ["SHA256:dev"])
        XCTAssertEqual(r["socket"] as? String, "/run/user/1000/thurm/thurmd.sock")
        XCTAssertEqual(Set(r.keys), ["name", "host", "port", "user", "host_keys", "socket"])
        XCTAssertEqual(d["host"] as? String, "studio.tail.ts.net", "the Mac stays the first host")
    }

    func testQRCodeStaysScannableWithManyHosts() {
        XCTAssertEqual(PhonePairing.correctionLevel(remotes: 0), "M")
        XCTAssertEqual(PhonePairing.correctionLevel(remotes: 6), "M")
        XCTAssertEqual(PhonePairing.correctionLevel(remotes: 7), "L")
        let hosts = (0..<10).map {
            PhonePairing.RemoteHost(name: "host\($0)", address: "host\($0).tail1234.ts.net", user: "me",
                                    hostKeys: ["SHA256:YU6LvFHdTg1dnOOH+NqrecmSEjtnOCbFmqP7ct+qH0o"])
        }
        let link = PhonePairing.payload(name: "studio", host: "studio.tail1234.ts.net", port: 22, user: "me",
                                        hostKeys: ["SHA256:abc"], relay: "https://push.example", code: "x",
                                        remotes: hosts)
        XCTAssertNotNil(PhonePairing.qrImage(link, remotes: hosts.count))
    }
}
