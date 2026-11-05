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

## Carrying the source

[Chapter 13](13-a-room-with-a-view.md#shipping-the-source) put `src/` into the
binary with `rust-embed`, one deflated file at a time, 3.9 MB in all. It's a
nice thing to browse. It's no use to an agent that wants to fix a bug, because
`src/` can't be built: there's no `Cargo.toml`, no `Cargo.lock`, no `build.rs`,
no eBPF C programs, no dashboard assets, none of the files the code pulls in
with `include_str!`, and no tests to prove a fix with.

So the question for
[#1570](https://github.com/reliaburger/reliaburger/issues/1570) was: what's the
smallest honest thing to embed, and how do we know it's complete?

### What goes in

The answer is three sets, named in one list, `ALLOWLIST` in
`src/relish/source_bundle/pack.rs`:

- **The build set**: everything `cargo build --bins` reads. `Cargo.toml`,
  `Cargo.lock`, `build.rs`, `clippy.toml`, `LICENSE`, `src/`, `ebpf/`,
  `benches/` (Cargo refuses a manifest whose declared benchmarks are missing,
  even for a plain build), `brioche/dist/`, `docs/manual/`, `examples/`, and
  the four files under `scripts/release/` that the code includes.
- **The test set**: `tests/`, `proptest-regressions/`, the `Makefile` and
  `.config/` (nextest's configuration). An agent that patches a function should
  run that function's tests.
- **The read set**: `CLAUDE.md`, the design docs, the book, the whitepaper,
  the roadmap, the release log and the testing, releasing and flakes docs. The
  reasoning behind the code, at the same version as the code.

What stays out is just as deliberate: 34 MB of qualification evidence, the
website, the talks, the plans. Everything in the tree would be 9.6 MB
compressed; the three sets are 4.7 MB.

An allowlist rots. Someone adds an `include_str!` of a file nobody thought to
list, and the next release carries source that doesn't build. We found out how
easily on the first try: a grep for `include_str!("` found every include on one
line, and missed the one in `src/relish/quickstart/artifacts.rs` that rustfmt
had split across two. Building from the extracted bundle failed within thirty
seconds:

```
error: couldn't read `src/relish/quickstart/../../../scripts/release/guest-images.json`
```

That's the check we keep. A CI job, `source bundle builds`, extracts the bundle
from a freshly built `relish` into an empty directory and runs `cargo build
--locked --bins` there. If the build needs a file the list leaves out, CI says
so before a user does.

### One tarball, not a thousand files

`rust-embed` compressed each file on its own. That throws away most of what a
compressor is good at, because source files repeat each other: the same
imports, the same derive lists, the same test boilerplate. Pack the files into
one tar archive first and compress the archive, and the compressor sees all of
it at once. Measured on the 0.2.7 tree:

| What | Raw | Compressed |
|---|---:|---:|
| `src/` alone, per-file deflate (the old embed) | 17.1 MB | 3.9 MB |
| The three sets, one tar, zstd level 3 | 27.3 MB | 6.7 MB |
| The three sets, one tar, zstd level 19 | 27.3 MB | 4.7 MB |

So the release binary carries 60% more source in a bundle 0.8 MB *larger*
than the old one, under 1% of a 100 MB `relish`. Level 19 takes about eight
seconds on a laptop, fine for a release build; level 3 takes a fraction of a
second, which matters for the builds you do fifty times a day. We'll come back
to those.

The `zstd` crate was already in `Cargo.lock`, pulled in by Arrow's IPC code
through DataFusion, and so were `tar` and `sha2`. The bundle adds no new crate
to the dependency graph.

### Build scripts

If you've written C, you know the step before compiling: `./configure`, or a
Makefile rule that generates a header. Cargo's version is `build.rs`. When a
package has one, Cargo compiles it into a small program, runs it on the build
machine, and only then compiles the crate. Reliaburger's `build.rs` already
compiled the eBPF programs with clang and recorded the git commit. Now it packs
the source too.

A build script talks to Cargo by printing lines to standard output. Three
kinds matter here:

```rust
println!("cargo:rerun-if-changed=src");
println!("cargo:rustc-env=RELIABURGER_SOURCE_DIGEST={}", pack::digest(&tar));
println!("cargo:rustc-cfg=source_bundle_embedded");
```

`rerun-if-changed` tells Cargo when to run the script again. Without any such
line, Cargo reruns it whenever any file in the package changes; with one, it
watches only what you name, and a directory is watched recursively.
`rustc-env` sets an environment variable for the compiler, which the crate
reads at compile time with the `env!` macro: `env!("RELIABURGER_SOURCE_DIGEST")`
becomes a `&'static str` baked into the binary, and a missing variable is a
compile error, not a runtime surprise. `rustc-cfg` turns on a configuration
flag, like passing `-DSOURCE_BUNDLE_EMBEDDED` to a C compiler, which the crate
tests with `#[cfg(source_bundle_embedded)]`. Since Rust 1.80 the compiler warns
about cfg names it doesn't know, so the script also declares it with
`cargo:rustc-check-cfg=cfg(source_bundle_embedded)`.

Where does the script put the tarball? Not in the source tree: a build must
never write into the code it's building. Cargo gives every build script a
private scratch directory and passes its path in the `OUT_DIR` environment
variable. The script writes `$OUT_DIR/source.tar.zst`, and the crate picks it
up with `include_bytes!`:

```rust
#[cfg(source_bundle_embedded)]
static BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/source.tar.zst"));
```

Three macros, all expanded at compile time. `env!("OUT_DIR")` is the
directory; `concat!` glues string literals together, the way adjacent string
literals join in C; and `include_bytes!` reads the file into the binary as a
`&'static [u8; N]`, a fixed-size byte array that lives as long as the program.
There's no file to open at run time and nothing to go missing. It's the same
idea as `xxd -i` generating a C array, without the generated file. A `static`,
rather than a `const`, matters for something this big: a `const` is inlined
wherever it's used, while a `static` is one value at one address.

One wrinkle: a build script can't use the crate it builds, since the crate
doesn't exist yet. But the packing code needs unit tests, and tests live in the
crate. The way out is the `#[path]` attribute, which tells the compiler where
a module's file is:

```rust
// build.rs
#[path = "src/relish/source_bundle/pack.rs"]
mod pack;
```

The same file is a module of the library too, so `build.rs` and the tests run
the same code. The cost is a rule written at the top of `pack.rs`: it may use
only the standard library and crates that are both build dependencies and
normal ones. That's why `Cargo.toml` grows a `[build-dependencies]` section
listing `tar`, `zstd` and `sha2` again.

### Deterministic tarballs

The bundle needs an identity. `relish agent source version` (#1571) reports
it, the release metadata will record it, and an agent comparing a node's
commit with its `relish` needs to know two bundles are the same. So two builds
of the same tree must produce the same bytes, and a tar archive doesn't do that
by default. Each entry header records the file's modification time, its owner
and group, its permissions, and the order the files came in, which on most
filesystems is whatever order the directory happens to list them in.

`pack::write_tar` pins each of those down:

```rust
let mut header = tar::Header::new_gnu();
header.set_entry_type(tar::EntryType::Regular);
header.set_size(data.len() as u64);
header.set_mode(if is_executable(&full)? { 0o755 } else { 0o644 });
header.set_uid(0);
header.set_gid(0);
header.set_mtime(MTIME);
builder.append_data(&mut header, path, data.as_slice())?;
```

The paths arrive sorted, the time is the Unix epoch, the owner is root, and the
mode is one of two values: the executable bit survives, because the test
scripts need it, and nothing else does. Directory entries aren't written at
all; extraction creates parent directories as it goes. A path longer than the
100 bytes a tar header has room for gets a GNU long-name entry, which the
`tar` crate writes with a fixed time of its own. The `?` after
`is_executable(&full)` works inside a conditional expression like anywhere
else: if reading the file's metadata fails, the whole function returns the
error.

The digest is the SHA-256 of the *uncompressed* tar. Hashing the compressed
bytes would make a debug build and a release build of the same commit
disagree, and tie the identity to one zstd version's output. The tar is the
source; the compression is packaging.

Which files go in? In a git checkout, `git ls-files --cached --others
--exclude-standard` lists them: tracked files plus untracked ones `.gitignore`
doesn't exclude. That keeps out the Go demo's compiled binary that sits in
`examples/` after a tour, and `.DS_Store` files, without a second copy of the
ignore rules. Outside a checkout, an extracted bundle for instance, there's no
git to ask, so `pack::list_files` walks the directories. The CI job checks the
two agree: the `relish` built from the extracted tree must report the same
digest as the one it was extracted from.

`build.rs` also records `RELIABURGER_SOURCE_DIRTY`: whether `git status
--porcelain` lists anything in the allowlisted paths. A local build with an
uncommitted change embeds the change and says so. A release build must not, so
the release workflow sets `RELIABURGER_REQUIRE_CLEAN_SOURCE=1` and `build.rs`
panics if git lists a change, or can't say. A panic is how a build script fails
a build; the message lands in Cargo's error output.

### Reading it back: zstd streams

At run time, `source_bundle::tarball()` turns the embedded bytes back into a
tar archive:

```rust
zstd::stream::decode_all(BUNDLE).map_err(SourceBundleError::Read)
```

`decode_all` is a convenience over the crate's streaming `Decoder`, and the
streaming shape is worth a look, because it's how Rust does I/O generally. A
byte slice, `&[u8]`, implements the standard `Read` trait, so it can be read
like a file. `zstd::stream::read::Decoder::new(reader)` wraps any `Read` and
is itself a `Read` that yields decompressed bytes, and `tar::Archive::new`
wraps any `Read` and yields entries. Each adaptor owns the one inside it, so
the chain is a single value you can move around, much like a Unix pipe
`zstd -d | tar -t` built out of types. A trait here is Rust's version of a Go
interface, except that the compiler generates specialised code for each
concrete type in the chain instead of dispatching through a table at run time.

We decompress the whole archive into memory and then read the tar out of it
for a duller reason: a build that doesn't embed the bundle (more on that next)
produces the same uncompressed bytes by packing the checkout, and both paths
then share one tar reader. 27 MB in memory, for a command a human or an agent
runs on purpose, is fine.

Extraction goes through the same reader. `extract_from` refuses a directory
that isn't empty, and leans on `tar::Entry::unpack_in`, which refuses any entry
whose path would land outside the target directory, an absolute path or one
climbing out with `..`. We wrote the archive ourselves, but checking costs
nothing, and `relish agent source extract` (next section) builds on this
function.

### What it cost the inner loop

The research note recommended measuring the warm-build cost before committing
to embedding everywhere, and offered a fallback if it hurt. It hurt. Here's a
warm debug `cargo build --bin relish` on an Apple-silicon laptop, after
touching one file:

| Touch | Before | Bundle in every build | Final |
|---|---:|---:|---:|
| A file in `src/` | 17.6 s | 20.0 s | 10–16 s |
| A book chapter or a test | 0.5 s | 21.1 s | 0.3 s |

The first row is tolerable: the crate recompiles anyway, and packing at level 3
adds a couple of seconds. The second isn't. Watching the allowlist means every
edit to the book or to an integration test reruns the build script, which
changes the embedded bytes, which recompiles the whole crate and relinks every
test binary. In a project whose rules say "update the book in the same
change", that's most edits.

So only release builds embed the bundle, plus any build with the
`embed-source` feature. Everything else skips the packing in `build.rs`,
watches nothing new, and has `source_bundle` pack the checkout it was built in
at run time, from the path in `env!("CARGO_MANIFEST_DIR")`. The `Origin` enum
says which happened:

```rust
pub enum Origin {
    /// Compiled into the binary.
    Embedded,
    /// Packed at run time from the checkout this binary was built in.
    Checkout(PathBuf),
}
```

Each public function picks its body with `#[cfg]` on a block:

```rust
pub fn digest() -> Result<String, SourceBundleError> {
    #[cfg(source_bundle_embedded)]
    {
        Ok(env!("RELIABURGER_SOURCE_DIGEST").to_string())
    }
    #[cfg(not(source_bundle_embedded))]
    {
        Ok(pack::digest(&tarball()?))
    }
}
```

Unlike C's `#ifdef`, the compiler still parses both blocks, but only one is
compiled, and since a block is an expression, whichever survives is the
function's return value. A debug `relish` copied to another machine loses its
checkout, and says so with an error that names the two ways to embed the
bundle.

The trade is that a debug build's tests exercise the checkout path, not the
embedded one. The CI job covers the gap: it builds with
`--features embed-source`, runs the bundle's tests against the embedded copy,
then extracts it and builds the extracted tree with the feature on, which runs
the packing a second time in a directory with no git. Clippy's all-features
pass covers both sides of every `#[cfg]`.

### The tests

The tests in `src/relish/source_bundle.rs` come in two kinds. The first kind
runs against whatever this build carries:

- `bundle_carries_the_manifest_lockfile_build_script_and_entry_points`: the
  files without which nothing builds, plus one from each of `ebpf/`,
  `docs/manual/` and `tests/`.
- `a_file_read_back_equals_the_checkouts`: byte for byte, including this test's
  own file.
- `paths_are_repository_relative_and_inside_the_allowlist`: `src/lib.rs`, never
  `lib.rs` or `/home/...`, and nothing the list doesn't name. The old embed's
  paths were relative to `src/`, which made every path an agent quoted subtly
  wrong.
- `the_digest_is_the_tarballs`: the recorded digest matches the bytes.
- `the_bundle_stays_under_six_megabytes_at_release_level`: a debug build
  compresses at level 3, so the test compresses the tarball at 19 itself and
  checks what a release would embed. The failure message says what to do: drop
  something from the list, or raise the ceiling on purpose.

The second kind packs a small tree in a temporary directory, so the result is
known exactly: two packs of the same files give the same digest and the same
compressed bytes even after a file is rewritten with a new modification time;
a walk lists sorted paths and skips `.DS_Store`; a missing allowlisted path
fails the pack instead of quietly shipping less; extraction reproduces the
tree, keeps the executable bit, and refuses a directory with something in it.

One more knob. Build scripts and their dependencies compile unoptimised by
default, even in a release build, so `build.rs` would have run level 19 through
an unoptimised `zstd-sys` (the C library under the `zstd` crate), and so would
the size test. `Cargo.toml` names the package in
`[profile.*.package.zstd-sys]` to optimise it everywhere, the same trick it
already used for `sha2` and `ring`. The size test takes about eight seconds.

### What we didn't do

- **Embed everything.** 9.6 MB for qualification evidence that no agent needs
  to fix a bug.
- **Keep `rust-embed` for the source.** It's still how the manual, the examples
  and the dashboard's assets get in, where per-file access matters more than
  the last megabyte. For the source, one archive is both smaller and the shape
  extraction wants.
- **Put the bundle in `bun` too.** Nodes run `bun`, and `relish` already knows
  which commit each one runs. A mismatch warning in #1571, plus GitHub's tree at
  the node's commit, covers it for 0.2.7.
- **Record the digest in `candidate.json` yet.** The build jobs run on
  separate runners per platform and the candidate job doesn't run the
  binaries, so carrying the digest across needs its own change to the release
  workflow. It's on #1570's list.

### Lessons

The allowlist was wrong on the first try, and a grep would never have found
out. The only check that means anything is the one the claim makes: take the
bundle out of the binary, put it somewhere empty, and build it. Everything else
in this section is in service of making that check cheap and that build
reproducible.

## Reading the source

A bundle nobody can read is just weight. [#1571](https://github.com/reliaburger/reliaburger/issues/1571)
gives it five commands, all under `relish agent source`, all speaking the
envelope from the start of this chapter:

| Command | Answers |
|---|---|
| `version` | What the bundle is (commit, `dirty`, digest, file count) and which commit each node runs |
| `tree [prefix] [--depth N]` | Files with their size and line count, optionally rolled up per directory |
| `grep PATTERN [--path GLOB]` | Matching lines with context and a permalink each |
| `show PATH[:FROM-TO]` | Up to 400 numbered lines, plus the `fn` or `impl` they sit inside |
| `extract DIR` | The whole tree, written out as a git repository |

Why not just tell the agent to `git clone` the repository? Because the agent
then has to guess which commit to check out, and it'll guess `main`. The
bug it's chasing is in whatever the cluster runs, which is rarely `main`.
`relish` knows exactly which code it was built from; asking it removes the
guess.

That shifts the question rather than answering it, though. The binary knows
its own commit. Does the *cluster* run that commit?

### Is this the code the nodes run?

An agent reading `src/bun/scheduler.rs` to explain why a node placed an app
where it did is only right if the node runs that file. So every `source`
command asks first. There's no cluster-wide versions endpoint, but `relish
wtf` already shows a builds row, and it gets it the way we do now: list the
nodes through the entry node, then ask each one for `/v1/version` through the
same entry node, all at once.

```rust
let answers = futures_util::future::join_all(up.iter().map(|node| async move {
    match client.via_node(&node.node_id) {
        Ok(node_client) => ask(&node_client).await,
        Err(error) => Err(error.to_string()),
    }
}))
.await;
```

`async move { ... }` is an *async block*: an expression whose value is a
future, a computation that hasn't run yet, rather than the result. (`move`
copies the `node` reference into the block, so the future doesn't borrow a
loop variable that's about to change.) `map` turns the list of nodes into a
list of those futures and `join_all` runs them concurrently, finishing when
the last one does. Nothing runs before the final `.await`: a Rust future is
inert until something polls it, unlike a JavaScript promise or a goroutine,
which start the moment you create them. Each request also sits inside a
two-second `tokio::time::timeout`, because the check is a courtesy on top of
the answer and mustn't hold the answer up.

What comes back is an enum, serialised with `#[serde(tag = "state")]` as in
[Chapter 1](01-hello-container.md):

```rust
pub enum ClusterSource {
    Offline,
    Unreachable { reason: String },
    Checked { nodes: Vec<NodeSource> },
}
```

`Unreachable` isn't an error. With no cluster up, the source is still this
binary's and `grep` still works; the envelope says `ok: true` and
`cluster.state` says why there's no comparison. A node on a different commit
isn't an error either. It becomes a warning on *every* `source` answer, not
just `version`'s, because an agent that went straight to `grep` needs to see
it too:

```
source_mismatch: this relish was built from a1b2c3d, but node-2 runs 3fcb1fd
(https://github.com/reliaburger/reliaburger/tree/3fcb1fd); this source may not
be the code those nodes run
```

The link is the useful bit: GitHub's tree at the node's commit is the code the
node actually runs. Carrying the bundle in `bun` too would avoid the
round trip to GitHub, and we decided against it in the previous section.

Two choices here were about trust. The comparison treats an unknown commit,
on either side, as a mismatch: nothing proves they match, so we don't say
they do. And nothing is cached. A node upgraded a minute ago must not be
reported as the old build, and two seconds is cheap next to an agent spending
twenty minutes reasoning about the wrong code. `--offline` skips the question
entirely, which is what CI wants.

### Permalinks, and `?` on an `Option`

Each `grep` match and each `show` range carries a GitHub permalink, so a human
reading the agent's explanation can click through to the same lines. Only
when that's honest, though:

```rust
fn permalink(&self, path: &str, anchor: &str) -> Option<String> {
    let commit = self.build.commit.as_deref()?;
    (self.build.dirty == Some(false))
        .then(|| format!("{REPOSITORY_URL}/blob/{commit}/{path}#{anchor}"))
}
```

You've seen `?` return early on an `Err`. It works on `Option` too, in a
function that returns `Option`: if the build doesn't know its commit,
`as_deref()?` returns `None` from `permalink` on the spot. `bool::then` turns
`true` into `Some(closure())` and `false` into `None`. So a build with
uncommitted changes, or one that couldn't ask git, gets no link at all,
because GitHub doesn't have its changes and a link to the committed version
of a line someone edited is worse than none.

### Finding the enclosing function without a parser

`show src/relish/client.rs:340-400` answers with those lines, and also with
the items they sit inside:

```json
"enclosing": [{"kind": "impl", "line": 9, "name": "impl Client"},
              {"kind": "fn", "line": 16, "name": "fetch"}]
```

That saves an agent a second call to find out whose body it's looking at. The
obvious tool is a Rust parser, the `syn` crate. We didn't use one. The bundle
holds Markdown, C, TOML and shell as well as Rust, a parser would have to
handle half-edited files in a development checkout, and the tree has a
property that makes something much simpler work: rustfmt formats every Rust
file, and CI enforces it.

So `enclosing_items` walks upwards from the first line shown, tracking an
indentation limit. A line indented less than the limit that starts an item
(`fn`, `impl`, `trait`, `mod`, past any `pub(crate)`, `async` or `unsafe`) is an
enclosing item, and the limit drops to its indentation. Any other
less-indented line, such as the closing brace of a sibling, also lowers the
limit, so a function that ended above can't be mistaken for the one we're in.
The one wrinkle is a signature rustfmt split across lines:

```rust
    pub async fn fetch(
        &self,
        path: &str,
    ) -> Result<String, Error> {
        let url = format!("http://localhost{path}");
```

Walking up from the `let`, the first less-indented line is `) -> Result...`,
which isn't an item. Lines starting with `)`, `>` or `where` continue a
signature, so they leave the limit just above their own indentation and the
walk carries on to `pub async fn fetch(`. It's a heuristic and it says so in
its tests, but on rustfmt'd code it's right, and it's thirty lines.

### Extract, and what `read_only` means

`extract` writes the tree with the same `extract_from` the CI job relied on,
adds a `.reliaburger-source.json` stamp (version, commit, `dirty`, digest),
and then makes the directory a git repository with one commit, authored as
`relish`. That last step is for the agent's next move. After it patches a
function, `git diff` shows exactly the patch, against exactly the source the
binary was built from, with no noise from a different `main`. If git isn't
installed, the tree is still written and `data.git` says `skipped` with the
reason.

The answer's `next` suggests the build:

```json
{"run": "cargo build --locked --bins --manifest-path ./src-copy/Cargo.toml",
 "why": "build every binary from this source", "read_only": false}
```

`read_only: false` for a build may look odd. Nothing in the cluster changes.
But the contract's wording is "changes nothing anywhere", and a build writes a
`target/` directory, downloads crates and runs a build script. Agents ask a
human before a step with `read_only: false`; for a multi-gigabyte build, that
seems right. `git diff`, which only reads, is `true`.

The CI job from the previous section now uses the real command. It extracts
with `relish agent source extract "$RUNNER_TEMP/source-bundle" --offline`,
builds there, extracts again from the rebuilt binary, and compares the two
digests with `jq -r .data.digest`. Reading a field from the envelope replaced a
`grep` over the hidden `relish source --extract` flag's text output, and the
flag is gone.

### The tests

The commands are plain functions over a `Bundle`, a tarball unpacked in memory
plus the build it claims, so the tests never touch the real 950-file bundle.
A fixture packs six files from a temporary directory with the same
`pack::write_tar` the build script uses, and claims a fixed commit. The
snapshots then can't drift every time the real source changes, and the digest
in them is stable.

There's a snapshot of each command's envelope: `version` with one matching
node and one on an older commit (the mismatch warning included), `tree` flat
and rolled up, a `grep` match with context, a `show` range inside a method of
an `impl`, a missing path with its close matches, and an `extract`. Unit tests
cover the rest: commits matching when one side is abbreviated and not
matching when either is unknown; no permalinks for a dirty build; a bad regex
being `usage` with a hint; a long line cut at 400 bytes; the enclosing scan
over a multi-line signature, a sibling function that ended above and a test
inside `mod tests`; `extract` refusing a directory with anything in it and
leaving a repository whose `git status` is clean.

### What we didn't do

- **A `source diff` against a node's commit.** GitHub's compare view does it,
  and the warning links the tree.
- **Search across the cluster's version.** Only this binary's source is here.
  When they differ, the warning says so instead of guessing.
- **An index.** `grep` scans 27 MB in memory in about 0.3 seconds, even in a
  debug build; an index
  would be one more thing to keep in step with the bundle.
- **Overwrite on extract.** An agent that extracts into the wrong directory
  shouldn't lose anything. `conflict` and a fresh path is cheap.

### Lessons

The interesting part of "show the agent the source" turned out not to be
showing it. It was making every answer say whether that source is the one that
matters: the commit, the `dirty` flag, the digest, the mismatch warning, a
permalink only when it's true. An agent will reason confidently about whatever
it's given. The tool's job is to make sure what it's given is labelled.
