# graphrt-provider-k8s

Kubernetes provider of the graphrt gateway: scales the agent Deployments of a
deployment between zero and N replicas over a minimal REST client (no kube-rs),
picks which replica goes away on scale-in and reports ready replicas. Works with
any conformant cluster (self-hosted, kind, k3d/k3s, GKE, EKS, AKS). Its API
follows `graphrt-provider-api` and may change between minor releases.

Part of [graphrt](https://github.com/Graphrt/graphrt) — **run your agent everywhere**: one small runtime for
agentic AI workflows on servers, containers, the edge and phones.

| Feature | Adds |
| --- | --- |
| `kubeconfig` (default) | Out-of-cluster credentials from kubeconfig files (token, token file, client certificate, exec plugins such as `aws eks get-token`) |
| `deploy` (default) | `DeployDriver` for `graphrt deploy --target k8s`: server-side apply of Deployment, Service and Secret |
| `fake-api-server` | In-process fake Kubernetes API server for the conformance suite (test-only) |

A gateway running in the cluster only needs its service account:
`--no-default-features` leaves out kubeconfig support and the deploy driver.

- Guide: [Kubernetes provider](https://github.com/Graphrt/graphrt/blob/main/docs/kubernetes-provider.md)
- Project README: <https://github.com/Graphrt/graphrt#readme>

Licensed under the Apache License, Version 2.0.
