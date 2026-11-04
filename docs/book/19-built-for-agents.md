# Built for Humans and Their Agents

Watch an LLM agent debug a crash-looping app with `relish`. It runs `relish
status`, gets a table with columns lined up for a human eye, and guesses where
one column ends and the next begins. It runs `relish wtf`, sees exit code 2,
and decides it typed the command wrong, because in most CLIs 2 means a usage
error. (In `wtf` before 0.2.2 it meant "warnings".) It runs `relish` with no
arguments to look around and gets a TUI it can't see. None of this is the
agent being stupid. We wrote every one of those interfaces for a person at a
terminal, and the agent isn't one.

0.2.7 fixes that without making the human interface worse. Most of the
release is a family of commands, `relish agent`, that answer the same
questions in a shape a model can rely on: where am I, what's wrong, what does
the code at this version actually do, does my fix work. The reasoning is in
the [agent research note](../plans/2026-11-04-research-agent-friendly.md) and
the order of the work in the [0.2.7 plan](../plans/2026-11-04-plan-0.2.7.md).

Like Chapter 18, this chapter grows with the release. Each issue fills in its
part as it lands. The first is the one everything else stands on: the output
contract (#1565).

## One envelope for every answer

The contract fits in four rules. Every `relish agent` command prints exactly
one line of JSON on stdout. It never prompts, never opens a TUI and never reads
stdin. It stays under a byte budget. And its exit code says whether it
answered, never what the answer was.

The JSON is always the same envelope:

```json
{"command":"version","data":{...},"next":[],"ok":true,
 "relish":{"commit":"137370e9...","dirty":null,"source_digest":null,"version":"0.2.7"},
 "schema":"relish.agent/v1","truncated":false,"warnings":[]}
```

`schema` names the contract's version, so a `v2` can't surprise anyone.
`relish` says which build answered, which matters more than it looks: later in
0.2.7 an agent reads the source this exact binary was built from, and it needs
to know it's looking at the right commit. `data` is the answer. `next` is a
list of commands worth running next, each tagged `read_only`, so an agent can
tell "look at the logs" from "roll back the deploy" without parsing English.

A failure swaps `data`, `truncated`, `next` and `warnings` for one `error`
object, with a `code` from a closed list: `usage`, `no_endpoint`,
`unreachable`, `dns`, `tls_untrusted`, `timeout`, `unauthenticated`,
`forbidden`, `not_found`, `conflict`, `api`, `source_mismatch`, `build_failed`,
`unsupported_platform` and `internal`. Closed matters. An agent can branch on
`unauthenticated` ("ask for a token") versus `unreachable` ("is the cluster
up?") only if the list doesn't quietly grow a new spelling.

Even clap's usage errors come out as an envelope. `relish agent version
--bogus` prints a failure with the `usage` code on stdout and exits 2. For
every other command, clap's message still goes to stderr as before.

## The Rust: four ways to write an enum

The envelope is a handful of serde types in
`src/relish/agent/envelope.rs`. The interesting part is how a Rust enum turns
into JSON, because serde gives you four choices and we use three of them.

Start with the error code. It's a plain enum with no data in its variants:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Usage,
    NoEndpoint,
    Unreachable,
    // ...
    Internal,
}
```

A variant with no data serialises as its name, a string. `rename_all =
"snake_case"` turns `NoEndpoint` into `"no_endpoint"`, so Rust keeps its
naming convention and JSON gets its own. If you're coming from Go, this is
roughly a `string` type with a set of `const` values, except the compiler
knows the set is closed: a `match` on `ErrorCode` that forgets a variant
doesn't compile. That's how `exit_status` maps every code to an exit status
with no default branch to hide a new one.

Now the envelope itself. An answer and a failure have different fields, and
the natural Rust for "one of two shapes" is an enum whose variants carry data:

```rust
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Outcome<T> {
    Answered { data: T, truncated: bool, next: Vec<NextStep>, warnings: Vec<String> },
    Failed { error: AgentError },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Envelope<T> {
    schema: SchemaVersion,
    command: CommandName,
    ok: bool,
    relish: BuildInfo,
    #[serde(flatten)]
    outcome: Outcome<T>,
}
```

`<T>` makes both types generic over the command's `data`, as Go's type
parameters do: `Envelope<VersionData>` for `version`, `Envelope<SchemaData>`
for `schema`. Unlike Go, Rust compiles a separate copy of the code for each
`T` it's used with, so there's no boxing or interface call at run time.

serde's default for an enum with data is *externally tagged*: the variant's
name wraps its fields, `{"Answered": {"data": ...}}`. *Internally tagged*
(`#[serde(tag = "kind")]`) puts the name inside, `{"kind": "Answered",
"data": ...}`. *Adjacently tagged* (`tag` plus `content`) splits it into two
keys. And *untagged*, which we use, writes the variant's fields with no name
at all. `#[serde(flatten)]` on the field then lifts those fields up into the
envelope, so `data` sits next to `ok` instead of inside an `outcome` object.

Why not internal tagging on `ok`? It's the obvious idea: `ok: true` is the
answer, `ok: false` the failure, so let `ok` be the tag. serde's tags are
strings, though, and `ok` is a boolean. So `ok` is an ordinary field, and the
constructors keep it honest. The fields are private (no `pub`), which in Rust
means code outside the module can't build an `Envelope` with a struct literal.
The only ways in are two functions:

```rust
impl<T> Envelope<T> {
    pub fn answered(command: CommandName, relish: BuildInfo, data: T) -> Self { ... }
    pub fn failed(command: CommandName, relish: BuildInfo, error: AgentError) -> Self { ... }
}
```

`impl<T> Envelope<T>` reads "for any `T`, here are methods on `Envelope<T>`".
`Self` is the type being implemented. Neither function lets you pair `ok: true`
with an `error`. In Go you'd get the same guarantee from unexported fields and
a `New...` function; Python can only ask nicely.

The externally tagged default comes back later. #1083 wraps text a workload
controls, such as log lines, in an object marked as untrusted:
`{"untrusted": {"origin": "workload_log", "lines": [...]}}`. That's exactly
what serde writes for a variant `Untrusted { origin, lines }` of an enum with
no tagging attribute, so `data` types can carry it without the envelope
knowing anything about it.

One more enum is worth a look, because it has a single variant:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub enum SchemaVersion {
    #[serde(rename = "relish.agent/v1")]
    V1,
}
```

A `const` string would print the same thing. The enum does better in the
schema, which says the field is exactly `"relish.agent/v1"` rather than any
string.

## Why the exit code isn't the verdict

`relish wtf` exits 1 when it finds a problem and 3 for warnings. That's right
for a shell script: `relish wtf || page-someone` works. It's wrong for an
agent, which reads a non-zero exit as "the command failed" and goes looking
for the mistake in its own command line. Worse, the agent can't tell "the
cluster has problems" from "I couldn't reach the cluster", and those need
opposite next steps.

So `relish agent` commands split the two questions. The exit code answers
"did the command answer?", and the answer, however bad, lives in `data`:

| Exit | Meaning |
|------|---------|
| 0 | answered |
| 2 | usage error, as clap's own |
| 3 | couldn't reach or authenticate to the cluster |
| 4 | the cluster refused (403, a policy) |
| 5 | anything else |

`relish agent diagnose`, when it lands, exits 0 on a cluster with three
critical findings. We didn't change `wtf`'s codes to match; scripts depend on
them, and the plan defers that question to before 1.0.

The mapping from errors the rest of `relish` already returns is one `From`
implementation. `impl From<&RelishError> for AgentError` teaches Rust how to
make an `AgentError` from a borrowed `RelishError`, so `AgentError::from(&e)`
works anywhere:

```rust
RelishError::ApiError { status, .. } => {
    let code = match status {
        401 => ErrorCode::Unauthenticated,
        403 => ErrorCode::Forbidden,
        404 => ErrorCode::NotFound,
        409 => ErrorCode::Conflict,
        _ => ErrorCode::Api,
    };
    // ...
}
```

The `..` in the pattern means "and other fields I don't care about". Two codes,
`dns` and `tls_untrusted`, have no source yet: the HTTP client folds both into
"unreachable", and #1567 splits them.

## Why a byte budget

An agent pays for every byte it reads, in tokens and in context it can't use
for anything else. Claude Code warns when an MCP tool's output passes 10,000
tokens ([docs](https://code.claude.com/docs/en/mcp)). One `relish logs` on a chatty app can blow past that. So every
`relish agent` command has a budget, 24 KiB by default (about 6,000 tokens),
and `--max-bytes` to change it.

Cutting the output at 24 KiB would be easy and useless: half a JSON document
is a parse error. The budget in `src/relish/agent/budget.rs` trims the *data*
instead, and keeps the document whole:

1. Serialise the envelope to a `serde_json::Value`, a tree we can edit.
2. If it fits, print it.
3. Otherwise, find the biggest non-empty array in `data` and drop items from
   its end until we've saved roughly the excess. Commands put what matters at
   the front (worst findings, newest events), so the end is what goes.
4. When no array is left to trim, shorten the longest string and end it with
   `…[cut]`.
5. Then drop the command's own `next` steps and warnings from the end.
6. If it still doesn't fit, give up honestly: a `usage` failure whose hint
   names the budget that would.

Objects never get trimmed. Their keys are the command's schema, and an answer
missing a required field is worse than one with fewer items. That puts a rule
on every future command: a big collection in `data` must be an array, not a
map.

A trimmed answer says it was trimmed. `truncated` is `true`, a warning names
each cut (`/data/apps: kept 34 of 200 items`, the path being a JSON Pointer),
and the first `next` step is the same command line with a `--max-bytes` big
enough for everything. The command line comes from `std::env::args()`, with
`--max-bytes` and `--token` taken out. Echoing a secret into the output an
agent stores in its transcript would be a fine way to leak it.

Two details are worth pointing out. The first is strings. Rust's `String` is
UTF-8, and `truncate` panics if you ask it to cut through the middle of a
multi-byte character. `floor_char_boundary` steps back to the nearest
character start first:

```rust
let keep = text.floor_char_boundary(target);
text.truncate(keep);
text.push_str(STRING_CUT_MARKER);
```

C would happily give you half an `é`. Python's strings are sequences of code
points, so the problem doesn't arise, and Go's `s[:n]` hands you invalid UTF-8
without complaint.

The second is key order. The envelope's keys come out in alphabetical order:
`command`, `data`, `next`, `ok`, and so on. That's because the budget works
on `serde_json::Value`, whose objects are a `BTreeMap` unless you turn on
serde_json's `preserve_order` feature. We didn't, because features in Cargo
are global: switching it on would reorder the JSON of every other command
that goes through `Value`. The cheaper fix was to pass every envelope through
`Value`, trimmed or not, so the order is at least always the same. No JSON
reader should care.

How do we know the budget holds? A property test. proptest generates
thousands of random envelopes, with nested arrays and objects, strings full
of control characters, emoji and multi-byte letters, huge error messages,
and random budgets from 1 KiB up, and checks that every output is valid JSON
no longer than its budget. It found nothing in the final version, which is
the point. While the trimmer was a stub, it failed on the first run.

## A schema from the types

`relish agent schema` prints the JSON Schema of the envelope and of each
command's `data`. Later, `relish mcp` advertises the same schemas as each
tool's `outputSchema`. One definition, two front ends.

Writing the schema by hand would mean keeping two descriptions of the same
thing in step forever. Instead we generate it from the Rust types with
[schemars](https://docs.rs/schemars), which reads the same `#[serde(...)]`
attributes serde does. `#[derive(JsonSchema)]` next to `#[derive(Serialize)]`
is the whole cost per type. That's the one new dependency in this change.
schemars 1.2.3 was already in our `Cargo.lock`, as an optional dependency of
`byte-unit` (which openraft uses) that nothing switched on. Building it adds
schemars itself, its derive macro and `serde_derive_internals`, the piece of
serde's own derive that lets schemars read serde's attributes exactly as
serde does.

Two things surprised us. schemars describes *deserialisation* by default, and
our types only serialise, so we ask for the other contract explicitly:

```rust
fn schema_of<T: JsonSchema>() -> Value {
    schemars::generate::SchemaSettings::default()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value()
}
```

`T: JsonSchema` is a *trait bound*: this function works for any `T` that
implements `JsonSchema`, and the compiler checks it at every call. And the
`///` doc comments on each type and field become the schema's
`description`s. That's lovely, until you notice that a comment written for
the Rust reader ("the fields are private so the only ways to build one
are...") now explains Rust internals to an agent. Those notes moved into
plain `//` comments, which schemars ignores.

## Tests

The snapshot tests in `src/relish/agent/mod.rs` pin each shape: an answer, a
truncated answer, one failure for each exit class, and the envelope's
schema. A change to any of them is a change to what every agent reads, so it
should show up in review as a snapshot diff. The property test above holds
the budget. And black-box tests in `tests/suite/relish_cli.rs` run the real
binary the way an agent's tool runner does, with stdin closed and no `TERM`,
checking for one line of JSON, an empty stderr and the right exit code,
including for usage errors and a refused endpoint.

## What we chose not to do

- **No human mode for `relish agent`.** `-o` is accepted and ignored. A
  second format means a second thing to keep correct, and people already have
  every other command.
- **No pretty-printing.** One compact line is fewer tokens, and `jq` exists.
- **No new exit codes for `wtf`.** Scripts depend on them.
- **No trimming of objects or keys.** See above; a smaller answer beats a
  malformed one.
