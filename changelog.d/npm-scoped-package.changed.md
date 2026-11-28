- The browser package is published to npm as `@graphrt-dev/graphrt`, not
  `graphrt`, which npm refused as too close to `graphql`. Install it with
  `npm install @graphrt-dev/graphrt` and import `@graphrt-dev/graphrt/client`,
  `/ui`, `/chat`, `/monitor` and `/console`. `web/scripts/first-publish.sh
  --rename v0.6.3` publishes 0.6.3 from its tag under the new name.
