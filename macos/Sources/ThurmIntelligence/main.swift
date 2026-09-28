// thurm-intelligence: runs prompts for thurmd on Apple Intelligence's on-device model.
//
// JSON lines over stdio. On start it writes `{"ready":true}` or `{"ready":false,"error":…}`,
// then answers each request in order:
//
//   → {"id":1,"instructions":"…","prompt":"…","choices":["a","b"],"max_tokens":60}
//   ← {"id":1,"text":"…"}  or  {"id":1,"error":"…"}
//
// `choices` (optional) constrains the answer to one of the strings (guided generation).
// Every request gets a fresh session: the daemon sends all the context a prompt needs.

import Foundation
#if canImport(FoundationModels)
import FoundationModels
#endif

struct Ask: Decodable {
    let id: UInt64
    let instructions: String
    let prompt: String
    let choices: [String]?
    let max_tokens: Int?
}

func emit(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object),
          var line = String(data: data, encoding: .utf8) else { return }
    line += "\n"
    FileHandle.standardOutput.write(line.data(using: .utf8)!)
}

#if canImport(FoundationModels)
@available(macOS 26.0, *)
func unavailableReason() -> String? {
    switch SystemLanguageModel.default.availability {
    case .available:
        return nil
    case .unavailable(.deviceNotEligible):
        return "this Mac does not support Apple Intelligence"
    case .unavailable(.appleIntelligenceNotEnabled):
        return "Apple Intelligence is turned off in System Settings"
    case .unavailable(.modelNotReady):
        return "the on-device model is not ready yet (still downloading)"
    case .unavailable(let other):
        return "the on-device model is unavailable (\(other))"
    }
}

@available(macOS 26.0, *)
func answer(_ ask: Ask) async throws -> String {
    let session = LanguageModelSession(instructions: ask.instructions)
    let options = GenerationOptions(temperature: 0.2, maximumResponseTokens: ask.max_tokens ?? 120)
    if let choices = ask.choices, !choices.isEmpty {
        let root = DynamicGenerationSchema(name: "Answer", anyOf: choices)
        let schema = try GenerationSchema(root: root, dependencies: [])
        let response = try await session.respond(to: ask.prompt, schema: schema, options: options)
        return try response.content.value(String.self)
    }
    return try await session.respond(to: ask.prompt, options: options).content
}

@available(macOS 26.0, *)
func serve() async {
    if let reason = unavailableReason() {
        emit(["ready": false, "error": reason])
        return
    }
    emit(["ready": true])
    while let line = readLine(strippingNewline: true) {
        guard let data = line.data(using: .utf8),
              let ask = try? JSONDecoder().decode(Ask.self, from: data) else {
            continue
        }
        do {
            emit(["id": ask.id, "text": try await answer(ask)])
        } catch {
            emit(["id": ask.id, "error": String(describing: error)])
        }
    }
}
#endif

setvbuf(stdout, nil, _IOLBF, 0)
#if canImport(FoundationModels)
if #available(macOS 26.0, *) {
    await serve()
} else {
    emit(["ready": false, "error": "Apple Intelligence needs macOS 26 or later"])
}
#else
emit(["ready": false, "error": "built without the FoundationModels framework"])
#endif
