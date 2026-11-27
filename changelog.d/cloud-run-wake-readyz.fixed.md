- The Cloud Run provider wakes an agent with `GET /readyz` instead of
  `/healthz`: Cloud Run's front end reserves `/healthz` and answers it with
  its own 404, so a wake never reached the container (found on real Cloud Run).
