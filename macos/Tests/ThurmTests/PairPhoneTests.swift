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
    }
}
