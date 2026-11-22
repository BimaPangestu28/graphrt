# Example plugins

| Directory | Language | Shows |
| --- | --- | --- |
| [`python-sentiment`](python-sentiment) | Python (stdlib SDK) | streaming chunks, branching handles, a tool |
| [`node-uppercase`](node-uppercase) | JavaScript SDK | streaming, pause/resume approval, a tool |
| [`rust-text-stats`](rust-text-stats) | Rust SDK | progress events, branching, a tool |
| [`wasm-text-tools`](wasm-text-tools) | WebAssembly | sandboxed nodes in a portable `.wasm` module |

[`demo.yaml`](demo.yaml) uses the Python, JavaScript and WebAssembly plugins in
one workflow:

```sh
graphrt run plugins/examples/demo.yaml --plugins plugins/examples \
    --input review="I love this, the support was great"
# positive (1.0) | I LOVE THIS, THE SUPPORT WAS GREAT | i-love-this-the-support-was-great
```

Try one without installing it:

```sh
graphrt run my-workflow.yaml --plugins plugins/examples
```

or install it into `~/.graphrt/plugins`:

```sh
graphrt plugin install plugins/examples/python-sentiment
```

A workflow using the sentiment plugin:

```yaml
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: review, type: paragraph, required: true }] }
    - { id: mood, type: sentiment.analyze, text: "{{#start.review#}}" }
    - { id: happy, type: answer, answer: "Thanks! ({{#mood.score#}})" }
    - { id: sad, type: answer, answer: "Sorry to hear that." }
    - { id: meh, type: answer, answer: "Noted." }
  edges:
    - { source: start, target: mood }
    - { source: mood, target: happy, handle: positive }
    - { source: mood, target: sad, handle: negative }
    - { source: mood, target: meh, handle: neutral }
```

The Rust example must be built first:
`cargo build --release -p graphrt-plugin-sdk --example text_stats`. Its
manifest starts `run.sh`, which execs the binary from the workspace `target/`
directory (a manifest `command` may not leave the plugin directory with `..`).
