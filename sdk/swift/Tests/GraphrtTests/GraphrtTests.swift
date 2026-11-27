import Foundation
import Graphrt
import XCTest

/// examples/mobile/, found from this file (the tests run from the repository).
private let mobileDir = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    .deletingLastPathComponent().deletingLastPathComponent()
    .appendingPathComponent("examples/mobile")

private func read(_ name: String) throws -> String {
    try String(contentsOf: mobileDir.appendingPathComponent(name), encoding: .utf8)
}

private final class FakePhone: Phone, @unchecked Sendable {
    private let lock = NSLock()
    private var sentTo: [String] = []
    var sent: [String] { lock.withLock { sentTo } }
    func batteryPercent() -> Int { 81 }
    func sendSms(to: String, text: String) -> Bool {
        lock.withLock { sentTo.append(to) }
        return true
    }
}

private final class FakeScreen: Screen {
    let approve: Bool
    var answer = ""
    var tools: [String] = []
    var blocks: [UiBlock] = []
    var asked: [String] = []
    var errors: [String] = []
    init(approve: Bool) { self.approve = approve }
    func appendAnswer(_ text: String) { answer += text }
    func showToolActivity(_ name: String) { tools.append(name) }
    func showBlock(_ block: UiBlock) { blocks.append(block) }
    func askToApprove(toolName: String, argumentsJson: String) async -> Bool {
        asked.append(toolName)
        return approve
    }
    func showError(_ message: String) { errors.append(message) }
}

final class GraphrtTests: XCTestCase {
    private func workflow(_ phone: Phone) throws -> WorkflowHandle {
        try createRuntime(configJson: read("runtime-config.json"), phone: phone).loadWorkflow(document: read("agent.yaml"))
    }

    func testAnApprovedCallRunsAndTheAnswerStreams() async throws {
        let phone = FakePhone()
        let screen = FakeScreen(approve: true)
        let status = try await runAgent(workflow: workflow(phone), screen: screen)
        XCTAssertEqual(status, .succeeded)
        XCTAssertEqual(screen.answer, "Done: sent to Ada")
        XCTAssertEqual(screen.asked, ["send_message"])
        XCTAssertEqual(screen.tools, ["battery_level", "send_message"])
        XCTAssertEqual(phone.sent, ["Ada"])
        XCTAssertTrue(screen.errors.isEmpty)
    }

    func testARejectedCallNeverReachesThePhone() async throws {
        let phone = FakePhone()
        let screen = FakeScreen(approve: false)
        _ = try await runAgent(workflow: workflow(phone), screen: screen)
        XCTAssertEqual(screen.answer, "Done: The user did not approve this tool call: The user said no.")
        XCTAssertTrue(phone.sent.isEmpty)
    }

    func testAPausedRunSurvivesStorage() async throws {
        let workflow = try workflow(FakePhone())
        var last: RunEvent?
        for try await event in workflow.events(inputsJson: #"{"query": "q"}"#) { last = event }
        guard case .paused(let paused) = last else { return XCTFail("the run should pause") }
        var storage: [String: String] = [:]
        let restored = try persistAndRestore(paused: paused, storage: &storage)
        let request = try XCTUnwrap(restored.requests().first)
        XCTAssertEqual(request.kind, .toolApproval)
        XCTAssertEqual(request.toolCalls.first?.name, "send_message")
        try restored.approve(key: request.key, reason: nil)
        var answer = ""
        for try await event in workflow.resumeEvents(paused: restored) {
            if case .finished(let result) = event { answer = result.answer }
        }
        XCTAssertEqual(answer, "Done: sent to Ada")
    }

    func testAToolPastItsTimeLimitIsCancelled() async throws {
        let cancelled = expectation(description: "the tool's task is cancelled")
        let runtime = try Runtime(configJson: #"""
            {"scripted_models": {"one": [{"tool_calls": [{"name": "wait"}]}, {"text": "{tool_result}"}]}}
            """#)
        try runtime.registerTools(provider: "app") { tools in
            tools.tool("wait", description: "Waits forever.", timeoutMs: 100) { _ in
                do {
                    try await Task.sleep(nanoseconds: 60_000_000_000)
                } catch {
                    cancelled.fulfill()
                    throw error
                }
                return toolResult("too late")
            }
        }
        let workflow = try runtime.loadWorkflow(document: """
            version: "1"
            graph:
              nodes:
                - { id: start, type: start }
                - { id: a, type: agent, model: { provider: one, name: s }, query: go, tools: [app/*] }
                - { id: reply, type: answer, answer: "{{#a.text#}}" }
              edges:
                - { source: start, target: a }
                - { source: a, target: reply }
            """)
        var answer = ""
        for try await event in workflow.events(inputsJson: "{}") {
            if case .finished(let result) = event { answer = result.answer }
        }
        XCTAssertEqual(answer, "the tool did not answer within 100 ms and was cancelled")
        await fulfillment(of: [cancelled], timeout: 5)
    }

    func testSkillsAndLessons() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("graphrt-swift-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let config = #"{"skills_dir": "\#(dir.path)/skills", "learning_dir": "\#(dir.path)/learning"}"#
        let runtime = try Runtime(configJson: config)
        XCTAssertEqual(try installSkillAndReviewLessons(runtime: runtime), ["Answer in Indonesian."])
        XCTAssertEqual(try runtime.listSkills().map(\.name), ["trip-planner"])
    }

    func testUiBlocksAreChecked() {
        XCTAssertEqual(checkBlock(json: #"{"$ui": "stats", "version": 1, "items": [{"label": "Battery", "value": 81}]}"#), "stats")
        XCTAssertNil(checkBlock(json: #"{"$ui": "stats", "version": 9}"#))
    }

    func testErrorsAreTyped() throws {
        XCTAssertThrowsError(try Runtime(configJson: #"{"nope": 1}"#)) { error in
            guard case GraphrtError.InvalidConfig = error else { return XCTFail("\(error)") }
        }
        let runtime = try Runtime(configJson: "{}")
        XCTAssertThrowsError(try runtime.listSkills()) { error in
            guard case GraphrtError.Unsupported = error else { return XCTFail("\(error)") }
        }
        XCTAssertFalse(graphrtVersion().isEmpty)
    }
}
