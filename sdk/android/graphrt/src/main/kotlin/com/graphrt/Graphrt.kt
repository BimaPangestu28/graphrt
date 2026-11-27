// Kotlin helpers over the generated binding (package uniffi.graphrt_ffi): runs as
// Flows, and native tools declared with a small builder.
package com.graphrt

import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.ProducerScope
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import uniffi.graphrt_ffi.HostToolCall
import uniffi.graphrt_ffi.HostToolResult
import uniffi.graphrt_ffi.HostToolSpec
import uniffi.graphrt_ffi.PausedRun
import uniffi.graphrt_ffi.RunEvent
import uniffi.graphrt_ffi.RunHandle
import uniffi.graphrt_ffi.RunListener
import uniffi.graphrt_ffi.Runtime
import uniffi.graphrt_ffi.ToolCallException
import uniffi.graphrt_ffi.ToolProvider
import uniffi.graphrt_ffi.WorkflowHandle

/** Whether this event ends the run ([RunEvent.Paused] or [RunEvent.Finished]). */
val RunEvent.isTerminal: Boolean
    get() = this is RunEvent.Paused || this is RunEvent.Finished

/**
 * Start a run and receive its typed events as a cold [Flow]. Collecting starts the
 * run; the flow completes after [RunEvent.Paused] or [RunEvent.Finished]. Cancelling
 * the collector (leaving the screen) aborts the run.
 */
fun WorkflowHandle.events(inputsJson: String, sysJson: String = "{}"): Flow<RunEvent> =
    runFlow { listener -> start(inputsJson, sysJson, listener) }

/** Resume a paused run with the answers given on [paused], as a [Flow] like [events]. */
fun WorkflowHandle.resumeEvents(paused: PausedRun, secretsJson: String = ""): Flow<RunEvent> =
    runFlow { listener -> resumePaused(paused, secretsJson, listener) }

private fun runFlow(launch: (RunListener) -> RunHandle): Flow<RunEvent> =
    callbackFlow {
        val run = launch(ChannelListener(this))
        awaitClose { if (!run.isFinished()) run.abort("cancelled by the app") }
    }.buffer(Channel.UNLIMITED) // never block the runtime's threads on a slow collector

private class ChannelListener(private val scope: ProducerScope<RunEvent>) : RunListener {
    override fun onEvent(event: RunEvent) {
        scope.trySend(event)
        if (event.isTerminal) scope.channel.close()
    }
}

/** Handles one call of a native tool: the arguments as a JSON object, already checked against the schema. */
typealias ToolHandler = suspend (argumentsJson: String) -> HostToolResult

/** Declares the native tools of one provider; see [Runtime.registerTools]. */
class ToolsBuilder internal constructor() {
    internal val specs = mutableListOf<HostToolSpec>()
    internal val handlers = mutableMapOf<String, ToolHandler>()

    /**
     * A tool the model can call. [parametersJson] is the JSON Schema of its arguments
     * object; [timeoutMs] limits one call (30 s by default), after which the call's
     * coroutine is cancelled. Throw [ToolCallException.Failed] to tell the model why
     * the tool failed.
     */
    fun tool(
        name: String,
        description: String,
        parametersJson: String = """{"type":"object","properties":{}}""",
        timeoutMs: ULong? = null,
        handler: ToolHandler,
    ) {
        require(name !in handlers) { "tool `$name` is declared twice" }
        specs += HostToolSpec(name = name, description = description, parametersJson = parametersJson, timeoutMs = timeoutMs)
        handlers[name] = handler
    }
}

private class BuiltTools(private val handlers: Map<String, ToolHandler>) : ToolProvider {
    override suspend fun callTool(call: HostToolCall): HostToolResult {
        val handler = handlers[call.name] ?: throw ToolCallException.Failed("no tool `${call.name}`")
        return handler(call.argumentsJson)
    }
}

/**
 * Register native tools as provider [provider]; agents use them as `provider/tool`.
 * Register before loading the workflows that use them.
 */
fun Runtime.registerTools(provider: String, block: ToolsBuilder.() -> Unit) {
    val builder = ToolsBuilder().apply(block)
    registerToolProvider(provider, builder.specs.toList(), BuiltTools(builder.handlers.toMap()))
}

/** A successful tool result: [text] for the model, optional structured [dataJson]. */
fun toolResult(text: String, dataJson: String? = null): HostToolResult =
    HostToolResult(text = text, dataJson = dataJson, isError = false)
