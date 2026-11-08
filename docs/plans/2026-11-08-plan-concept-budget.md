# Plan: a concept budget for what an agent has to learn

8 November 2026. The concept-budget part of
[#1681](https://github.com/reliaburger/reliaburger/issues/1681) ("the whole
platform fits in your agent's context"), in the
[0.2.8 milestone](https://github.com/reliaburger/reliaburger/milestone/30).
The design note is the
[comment on #1681](https://github.com/reliaburger/reliaburger/issues/1681#issuecomment-6716386522).
The rest of #1681, tokens and time per gym task against Kubernetes from the
comparison harness ([#1679](https://github.com/reliaburger/reliaburger/issues/1679))
and the numbers on the website, stays open there.

## What we're building

A test that counts the concepts an agent has to learn before it can drive
Reliaburger, compares the counts with a checked-in budget, and fails when
they move. Growth needs a deliberate edit to the budget, with a reason, in
the same pull request. Shrinkage asks for the budget to come down, so it
ratchets.

## What counts

| Count | Read from | Rule |
|---|---|---|
| Resource kinds | serde's field list for `config::Config`, the type `relish apply` parses | Every top-level table: `app`, `job`, `namespace`, `permission`, `policy`, `build`, `template`, `use`. Kubernetes YAML is imported into these, so it adds none. Node config isn't applied, so it doesn't count |
| Commands | `Cli::command()` through clap's `CommandFactory` | Every subcommand at every depth. `relish` itself and clap's generated `help` don't count. Hidden commands do: hiding one mustn't be a way under the budget, and agents still meet them in docs and errors |
| Flags | the same tree | Every named argument (with a long or short name) on every command, hidden and global ones included. A global flag counts once, where it's declared. Positionals and clap's generated `--help` and `--version` don't count |
| Distinct flag names | the same tree | Unique long names (short-only flags by their short name). `--namespace` on thirty commands is one idea, not thirty |

## Where the budget lives

[`docs/testing/concept-budget.json`](../testing/concept-budget.json), one
entry per count, each with its `budget` and the `reason` for that value. It
sits beside `manual-owner-policy.json`, the other checked-in file a test
holds the code to.

## How it ratchets

- Measured over budget: the test fails, names the count, and says to raise
  the budget in the JSON file in the same pull request, with a reason.
- Measured under budget: the test fails and asks for the budget to be
  lowered to the measured number.
- Equal: it passes. A budget is therefore always the number we ship, and
  every change to it is a reviewed diff.
- An empty `reason` fails too.

## Build order (tests first)

1. Library module `relish::concept_budget`: count a clap `Command` tree,
   read a serde struct's field names, parse the budget file and compare.
   Unit tests on a small hand-built `Command` and a test struct: nested and
   hidden commands count, `help` doesn't, globals count once, positionals
   don't, distinct names fold repeats, and the comparison's three outcomes
   with their messages.
2. The binary test `the_concept_counts_match_their_budget` in
   `src/bin/relish.rs`, wiring the real `Cli` and `Config` to the file.
3. The budget file with today's counts.
4. Docs: a section in [docs/testing.md](../testing.md) and one in chapter 19
   of the book. The manual's agent chapter is compiled into
   `relish agent guide`, so it stays as it is.

## `relish agent guide`

Its 64 KiB default budget already has a test,
`the_agent_chapter_fits_the_guides_default_budget` in
`src/relish/agent/guide.rs`. The docs point at it; nothing new duplicates it.

## Out of scope here

- Tokens and time per gym task against Kubernetes (#1679's harness).
- Showing the counts on the website.
- A budget on the fields inside each resource kind, or on flag values.
