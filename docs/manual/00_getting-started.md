# Getting started

Reliaburger is a batteries-included container orchestrator in a single binary.
One agent (`bun`) runs on every node; one CLI (`relish`) drives it. No
add-ons to install, no YAML sprawl.

## Install

The guided path detects, installs and configures everything:

```sh
relish setup
```

It finds or installs `bun` (verified against the release signatures), asks a
few questions and writes a starter `reliaburger.toml`. Pass `--yes` to accept
every default.

Building from source works too:

```sh
git clone https://github.com/reliaburger/reliaburger
cd reliaburger && cargo build --bins
```

## First run

No container runtime needed — the built-in ProcessGrill runtime supervises
plain OS processes:

```sh
bun --runtime process
```

Leave it running. In a second terminal:

```sh
relish manual examples        # drop the example configs here
relish apply examples/phase-1/proc-first-run.toml
relish status
```

You should see the `hello` workload in `Running` state. Open
<http://127.0.0.1:9117/> for the web dashboard, or run `relish` with no
arguments for the terminal one.

## Real containers

With runc (Linux) or Apple Container (macOS) installed, `bun` auto-detects
it, and the `container-*` examples pull real OCI images:

```sh
bun                            # picks runc/apple/process automatically
relish apply examples/phase-1/container-nginx.toml
```

## Where to next

- Deploy and manage an app: `deploy-an-app`
- Form a real cluster: `cluster-basics`
- Diagnose a broken cluster (`wtf`, `trace`, `bench`): `diagnostics`
- Press `/` in this manual to search; `q` quits.
