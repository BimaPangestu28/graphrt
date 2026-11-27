- `graphrt serve --channel telegram=WORKFLOW` for a workflow whose start requires
  one input (such as `message`, which the Desktop app writes) put the customer's
  text only in `sys.query`, so every run failed and the customer read "something
  went wrong". The one required input now takes the message, and a workflow whose
  required inputs a message cannot fill is refused at startup with their names.
- The server's own sentences on a channel come in Indonesian too: `language: id`
  in the channels file or `--channel-language id` (default `en`); single
  `messages` still replace single sentences.
