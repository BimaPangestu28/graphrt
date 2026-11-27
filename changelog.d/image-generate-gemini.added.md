- A second image generator: `GeminiImages` in `graphrt-image` (feature
  `generate-gemini`) for the image models of the Gemini API, behind the same
  `ImageGenerator` trait. `--image-model gemini/NAME` selects it
  (`GEMINI_API_KEY` or `GOOGLE_API_KEY`, optional `GEMINI_BASE_URL`), and
  `NAME` or `openai/NAME` keeps the OpenAI images API. A `WIDTHxHEIGHT` size
  is mapped to the closest aspect ratio and size class Gemini knows. The key
  travels in a header, redirects are not followed, an answer that points to
  a file is refused, not fetched, and a refusal by the provider is reported
  with its reason.
