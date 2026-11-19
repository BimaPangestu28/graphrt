# Images in tool results

Tools can return images next to their text result, and the `agent` node shows
them to the model. This is what a vision "computer use" loop needs: a
`screenshot` tool returns the screen, the model sees it and answers with a
`click {x, y}` call.

## Data model

`graphrt_core::tool::ToolOutput` has an `images` field next to `text`, `data`
and `is_error`:

```rust
pub enum ToolImage {
    /// Inline bytes, base64 encoded (standard alphabet, padding optional).
    Base64 { media_type: String, data_base64: String },
    /// A remote image (`https://` only).
    Url { url: String },
}

let output = ToolOutput::text("1280x800 screenshot").with_image(ToolImage::png(base64_png));
```

`ToolOutput::text` and `ToolOutput::error` are unchanged and produce no
images. `ToolOutput` now derives `Default`, so struct literals can use
`..ToolOutput::default()`; literals that list every field must add
`images: Vec::new()`.

In the `agent` node a tool result becomes one `tool` message whose content is
the (truncated) text followed by one `ContentPart::Image` per image
(`data:<media_type>;base64,<data>` or the https URL). Each provider adapter
decides how to put those parts on the wire.

## Limits

Checked by `graphrt_core::tool::check_images`, both where results enter the
host (plugin `tool.call` results) and in the `agent` node (every tool source,
including native Rust tools and MCP servers):

| Limit | Value |
| --- | --- |
| Images per result | 4 (`MAX_TOOL_IMAGES`) |
| Decoded size per image | 5 MiB (`MAX_TOOL_IMAGE_BYTES`) |
| Media types | `image/png`, `image/jpeg`, `image/webp`, `image/gif` |
| URLs | `https://` only; `data:` URLs must use the base64 form |

A plugin result that breaks a limit fails that tool call; the model receives
the error text and can retry. In the agent node, invalid images from other
sources are dropped and the model is told why in the tool result text.

Plugin results also have to fit in one protocol line
(`limits.max_message_bytes`, 16 MiB by default). Base64 is about 4/3 of the
raw size, so four 5 MiB images do not fit the default; the SDKs check the
encoded line before sending it and report `message_too_large` instead.

Images are resent on every later model turn, which adds up quickly in a
screenshot loop. `agent.keep_tool_images: N` keeps only the images of the last
N image-bearing tool results in the conversation; older ones are replaced by
a text note such as `[1 image removed from history]`. The default is 3, which
suits computer-use loops; set it to `null` to keep every image, or `0` to keep
none after the turn that produced them.

## Provider encoding

Native means the image goes inside the provider's tool-result structure.
Fallback means the tool result keeps only the text, and the images follow in a
user turn right after the run of tool results (after the last of several
parallel results), each result's images introduced by
``Images returned by tool `<name>` (call <id>):``. On OpenAI-compatible servers
the tool message text also ends with `[N images attached after the tool
results]`, so the model connects the two.

| Provider | Encoding | Source |
| --- | --- | --- |
| `anthropic` | Native: `tool_result.content` is `[{"type":"text"}, {"type":"image","source":{...}}]` (`base64` or `url` source). Without images `content` stays a string. | Verified: Anthropic "Handle tool calls" docs (content may be text, image, document or search_result blocks). |
| `openai` (and every OpenAI-compatible server) | Fallback: `role: tool` message with text, then one `role: user` message with `image_url` parts after the last consecutive tool message. | Verified: OpenAI chat completions reference, "For tool messages, only type `text` is supported." |
| `gemini`, model `gemini-3*` or later | Native: `functionResponse.parts: [{"inlineData": {"mimeType", "data"}}]` (`fileData` for URLs). | Verified: Gemini API reference (`FunctionResponse.parts`, `FunctionResponseBlob`) and function-calling guide ("multimodal function responses", Gemini 3 series; png, jpeg, webp). |
| `gemini`, older or unrecognized models | Fallback: `inlineData` / `fileData` parts in the user turn, after all `functionResponse` parts of that turn. | Assumed: mixing media parts with function responses in one user turn is accepted by Gemini 2.x. |
| `bedrock`, Anthropic Claude and Amazon Nova models | Native: `toolResult.content: [{"text"} or {"json"}, {"image": {"format", "source": {"bytes"}}}]`. | Verified: Bedrock `ToolResultContentBlock` reference ("only supported by Amazon Nova and Anthropic Claude 3 and 4 models"). |
| `bedrock`, other models | Fallback: `image` blocks in the user turn after the `toolResult` blocks. | Assumed. Bedrock needs base64 images; URL images are an error, as for user images. |

GIF in a Gemini function response is not listed as supported by Google; the
adapter sends it anyway and the API answers with a 400 that names the type.

## Plugin protocol

`tool.call` results gain an optional `images` array:

```json
{"text": "screenshot taken", "images": [{"media_type": "image/png", "data_base64": "iVBORw0..."}, {"url": "https://example.com/a.png"}]}
```

The host announces support in `initialize`:

```json
{"protocol_version": 1, "host": {...}, "max_message_bytes": 16777216, "features": ["tool_images"]}
```

## Compatibility

- Protocol version stays `1`: `images` is optional and `features` is an
  additive `initialize` parameter.
- Older hosts reject unknown result fields. The TypeScript, Python and Rust
  SDKs therefore only send `images` when the host listed `tool_images`;
  otherwise they drop the images and append
  `[N image(s) omitted: the graphrt host does not support image tool results]`
  to the text, so the tool still works.
- Older SDKs never send `images`, and new hosts accept results without it.
- Saved agent pause states from earlier versions resume unchanged (the new log
  field defaults to 0).
- Results without images produce byte-identical provider requests.
