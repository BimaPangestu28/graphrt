- `graphrt gateway` now sets `sys.user_id` of a run to the user whose token it
  verified, over anything the client sent, as `graphrt serve --verify-jwt`
  does. Before, a client could send its own `sys.user_id`, so a workflow that
  reads `{{#sys.user_id#}}` (one user's lessons with `learning.user`, one
  user's files) could run as another user behind a gateway, and saw no user
  when the client sent none.
