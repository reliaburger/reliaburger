# Code audit evidence, 17 September 2026

Baseline: `fc11d253cf081d8eb9ef8f41389c2f98a0000545` on
`codex/v0.1.0-release`, [PR #165](https://github.com/reliaburger/reliaburger/pull/165).
Audit branch: `codex/codebase-completion-audit`.
[Findings and completion tests](../plans/archive/2026-09-17-codebase-completion-plan.md)
use C/H/F/V IDs; [progress](../progress.md) owns their state.

This audit changes documentation. Temporary observation tests were run against
the unchanged implementation and then removed. They intentionally assert observed
bad behaviour, so “3 passed” below means three defects were observed. They are
not regressions asserting the correct behaviour and must not be added to CI as-is.

## Export observations: C01, C02, C04

Run from the repository root after building the debug Relish binary. This script
uses a fresh temporary home and local directories. It makes no cluster calls.
The files contain arbitrary bytes because the exporter copies files; these probes
exercise archive identity and error handling, not Parquet decoding.

```python
import os,pathlib,tempfile,subprocess,json
root=pathlib.Path(tempfile.mkdtemp(prefix='rb-audit-export-'));src=root/'source';src.mkdir();a=root/'archive-a';b=root/'archive-b'
env=os.environ.copy()
for k in ['RELIABURGER_ENDPOINT','RELIABURGER_TOKEN','RELIABURGER_CA_CERT']:env.pop(k,None)
env['RELIABURGER_HOME']=str(root/'home')
binary=str(pathlib.Path('target/debug/relish').resolve())
def run(dest):
 r=subprocess.run([binary,'logs-export','--source',str(src),'--dest',str(dest)],env=env,capture_output=True,text=True,timeout=20)
 return {'exit':r.returncode,'stdout':r.stdout.strip(),'stderr':r.stderr.strip()}
f=src/'logs_000000.parquet';f.write_bytes(b'first batch');print('first',json.dumps(run(a)))
f.unlink();f.write_bytes(b'second batch');print('reused_name',json.dumps(run(a)));print('archive_contents',[(p.name,p.read_text()) for p in a.rglob('*.parquet')])
print('second_destination',json.dumps(run(b)));print('second_destination_files',list(b.rglob('*.parquet')))
f.unlink();f.mkdir();print('unreadable_parquet_entry',json.dumps(run(a)))
print('root',str(root))
```

Observed results:

| Step | CLI result | Filesystem evidence |
|---|---|---|
| Export first generation to A | Exit 0, one file | First generation present |
| Replace the source file and export to A | Exit 0, one file | Only the second generation remains under the same archive name |
| Export the same source to fresh destination B | Exit 0, “no new files to export” | B has no Parquet files |
| Replace source file with a directory ending in `.parquet` | Exit 0, “no new files to export” | Read failure silently discarded |

C03's in-place checkpoint write, swallowed agent save errors and concurrency
problem were established from source, not a power-loss test. C22's checkpoint
history growth was also source-inspected. Those completion tests remain future work.

## Duration overflow: C17

Using a temporary `RELIABURGER_HOME` and removing endpoint/token/CA overrides:

```sh
relish logs web --since 18446744073709551615d
```

The debug binary exited **101** with `attempt to multiply with overflow` at
`src/relish/commands.rs:194`. The parser computes `amount * multiplier` before
`saturating_sub`; the latter cannot protect the former. The source also implies
wrapping under ordinary release overflow settings, but no release-mode reproduction
was run here. The earlier release-plan assertion that this multiplication was
checked has been corrected.

## Certificate and port observations: C13, C16, C18

The following was temporarily saved as `tests/audit_observations.rs` and run with:

```sh
cargo test --test audit_observations -- --nocapture
```

```rust
// Temporary audit probes. These assert the observed defects, not desired behaviour.
use reliaburger::sesame::{ca, types::SerialNumber};

#[test]
fn observe_short_certificate_lifetime_quantisation() {
    let root = ca::generate_root_ca("audit", SerialNumber(1)).unwrap();
    let (der, _, _) = ca::issue_end_entity_cert(
        "audit.example", SerialNumber(2), std::time::Duration::from_secs(60),
        &["audit.example".into()], &[rcgen::ExtendedKeyUsagePurpose::ServerAuth],
        &root.signing_keypair, &root.certificate_params,
    ).unwrap();
    let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
    let span = cert.validity().not_after.timestamp() - cert.validity().not_before.timestamp();
    println!("requested_seconds=60 actual_validity_seconds={span}");
    assert_ne!(span, 60);
}

#[test]
fn observe_non_ascii_san_panics_in_a_result_returning_api() {
    let root = ca::generate_root_ca("audit", SerialNumber(1)).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ca::issue_end_entity_cert("audit.example", SerialNumber(2), std::time::Duration::from_secs(60),
            &["caf\u{e9}.example".into()], &[rcgen::ExtendedKeyUsagePurpose::ServerAuth],
            &root.signing_keypair, &root.certificate_params)
    }));
    assert!(result.is_err());
    println!("non_ascii_san_panicked=true");
}

#[tokio::test]
async fn observe_allocator_reports_exhausted_with_a_free_port() {
    let allocator = reliaburger::grill::port::PortAllocator::new(10000,60000);
    for port in 10000..59999 { allocator.reserve(port).await.unwrap(); }
    let mut refusals = 0;
    for _ in 0..32 {
        match allocator.allocate().await {
            Ok(port) => allocator.release(port).await.unwrap(),
            Err(_) => refusals += 1,
        }
    }
    println!("one_free_port=true premature_exhaustion_count={refusals}/32");
    assert!(refusals > 0);
}
```

Observed output:

```text
requested_seconds=60 actual_validity_seconds=0
non_ascii_san_panicked=true
one_free_port=true premature_exhaustion_count=32/32
3 passed; 0 failed; 0 ignored; finished in 0.08s
```

Certificate issuance rounds both endpoints to calendar dates, so the short leaf's
validity collapsed. The probe's span depends on whether it crosses midnight;
precise timestamps should be tested with an injected clock in the actual fix.
The invalid SAN panic was caught deliberately. This proves a library API panic,
not that an unauthenticated remote caller can reach it through TLS admission.
The CA/key pair existed only in memory and no key material was printed.

The port result is stochastic because the allocator tries random candidates.
One free port remains in a 50,000-port pool; all 32 audit attempts incorrectly
reported exhaustion. The regression should use a deterministic invariant or
controlled randomness, not depend on reproducing that exact count.

The compiler also emitted the existing macOS large `__eh_frame` linker warning.
The probes compiled and completed; no source or test file from them remains in
the audit branch.

## Hosted baseline evidence

The base PR's checks were retrieved again during this audit. All applicable jobs
passed on `fc11d25`:

- [Source CI run](https://github.com/reliaburger/reliaburger/actions/runs/35182538622):
  portable Linux/macOS, dependency audit, privileged Linux (including the workflow's
  rootless lane), multi-node cluster, single-node and cluster upgrade, wall-clock
  acceptance, coverage, fast/large benchmarks and 10k-member scale acceptance.
- [Build and packaging run](https://github.com/reliaburger/reliaburger/actions/runs/35182538716):
  four native platform artefacts, packaging tests and PDF build. Tag-gated
  `release` and `validate-release` were skipped.

The existing local release work also recorded 3,366 default and 3,326 no-default
passing nextest tests, 43 skips in each configuration, and clean all-target Clippy.
Those full suites were not rerun for this documentation-only audit. The prior
no-default run reported one passing-but-leaky test without identifying it under
the selected output filter; H08 keeps that investigation open. The doctest command
had zero tests, which is why H07 remains open despite a successful command.

The [laptop record](2026-09-17-laptop.md) contains the earlier three-VM development
qualification and its exclusions. No new destructive cluster run, signed public
release install, sustained renewal test or full real catalogue run was performed
for this audit. V01–V04 remain unchecked.

## How to close a finding

First replace an observation with a failing test of the required behaviour. After
the fix, retain that test and the relevant runtime/failure-injection evidence.
Record the commit and exact build in progress; update the owning book chapter.
Do not change a verdict to “passed” merely because missing evidence is inconvenient.


Documentation verification: all 72 plan IDs match unique unchecked entries in
progress; all 136 relative Markdown links checked across the changed documents
resolve to existing paths. `cargo fmt` and `git diff --check` pass. These checks
validate the audit artefacts, not the unimplemented completion tests.


## Dependency alert discovered during publication of this audit

GitHub's push response reported an open moderate dependency vulnerability. A
read-only query of [Dependabot alert #13](https://github.com/reliaburger/reliaburger/security/dependabot/13)
returned `open`, package `thrift`, GHSA-2f9f-gq7v-9h6m / CVE-2026-43868,
versions `< 0.23.0`, first patched version `0.23.0`. The alert was created and last
updated on 24 July 2026. Its title is “Apache Thrift has a Memory Allocation with
Excessive Size Value Vulnerability”.

`cargo tree --locked --offline -i thrift` confirms an active compiled path:

```text
thrift 0.17.0 → parquet 54.3.1 → datafusion 45.0.0 → reliaburger
```

This does not prove the affected parser is reachable through a public Reliaburger
endpoint. H12 retains that analysis and dependency remediation as a publication
prerequisite. The recorded RustSec pass and five exceptions remain accurate;
neither closes this separate GitHub advisory. No dependency was modified or
alert dismissed during the documentation audit.
