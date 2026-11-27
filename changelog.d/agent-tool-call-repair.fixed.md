- `agent` steps forgive the tool calls small models write, for every tool:
  arguments are put into the types the tool's schema asks for (`"400"` for an
  integer, `"false"` for a boolean, JSON text for a list, `width` for the only
  not-given `max_width`, `{}` for a plain value counts as not given); a call
  written as JSON text (`{"name": "resize", "parameters": {...}}`) is made when
  it names an offered tool; a call that leaves out a required argument is
  answered once with the argument's name and description so the model calls
  again, instead of reaching a tool that can only refuse it.
