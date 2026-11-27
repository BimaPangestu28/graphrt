- Model provider keys and `GRAPHRT_API_KEY` read from the environment lose
  surrounding whitespace. A key mounted from a file (Cloud Run Secret Manager,
  AWS Secrets Manager, Kubernetes secrets) usually ends with a newline, which
  made every model call fail with `model_network: builder error`.
