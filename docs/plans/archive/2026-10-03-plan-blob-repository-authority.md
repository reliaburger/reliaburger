# Repository authority for shared blob bytes (#531)

A digest identifies content, not permission to publish or read it. Scoped publication must prove every configuration, layer and index child belongs to the named repository, even when the physical CAS already holds it for another namespace.

1. First reproduce scoped manifest publication and a HEAD that incorrectly skips the required destination upload.
2. Persist upload evidence for the exact repository and lease generation before acknowledging successful digest verification. Accept existing authoritative catalogue references or completed destination uploads. Require the same evidence for scoped blob probes and reads.
3. Delete evidence with collected/corrupt blobs; remove only the retiring repository generation's evidence while preserving shared content and other owners.
4. Exercise configuration/layer/index descriptors, successful normal pushes, durable restart reuse, rejected digest completion and exact-generation retirement. Update chapter 5 and run portable CI and the standard-client gate.

Local blob directories gain repository upload receipts. Missing receipts confer no authority, so bare blobs uploaded under the old format must be uploaded again before scoped publication. Existing catalogue references continue to provide authority. Receipt identities include leased ownership generations. There are no wire struct changes; state generation 51, shared with the review train's other durable fixes, covers these stricter local authority semantics.
