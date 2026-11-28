- The chat page (`--chat WORKFLOW` without `=INPUT`) puts the message in the one
  start input a workflow requires, as channels do. It always sent `sys.query`,
  so a workflow that asks for `message` (like the chatbots Graphrt Desktop puts
  online with `graphrt deploy --direct --chat`) answered every chat message with
  "Something went wrong". A workflow that requires several inputs is refused at
  startup with what to change.
