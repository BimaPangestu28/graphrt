// The Swift examples of docs/mobile.md. The text between `// snippet:start NAME`
// and `// snippet:end` is the code the guide shows; scripts/check-doc-snippets.py
// fails when the two differ. GraphrtTests runs every function here.
import Foundation
import Graphrt

/// Stand-ins for what an app has: its UI and its phone APIs.
protocol Screen: AnyObject {
    func appendAnswer(_ text: String)
    func showToolActivity(_ name: String)
    func showBlock(_ block: UiBlock)
    func askToApprove(toolName: String, argumentsJson: String) async -> Bool
    func showError(_ message: String)
}

protocol Phone: Sendable {
    func batteryPercent() -> Int
    func sendSms(to: String, text: String) -> Bool
}

func createRuntime(configJson: String, phone: Phone) throws -> Runtime {
    // snippet:start swift-runtime
    // Off the main actor: creating a runtime may connect to MCP servers.
    let runtime = try Runtime(configJson: configJson)
    try runtime.registerTools(provider: "device") { tools in
        tools.tool("battery_level", description: "The battery level of the phone, in percent.") { _ in
            toolResult("\(phone.batteryPercent()) %")
        }
        tools.tool(
            "send_message",
            description: "Send a text message.",
            parametersJson: """
                {"type": "object",
                 "properties": {"to": {"type": "string"}, "text": {"type": "string"}},
                 "required": ["to", "text"]}
                """,
            timeoutMs: 10_000
        ) { argumentsJson in
            let args = try JSONDecoder().decode([String: String].self, from: Data(argumentsJson.utf8))
            guard let to = args["to"], phone.sendSms(to: to, text: args["text"] ?? "") else {
                throw ToolCallError.Failed(reason: "the SMS could not be sent")
            }
            return toolResult("sent to \(to)")
        }
    }
    // snippet:end
    return runtime
}

func runAgent(workflow: WorkflowHandle, screen: Screen) async throws -> RunStatus {
    // snippet:start swift-run
    var events = workflow.events(inputsJson: #"{"query": "Tell Ada my battery level"}"#)
    while true {
        var paused: PausedRun?
        for try await event in events {
            switch event {
            case .answerChunk(let text): screen.appendAnswer(text)
            case .toolCalled(_, _, let name, _): screen.showToolActivity(name)
            case .ui(_, let block): screen.showBlock(block)
            case .paused(let run): paused = run
            case .finished(let result): if let error = result.error { screen.showError(error) }
            default: break
            }
        }
        guard let run = paused else { return .succeeded }
        // The run waits for a person: ask, answer every request, resume.
        for request in run.requests() where request.kind == .toolApproval {
            guard let call = request.toolCalls.first else { continue }
            if await screen.askToApprove(toolName: call.name, argumentsJson: call.argumentsJson) {
                try run.approve(key: request.key, reason: nil)
            } else {
                try run.reject(key: request.key, reason: "The user said no.")
            }
        }
        events = workflow.resumeEvents(paused: run)
    }
    // snippet:end
}

func persistAndRestore(paused: PausedRun, storage: inout [String: String]) throws -> PausedRun {
    // snippet:start swift-persist
    // Keep the paused run in the app's private storage, for instance until a
    // push notification brings the person back.
    storage["paused:\(paused.runId())"] = paused.toJson()

    // Later, even after the app was restarted:
    let restored = try PausedRun.fromJson(json: storage["paused:\(paused.runId())"] ?? "")
    // snippet:end
    return restored
}

func installSkillAndReviewLessons(runtime: Runtime) throws -> [String] {
    // snippet:start swift-skills-learning
    // Needs `skills_dir` and `learning_dir` in the runtime config.
    _ = try runtime.installSkill(request: SkillInstall(skillMd: """
        ---
        name: trip-planner
        description: Use when someone plans a trip.
        ---
        Ask for the dates and the budget before suggesting anything.
        """))

    // A person corrected the agent: propose a lesson, the owner decides.
    let lesson = try runtime.proposeLesson(proposal: LessonProposal(
        agent: "assistant",
        user: "ada",
        kind: .preference,
        text: "Answer in Indonesian.",
        authorAuthenticated: true
    ))
    _ = try runtime.decideLesson(agent: "assistant", id: lesson.id, decision: .approve)
    let approved = try runtime.listLessons(agent: "assistant", status: .approved).map(\.text)
    // snippet:end
    return approved
}

func checkBlock(json: String) -> String? {
    // snippet:start swift-ui
    // Blocks inside a run arrive checked as RunEvent.ui. Check blocks from
    // anywhere else (a server, stored messages) before rendering them:
    let block = try? validateUiBlock(blockJson: json) // nil: never render it
    // snippet:end
    return block?.kind
}
