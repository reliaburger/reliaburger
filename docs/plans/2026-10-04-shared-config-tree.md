# One configuration tree for CLI and GitOps

Resolve [#548](https://github.com/reliaburger/reliaburger/issues/548) in the review2 merge train. Preserve expected-behaviour tests for defaults, directory namespaces and real unsigned/trusted-signed commits before changing production. All three failed on unchanged production in `/tmp/reliaburger-review2-root-grouped-red.log`.

Read each caller's tree into relative paths and pass it through one typed-default resolver. Keep CLI override warnings and explicit GitOps duplicate refusal, refuse partial trees and cross-namespace identity collisions, and treat the configured watch directory as the root. Update the book, deployment manual and whitepaper claims.

Focused tree/compiler/Git/signature tests and complete portable CI remain required before publication. Required GitHub checks must pass before merging this child. Job reconciliation remains a separate #549 fix; its tests and implementation are not included here.
