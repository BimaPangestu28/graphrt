import Foundation

/// How long "Run" waits for the final result.
private let runTimeoutMs: UInt64 = 10_000

/// Runs the offline `hello` workflow (start -> template -> end) through the
/// graphrt UniFFI binding and publishes what happened for the UI.
///
/// graphrt calls that may block (`Runtime(configJson:)`, `waitJson`) run in
/// detached tasks, never on the main actor. Listener callbacks arrive on
/// graphrt's worker threads and hop back to the main actor before touching
/// published state.
@MainActor
final class HelloWorkflowModel: ObservableObject {
    @Published private(set) var status = "loading..."
    @Published private(set) var resultText = ""
    @Published private(set) var eventsText = ""

    private var workflow: WorkflowHandle?

    /// Creates the runtime and compiles `hello.yaml` from the app bundle.
    func load() async {
        do {
            let loaded = try await Task.detached(priority: .userInitiated) {
                try Self.loadBundledWorkflow()
            }.value
            workflow = loaded
            status = "graphrt \(graphrtVersion()) loaded workflow '\(loaded.name() ?? "?")'"
        } catch {
            report(action: "loading hello.yaml", error: error)
        }
    }

    /// Starts a run and waits for its final result.
    func run(name: String, excited: Bool) {
        guard let workflow else { return }
        resultText = "running..."
        Task.detached(priority: .userInitiated) {
            do {
                let inputsJson = try Self.inputsJson(name: name, excited: excited)
                let runHandle = try workflow.run(inputsJson: inputsJson, sysJson: "{}", listener: SilentListener())
                let resultJson = runHandle.waitJson(timeoutMs: runTimeoutMs) ?? #"{"status":"timeout"}"#
                print("[GraphrtSample] result: \(resultJson)")
                let description = Self.describe(resultJson: resultJson)
                await MainActor.run { self.resultText = description }
            } catch {
                await MainActor.run { self.report(action: "run", error: error) }
            }
        }
    }

    /// Starts a run and prints events as the listener receives them.
    func stream(name: String, excited: Bool) {
        guard let workflow else { return }
        eventsText = ""
        let listener = StreamingListener { line in
            print("[GraphrtSample] \(line)")
            Task { @MainActor in self.eventsText += (self.eventsText.isEmpty ? "" : "\n") + line }
        }
        do {
            let inputsJson = try Self.inputsJson(name: name, excited: excited)
            // The handle is not needed: the listener reports completion.
            _ = try workflow.run(inputsJson: inputsJson, sysJson: "{}", listener: listener)
        } catch {
            report(action: "stream", error: error)
        }
    }

    private func report(action: String, error: Error) {
        print("[GraphrtSample] \(action) failed: \(error)")
        status = "\(action) failed: \(error)"
    }

    private nonisolated static func loadBundledWorkflow() throws -> WorkflowHandle {
        guard let url = Bundle.main.url(forResource: "hello", withExtension: "yaml") else {
            throw CocoaError(.fileNoSuchFile, userInfo: [NSFilePathErrorKey: "hello.yaml"])
        }
        let document = try String(contentsOf: url, encoding: .utf8)
        let runtime = try Runtime(configJson: "{}")
        // The workflow handle keeps the runtime alive.
        return try runtime.loadWorkflow(document: document)
    }

    private nonisolated static func inputsJson(name: String, excited: Bool) throws -> String {
        let inputs: [String: Any] = ["name": name, "excited": excited]
        let data = try JSONSerialization.data(withJSONObject: inputs)
        return String(decoding: data, as: UTF8.self)
    }

    private nonisolated static func describe(resultJson: String) -> String {
        let result = (try? JSONSerialization.jsonObject(with: Data(resultJson.utf8))) as? [String: Any]
        let outputs = result?["outputs"] as? [String: Any]
        let status = result?["status"] as? String ?? "?"
        let greeting = outputs?["greeting"] as? String ?? "-"
        return "status: \(status)\ngreeting: \(greeting)\n\n\(resultJson)"
    }
}

/// Listener for runs whose result is read with `waitJson`.
private final class SilentListener: EventListener {
    func onEvent(eventJson: String) {}
    func onAnswerChunk(text: String) {}
    func onFinish(resultJson: String) {}
}

/// Forwards every event, answer chunk and the final result to `emitLine`.
private final class StreamingListener: EventListener {
    private let emitLine: @Sendable (String) -> Void

    init(emitLine: @escaping @Sendable (String) -> Void) {
        self.emitLine = emitLine
    }

    func onEvent(eventJson: String) {
        let event = (try? JSONSerialization.jsonObject(with: Data(eventJson.utf8))) as? [String: Any]
        let type = event?["type"] as? String ?? "?"
        let nodeSuffix = (event?["node_id"] as? String).map { " [\($0)]" } ?? ""
        emitLine("event: \(type)\(nodeSuffix)")
    }

    func onAnswerChunk(text: String) {
        emitLine("answer chunk: \(text)")
    }

    func onFinish(resultJson: String) {
        emitLine("finish: \(resultJson)")
    }
}
