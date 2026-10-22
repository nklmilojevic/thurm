import XCTest
@testable import Thurm

final class CompletionCandidateTests: XCTestCase {
    /// Accepting these leaves the cursor in the word: a directory, or an option's value.
    func testContinuesWord() {
        let cases: [(String, String, Bool)] = [
            ("src/", "Directory", true),
            ("src/", "Value", true),
            ("--output=", "Flag", true),
            ("--output", "Flag", false),
            ("plan", "Value", false),
            ("main.rs", "File", false),
        ]
        for (text, kind, expected) in cases {
            let item = CompletionCandidate(text: text, kind: kind, description: nil)
            XCTAssertEqual(item.continuesWord, expected, "\(text) (\(kind))")
        }
    }
}
