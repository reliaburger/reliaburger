# Landing page

Static HTML and CSS for reliaburger.com. No build step, framework, external
fonts, analytics or third-party requests. Publish the contents of this directory
with the GitHub Pages workflow (`.github/workflows/static.yml`); workflow and
custom-domain configuration are managed separately.

JavaScript: two small scripts, each for one thing. `assets/copy.js` adds a
copy button beside every command snippet (`<pre>`) with a short "Copied"
confirmation announced through a live region, and makes each heading's `#`
permalink copy the section's address as well as navigate to it. It adds the
buttons itself, so without JavaScript there's no dead button: the snippets are
plain, selectable text and the permalinks are ordinary links. The Clipboard API
needs https or localhost; opened from disk the script falls back to selecting
the text and `document.execCommand("copy")`.

`assets/tour-player.js` (decision D2 in
`docs/plans/2026-09-23-zero-to-cluster.md`) plays the
tour's recording, `assets/tour.cast`, with a vendored copy of the asciinema
player in `assets/asciinema/`. It loads the player and the recording from this
site, and only when someone opens the tour; nothing is fetched from anywhere
else. With scripts disabled a `<noscript>` note links the `.cast` file, and the
written tour works as before. Don't add other scripts. The footer names both,
so change the two together.

Every `<section>` has a stable `id` (`install`, `tour`, `start`, `docs`,
`internals`, `contributing`) and every section heading a `#` permalink, shown on
hover or focus, so a section can be shared as `reliaburger.com/#install`.
Renaming an `id` breaks links people have already shared; add a new section
instead. `tests/suite/website.rs` checks the ids, the permalinks and that the
page leads with the one-line install.

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

The page leads with the one-line install now that 0.1.0 is published;
building from source is the second path. The source quickstart intentionally
matches `examples/phase-1/proc-first-run.toml`.

`install.sh` is a small HTTPS bootstrap for the versioned release installer.
Both it and the generated installer are POSIX sh, so `curl … | sh` works where
`sh` is dash or busybox, not only bash. `scripts/release/test_package.py` runs
them under every POSIX shell it finds and under `shellcheck -s sh` when present.
It fails with a release-not-published message if no release exists. The
generated installer itself lives in the
GitHub release, with native CLI checksums supplied by release packaging.
