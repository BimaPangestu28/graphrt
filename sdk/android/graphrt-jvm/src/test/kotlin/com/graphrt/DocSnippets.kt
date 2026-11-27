// The Kotlin examples of docs/mobile.md. The text between `// snippet:start NAME`
// and `// snippet:end` is the code the guide shows; scripts/check-doc-snippets.py
// fails when the two differ. DocSnippetsTest runs every function here.
package com.graphrt

import uniffi.graphrt_ffi.HostToolResult
import uniffi.graphrt_ffi.LessonDecision
import uniffi.graphrt_ffi.LessonKind
import uniffi.graphrt_ffi.LessonProposal
import uniffi.graphrt_ffi.LessonStatus
import uniffi.graphrt_ffi.PausedRun
import uniffi.graphrt_ffi.RequestKind
import uniffi.graphrt_ffi.RunEvent
import uniffi.graphrt_ffi.RunStatus
import uniffi.graphrt_ffi.Runtime
import uniffi.graphrt_ffi.SkillInstall
import uniffi.graphrt_ffi.ToolCallException
import uniffi.graphrt_ffi.UiBlock
import uniffi.graphrt_ffi.WorkflowHandle
import uniffi.graphrt_ffi.validateUiBlock

/** Stand-ins for what an app has: its UI and its phone APIs. */
interface Screen {
    fun appendAnswer(text: String)
    fun showToolActivity(name: String)
    fun showBlock(block: UiBlock)
    suspend fun askToApprove(toolName: String, argumentsJson: String): Boolean
    fun showError(message: String)
}

interface Phone {
    fun batteryPercent(): Int
    fun sendSms(to: String, text: String): Boolean
}

fun createRuntime(configJson: String, phone: Phone): Runtime {
    // snippet:start kotlin-runtime
    // Off the main thread: creating a runtime may connect to MCP servers.
    val runtime = Runtime(configJson)
    runtime.registerTools("device") {
        tool("battery_level", "The battery level of the phone, in percent.") {
            toolResult("${phone.batteryPercent()} %")
        }
        tool(
            name = "send_message",
            description = "Send a text message.",
            parametersJson = """
                {"type": "object",
                 "properties": {"to": {"type": "string"}, "text": {"type": "string"}},
                 "required": ["to", "text"]}
            """,
            timeoutMs = 10_000u,
        ) { argumentsJson ->
            val args = Json.parseObject(argumentsJson)
            if (!phone.sendSms(args.getValue("to"), args.getValue("text"))) {
                throw ToolCallException.Failed("the SMS could not be sent")
            }
            toolResult("sent to ${args.getValue("to")}")
        }
    }
    // snippet:end
    return runtime
}

suspend fun runAgent(workflow: WorkflowHandle, screen: Screen): RunStatus {
    // snippet:start kotlin-run
    var events = workflow.events("""{"query": "Tell Ada my battery level"}""")
    while (true) {
        var paused: PausedRun? = null
        events.collect { event ->
            when (event) {
                is RunEvent.AnswerChunk -> screen.appendAnswer(event.text)
                is RunEvent.ToolCalled -> screen.showToolActivity(event.name)
                is RunEvent.Ui -> screen.showBlock(event.block)
                is RunEvent.Paused -> paused = event.run
                is RunEvent.Finished -> event.result.error?.let(screen::showError)
                else -> {}
            }
        }
        val run = paused ?: return RunStatus.SUCCEEDED
        // The run waits for a person: ask, answer every request, resume.
        for (request in run.requests()) {
            if (request.kind != RequestKind.ToolApproval) continue
            val call = request.toolCalls.first()
            if (screen.askToApprove(call.name, call.argumentsJson)) {
                run.approve(request.key, null)
            } else {
                run.reject(request.key, "The user said no.")
            }
        }
        events = workflow.resumeEvents(run)
    }
    // snippet:end
}

fun persistAndRestore(paused: PausedRun, storage: MutableMap<String, String>): PausedRun {
    // snippet:start kotlin-persist
    // Keep the paused run in the app's private storage, for instance until a
    // push notification brings the person back.
    storage["paused:${paused.runId()}"] = paused.toJson()

    // Later, even after the app was restarted:
    val restored = PausedRun.fromJson(storage.getValue("paused:${paused.runId()}"))
    // snippet:end
    return restored
}

fun installSkillAndReviewLessons(runtime: Runtime): List<String> {
    // snippet:start kotlin-skills-learning
    // Needs `skills_dir` and `learning_dir` in the runtime config.
    runtime.installSkill(
        SkillInstall(
            skillMd = """
                ---
                name: trip-planner
                description: Use when someone plans a trip.
                ---
                Ask for the dates and the budget before suggesting anything.
            """.trimIndent(),
        ),
    )

    // A person corrected the agent: propose a lesson, the owner decides.
    val lesson = runtime.proposeLesson(
        LessonProposal(
            agent = "assistant",
            user = "ada",
            kind = LessonKind.PREFERENCE,
            text = "Answer in Indonesian.",
            authorAuthenticated = true,
        ),
    )
    runtime.decideLesson("assistant", lesson.id, LessonDecision.APPROVE)
    val approved = runtime.listLessons("assistant", LessonStatus.APPROVED).map { it.text }
    // snippet:end
    return approved
}

fun checkBlock(json: String): String? {
    // snippet:start kotlin-ui
    // Blocks inside a run arrive checked as RunEvent.Ui. Check blocks from
    // anywhere else (a server, stored messages) before rendering them:
    val block = try {
        validateUiBlock(json)
    } catch (e: Exception) {
        null // never render a block that failed validation
    }
    // snippet:end
    return block?.kind
}

/** A tiny JSON object reader for the snippets (an app uses kotlinx.serialization or org.json). */
object Json {
    fun parseObject(json: String): Map<String, String> =
        Regex(""""([^"]+)"\s*:\s*"([^"]*)"""").findAll(json).associate { it.groupValues[1] to it.groupValues[2] }
}

/** Keeps the HostToolResult import used, as the guide's tool section shows it. */
fun failedResult(reason: String) = HostToolResult(text = reason, isError = true)
