# Landing page

Static HTML and CSS for reliaburger.com. No build step, framework, external
fonts, analytics or third-party requests. Publish the contents of this directory
with the GitHub Pages workflow (`.github/workflows/static.yml`); workflow and
custom-domain configuration are managed separately.

JavaScript: one script, for one thing (decision D2 in
`docs/plans/2026-09-23-zero-to-cluster.md`). `assets/tour-player.js` plays the
tour's recording, `assets/tour.cast`, with a vendored copy of the asciinema
player in `assets/asciinema/`. It loads the player and the recording from this
site, and only when someone opens the tour; nothing is fetched from anywhere
else. With scripts disabled a `<noscript>` note links the `.cast` file, and the
written tour works as before. Don't add other scripts. The footer says the same,
so change both together.

The player is asciinema-player 3.17.0 (Apache 2.0, `assets/asciinema/LICENSE`),
the `dist/bundle/asciinema-player.min.js` and `asciinema-player.css` files from
the npm tarball
`https://registry.npmjs.org/asciinema-player/-/asciinema-player-3.17.0.tgz`
(integrity `sha512-JbjNJmA2TLIeYNaOEja+kVSzXadKoqpIzVVmfBGNj2DmdtE/vExBCnkE8NYEcpaQcDvUYg8Ltt0urT80frACmw==`).
The bundle is self-contained: no fonts, workers or other files to fetch. To
upgrade, replace those three files from a newer tarball and check the
recording still plays.

### Re-recording the tour

`scripts/demo/tour.sh` runs the tour for real: it reads the commands from this
page's `data-tour` elements, types each one, runs it and waits on the
cluster's real state where the tour says to wait. `tests/suite/website.rs`
runs `tour.sh --check`, which fails if the page gains a command the script
doesn't know how to run. To record, with `bun` and `relish` for Linux in
`target/tour-bins` (built `--release --features ebpf`), the host `relish` on
`PATH` or in `RELISH`, and no other quickstart cluster on the default ports:

```sh
RELIABURGER_HOME=~/.rbtour scripts/demo/tour.sh \
  --record docs/website/assets/tour.cast --setup target/tour-bins
RELIABURGER_HOME=~/.rbtour relish local destroy --yes
```

`--record` runs the tour inside `asciinema rec` (110x32, idle time cut to two
seconds) and then plays the setup step four times faster, because setup redraws
its timers several times a second and idle trimming can't shorten it. The
narration says both, and every wait prints how long it really took. Until the
release is published, the recording shows `relish setup --quickstart
--development-binaries` where the tour says `curl … | sh`; the script says so on
screen. It applies the demo URL when it answers, and
`examples/kubernetes/podinfo.yaml` (with a note) when it doesn't.
`asciinema play docs/website/assets/tour.cast` plays the result in a terminal.

The "Try it in five minutes" tour is a `<details>` element, so it opens and
closes without script. Each command in it carries a `data-tour` attribute.
`tests/suite/website.rs` parses every one of those with the real `relish`
command-line definition, and does the same for the manual's copy of the tour
(`docs/manual/08_five-minute-tour.md`), so edit both together.
`scripts/ci/select-jobs.sh` treats this page as code for that reason.

The tour applies `https://reliaburger.com/demo/podinfo.yaml`. That file isn't
committed here: the Pages workflow copies `examples/kubernetes/podinfo.yaml`
into `demo/` at deploy time, so the site always serves the manifest CI tests.
To preview it locally, copy it the same way (`demo/` is ignored by version control).

Preview from the repository root:

```sh
python3 -m http.server 8080 --bind 127.0.0.1 --directory docs/website
```

Open `http://127.0.0.1:8080/`. Asset paths are relative so the page also works
under a GitHub project-site path.

GitHub is the source of truth for documentation. The whitepaper PDF link goes
to the Build & Release workflow, whose `reliaburger-pdfs` artefact includes
`reliaburger-whitepaper.pdf`. There was no latest published release available
when this page was created. Once a release includes that asset, the link can
point to `https://github.com/reliaburger/reliaburger/releases/latest/download/reliaburger-whitepaper.pdf`.
Until then, do not advertise that download URL as working. Build artefacts
require GitHub sign-in and have limited retention.

Update the release-status paragraph, and remove the tour's "Arrives with 0.1.0"
label, when the installer actually ships. The
source quickstart intentionally matches `examples/phase-1/proc-first-run.toml`.

`install.sh` is a small HTTPS bootstrap for the versioned release installer.
Both it and the generated installer are POSIX sh, so `curl … | sh` works where
`sh` is dash or busybox, not only bash. `scripts/release/test_package.py` runs
them under every POSIX shell it finds and under `shellcheck -s sh` when present.
It intentionally fails with a release-not-published message until v0.1.0 exists.
The page labels this path as pending; remove that label only after published
candidate qualification passes. The generated installer itself lives in the
GitHub release, with native CLI checksums supplied by release packaging.
