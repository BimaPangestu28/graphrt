package dev.graphrt.sample

import android.util.Log
import com.graphrt.events
import com.graphrt.registerTools
import com.graphrt.resumeEvents
import com.graphrt.toolResult
import org.json.JSONObject
import uniffi.graphrt_ffi.PausedRun
import uniffi.graphrt_ffi.RequestKind
import uniffi.graphrt_ffi.RunEvent
import uniffi.graphrt_ffi.Runtime
import uniffi.graphrt_ffi.ToolCallException
import uniffi.graphrt_ffi.WorkflowHandle

private const val LOG_TAG = "GraphrtSample"

/**
 * The phone assistant of examples/mobile/agent.yaml: a scripted model calls the
 * app's native tools (`device/battery_level`, then `device/send_message`, which
 * waits for approval) and streams its answer.
 */
class AgentDemo(
    private val batteryPercent: () -> Int,
    /** Asks the person; `true` approves the call. */
    private val approve: suspend (toolName: String, argumentsJson: String) -> Boolean,
    private val emitLine: (String) -> Unit,
) {
    /** Native tools first, then the workflow, which refers to them. */
    fun load(configJson: String, document: String): WorkflowHandle {
        val runtime = Runtime(configJson)
        runtime.registerTools("device") {
            tool("battery_level", "The battery level of the phone, in percent.") {
                val percent = batteryPercent()
                // A UI block for the app to render; it arrives checked as RunEvent.Ui.
                val tile = """{"${'$'}ui":"stats","version":1,"items":[{"label":"Battery","value":$percent}]}"""
                toolResult("$percent %", dataJson = tile)
            }
            tool(
                name = "send_message",
                description = "Send a text message.",
                parametersJson = """{"type":"object","properties":{"to":{"type":"string"},"text":{"type":"string"}},"required":["to","text"]}""",
                timeoutMs = 10_000u,
            ) { argumentsJson ->
                val to = JSONObject(argumentsJson).optString("to")
                if (to.isEmpty()) throw ToolCallException.Failed("no recipient")
                // A real app sends the message here (SmsManager, an intent...).
                toolResult("sent to $to")
            }
        }
        return runtime.loadWorkflow(document)
    }

    /** Runs the agent until it finishes, answering every approval request on the way. */
    suspend fun run(workflow: WorkflowHandle, query: String) {
        var events = workflow.events(JSONObject().put("query", query).toString())
        while (true) {
            var paused: PausedRun? = null
            events.collect { event ->
                when (event) {
                    is RunEvent.ToolCalled -> log("agent tool: ${event.name}")
                    is RunEvent.ToolFinished -> log("agent tool done: ${event.name} error=${event.isError}")
                    is RunEvent.ToolDenied -> log("agent tool denied: ${event.name}")
                    is RunEvent.AnswerChunk -> log("agent answer chunk: ${event.text}")
                    is RunEvent.Ui -> log("agent ui: ${event.block.kind}")
                    is RunEvent.Paused -> paused = event.run
                    is RunEvent.Finished -> log("agent finished: ${event.result.status} ${event.result.answer}")
                    else -> {}
                }
            }
            val run = paused ?: return
            for (request in run.requests()) {
                if (request.kind != RequestKind.ToolApproval) continue
                val call = request.toolCalls.first()
                log("agent paused: ${call.name} ${call.argumentsJson}")
                if (approve(call.name, call.argumentsJson)) {
                    run.approve(request.key, null)
                } else {
                    run.reject(request.key, "The person said no.")
                }
            }
            events = workflow.resumeEvents(run)
        }
    }

    private fun log(line: String) {
        Log.i(LOG_TAG, line)
        emitLine(line)
    }
}
