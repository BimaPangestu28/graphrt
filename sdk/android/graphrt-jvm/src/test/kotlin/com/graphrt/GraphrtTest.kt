package com.graphrt

import java.io.File
import java.nio.file.Files
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import uniffi.graphrt_ffi.GraphrtException
import uniffi.graphrt_ffi.RequestKind
import uniffi.graphrt_ffi.RunEvent
import uniffi.graphrt_ffi.RunStatus
import uniffi.graphrt_ffi.Runtime
import uniffi.graphrt_ffi.UiBlock
import uniffi.graphrt_ffi.graphrtVersion
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

private val repo = File(System.getProperty("graphrt.repo"))
private val agentYaml = repo.resolve("examples/mobile/agent.yaml").readText()
private val config = repo.resolve("examples/mobile/runtime-config.json").readText()

private class FakePhone : Phone {
    val sent = mutableListOf<String>()
    override fun batteryPercent() = 81
    override fun sendSms(to: String, text: String): Boolean {
        sent += to
        return true
    }
}

private class FakeScreen(private val approve: Boolean) : Screen {
    val answer = StringBuilder()
    val tools = mutableListOf<String>()
    val blocks = mutableListOf<UiBlock>()
    val asked = mutableListOf<String>()
    val errors = mutableListOf<String>()
    override fun appendAnswer(text: String) {
        answer.append(text)
    }
    override fun showToolActivity(name: String) {
        tools += name
    }
    override fun showBlock(block: UiBlock) {
        blocks += block
    }
    override suspend fun askToApprove(toolName: String, argumentsJson: String): Boolean {
        asked += toolName
        return approve
    }
    override fun showError(message: String) {
        errors += message
    }
}

class GraphrtTest {
    @Test
    fun versionMatchesTheCrates() {
        assertTrue(graphrtVersion().matches(Regex("""\d+\.\d+\.\d+.*""")))
    }

    @Test
    fun anApprovedCallRunsAndTheAnswerStreams() = runBlocking {
        val phone = FakePhone()
        val workflow = createRuntime(config, phone).loadWorkflow(agentYaml)
        val screen = FakeScreen(approve = true)
        val status = withTimeout(20_000) { runAgent(workflow, screen) }
        assertEquals(RunStatus.SUCCEEDED, status)
        assertEquals("Done: sent to Ada", screen.answer.toString())
        assertEquals(listOf("send_message"), screen.asked)
        assertEquals(listOf("battery_level", "send_message"), screen.tools)
        assertEquals(listOf("Ada"), phone.sent)
        assertTrue(screen.errors.isEmpty())
    }

    @Test
    fun aRejectedCallNeverReachesThePhone() = runBlocking {
        val phone = FakePhone()
        val workflow = createRuntime(config, phone).loadWorkflow(agentYaml)
        val screen = FakeScreen(approve = false)
        withTimeout(20_000) { runAgent(workflow, screen) }
        assertEquals("Done: The user did not approve this tool call: The user said no.", screen.answer.toString())
        assertTrue(phone.sent.isEmpty())
    }

    @Test
    fun aPausedRunSurvivesStorage() = runBlocking {
        val workflow = createRuntime(config, FakePhone()).loadWorkflow(agentYaml)
        val events = workflow.events("""{"query": "q"}""").toList()
        val paused = (events.last() as RunEvent.Paused).run
        val restored = persistAndRestore(paused, mutableMapOf())
        val request = restored.requests().single()
        assertEquals(RequestKind.ToolApproval, request.kind)
        assertEquals("send_message", request.toolCalls.single().name)
        restored.approve(request.key, null)
        val finished = workflow.resumeEvents(restored).toList().last() as RunEvent.Finished
        assertEquals("Done: sent to Ada", finished.result.answer)
    }

    @Test
    fun aToolPastItsTimeLimitIsCancelled() = runBlocking {
        val cancelled = CompletableDeferred<Unit>()
        val runtime = Runtime(
            """{"scripted_models": {"one": [
                {"tool_calls": [{"name": "wait"}]}, {"text": "{tool_result}"}]}}""",
        )
        runtime.registerTools("app") {
            tool("wait", "Waits forever.", timeoutMs = 100u) {
                try {
                    awaitCancellation()
                } finally {
                    cancelled.complete(Unit)
                }
            }
        }
        val workflow = runtime.loadWorkflow(
            """
            version: "1"
            graph:
              nodes:
                - { id: start, type: start }
                - { id: a, type: agent, model: { provider: one, name: s }, query: go, tools: [app/*] }
                - { id: reply, type: answer, answer: "{{#a.text#}}" }
              edges:
                - { source: start, target: a }
                - { source: a, target: reply }
            """.trimIndent(),
        )
        val finished = withTimeout(20_000) { workflow.events("{}").toList().last() } as RunEvent.Finished
        assertEquals("the tool did not answer within 100 ms and was cancelled", finished.result.answer)
        withTimeout(5_000) { cancelled.await() }
    }

    @Test
    fun skillsAndLessons() {
        val dir = Files.createTempDirectory("graphrt-kotlin").toFile()
        try {
            val runtime = Runtime(
                """{"skills_dir": "${dir.resolve("skills")}", "learning_dir": "${dir.resolve("learning")}"}""",
            )
            assertEquals(listOf("Answer in Indonesian."), installSkillAndReviewLessons(runtime))
            assertEquals("trip-planner", runtime.listSkills().single().name)
        } finally {
            dir.deleteRecursively()
        }
    }

    @Test
    fun uiBlocksAreChecked() {
        assertEquals("stats", checkBlock("""{"${'$'}ui": "stats", "version": 1, "items": [{"label": "Battery", "value": 81}]}"""))
        assertEquals(null, checkBlock("""{"${'$'}ui": "stats", "version": 9}"""))
    }

    @Test
    fun errorsAreTyped() {
        assertFailsWith<GraphrtException.InvalidConfig> { Runtime("""{"nope": 1}""") }
        val runtime = Runtime("{}")
        assertFailsWith<GraphrtException.Unsupported> { runtime.listSkills() }
        assertFailsWith<GraphrtException.InvalidInput> {
            runtime.registerTools("bad name") { tool("t", "d") { toolResult("") } }
        }
        assertNotNull(failedResult("x"))
    }
}
