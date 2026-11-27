# Files and images: research, design and threat model

This page records why the file and image capabilities look the way they do:
what was already in the runtime, how other runtimes and model APIs handle
files and images, the options that were compared, the decisions, and the
threats each decision has to hold against. The user guide is
[files-and-images.md](files-and-images.md).

Researched and decided 2026-11-26. External facts were read from the vendors'
current documentation on that date and are cited inline.

## The three requirements

| Requirement | What it means here |
| --- | --- |
| Modular | Files and images are separate crates and separate switches. Storage and image generation sit behind traits. An agent that uses neither pays nothing: no code in the slim builds, no start-up work, no idle memory. |
| Fast | Nothing is read whole when a window is enough. Buffers move, they are not copied. Disk I/O, hashing, document extraction and image decoding run on the blocking pool. |
| Secure | Default deny. An agent reaches one directory. Sizes, counts and pixel dimensions are bounded before work starts. Types come from content. Every threat in the model below has a test. |

## What the runtime already had

Reading the code first changed the plan: about half of the "file capability"
existed, as contracts without a place to keep files.

| Existing piece | Where | Gap |
| --- | --- | --- |
| `File` value (`name`, `kind`, `transfer`, `url`, `reference`, `mime_type`, `size`) | `graphrt-core::value` | none, reused as is |
| `FileResolver` trait with `read` and `store`, `ChainResolver`, `DataUrlResolver` | `graphrt-core::file` | `store` had no implementation anywhere |
| `HttpFileResolver`: URLs through the SSRF safe egress policy | `graphrt-http` | none, reused for URL images |
| `LocalDirResolver` (`--files-dir`): read only, by reference | `graphrt-cli` | read only, no listing, no quota, not usable by an agent |
| `document-extractor` node: PDF, Office, spreadsheets, EPUB, e-mail with zip and PDF bomb limits | `graphrt-doc` | a node, not something an agent can call on a file it found |
| Vision: `vision: {enabled}` turns image files into `ContentPart::Image { url }` | `graphrt-llm::nodes::common` | only files that already have a URL. A stored image is an error. Nothing checks size, pixels or type, and metadata goes to the provider untouched. |
| Images in tool results (`ToolOutput::images`, 4 images, 5 MiB each) | `graphrt-core::tool`, [tool-images.md](tool-images.md) | none, reused by `image/view` |
| Tool providers (`ToolRuntime`, `ToolRegistry`), `tool_context` for per-run values the model cannot see | `graphrt-core::tool` | none, reused for the tools and for user scopes |
| Untrusted JSON never becomes a file value (`Value::from_json`) | `graphrt-core::value` | none. It is what makes file references unforgeable by models, plugins and HTTP bodies. |

So the work was: a place to keep files with real isolation, tools over it,
and an image pipeline between stored files and model providers. No second
file model and no second tool mechanism.

## How others do it

### Model APIs

| | Anthropic | OpenAI | Gemini |
| --- | --- | --- | --- |
| Image input | base64, URL or `file_id` | URL, base64 data URL or file id | inline bytes or Files API |
| Image types | JPEG, PNG, GIF, WebP | PNG, JPEG, WebP, GIF (first frame) | PNG, JPEG, WebP, HEIC, HEIF |
| Per image | 10 MB base64 on the API, 5 MB on Bedrock, 8000 px per side | 30,000 patches, larger is rejected | within the request limit |
| Per request | 128 MB, 100 images (600 on large context models). Above 20 images a lower pixel limit applies to each. | 512 MB, 1,500 images | 20 MB inline, 3,600 images |
| Resizing | Downscaled above 1,568 px on the long edge (2,576 px on the high resolution tier) unless the block asks for an error | by `detail` | by media resolution |
| Hosted files | 500 MB per file, 1 TB per organization, uploads cannot be downloaded | Files API | 2 GB per file, 20 GB per project, deleted after 48 hours, uploads cannot be downloaded |
| Image output | none | `POST /v1/images/generations`, GPT image models always answer with base64 | image models through the Interactions API, base64, SynthID watermark |

Sources: Anthropic [vision](https://platform.claude.com/docs/en/build-with-claude/vision),
[Files API](https://platform.claude.com/docs/en/build-with-claude/files) and
[PDF support](https://platform.claude.com/docs/en/build-with-claude/pdf-support).
OpenAI [images and vision](https://developers.openai.com/api/docs/guides/images-vision) and
[image generation reference](https://developers.openai.com/api/reference/resources/images/methods/generate).
Gemini [image understanding](https://ai.google.dev/gemini-api/docs/image-understanding),
[Files API](https://ai.google.dev/gemini-api/docs/files) and
[image generation](https://ai.google.dev/gemini-api/docs/image-generation).

What follows from the table:

- The three providers agree on four image types (JPEG, PNG, GIF, WebP) and
  disagree on everything else. A runtime that passes user images through
  inherits each provider's failure mode. Preparing the image once, to limits
  every provider accepts, removes that.
- Provider file stores are not a place to keep an agent's files. Uploads
  cannot be read back, one of them deletes files after two days, and a file id
  works with one provider. Anthropic's own page warns never to accept a file
  id from an end user, because any key of the workspace can read any file.
- An image above the long edge limit costs upload time and tokens for pixels
  the model never sees. Downscaling before the request is a pure gain.

### Agent protocols and SDKs

- **MCP** (specification 2025-11-25): a tool result is a list of content
  items: `text`, `image` (base64 with `mimeType`), `audio`, `resource_link`
  (a URI the client may fetch) and embedded `resource`. Roots are `file://`
  URIs a client exposes to a server, and the specification requires clients to
  validate them against path traversal and servers to respect them. This is
  the same split Graphrt uses: small content inline (`ToolOutput::images`),
  large content by reference (`File` with a `reference`).
- **Vercel AI SDK**: messages carry `file` parts (`mediaType`, `data` as
  base64, bytes or URL), and tool results can return `file-data` items. Its
  documentation notes that the default URL download does not check what a host
  name resolves to and tells you to supply a guarded download for untrusted
  URLs. Graphrt already resolves and checks every address itself
  (`HttpFileResolver`).
- **LangChain / LangGraph**: standard content blocks `image`, `file`, `audio`
  with `url`, `base64` or a provider file id, and `mime_type`. Files as such
  are left to the application.
- **Capability based file systems** (WASI, `cap-std`): a program holds a
  directory handle and can only open paths below it. On Linux `cap-std` uses
  `openat2` with `RESOLVE_BENEATH`, which makes escaping through a link
  impossible even under a race.

None of them gives an agent a quota, a type check or a per-user boundary.
Those are left to the application, which is where most deployments get them
wrong. That is the part worth building in.

## Decisions

### 1. Native crates, not plugins

| Option | For | Against |
| --- | --- | --- |
| Built into `graphrt-core` | no wiring | core must stay free of I/O and compile to wasm32. Every build would carry it. |
| **Separate crates behind cargo features** | off by default in slim builds, run on phones through the FFI crate, share `File` and `FileResolver` with every node | two more crates to version |
| Process plugin in the catalog | no runtime change | a process plugin is native code running as the user with no sandbox, needs Node on the machine, and does not exist on phones. It also cannot return file values, by design. |
| WebAssembly plugin | sandboxed | the sandbox has no file system on purpose |
| MCP filesystem server | works today with `--mcp` | an extra process per agent, no quota, no type checks, no user scopes, not on phones |

Decision: `graphrt-files` and `graphrt-image`. The plugin catalog is the
wrong home for the one thing that must be trusted to keep an agent inside a
directory.

### 2. Own storage behind a trait, not provider file stores

`FileStore` has six methods (`stat`, `read`, `write`, `delete`, `list`,
`sha256`). `DirStore` and `MemoryStore` ship. Object storage crates were
considered (`object_store` 0.14, `opendal` 0.59) and left out: each pulls a
large tree for backends most agents never use, and the trait is small enough
for a host to implement over the client it already has. Limits, quotas,
scopes and type checks live in `Workspace`, above the store, so a new backend
cannot forget them. An S3 backend was added later with a client of its own,
see [decision 13](#13-object-storage-with-a-client-of-our-own).

### 3. No links, checked per component

| Option | Holds against | Cost |
| --- | --- | --- |
| Canonicalize, then check the prefix (what `--files-dir` does) | links that lead out | two path walks, and a link swapped between the check and the open wins |
| `cap-std` (`openat2` with `RESOLVE_BENEATH`) | links that lead out, races included | about eight new crates. The workspace forbids `unsafe`, so calling `openat2` directly is not an option. |
| **Walk every component, refuse any link, open with `O_NOFOLLOW`** | every link, also ones that stay inside | a narrow race on intermediate directories (see residual risks) |

Decision: the third. "No links at all" is a rule a reviewer can check in one
function, it needs no new dependency, and the final component is protected by
the open flags, so a link swapped in at the last moment is refused by the
kernel. On Windows the same walk refuses junctions and symbolic links, and the
file is opened with `FILE_FLAG_OPEN_REPARSE_POINT`.

Paths are a type. `WorkspacePath::parse` is the only way to make one, and the
same rules hold on every operating system, so a path a model learned on Linux
does not become a different file on Windows.

### 4. Types from content, with our own sniffer

`infer` (0.22) and `file-format` (0.29) detect hundreds of formats. The
workspace needs about twenty five signatures and one thing neither crate
does: deciding that text is active content (HTML or SVG) wherever the first
tag is. A 250 line module with no dependency does both, and the answer is one
of eight classes the rest of the code switches on.

### 5. The agent reads files through tools, not content blocks

| Option | For | Against |
| --- | --- | --- |
| Provider document blocks (`document`, `file_id`) | the provider sees the layout of a PDF | one provider each, the whole file enters the context, limits differ |
| **Tools that return a window of text** | every provider, local models included. A 300 page PDF costs one page of context at a time. The agent can search before it reads. | scanned documents have no text (vision covers single pages) |

Decision: tools. `files/read` extracts documents with `graphrt-doc`, which
already bounds zip and PDF decompression, and keeps the text of the last
document so paging does not parse it again.

### 6. Search is a text search

A regular expression from a model is an input to a pattern engine. Rust's
`regex` crate is linear time, but it is still a second parser fed by a model,
and literal search answers almost every "which file mentions X" question. The
tool takes a text, reads text files in steps of 256 KiB and stops at 50
matches or 64 MiB. Documents are not searched: that is what a knowledge base
is for, and the tool says how many it skipped.

### 7. Users are separated by a scope the model cannot touch

The `agent` node already has `tool_context`: values rendered from the run and
sent to a tool provider beside the arguments, never shown to the model and
never recorded. The workspace scope travels there. Each scope is a directory
named by a hash of the scope text, with its own quota. A provider in per-user
mode refuses a call without a scope.

### 8. Files leave the agent as file values

A tool puts a `File` in its structured result. The `agent` node's `files`
output and the `tool` node's `files` output collect them. Because untrusted
JSON never becomes a file value, only native tools can produce one, and the
`reference` (`workspace:<path>`) is resolved by a `FileResolver` that checks
it against its own workspace again.

Download buttons, previews and cards are the host's job. The file value
carries the detected type and `inline_safe`, which is what a user interface
needs to choose between showing a file and offering it.

<a id="images"></a>

### 9. Images are prepared before they reach a provider

A new contract in `graphrt-core::file`, `ImagePreparer`, turns an image file
into a URL a model can take. Without one registered, vision behaves as
before. `graphrt-image` implements it:

1. Read the bytes through the `FileResolver` (workspace, upload, or a URL
   through the egress policy), with a byte limit.
2. Probe the header with our own parser: type from magic bytes, width and
   height. Refuse what is not PNG, JPEG, GIF or WebP, and refuse a pixel count
   above the limit. The decoder has not run yet.
3. Within limits and small enough: remove metadata at the container level
   (EXIF, XMP, IPTC, text chunks), without decoding, and pass the pixels on
   untouched.
4. Too large for a provider: decode with hard limits on width, height and
   allocation, apply the EXIF orientation, downscale to 1,568 px on the long
   edge and encode again. This runs on the blocking pool.
5. Return a `data:` URL.

| Option for decoding | For | Against |
| --- | --- | --- |
| libvips or ImageMagick bindings | fast, every format | C code with a long CVE history, not available on phones, breaks the single binary |
| `zune-image` 0.5 | fast pure Rust decoders | younger, smaller format and encoder coverage |
| **`image` 0.25 with only `png`, `jpeg`, `gif`, `webp`, no `rayon`** | pure Rust, strict width and height limits, the decoders most Rust software ships | larger than a single format crate |

`image::Limits` documents `max_alloc` as not strict for every decoder, so the
pixel check in step 2 does not rely on it: the header parser refuses the
image before a decoder allocates anything.

The same pipeline backs the `image` tools (`info`, `view`, `resize`,
`convert`, `thumbnail`), which work on workspace files.

### 10. Image generation behind a trait

`ImageGenerator` has one method. The first implementation speaks the OpenAI
images API (`POST {base_url}/images/generations`), which GPT image models and
many compatible servers implement and which always answers with base64. The
bytes a provider returns are treated like any upload: probed, bounded and
stored in the workspace. Gemini's image models use the newer Interactions API
and are a second implementation of the same trait, not part of this work.

### 11. Features and defaults

| Build | `files` | `image` |
| --- | --- | --- |
| Default CLI (`cargo install`, release binaries) | on, inert without `--workspace` | on, inert without `--workspace` or `vision` |
| `agent-minimal`, `agent-slim` (the budgeted builds) | off | off |
| Phones (`graphrt-ffi`) | opt in | opt in |

Off means not compiled: the flags do not exist, and the code paths that
collect files from tool results are behind the `tool-files` feature of
`graphrt-core` and `graphrt-llm`. See
[What it costs a build that does not use it](#what-it-costs-a-build-that-does-not-use-it).

### 13. Object storage with a client of our own

Added 2026-11-27. Agents that run as several replicas, or on machines whose
disk does not stay, need the workspace somewhere shared.

| Option | For | Against |
| --- | --- | --- |
| AWS SDK for Rust (`aws-sdk-s3`) | complete, maintained by AWS | about 100 crates. `aws-sdk-*` and `aws-config` are banned in `bench/budgets.toml` because of what they do to the size of every build that links them. |
| `object_store` 0.14 | S3, GCS, Azure behind one trait | pulls `quick-xml`, `chrono`, `itertools`, `humantime` and more, and its S3 client does far more than six operations |
| `opendal` 0.59 | forty backends | the largest tree of the three |
| `rust-s3`, `rusty-s3` | small | one more signer next to the two the repository already has |
| **Five requests over `reqwest`, signed by the signer the gateway already ships** | no new crate in the lock file. The SigV4 signer and the credential chain (environment, ECS, EKS, EC2 roles) of `graphrt-provider-aws` are reused, and they are tested against the AWS test vectors. | we own about 450 lines, and only the S3 API is covered |

Decision: the last. `S3Store` is behind the `s3` feature of `graphrt-files`
and the `files-s3` feature of the CLI. Both are off by default, and the
budgeted builds cannot enable them by accident because they do not enable
`files` at all.

What the store does not do: multipart uploads (a workspace file is at most
32 MiB, far below the 5 GiB of one `PutObject`), versioning, server side
encryption settings (set them on the bucket), and Google Cloud Storage or
Azure Blob (GCS has an S3 compatible endpoint, Azure needs its own store
behind the same trait).

Checked against a real store, SeaweedFS 3 in a container, with the
`a_real_object_store_when_one_is_configured` test and the CLI: signatures,
names with URL characters, ranges, conditional creation, listing.

## Threat model

Who is trusted: the operator (who starts the runtime, picks the workspace
directory and writes the providers file). Who is not: the model, the content
of every file and image, end users, and the author of a workflow as far as
the host file system goes.

| # | Threat | Defence | Test |
| --- | --- | --- | --- |
| F1 | Path traversal (`../`, absolute paths, `..\`, drive letters, encoded dots) | `WorkspacePath` refuses them before storage is touched. Encoded dots stay literal text. | `path::tests::refuses_traversal_in_every_spelling`, `refuses_absolute_paths_drives_and_streams`, `tools::every_escape_attempt_is_an_error_result`, `agent::a_model_that_tries_to_leave_the_workspace_gets_errors_and_nothing_else`, CLI `an_input_cannot_steer_a_write_out_of_the_workspace` |
| F2 | Symbolic link, junction or hard to see link planted in the workspace | Every component is checked and a link is refused, for read, stat, hash, write, delete and list. Final component opened with `O_NOFOLLOW`. | `dir_store::links_never_lead_out_of_the_root` |
| F3 | Windows specifics: device names, alternate data streams, trailing dots | Refused on every system | `path::tests::refuses_windows_device_names`, `refuses_hidden_names_trailing_dots_and_empty_components` |
| F4 | Names that display as something else (bidi override, zero width, control characters) | Refused | `path::tests::refuses_control_and_invisible_characters`, `resolver::tests::file_names_from_outside_are_made_safe` |
| F5 | FIFO or device that blocks or never ends | Only regular files are opened, and with `O_NONBLOCK` | `dir_store::special_files_are_not_opened` |
| F6 | Filling the disk: one huge file, many files, many small writes | Per file size, total bytes and file count, checked under one lock so concurrent writers cannot pass together | `workspace::tests::size_quota_and_count_limits_hold`, `concurrent_writers_cannot_pass_the_quota_together`, `tools::limits_come_back_as_errors_the_model_can_act_on` |
| F7 | Flooding the context: a huge file, endless lines, a search that matches everything | Windowed reads with a character budget, lines cut at 16 KiB, search capped by matches, files and bytes | `tools::reading_pages_through_long_files`, `search_stops_at_its_limits`, `text::tests::an_endless_line_is_cut_not_buffered` |
| F8 | MIME spoofing: a program named `photo.png`, a PDF named `notes.txt` | The type is sniffed from content. The extension only refines text and zip containers. | `sniff::tests::the_extension_never_overrides_the_content`, `workspace::tests::the_type_comes_from_the_bytes_and_programs_are_refused` |
| F9 | Storing something the system would run | ELF, PE, Mach-O, WebAssembly and `#!` scripts are refused. Files are created `0600`, directories `0700`. | `tools::programs_cannot_be_written_whatever_the_name`, `dir_store::files_and_directories_are_owner_only` |
| F10 | Active content served back to a user interface (HTML, SVG with script) | Detected wherever the tag is and whatever the extension, reported as `text/html` or `image/svg+xml` with `inline_safe: false` | `sniff::tests::html_and_svg_are_active_whatever_they_are_called`, `tools::programs_cannot_be_written_whatever_the_name` |
| F11 | Zip and PDF decompression bombs in a document the agent reads | `graphrt-doc` limits entries, decompressed bytes, pages and cells before parsing. Documents above `max_document_bytes` are not opened. Parser panics are caught. | `graphrt-doc` `tests/limits.rs` and `tests/adversarial.rs`, `tools::documents_are_read_as_text` (broken PDF) |
| F12 | Prompt injection through file content | Content is framed between boundaries that change on every call. The tools cannot exceed the workspace whatever the model decides. `require_approval` for destructive tools. | `tools::file_content_is_framed_as_data` |
| F13 | One user reading another user's files | Scope in `tool_context`, hashed directory per scope, calls without a scope refused, the model cannot pass a scope, references of another scope do not resolve | `workspace::tests::scopes_are_invisible_to_each_other_and_to_the_root`, `tools::users_only_reach_their_own_files`, `agent::the_scope_comes_from_the_workflow_not_from_the_model`, `agent::a_scoped_workspace_fails_closed_without_tool_context` |
| F14 | Forged file references (a model or a plugin returns `{"$file": true, "reference": ...}`) | Untrusted JSON never becomes a file value. `workspace:` references are parsed as workspace paths and checked against the resolver's own view. | `graphrt-core` `tool::collect_files` doc test, `resolver::tests::reads_workspace_references_and_ignores_others` |
| F15 | A crash that leaves half a file | Writes go to a hidden temporary file that is renamed into place | `dir_store::writes_leave_no_temporary_files_and_hashes_stream`, `concurrent_writers_never_tear_a_file` |
| F16 | Heavy file work stalling every other run | Disk I/O, hashing and extraction run on the blocking pool | `dir_store::heavy_file_work_does_not_block_the_async_worker` |
| S1 | A path that reaches another prefix or another bucket, or that turns into query parameters (`#`, `&`, `=`, `%2e%2e`, `;`) | A key is the prefix plus a `WorkspacePath`, percent encoded byte by byte. Bucket, region, endpoint and prefix are validated when the store is built. | `s3::a_path_cannot_reach_another_prefix_the_bucket_or_the_query`, `s3::tests::bucket_names_cannot_carry_url_syntax` |
| S2 | Objects planted in the bucket that are not workspace files (`..`, hidden names, another prefix, names that display as something else) | A listing returns only keys below the prefix that parse as workspace paths | `s3::objects_that_are_not_workspace_files_are_not_listed` |
| S3 | Unsigned or altered requests, a damaged upload | SigV4 on every request with the SHA-256 of the body in the signature. The test server recomputes both. | `s3::every_request_is_signed_and_a_wrong_key_is_refused` |
| S4 | A signed request sent to another host by a redirect | Redirects are not followed and are reported | `s3::redirects_are_not_followed` |
| S5 | A secret key or session token in an error, a log or `Debug` output | Errors are built from the status, the error code and a bounded message. Transport errors carry no URL. `Debug` prints neither. | `s3::secrets_never_appear_in_errors_or_debug_output` |
| S6 | File content sent unencrypted, credentials or a path smuggled into the endpoint | `http://` needs `allow_http`. An endpoint is an origin: no user, path, query or fragment. | `s3::plain_http_and_malformed_configuration_are_refused`, `s3::tests::endpoints_are_origins_and_http_needs_a_switch` |
| S7 | A store that answers with more than asked: a whole object for a range, a huge listing, pages without end, an XML entity bomb | Bodies are read up to the requested size, listings up to 4 MiB a page and 64 pages. The XML reader expands the five standard entities and nothing else. | `s3::a_server_that_sends_too_much_is_cut_off`, `s3::tests::a_listing_is_read_without_expanding_anything` |
| S8 | A store that hangs or throttles | A time limit per request, three attempts with a growing pause, then an error the agent sees | `s3::a_store_that_does_not_answer_times_out`, `s3::throttling_is_retried_and_a_dead_store_is_reported` |
| S9 | Two replicas creating the same file | `If-None-Match: *` on a create, so the store refuses the second one | `s3::a_create_does_not_replace_what_another_process_wrote` |
| S10 | The rules of a workspace lost on a new backend (size, quota, programs, user scopes) | They live in `Workspace`, above the store | `s3::a_workspace_keeps_its_rules_over_object_storage` |
| I1 | Image bomb: a small file that declares a huge canvas | Header probe refuses the pixel count before any decoder runs (5.5 µs for a 64 megapixel PNG). Decoder limits on width, height and allocation are a second line. | `probe::a_huge_canvas_is_refused_from_the_header_alone`, `tools::bombs_spoofed_files_and_escapes_are_error_results`, `prepare::spoofed_vision_inputs_are_refused` |
| I2 | Malformed image aimed at a decoder or at our own header parsers | Pure Rust decoders, no `unsafe` in the workspace, checked indexing only, decoding behind a panic boundary on the blocking pool | `probe::truncated_files_fail_cleanly_at_every_length`, `probe::corrupted_files_never_panic_or_exceed_the_limits` |
| I3 | Image type spoofing (SVG, HTML, HEIC or a program sent as `image/png`) | Only PNG, JPEG, GIF and WebP by magic bytes reach a model or a tool. The media type sent to a provider is the detected one. | `probe::spoofed_and_unsupported_content_is_refused`, `prepare::spoofed_vision_inputs_are_refused`, `nodes::a_spoofed_vision_input_stops_the_node_before_the_model_is_called`, `graphrt-llm` `a_refused_image_fails_the_node_and_is_never_sent_as_it_was`, CLI `image_tools_inspect_and_thumbnail_a_workspace_image` |
| I4 | Location and device data in EXIF (and XMP, IPTC, text chunks) sent to a model provider or written to a result | Metadata is cut out of the container before an image leaves the runtime, rotated photos are turned upright first, and files the tools write are encoded without any | `metadata::*` (one test per format), `metadata::nothing_prepared_for_a_model_carries_metadata`, `prepare::a_rotated_photo_is_turned_upright_before_its_metadata_goes`, `nodes::a_stored_photo_reaches_the_model_checked_and_without_metadata` |
| I5 | SSRF through an image URL | URL images are fetched by the model provider, not by the runtime. With inlining on, the fetch goes through the egress policy of `graphrt-http`. A generator answer that holds a URL is refused, not fetched, and redirects of the generator are not followed. | `prepare::remote_images_are_left_to_the_provider_unless_inlined`, `openai::an_answer_with_a_url_is_refused_not_fetched`, `openai::redirects_are_not_followed`, `graphrt-http` `files::tests::rejects_private_addresses_and_non_http` |
| I6 | Prompt injection through text in an image | Same position as F12: the image is data, `image/view` says so, tools stay bounded, approvals for what matters | `tools::view_shows_a_checked_image_to_the_model` |
| I7 | Output of an image generator trusted as an image, or its key leaking | Probed, bounded and stripped like an upload before it is stored. The key is scrubbed from errors and never printed. | `tools::generated_images_are_checked_like_uploads`, `openai::errors_are_reported_without_the_key` |
| I8 | Polyglot files: a valid image with a script, an archive or a page appended | Everything after the end of the image is dropped when metadata is removed | `metadata::*` (each fixture carries a payload after the image) |
| I9 | Decoding a large image stalling every other run | Probe, decode, resize, encode and base64 run on the blocking pool | `prepare::preparing_a_large_image_does_not_block_the_async_worker` |

### Residual risks

- **Races on intermediate directories.** Between the check of a directory
  and the open of a file below it, a process running as the same user with
  write access to the workspace could replace that directory with a link.
  Such a process can already read everything the user can. Closing the gap
  fully needs `openat2` (`cap-std`), which the `FileStore` trait leaves room
  for.
- **Hard links.** A hard link to a file outside the workspace cannot be told
  from a regular file. The tools cannot create one. Do not let untrusted
  local users write into the workspace directory.
- **Prompt injection.** Framing content lowers the odds, it does not remove
  them. What holds regardless is the boundary of the tools.
- **Quota drift.** The usage of a workspace is counted once and then kept by
  the writes that go through it. Files changed behind its back are counted
  again on the next start (`Workspace::recount`).
- **Quota across replicas.** With a shared object store each process keeps
  its own count of the workspace between starts, so replicas writing at the
  same time can pass the quota by what the others wrote. Bucket policies and
  lifecycle rules are the hard limit there.
- **A store that ignores `If-None-Match`.** Creation without overwrite is
  then only checked inside one process.
- **CPU time of document extraction.** Bounded by size limits, not by a
  timer. Run agents with a node or run timeout.

## Performance

Measured with `cargo run --release -p graphrt-files --example bench` on a
13th Gen Intel Core i7-13700, Linux 7.0, rustc 1.95, tmpfs temp directory,
2026-11-26. Medians.

| What | Result |
| --- | --- |
| Check one path | 116 ns |
| Detect the type of a file (8 KiB window) | 3.6 µs |
| Write 64 MiB (temporary file and rename) | 1,432 MiB/s |
| Read 64 MiB | 2,226 MiB/s |
| SHA-256 of 64 MiB (streamed, 64 KiB buffer) | 1,644 MiB/s |
| `files/write`, small file | 22 µs |
| `files/read`, small file | 29 µs |
| `files/read`, last 10 lines of a 32 MiB file | 20 ms (1,598 MiB/s), one 256 KiB buffer at a time |
| `files/search`, 32 MiB of text | 55 ms (587 MiB/s) |
| List 2,000 files in 20 directories | 1.8 ms |
| `files/search` across 2,000 small files | 59 ms |

How the code gets there:

- A file is never read whole unless the caller needs all of it. `read` and
  `search` walk it in steps of 256 KiB and keep only the unfinished last line
  between steps.
- Buffers are moved into the store and out of it. The only copy on the write
  path is the one into the kernel.
- `DirStore` hashes in one blocking task with a 64 KiB buffer. Other stores
  get a chunked default.
- The text of the last extracted document is kept, so paging through a PDF
  parses it once.

### Images

Measured with `cargo run --release -p graphrt-image --example bench`, same
machine, one core.

| What | Input | Result |
| --- | --- | --- |
| Read the header (format, size, metadata) | 12 MP JPEG, 2.5 MiB | 103 ns |
| Refuse an image bomb | 64 MP PNG in 0.3 MiB | 5.5 µs, nothing decoded |
| Remove metadata, pixels untouched | 12 MP JPEG, 2.5 MiB | 2.8 ms |
| Prepare for a model, no resize needed | 1024x768 JPEG | 0.11 ms |
| Prepare for a model: decode, downscale, encode | 12 MP JPEG (4000x3000), 2.5 MiB | 155 ms, sends 0.39 MiB |
| The same | 24 MP JPEG (6000x4000), 5.1 MiB | 222 ms, sends 0.33 MiB |
| The same | 2560x1440 PNG, 0.9 MiB | 124 ms, sends 0.22 MiB |
| Thumbnail, 256 px | 12 MP JPEG | 67 ms |

- The common case, an image that already fits, never reaches a decoder: one
  pass over the container and a base64 encode.
- A reduction to half or less averages source pixels in one pass instead of
  running a bicubic kernel that grows with the reduction. The first version
  used bicubic throughout and took 228 ms and 459 ms for the two photos.
- Sending 0.39 MiB instead of 2.5 MiB is the larger gain: less upload, and
  no tokens spent on pixels the provider would scale away.

### Object storage

`cargo run --release -p graphrt-files --features s3 --example s3_bench`,
same machine, SeaweedFS 3 in a container on loopback, medians.

| What | Result |
| --- | --- |
| Stat | 0.39 ms |
| Read a small object | 0.61 ms |
| Write a small object | 1.17 ms |
| Read 16 MiB | 11 ms (1,462 MiB/s) |
| Write 16 MiB (hash, sign, upload) | 89 ms (180 MiB/s) |
| Read a 256 KiB window in the middle of 16 MiB | 1.09 ms |
| List 200 objects | 3.7 ms |
| `files/read`, small file | 1.9 ms |
| `files/search` across 200 small objects | 101 ms |

The release build of the default CLI grows by 198 KiB with `files-s3`
(32,835,312 to 33,037,552 bytes, x86_64 Linux), which is the credential
chain and the signer. It stays opt in.

Against a store in another region every request costs a round trip of tens
of milliseconds, which makes `files/search` over many files the slow case.
The windowed reads of `files/read` stay one ranged request per 256 KiB.

### What it costs a build that does not use it

Nothing, and that took a decision. The first version added the `files`
output of the `agent` node unconditionally: 11 KiB of code in
`graphrt-llm`. On the static musl build that was enough to move the layout
of the binary and touch 18 more pages at start: idle RSS of `agent-slim`
went from 6.05 to 6.12 MiB, and CI, where the budget is 6.0 MiB and main
sits at 5.95, failed.

So everything a slim agent cannot use is behind a cargo feature:
`tool-files` in `graphrt-core` and `graphrt-llm` (the `files` outputs), and
`files` in the CLI (the flags, the workspace, the tools). A build without
native file tools has nothing that could return a file anyway, because
plugins and MCP servers cannot create file values.

`agent-minimal` and `agent-slim` (static musl, x86_64, `min-size`), main
against this change, 21 starts each, taken in turns on one machine:

| Build | Binary | Idle RSS | exec -> `/healthz` p50 / p95 |
| --- | ---: | ---: | ---: |
| `agent-slim`, main | 7,588,136 B | 6.051 MiB | 3.46 / 3.69 ms |
| `agent-slim`, with file workspaces | 7,587,848 B | 6.051 MiB | 3.48 / 3.71 ms |
| `agent-minimal`, main | 4,439,976 B | 4.035 MiB | 3.20 / 3.39 ms |
| `agent-minimal`, with file workspaces | 4,440,152 B | 4.035 MiB | 3.23 / 3.34 ms |

Images follow the same rule: the call from the model nodes to the
`ImagePreparer` is behind `vision-preparer` in `graphrt-llm`, and the
`--image-*` flags behind `image` in the CLI. With both crates in the tree,
`agent-slim` measures 6.047 MiB idle against 6.051 MiB for main, and
`agent-minimal` 4.031 against 4.035.

A build that does enable `files` and `image` pays for the code only when it
is used: nothing is opened, registered or allocated at start without
`--workspace`, and the image gate runs only for nodes with `vision` on.

## Deliberately left for later

- A user interface in Graphrt Desktop: attach files and images to an agent,
  see and save what it produced. The host side is small (a `Workspace` per
  agent, `FileTools` and `ImageTools` in the tool registry, a
  `VisionPreparer` in the services, the `files` output for the results), but
  it has to follow the runtime version the app builds against.
- HTTP routes to upload to and download from a workspace on `graphrt serve`.
  Today a host puts files in the directory or uses the crate.
- Creating PDF and Office files. Agents write text formats.
- Searching inside documents, and regular expression search.
- Native document blocks (sending a PDF to a provider as a document).
- Audio and video.
- A second image generator (Gemini) and image editing.
- HEIC and AVIF input. Phones produce HEIC, and it is refused today with a
  message that says to convert it. Decoding it in pure Rust is not ready.
- Animations. GIF and WebP animations pass through when they fit, and are
  reduced to their first frame when they have to be encoded again.
