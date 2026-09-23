# Reliaburger — Contributing Guide

Thanks for helping build Reliaburger. This repository is both a working `Rust` project and the source for *Building Reliaburger*, so a useful contribution usually improves the implementation, its tests, or the explanation around it.

## Before You Start

Read the project [documentation](docs/README.md), [roadmap](docs/roadmap.md), and [implementation progress](docs/progress.md) before starting substantial work. The roadmap is organised into phases, and we generally work through those phases in order.

For a new feature or a change to public behaviour:

1. Open or discuss an issue describing the problem and the proposed behaviour
2. Identify the owning subsystem and the relevant design document
3. Add or update tests before implementing the behaviour
4. Keep the change focused. Avoid unrelated refactors or speculative abstractions

Small fixes, documentation improvements, and tests can go straight to a pull request when the intended change is clear.

## Development Setup

You need a current `Rust` toolchain and Cargo. The project uses `Rust` edition 2024. Some ignored acceptance suites additionally require Linux, runc, Buildah, network namespaces, eBPF support, or Apple Container; portable checks should not depend on those tools.

Build the binaries with:

```sh
cargo build --bins
```

The repository's `Makefile` contains the supported development commands. Run `make help` to see them all.

## Tests First

Tests are part of the design, not a final inspection step. Start with a failing test that describes the behaviour you want, then implement the smallest change that makes it pass.

Use the narrowest relevant test while developing, then run the portable checks before opening a pull request:

```sh
make test                 # portable nextest suite
make test-doc             # `Rust` documentation tests
make fmt-check            # formatting check
make lint                 # Clippy, all features and none, warnings as errors
```

`make ci` runs the standard portable CI checks together. Use the specialised targets for ignored acceptance suites when your environment supports them: `make test-linux`, `make test-cluster`, `make test-upgrade`, and `make test-apple`.

Tests should describe behaviour in their names, cover failure and boundary cases, and avoid relying on timing or external services unless the test is an explicit integration or acceptance test. Unit tests belong beside the code; cross-subsystem tests belong in `tests/`.

## `Rust` And Code Style

Follow the existing `Rust` style and let `rustfmt` make the final formatting decision. Prefer clear, idiomatic code over cleverness:

- Borrow inputs where ownership is unnecessary, and use `Path`/`PathBuf` for filesystem paths
- Use typed state, identifiers, and errors instead of sentinel values or stringly-typed APIs
- Use `thiserror` for library errors and `anyhow` with context at binary or CLI boundaries
- Use Tokio synchronisation and async I/O in asynchronous code; do not block the runtime
- Avoid panics in production code. `unwrap` and similar shortcuts belong only in tests or in cases with a documented, provable invariant
- Add public API documentation and explain why for non-obvious comments

Run `make fmt` locally after editing Rust. Do not introduce unrelated formatting churn.

## Documentation And The Book

Documentation is part of the feature. Update the relevant design document, `docs/progress.md`, user documentation, or book chapter when a change affectsarchitecture, behaviour, configuration, commands, or the current roadmap.

Book chapters are written for programmers who may know C, Python, or Go but not Rust. When adding `Rust` syntax to a chapter for the first time explain it in plain language. Include the design reasoning, tests, trade-offs, and lessons learned rather than documenting only the finished code.

New example configurations go under the appropriate `examples/phase-N/` directory. Use the runtime prefix that matches the example: `proc-*`,
`container-*`, `apple-*`, or `runc-*`. Validate examples with:

```sh
cargo nextest run --test examples
```

`make test` runs it too.

## Pull Requests

A pull request should be small enough to review and should explain:

- what problem it solves and what behaviour changed;
- which tests or checks you ran, including any environment-specific limits;
- which documentation, design, or book material was updated;
- any follow-up work that is deliberately out of scope.

Keep commits focused and use clear commit messages. Do not commit generated build artefacts, credentials, cluster data, or local machine configuration.

Reviewers will look first for correctness, tests, compatibility, security, operational failure modes, and documentation. A contribution can be modest in size; it still needs to be understandable and verifiable.

## Developer Certificate of Origin

Reliaburger uses the [Developer Certificate of Origin](https://developercertificate.org/) (DCO). By contributing, you certify that you have the right to submit the work under the project's licence and agree to the DCO terms.

Sign off each commit by adding a `Signed-off-by` line with your name and email:

```sh
git commit -s
```

For existing commits, use `git commit --amend -s` or rebase as appropriate. The sign-off must match the contributor identity, and pull requests must contain a sign-off for every commit.

## Questions And Security

For design questions, open an issue with the relevant subsystem and a concrete example. Do not disclose security vulnerabilities in a public issue. Report them privately to the maintainers with enough detail to reproduce and assess the impact.

## End
