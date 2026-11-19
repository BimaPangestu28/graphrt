import SwiftUI

/// Name field, "Run" (final result) and "Stream events" (live events).
///
/// Launch arguments, handy for simulators and CI:
/// `-name Grace` pre-fills the name, `-autorun YES` presses both buttons once loaded.
struct ContentView: View {
    @StateObject private var model = HelloWorkflowModel()
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
                .buttonStyle(.borderedProminent)
                Text(model.status).font(.footnote).foregroundStyle(.secondary)
                OutputSection(title: "Result", text: model.resultText)
                OutputSection(title: "Events", text: model.eventsText)
            }
            .padding()
        }
        .task {
            await model.load()
            if UserDefaults.standard.bool(forKey: "autorun") {
                model.run(name: name, excited: excited)
                model.stream(name: name, excited: excited)
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
