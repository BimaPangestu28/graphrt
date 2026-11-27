- `graphrt serve` reads the skills of a served bundle (`.grt`), like
  `graphrt run bundle.grt`: each bundle that brings skills runs on a private
  store filled through the normal install gate, and on no other skills.
  `GET /v1/skills` lists them under `bundled`, read only, and the server
  prints each bundled skill with its grants at startup. A bundle without
  skills keeps using the server's store.
