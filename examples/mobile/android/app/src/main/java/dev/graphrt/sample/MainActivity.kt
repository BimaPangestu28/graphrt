package dev.graphrt.sample

import android.app.Activity
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import org.json.JSONObject
import uniffi.graphrt_ffi.EventListener
import uniffi.graphrt_ffi.Runtime
import uniffi.graphrt_ffi.WorkflowHandle
import uniffi.graphrt_ffi.graphrtVersion

private const val LOG_TAG = "GraphrtSample"
private const val WORKFLOW_ASSET = "hello.yaml"
private const val RUN_TIMEOUT_MS = 10_000UL
private const val EXTRA_NAME = "name"
private const val EXTRA_AUTORUN = "autorun"

/**
 * Runs the offline `hello` workflow (start -> template -> end) through the
 * graphrt UniFFI binding.
 *
 * "Run" waits for the final result; "Stream events" prints every engine event
 * as the run produces it. graphrt calls happen on [graphrtExecutor] (never the
 * UI thread) and listener callbacks arrive on graphrt's worker threads, so
 * every UI update is posted to [mainHandler].
 *
 * Intent extras, handy for emulators and CI:
 * `--es name Grace` pre-fills the name, `--ez autorun true` presses both buttons once loaded.
 */
class MainActivity : Activity() {
    private val graphrtExecutor: ExecutorService = Executors.newSingleThreadExecutor()
    private val mainHandler = Handler(Looper.getMainLooper())

    private var runtime: Runtime? = null
    private var workflow: WorkflowHandle? = null

    private lateinit var nameField: EditText
    private lateinit var excitedBox: CheckBox
    private lateinit var statusView: TextView
    private lateinit var resultView: TextView
    private lateinit var eventsView: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(buildLayout())
        graphrtExecutor.execute(::loadWorkflow)
    }

    override fun onDestroy() {
        graphrtExecutor.execute {
            workflow?.close()
            runtime?.close()
        }
        graphrtExecutor.shutdown()
        super.onDestroy()
    }

    private fun buildLayout(): ScrollView {
        nameField = EditText(this).apply { hint = "name"; setText(intent.getStringExtra(EXTRA_NAME) ?: "Ada") }
        excitedBox = CheckBox(this).apply { text = "excited" }
        val runButton = Button(this).apply { text = "Run"; setOnClickListener { runWorkflow() } }
        val streamButton = Button(this).apply { text = "Stream events"; setOnClickListener { streamWorkflow() } }
        statusView = TextView(this)
        resultView = monospaceTextView()
        eventsView = monospaceTextView()

        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
            listOf(nameField, excitedBox, runButton, streamButton, statusView, resultView, eventsView).forEach {
                addView(it, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            }
        }
        return ScrollView(this).apply {
            fitsSystemWindows = true
            addView(column, MATCH_PARENT, WRAP_CONTENT)
        }
    }

    private fun monospaceTextView() = TextView(this).apply {
        setTextIsSelectable(true)
        typeface = android.graphics.Typeface.MONOSPACE
        setPadding(0, 24, 0, 0)
    }

    private fun loadWorkflow() {
        try {
            val document = assets.open(WORKFLOW_ASSET).bufferedReader().use { it.readText() }
            val newRuntime = Runtime("{}")
            runtime = newRuntime
            workflow = newRuntime.loadWorkflow(document)
            showStatus("graphrt ${graphrtVersion()} loaded workflow '${workflow?.name()}'")
            if (intent.getBooleanExtra(EXTRA_AUTORUN, false)) {
                mainHandler.post { runWorkflow(); streamWorkflow() }
            }
        } catch (error: Exception) {
            reportError("loading $WORKFLOW_ASSET", error)
        }
    }

    private fun currentInputsJson(): String =
        JSONObject()
            .put("name", nameField.text.toString())
            .put("excited", excitedBox.isChecked)
            .toString()

    /** Starts a run and blocks a background thread until it finishes. */
    private fun runWorkflow() {
        val inputsJson = currentInputsJson()
        showResult("running...")
        graphrtExecutor.execute {
            val loadedWorkflow = workflow ?: return@execute reportError("run", IllegalStateException("workflow not loaded"))
            try {
                loadedWorkflow.run(inputsJson, "{}", SilentListener).use { run ->
                    val resultJson = run.waitJson(RUN_TIMEOUT_MS) ?: """{"status":"timeout"}"""
                    Log.i(LOG_TAG, "result: $resultJson")
                    showResult(describeResult(resultJson))
                }
            } catch (error: Exception) {
                reportError("run", error)
            }
        }
    }

    /** Starts a run and prints events as the listener receives them. */
    private fun streamWorkflow() {
        val inputsJson = currentInputsJson()
        mainHandler.post { eventsView.text = "events:" }
        graphrtExecutor.execute {
            val loadedWorkflow = workflow ?: return@execute reportError("stream", IllegalStateException("workflow not loaded"))
            try {
                // The handle is not needed: the listener reports completion.
                loadedWorkflow.run(inputsJson, "{}", StreamingListener(::appendEvent)).close()
            } catch (error: Exception) {
                reportError("stream", error)
            }
        }
    }

    private fun describeResult(resultJson: String): String {
        val result = JSONObject(resultJson)
        val greeting = result.optJSONObject("outputs")?.optString("greeting")
        return "status: ${result.optString("status")}\ngreeting: $greeting\n\n$resultJson"
    }

    private fun showStatus(text: String) = mainHandler.post { statusView.text = text }

    private fun showResult(text: String) = mainHandler.post { resultView.text = text }

    private fun appendEvent(line: String) = mainHandler.post { eventsView.append("\n$line") }

    private fun reportError(action: String, error: Exception) {
        Log.e(LOG_TAG, "$action failed", error)
        showStatus("$action failed: ${error.message}")
    }
}

/** Listener for runs whose result is read with `waitJson`. */
private object SilentListener : EventListener {
    override fun onEvent(eventJson: String) {}
    override fun onAnswerChunk(text: String) {}
    override fun onFinish(resultJson: String) {}
}

/** Forwards every event, answer chunk and the final result to [emitLine]. */
private class StreamingListener(private val emitLine: (String) -> Unit) : EventListener {
    override fun onEvent(eventJson: String) {
        Log.i(LOG_TAG, "event: $eventJson")
        val event = JSONObject(eventJson)
        val nodeId = event.optString("node_id").takeIf { it.isNotEmpty() }?.let { " [$it]" } ?: ""
        emitLine("event: ${event.optString("type")}$nodeId")
    }

    override fun onAnswerChunk(text: String) {
        Log.i(LOG_TAG, "answer chunk: $text")
        emitLine("answer chunk: $text")
    }

    override fun onFinish(resultJson: String) {
        Log.i(LOG_TAG, "finish: $resultJson")
        emitLine("finish: $resultJson")
    }
}
