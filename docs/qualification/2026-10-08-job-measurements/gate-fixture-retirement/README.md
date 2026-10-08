# Final gate fixture retirement

The persistent single-node throughput/recovery fixture retained its original
kernel links after Bun and all applications stopped. The final Linux gate's
standalone routing test refused a connection while these links were attached.

`proof.json` records all 291 original runtime intents in the retired phase,
no matching owner processes, and the four pinned links belonging to the exact
original kernel manifest. After this proof, the existing
`OnionEbpf::retire_owned_state` API retired that owner.
`retired-kernel-owner.json` preserves its terminal manifest. No original runtime
or kernel journal was deleted. The unchanged standalone routing test passed.

The subsequent focused namespace-adoption extension correctly refused two
synthetic backends directly published by that test without discovery owners.
Its fixture now explicitly removes only those two known keys after runtime
retirement before testing adoption. The product's refusal remains unchanged.
See the [qualification narrative](../../2026-10-07-reusable-job-executors.md)
for the full gate results.
