import XCTest
@testable import Thurm

final class AgentStatusDetailTests: XCTestCase {
    private func agent(_ status: AgentStatus, message: String? = nil, permission: UInt64? = nil) -> AgentState {
        AgentState(name: "Claude Code", kind: "claude", status: status, message: message, permission: permission)
    }

    /// Hooks can report a permission prompt while the status still says the agent works or
    /// idles: the prompt is what the row says.
    func testAPermissionPromptComesBeforeTheStatus() {
        for status in [AgentStatus.working, .idle, .done, .needsInput, .error] {
            XCTAssertEqual(agent(status, message: "Claude needs your permission to use Bash", permission: 4).statusDetail,
                           "Needs your permission to use Bash", "\(status)")
            XCTAssertEqual(agent(status, permission: 4).statusDetail, "Needs permission", "\(status)")
            XCTAssertEqual(agent(status, message: "  ", permission: 4).statusDetail, "Needs permission", "\(status)")
        }
        XCTAssertEqual(agent(.needsInput, message: "Claude is waiting for your input", permission: 4).statusDetail,
                       "Waiting for your input")
    }

    func testWithoutAPromptTheStatusSpeaks() {
        XCTAssertEqual(agent(.working).statusDetail, "Working")
        XCTAssertEqual(agent(.working, message: "Reading Models.swift").statusDetail, "Reading Models.swift")
        XCTAssertEqual(agent(.idle, message: "anything").statusDetail, "Idle")
        XCTAssertEqual(agent(.needsInput).statusDetail, "Needs input")
        XCTAssertEqual(agent(.error).statusDetail, "Failed")
    }
}
