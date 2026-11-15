# Desired-spec evidence for dry runs

Resolve [#550](https://github.com/reliaburger/reliaburger/issues/550) in the review2 merge train. A preview must compare complete desired specifications with namespace-qualified identities. Same-image changes require an update; missing or conflicting comparison evidence requires an unknown action. Offline create previews must state their assumption, and a reachable API comparison failure must propagate.

## Implementation and proof

First preserve failing expected-behaviour regressions for same-image settings, namespace identity, the real API/client/agent path, and a live comparison error. Then add shared specification fingerprints, scoped current-resource evidence and explicit unknown/offline presentation. Advance the wire counter to protocol 36 while retaining the train's state 51. Update the deployment manual and book together.

Thirty-four focused cases passed. Formatting and both Clippy feature matrices passed. The complete two-worker, zero-retry portable run completed 5,400 cases with 5,399 passing and an unchanged managed-status fixture timing out. Its one isolated diagnostic passed; both results are recorded in the flake register under #555. Both doctests, all 52 CI script cases and ignored-test ownership passed separately. The refreshed branch also inherits autoscale target validation and duplicate batch identity refusal. Required GitHub CI must pass on the published head before this child merges. The outer train remains for maintainer review.
