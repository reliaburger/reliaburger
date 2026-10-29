# Dependency exception review, 28 October 2026

This adds one exception to the
[26 October review](2026-10-26-dependency-exceptions.md), for 0.2.3 only. The
other exceptions are unchanged, and so is the review deadline, **18 November
2026**.

RustSec published
[RUSTSEC-2026-0414](https://rustsec.org/advisories/RUSTSEC-2026-0414) against
`pulldown-cmark` 0.13.4 on the morning of 28 October (advisory-db commit
[`40f5be6f`](https://github.com/rustsec/advisory-db/commit/40f5be6fb12ed93fc591bf4ab23b7d793b3f2239),
08:50 GMT). The dependency audit has failed on every branch since then, first
on PR #1406
([run 40660897706](https://github.com/reliaburger/reliaburger/actions/runs/40660897706)).

0.2.3's candidate, `d387d569`, was built on 26 October and locks 0.13.4. By
then it had passed every gate tier, and its 24-hour day run was most of the way
through. The maintainer decided on 28 October to ship that candidate with this
exception rather than build a new one and run the gate again. The fix is
[#1409](https://github.com/reliaburger/reliaburger/pull/1409) on `main` and
[#1408](https://github.com/reliaburger/reliaburger/pull/1408) on the 0.2.4
train: both move to 0.13.5, which changes only `Cargo.lock`.

## Why 0.2.3 isn't exposed

The advisory is a denial of service. With `Options::ENABLE_WIKILINKS` set, a
wikilink such as `[[a|]]` makes the parser's output grow exponentially, so a
few kilobytes of untrusted Markdown can exhaust memory. Parsing without that
option isn't affected.

Reliaburger parses Markdown in one place: the manual renderer, for `relish
manual` and the manual's web pages (`src/relish/manual/render.rs`,
`src/relish/manual/web.rs`). Both use `MARKDOWN_OPTIONS`, which is
`Options::ENABLE_TABLES` alone. The Markdown it parses is the manual,
compiled into the binary, not anything from a user or a peer. So the
vulnerable code path is never enabled, and the input is never untrusted.

## New exception

| Advisory | Kind | Reachability and disposition |
| --- | --- | --- |
| [RUSTSEC-2026-0414](https://rustsec.org/advisories/RUSTSEC-2026-0414), pulldown-cmark 0.13.4: exponential event growth with `ENABLE_WIKILINKS` | Vulnerability (denial of service) | Not reachable: Reliaburger never sets `ENABLE_WIKILINKS`, and parses only its own built-in manual. Shipped in 0.2.3; fixed on `main` by #1409, so no audit ignore is added. |

0.2.2 and 0.2.1 lock the same version, with the same options, and aren't
exposed either.

## Reproduce the review

```sh
git show d387d569:Cargo.lock | grep -A1 'name = "pulldown-cmark"'
git grep -n "ENABLE_\|Options::all" d387d569 -- src
```
