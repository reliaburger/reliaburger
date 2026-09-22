# SREday 2026 keynote

Slides for the talk that introduces Reliaburger 0.1.0 at SREday. It covers ten
years of Kubernetes scars, what building an orchestrator with Claude and Codex
was actually like, and a closer look at Mustard (gossip) and Onion (eBPF
service discovery).

`build.js` is the source of truth. It writes `reliaburger-sreday.pptx` with
speaker notes on every slide. The `.pptx` is generated, so it isn't committed.

```sh
cd docs/talks/sreday-2026
npm install
npm run build
```

Import the result into Google Slides with File → Import slides, or open it in
Keynote or PowerPoint.

The dashed "meme goes here" boxes are placeholders. Each slide's notes suggest
a meme, and the notes say `[CHECK: ...]` wherever a number needs refreshing on
the day (line and commit counts, the month Codex joined).

Technical claims on the Mustard and Onion slides were checked against the code
in `src/mustard/`, `src/council/`, `src/cluster/runtime.rs` and
`ebpf/onion_connect.bpf.c`, not just the design docs.
