# Physical image storage quota (#540)

The registry-wide maximum must count compressed blob files and in-flight upload and repository receipt bytes, including internal replication, cache fills and runtime image pulls. Catalogue totals cannot bound bare uploads or concurrent temporary files.

1. First reproduce repeated bare uploads and oversized chunks against a small configured ceiling through the existing HTTP router.
2. Introduce one shared, node-local byte budget for all writers of the selected image directory. Reserve before writing, reconcile confirmed writes/removals, reconstruct usage on startup, and move reservations with upload renames. A failed cleanup must not free occupied capacity. Charge atomic receipt replacements and reconstruct receipt usage at startup; bound payload-file counts so empty uploads and tiny receipts cannot grow without limit.
3. Inject the same BlobStore into the runtime image cache, retain logical per-repository policy separately, and test concurrent reservations, deduplication, cancellation/deletion and restart usage.
4. Update chapter 5 and the image manual. Run portable CI and the relevant image/registry gates.

The accounting is reconstructed from local disk. It introduces no wire or durable schema fields; the configuration now bounds compressed CAS, temporary upload and repository receipt payload bytes rather than logical manifest descriptor totals. Unpacked root filesystems, catalogue files and filesystem block allocation overhead are outside this payload-byte budget.

The shared helper checks file and containing-directory syncs. Explicit parent-entry syncing for newly created digest-directory ancestors is assigned to the final lifecycle hardening (#555); this quota child does not claim to close that power-loss window.
