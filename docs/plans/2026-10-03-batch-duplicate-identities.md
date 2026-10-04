# Reject duplicate batch job names

Issue: #542.

Batch dispatch and completion use job names as keys. Reject duplicate names, including across namespaces, before either public submission or node dispatch can register or start work.

The permanent regressions exercise the shared resolver, public raw HTTP submission, and authenticated internal raw HTTP dispatch. All three failed on unchanged production at train `08f2636e`; both HTTP paths accepted duplicate labels with 202 responses. Each HTTP regression also checks that rejection leaves no queued work, and public submission leaves no registered batch.

Add the name check to the shared resolver, explain the identity constraint in Chapter 8, then run the focused batch tests and `make ci`.

No wire or durable-state change.

Validation completed: focused batch tests and full offline `make ci` passed with two build/test workers and no retries or exclusions.
