# Reliaburger — Contributing Guide

Thanks for helping build Reliaburger. This repository is both a working `Rust` project and the source for *Building Reliaburger*, so a useful contribution usually improves the implementation, its tests, or the explanation around it.

## Before You Start

Read the project [documentation](docs/README.md) and [roadmap](docs/roadmap.md) before starting substantial work. 0.2.7 is released; the open work is in the roadmap's release list and known gaps, and in the GitHub milestones and issues they link.

For a new feature or a change to public behaviour:

1. Open or discuss an issue describing the problem and the proposed behaviour
2. Identify the owning subsystem and the relevant design document
3. Add or update tests before implementing the behaviour
4. Keep the change focused. Avoid unrelated refactors or speculative abstractions

Small fixes, documentation improvements, and tests can go straight to a pull request when the intended change is clear.

## Reporting A Bug

Open an issue with one of the [issue forms](https://github.com/reliaburger/reliaburger/issues/new/choose). Every form asks for the version, a reproducer, what you didn't test and whether an AI tool helped:

- **Bug report**: something doesn't do what it should, and you can show it.
- **Agent-drafted bug**: a draft from `relish agent report`, which your agent writes and you read, check and post. Its fields match the draft one to one.
- **Documentation error**: the manual, the book, a README or the website is wrong.
- **Test request**: behaviour no test covers, such as a surviving mutant or a flake, with the failing test attached or a plain statement that you couldn't write one.

Questions and ideas go to [Discussions](https://github.com/reliaburger/reliaburger/discussions); issues are for bugs with evidence. A report with no reproducer may be closed. If you can't reproduce something, say so: we'd rather have an honest `unverified` report than a confident guess.

Search the issues before you open one. A draft from `relish agent report` carries a fingerprint (the version, the error code and the finding id); if an issue already has it, add your evidence there.

### Labels

Triage uses these labels, and the maintainer reads `has-reproducer` and `patch-tested` reports first:

| Label | Meaning |
|---|---|
| `agent-drafted` | An agent drafted the report and a human posted it |
| `has-reproducer` | The report has a reproducer that we ran |
| `patch-tested` | The report comes with a fix and a test that fails without it |
| `unverified` | No reproducer yet; it may be closed if none turns up |
| `missed-mutant` | A mutant survived the tests, so a test is missing |

## AI Assistance

You're welcome to use AI tools, and so are we. Our own commits say so with a `Co-Authored-By` trailer naming the model. The rules are the same for everyone:

- **Disclose it.** Add a `Co-Authored-By` trailer to commits an AI tool helped write, and answer the AI-assistance question on the issue form.
- **A human answers for every line.** Read and understand everything you submit: code, tests and prose alike. "The agent wrote it" doesn't answer a review question.
- **Agents never post on their own.** An agent may draft an issue, a comment or a pull request, but a human reads it and sends it. `relish agent report` writes a draft and never posts.
- **Every bug fix carries its test.** A bug-fix pull request adds or changes a test that fails before the fix and passes after it. Say which tests you ran, and what you couldn't run.
- **Evidence over volume.** A report with no reproducer may be closed, and so may a pull request nobody has read.

If the repository's limit on open pull requests from accounts without write access stops you, open an issue and ask to be exempted.

## Development Setup

You need a current `Rust` toolchain and Cargo. The project uses `Rust` edition 2024. On Linux, `build.rs` also compiles the static job executor helpers with `cc -static`, so install a C compiler and a static libc (`build-essential` on Debian and Ubuntu, `gcc glibc-static` on Fedora). Some ignored acceptance suites additionally require Linux, runc, Buildah, network namespaces, eBPF support, or Apple Container; portable checks should not depend on those tools.

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

`make ci` runs the standard portable CI checks together, including `make check-ignored`, which fails when an `#[ignore]` reason doesn't name the target or script that runs it (see [who runs an ignored test](docs/testing.md#who-runs-an-ignored-test)). Use the specialised targets for ignored acceptance suites when your environment supports them: `make test-linux`, `make test-cluster`, `make test-upgrade`, `make test-standard-clients` and `make test-apple`.

Tests should describe behaviour in their names, cover failure and boundary cases, and avoid relying on timing or external services unless the test is an explicit integration or acceptance test. Unit tests belong beside the code; cross-subsystem tests belong in `tests/suite/` as a module of the single portable integration binary. Add a separate `tests/*.rs` binary only for gated, heavy or process-isolated suites (see `docs/design/test-harness.md`).

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

Documentation is part of the feature. Update the relevant design document, user documentation, or book chapter (and the roadmap, if the plan changes) when a change affects architecture, behaviour, configuration, commands, or the current roadmap.

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
- whether an AI tool helped, as the [AI-assistance policy](#ai-assistance) asks;
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

For design questions, start a [discussion](https://github.com/reliaburger/reliaburger/discussions) naming the subsystem, with a concrete example. Don't disclose a security vulnerability in a public issue: report it privately as [SECURITY.md](SECURITY.md) describes. There's no bug bounty.

## End
