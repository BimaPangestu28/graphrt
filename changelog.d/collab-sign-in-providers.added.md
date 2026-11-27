- `graphrt collab`: `auth.providers` signs people in with several identity
  providers, each a button in the app (Google and Microsoft on the hosted
  server). A provider whose `client_id_env` is empty is off until it is set.
  `GET /v1/info` lists them and `GET /v1/auth/login?provider=<id>` picks one.
  `email_verified_claim` keeps only emails the provider vouches for
  (Microsoft: `xms_edov`). The client's `login_url` and `LoopbackLogin::url`
  take the provider.
- OpenID Connect sign in (console and collab) accepts Microsoft's multi tenant
  issuers (`common`, `organizations`, `consumers`) and checks each ID token
  against the issuer of its tenant (`tid`).
