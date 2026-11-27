- `graphrt-files`: `S3Store`, a `FileStore` over S3 and S3 compatible object
  stores (MinIO, Cloudflare R2, Ceph, Garage, SeaweedFS), behind the new `s3`
  feature. Five signed requests over `reqwest` with the SigV4 signer and the
  credential chain of `graphrt-provider-aws`: no AWS SDK and no new crate in
  the lock file. Keys never leave the configured prefix, listings only show
  valid workspace paths, redirects are not followed, `http://` endpoints must
  be allowed explicitly, and creation without overwrite uses
  `If-None-Match: *`. `StoreError::Config` reports a store that cannot be set
  up as asked.
- `--workspace s3://bucket/prefix` in CLI builds with the opt in `files-s3`
  feature, configured by `GRAPHRT_S3_ENDPOINT`, `GRAPHRT_S3_REGION`,
  `GRAPHRT_S3_PATH_STYLE`, `GRAPHRT_S3_ALLOW_HTTP` and the usual AWS
  credential variables and roles. See `docs/files-and-images.md`.
