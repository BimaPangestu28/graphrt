- `graphrt serve --channel`: the first typing sign of a turn goes out before
  its run starts. A quick run used to stop it before it was sent, so a
  Telegram button could keep spinning and a WhatsApp message could stay
  unread. A paused run is marked as waiting, an answered request is taken off
  the approvers' lists and numbered choices are kept before the message that
  shows them is sent, so a quick answer to it is never taken for a new message.
