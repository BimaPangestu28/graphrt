import Foundation
import UIKit

/// The phone assistant of examples/mobile/agent.yaml: a scripted model calls the
/// app's native tools (`device/battery_level`, then `device/send_message`, which
/// waits for approval) and streams its answer. Uses the helpers of the Swift
/// package (sdk/swift/Sources/Graphrt/Graphrt+Async.swift).
@MainActor
final class AssistantModel: ObservableObject {
    @Published private(set) var transcript = ""
    /// The call waiting for the person, shown as a confirmation dialog.
    @Published var pendingApproval: PendingToolCall?

    private var workflow: WorkflowHandle?
    private var decision: CheckedContinuation<Bool, Never>?
    /// Approve without asking (`-autorun YES`, for the smoke test).
    var autoApprove = false

    func load() async {
        do {
            workflow = try await Task.detached(priority: .userInitiated) { try Self.loadAssistant() }.value
            log("assistant loaded")
        } catch {
            log("assistant load failed: \(error)")
        }
    }

    func run(query: String) {
        guard let workflow else { return }
        Task {
            do {
                try await drive(workflow, query: query)
            } catch {
                log("assistant failed: \(error)")
            }
        }
    }

    /// The person's answer to `pendingApproval`.
    func decide(_ approved: Bool) {
        pendingApproval = nil
        decision?.resume(returning: approved)
        decision = nil
    }

    private func drive(_ workflow: WorkflowHandle, query: String) async throws {
        let inputs = String(decoding: try JSONSerialization.data(withJSONObject: ["query": query]), as: UTF8.self)
        var events = workflow.events(inputsJson: inputs)
        while true {
            var paused: PausedRun?
            for try await event in events {
                switch event {
                case .toolCalled(_, _, let name, _): log("agent tool: \(name)")
                case .toolFinished(_, _, let name, let isError): log("agent tool done: \(name) error=\(isError)")
                case .toolDenied(_, _, let name): log("agent tool denied: \(name)")
                case .answerChunk(let text): log("agent answer chunk: \(text)")
                case .ui(_, let block): log("agent ui: \(block.kind)")
                case .paused(let run): paused = run
                case .finished(let result): log("agent finished: \(result.status) \(result.answer)")
                default: break
                }
            }
            guard let run = paused else { return }
            for request in run.requests() where request.kind == .toolApproval {
                guard let call = request.toolCalls.first else { continue }
                log("agent paused: \(call.name) \(call.argumentsJson)")
                if await ask(call) {
                    try run.approve(key: request.key, reason: nil)
                } else {
                    try run.reject(key: request.key, reason: "The person said no.")
                }
            }
            events = workflow.resumeEvents(paused: run)
        }
    }

    private func ask(_ call: PendingToolCall) async -> Bool {
        if autoApprove { return true }
        return await withCheckedContinuation { continuation in
            decision = continuation
            pendingApproval = call
        }
    }

    private func log(_ line: String) {
        // The smoke test reads these lines from the console.
        print("[GraphrtSample] \(line)")
        transcript += (transcript.isEmpty ? "" : "\n") + line
    }

    private nonisolated static func loadAssistant() throws -> WorkflowHandle {
        let runtime = try Runtime(configJson: try bundled("runtime-config", "json"))
        try runtime.registerTools(provider: "device") { tools in
            tools.tool("battery_level", description: "The battery level of the phone, in percent.") { _ in
                let percent = await MainActor.run { () -> Int in
                    UIDevice.current.isBatteryMonitoringEnabled = true
                    let level = UIDevice.current.batteryLevel // -1 on a simulator
                    return level < 0 ? 100 : Int(level * 100)
                }
                // A UI block for the app to render; it arrives checked as RunEvent.ui.
                let tile = #"{"$ui":"stats","version":1,"items":[{"label":"Battery","value":\#(percent)}]}"#
                return toolResult("\(percent) %", dataJson: tile)
            }
            tools.tool(
                "send_message",
                description: "Send a text message.",
                parametersJson: #"{"type":"object","properties":{"to":{"type":"string"},"text":{"type":"string"}},"required":["to","text"]}"#,
                timeoutMs: 10_000
            ) { argumentsJson in
                let args = try JSONDecoder().decode([String: String].self, from: Data(argumentsJson.utf8))
                guard let to = args["to"], !to.isEmpty else { throw ToolCallError.Failed(reason: "no recipient") }
                // A real app sends the message here (MFMessageComposeViewController...).
                return toolResult("sent to \(to)")
            }
        }
        return try runtime.loadWorkflow(document: try bundled("agent", "yaml"))
    }

    private nonisolated static func bundled(_ name: String, _ ext: String) throws -> String {
        guard let url = Bundle.main.url(forResource: name, withExtension: ext) else {
            throw CocoaError(.fileNoSuchFile, userInfo: [NSFilePathErrorKey: "\(name).\(ext)"])
        }
        return try String(contentsOf: url, encoding: .utf8)
    }
}
