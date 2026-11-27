import SwiftUI

/// Name field, "Run" (final result), "Stream events" (live events) and "Ask
/// the assistant" (native tools, an approval and a streamed answer).
///
/// Launch arguments, handy for simulators and CI:
/// `-name Grace` pre-fills the name, `-autorun YES` presses every button once
/// loaded and approves the assistant's request without asking.
struct ContentView: View {
    @StateObject private var model = HelloWorkflowModel()
    @StateObject private var assistant = AssistantModel()
    @State private var name = UserDefaults.standard.string(forKey: "name") ?? "Ada"
    @State private var excited = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text("graphrt sample").font(.title2.bold())
                TextField("name", text: $name)
                    .textFieldStyle(.roundedBorder)
                    .autocorrectionDisabled()
                Toggle("excited", isOn: $excited)
                HStack {
                    Button("Run") { model.run(name: name, excited: excited) }
                    Button("Stream events") { model.stream(name: name, excited: excited) }
                }
                Button("Ask the assistant") { assistant.run(query: "Tell Ada my battery level") }
                .buttonStyle(.borderedProminent)
                Text(model.status).font(.footnote).foregroundStyle(.secondary)
                OutputSection(title: "Result", text: model.resultText)
                OutputSection(title: "Events", text: model.eventsText)
                OutputSection(title: "Assistant", text: assistant.transcript)
            }
            .padding()
        }
        .alert(
            "Allow \(assistant.pendingApproval?.name ?? "")?",
            isPresented: Binding(get: { assistant.pendingApproval != nil }, set: { if !$0 { assistant.decide(false) } }),
            presenting: assistant.pendingApproval
        ) { _ in
            Button("Allow") { assistant.decide(true) }
            Button("Don't allow", role: .cancel) { assistant.decide(false) }
        } message: { call in
            Text(call.argumentsJson)
        }
        .task {
            await assistant.load()
            await model.load()
            if UserDefaults.standard.bool(forKey: "autorun") {
                assistant.autoApprove = true
                model.run(name: name, excited: excited)
                model.stream(name: name, excited: excited)
                assistant.run(query: "Tell Ada my battery level")
            }
        }
    }
}

private struct OutputSection: View {
    let title: String
    let text: String

    var body: some View {
        if !text.isEmpty {
            Text(title).font(.headline)
            Text(text)
                .font(.system(.caption, design: .monospaced))
                .textSelection(.enabled)
        }
    }
}
