# Plan: built for small local models

Issue [#1687](https://github.com/reliaburger/reliaburger/issues/1687), milestone
0.2.8 ("agents you can trust"). It builds on the agent gym
([#1086](https://github.com/reliaburger/reliaburger/issues/1086), `scripts/gym/`),
the Claude Code solver of the 0.2.7 exit test
([#1093](https://github.com/reliaburger/reliaburger/issues/1093)) and `relish mcp`.

## What it's for

Some teams won't send their cluster's state to a model API. Logs carry
customer data, an incident channel is private, or the rules say nothing
leaves the building. They can still have an agent, if it runs on their own
machine. So far every agent number we publish is Claude's. This plan asks
the same questions of an open model small enough for a laptop: does it find
the fault, does it stay safe, and where does a small context window get in
the way?

The issue asks for four things:

1. run the gym with an open local model (through Ollama) as well as Claude,
   and publish the results;
2. tune budgets and replies where small context windows struggle;
3. document running `relish mcp` with a local model, for privacy-minded teams;
4. design and plan first, here.

## How the gym drives a local model

A new solver, `scripts/gym/local_model.py`, plugs into the gym's existing
`--solver command:PROGRAM` seam, like `claude_code.py`:

```sh
GYM_LOCAL_MODEL=qwen3:8b python3 scripts/gym/gym.py run \
  --solver command:scripts/gym/local_model.py --runs 3 --check-patch --out DIR
```

It's the agent loop itself, about 500 lines of standard-library Python:

- It starts `relish mcp` through the gym's recording `relish`, as Claude Code
  starts the plugin's server, speaks MCP to it over stdio (initialise, list
  the tools, call them) and offers every tool to the model as an Ollama
  function tool, the MCP input schema as its parameters.
- It adds one more tool, `relish`, which runs a single `relish` command from
  an array of arguments (no shell). That's the counterpart of Claude Code's
  `Bash(relish *)`, so both solvers can reach the same commands, including
  the ones `relish mcp` leaves out on purpose (`agent report`, `agent verify`).
  It refuses `relish mcp` and `relish agent patch build|test|try`, as the
  Claude Code solver does.
- It sends the conversation to Ollama's `/api/chat` with the tools, runs
  every tool call in the reply, returns the results as `tool` messages, and
  stops when a reply asks for nothing. If that reply holds no JSON answer, or
  the turns run out, it asks once more with no tools and the answer's JSON
  Schema as Ollama's `format`, which forces structured output.
- Everything goes through the recording shim, so the scorer judges it from
  the same call log and audit log as every other solver. The token is the
  same read-only one.

### Why our own loop

We looked at three other ways and chose against each:

- **Claude Code pointed at Ollama.** Ollama serves an Anthropic-compatible
  API, so `claude` could run a local model. But Claude Code's own system
  prompt and tool definitions are written for Claude and take a large share
  of a small window before our prompt arrives, and a failure would say as
  much about that mismatch as about Reliaburger. We'd be measuring the wrong
  thing.
- **An off-the-shelf MCP client for Ollama** (`ollmcp`, Goose, Open WebUI).
  Each adds a dependency with its own release cadence, and we'd have to dig
  the per-turn token counts we want to publish out of each one. The manual names them for teams
  that want a chat window; the gym needs a loop it can see into.
- **A framework** (LangChain, LlamaIndex). Far more than a 30-turn loop needs,
  and the gym is stdlib-only by rule, so `make test-ci-scripts` runs anywhere.

The loop being ours also means the measurement is honest about what it
isn't: a production agent would add a better system prompt, retries and
summarisation. We add none, so the numbers are a floor.

## Which models

Ollama 0.40.15 from Homebrew on the Mac Studio (M5 Max, 128 GiB).

- **First: Qwen3 8B** (`qwen3:8b`, Q4_K_M, 5.2 GB, 40,960-token window). It
  calls tools in Ollama, it's Apache-2.0, and it runs on a 16 GB laptop,
  which is the machine a privacy-minded engineer actually has. Thinking is
  off by default (`GYM_LOCAL_THINK=1` turns it on): it multiplies the time
  per turn, and we want the plain model first.
- **Next, in a later run:** a 14B model (Qwen3 14B), a different family at the
  same size (Llama 3.1 8B or Granite), and an open model of about 20B
  (`gpt-oss:20b`), so the record shows whether a failure is the size or the
  family. Each one is pulled only for its run, and the record names its digest.

## What's measured

Per scenario, the gym's usual distribution: runs, verdicts (correct,
partial, wrong, no answer, unsafe), each check, mutating calls and audit
records under the solver's token, and where injected text went. The solver
adds, per session, from Ollama's own counts:

- tokens in (`prompt_eval_count` summed over turns) and out (`eval_count`);
- the largest single prompt, against the window (`num_ctx`);
- turns whose prompt filled the window. Ollama then drops the start of the
  conversation without saying so, and the start holds the task;
- turns, the tools called, and seconds.

The record gets a "Tokens and context" table from these (`record.py`). Cost
is zero and the record says it's a local model. The model's digest,
parameter count, quantisation and the Ollama version go in every run's
`solver-meta.json`.

Claude's numbers come from the [0.2.7 exit
test](../qualification/2026-11-06-v0.2.7-exit-test.md), on an earlier commit.
Rerunning Claude on the same commit is a Studio slot and API spend, and it's
the next run, not this one.

## Where small windows struggle

Measured on `relish` 0.2.8 at `044e38d`:

| What | Bytes | About |
|---|---|---|
| `relish agent guide`, the whole manual chapter | 67,833 | 17,000 tokens |
| `relish agent schema`, every command's output schema | 37,749 | 9,400 tokens |
| `relish mcp`'s 14 tools: names, descriptions and input schemas | 11,402 | 2,900 tokens |
| The same tools' output schemas (`relish mcp --list`) | about 80,000 | not sent to the model by our loop |
| Default budget of every other `relish agent` answer | 24,576 | 6,000 tokens |

(Tokens at about four bytes each; the record has Ollama's exact counts.)

Three things follow:

- **The guide doesn't fit an 8K or 16K window**, and on 32K it leaves little
  for anything else. A model that reads it first, as the skill tells Claude
  to, has spent its window before the cluster.
- **Three answers at the default budget fill a 16K window.** The history is
  sent again every turn, so a ten-turn session at 32K sends hundreds of
  thousands of tokens in, even when no single prompt is large.
- **The tool list is fixed overhead**, about 3,000 tokens on every turn.

The first run uses a 32K window and the defaults, to see what a model does
untuned. A second run at 8K shows the struggle directly.

## What to tune

In order, each only if the runs show it matters, and each with its test
first:

1. **A server-wide budget: `relish mcp --max-bytes N`.** Every tool's default
   `max_bytes` becomes N unless the call names one, so a client with a small
   window sets it once. Today the model has to know to pass `max_bytes`
   itself. The solver already tries this from the outside
   (`GYM_LOCAL_MAX_BYTES`), so the gym can show whether it helps before we
   ship it.
2. **A guide that answers a small budget with its table of contents** and the
   `manual_show` section ids, instead of trimming the chapter's tail, so the
   first call teaches the model where to look.
3. **Shorter tool descriptions,** if the fixed overhead turns out to matter.
4. **A small-model profile in the manual:** `--toolsets` and `--no-logs`
   choices that keep the list short.

Each change reruns the same scenarios at the same window, and the record
shows before and after.

### What the first runs changed

The [first record](../qualification/2026-11-09-gym-local-models.md) (Qwen3
8B, three passes) confirms item 1 and adds four more, in this order:

1. **The server-wide budget stays first, as the default the schema
   advertises.** The model sent `max_bytes` in 80 of 202 calls, copying
   schema defaults and preferring the largest (up to 1,048,576). Advertising
   8,192 from the solver moved 68 of 76 budgeted calls to it. So `relish mcp
   --max-bytes N` sets the advertised default as well as the fallback, and a
   ceiling is worth deciding on: nothing stops a client asking for a megabyte.
2. **`next` steps that lead into the source.** A small model follows every
   one; on the unschedulable deploy it read a 3,300-line file after
   `diagnose` had the answer. The order and wording of `next` for a finding
   that already names the fault get a look, with the gym as the test.
3. **The gym's apps in the source bundle.** `src/relish/gym/catalogue.rs`
   holds every scenario's apps and the injected log line, and the model found
   it with `source_grep`. The bundle, or the gym's scenarios, need a way to
   keep the answer key out of the solver's reach. Done: the scenarios moved
   to `scripts/gym/relish-scenarios.rs`, outside the bundle, which `build.rs`
   includes when it's there, and a test fails if any bundled file holds a
   prompt, an app's log line, the canary or a planted secret from either gym.
4. **The commands a small model won't type.** It used the `relish` tool once
   in 18 sessions that needed it. Proposals are MCP tools under `relish mcp
   --propose`; a later pass runs the proposal scenarios with it, and the
   manual says so for small models.
5. **The task as a system message** cost nothing and keeps the task when
   Ollama trims; the manual recommends it to anyone writing their own loop.

## How results are published

- **The record:** `docs/qualification/2026-11-09-gym-local-models.md`, with the
  date, the model and its digest, the Ollama and `relish` versions, the
  host, the settings, every scenario's results and the failures, written
  from the gym's own `record.md` and read by a person. Later runs get their
  own dated records.
- **The website:** the showcase ([#1690](https://github.com/reliaburger/reliaburger/issues/1690))
  quotes the local-model numbers beside Claude's on `/agents`, from the
  record, with the same test the Kubernetes comparison page has: every
  number on the page must be in the record it names.
- **The manual:** a section in "Working with an AI agent" on running `relish
  mcp` with a local model, and the book's chapter 19 on how the solver works
  and what it found.

## Studio time

A model run loads the host like a soak, so it takes a slot of its own on
the Studio by day and stops before the nightly qualification (21:30 London).
Ollama runs as a foreground `ollama serve` for the run, not a service.

## Tests

- `scripts/gym/test_local_model.py`, in `make test-ci-scripts`: a fake Ollama
  (an HTTP server in the test) and a fake `relish mcp` drive the solver
  through the real runner and shim. A tool call and its answer, the tools
  offered, the token and window counts, an answer asked for again as
  structured output, the turn limit, an unknown tool, thinking off by
  default, the `relish` tool's refusals and cut, and the record's tokens
  table.
- The real runs are the integration test; no CI job calls a model.

## Order

1. This plan, the solver and its tests, and the first record: every scenario
   at 32K, at 8K, and at 8K with the outside tunes (this PR, Part of #1687).
2. The tuning changes above, inside `relish`, each with the gym rerun.
3. The second and third models.
4. The website numbers, with the showcase.
