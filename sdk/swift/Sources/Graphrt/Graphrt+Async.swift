// Swift helpers over the generated binding: runs as AsyncThrowingStreams, and
// native tools declared with a small builder.
import Foundation

extension RunEvent {
    /// Whether this event ends the run (`.paused` or `.finished`).
    public var isTerminal: Bool {
        switch self {
        case .paused, .finished: return true
        default: return false
        }
    }
}

extension WorkflowHandle {
    /// Start a run and receive its typed events. The stream ends after `.paused`
    /// or `.finished`; ending the iteration early (a cancelled task) aborts the run.
    public func events(inputsJson: String, sysJson: String = "{}") -> AsyncThrowingStream<RunEvent, Error> {
        runStream { listener in try self.start(inputsJson: inputsJson, sysJson: sysJson, listener: listener) }
    }

    /// Resume a paused run with the answers given on `paused`, as a stream like `events`.
    public func resumeEvents(paused: PausedRun, secretsJson: String = "") -> AsyncThrowingStream<RunEvent, Error> {
        runStream { listener in try self.resumePaused(paused: paused, secretsJson: secretsJson, listener: listener) }
    }
}

private func runStream(_ launch: (RunListener) throws -> RunHandle) -> AsyncThrowingStream<RunEvent, Error> {
    // Unbounded: the runtime's threads never wait for a slow consumer.
    let (stream, continuation) = AsyncThrowingStream<RunEvent, Error>.makeStream(bufferingPolicy: .unbounded)
    do {
        let run = try launch(StreamListener(continuation))
        continuation.onTermination = { _ in
            if !run.isFinished() { run.abort(reason: "cancelled by the app") }
        }
    } catch {
        continuation.finish(throwing: error)
    }
    return stream
}

private final class StreamListener: RunListener, Sendable {
    private let continuation: AsyncThrowingStream<RunEvent, Error>.Continuation

    init(_ continuation: AsyncThrowingStream<RunEvent, Error>.Continuation) {
        self.continuation = continuation
    }

    func onEvent(event: RunEvent) {
        continuation.yield(event)
        if event.isTerminal { continuation.finish() }
    }
}

/// Handles one call of a native tool: the arguments as a JSON object, already
/// checked against the tool's schema.
public typealias ToolHandler = @Sendable (_ argumentsJson: String) async throws -> HostToolResult

/// Declares the native tools of one provider; see `Runtime.registerTools`.
public final class ToolsBuilder {
    fileprivate var specs: [HostToolSpec] = []
    fileprivate var handlers: [String: ToolHandler] = [:]

    fileprivate init() {}

    /// A tool the model can call. `parametersJson` is the JSON Schema of its
    /// arguments object; `timeoutMs` limits one call (30 s by default), after
    /// which the call's task is cancelled. Throw `ToolCallError.Failed` to tell
    /// the model why the tool failed.
    public func tool(
        _ name: String,
        description: String,
        parametersJson: String = #"{"type":"object","properties":{}}"#,
        timeoutMs: UInt64? = nil,
        handler: @escaping ToolHandler
    ) {
        precondition(handlers[name] == nil, "tool `\(name)` is declared twice")
        specs.append(HostToolSpec(name: name, description: description, parametersJson: parametersJson, timeoutMs: timeoutMs))
        handlers[name] = handler
    }
}

private final class BuiltTools: ToolProvider, Sendable {
    private let handlers: [String: ToolHandler]

    init(_ handlers: [String: ToolHandler]) {
        self.handlers = handlers
    }

    func callTool(call: HostToolCall) async throws -> HostToolResult {
        guard let handler = handlers[call.name] else {
            throw ToolCallError.Failed(reason: "no tool `\(call.name)`")
        }
        return try await handler(call.argumentsJson)
    }
}

extension Runtime {
    /// Register native tools as provider `provider`; agents use them as
    /// `provider/tool`. Register before loading the workflows that use them.
    public func registerTools(provider: String, _ build: (ToolsBuilder) -> Void) throws {
        let builder = ToolsBuilder()
        build(builder)
        try registerToolProvider(name: provider, tools: builder.specs, provider: BuiltTools(builder.handlers))
    }
}

/// A successful tool result: `text` for the model, optional structured `dataJson`.
public func toolResult(_ text: String, dataJson: String? = nil) -> HostToolResult {
    HostToolResult(text: text, dataJson: dataJson, isError: false)
}
