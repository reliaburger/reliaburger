# Competitive review and comparison pages

Base: `release-0-2-1` at `32effd96e2a349d5a0533aa8a51c22b1054338d0`.

## Scope

Write a dated, source-linked review of the requested projects and adjacent alternatives.
Add five primary comparison pages (Kubernetes, k3s, Nomad, Coolify and Uncloud)
and Docker Compose as a sixth baseline. Cover concept mappings, installation,
application operations, platform upgrades, learning burden and ideas worth adapting.
Keep the full review and adaptation recommendations in
`docs/research/2026-10-15-competitor-review.md`. Public comparison pages foreground
Reliaburger’s strengths and sourced trade-offs, with evidence boundaries in
`docs/website/compare/methodology.html`. There is no website build step.

## Order and evidence

1. Inspect pinned project source and current official documentation. Distinguish
   released Reliaburger 0.1.6, implementation on this train, and planned features.
2. Write the review and parallel comparison pages. Preserve the homepage visual
   language and existing section anchors; link all pages from a new `compare` section.
3. Check local links and anchors, parse Reliaburger command examples with the real
   CLI, inspect desktop and narrow-screen rendering, and run the existing `make ci`.
4. Open the local preview and submit a PR into `release-0-2-1`.

This is documentation, not a performance experiment. No benchmark, production
reliability or security ranking may be inferred from source inspection alone.
No subsystem behaviour changes, so the book's implementation chapters are unchanged.
