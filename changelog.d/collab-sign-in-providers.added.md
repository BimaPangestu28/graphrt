- `graphrt collab`: `auth.providers` signs people in with several identity
  providers, each a button in the app (Google on the hosted server, any
  OpenID Connect provider on a self hosted one). A provider whose
  `client_id_env` is empty is off until it is set. `GET /v1/info` lists them
  and `GET /v1/auth/login?provider=<id>` picks one. `email_verified_claim`
  keeps only emails the provider vouches for. The client's `login_url` and
  `LoopbackLogin::url` take the provider.
