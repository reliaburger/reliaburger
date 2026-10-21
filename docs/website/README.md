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
`docs/plans/archive/2026-09-23-zero-to-cluster.md`) plays the
tour's recording, `assets/tour.cast`, with a vendored copy of the asciinema
player in `assets/asciinema/`. The recording is always on show, with its
poster frame, ready to play without a click. The script loads the player and
the recording (about 1 MB) from this site when the figure comes within a screen
or so of the viewport (an `IntersectionObserver`; browsers without one load them
straight away), so a visitor who never scrolls down doesn't pay for them.
Nothing is fetched from anywhere else. With scripts disabled a `<noscript>`
note links the `.cast` file, and the written tour works as before. Don't add other scripts. The footer names both,
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
doesn't know how to run.

Every promoted release re-records the tour against its published install
([releasing step 9](../releasing.md#staging-a-candidate)). With no other
quickstart cluster on the default ports, and a throwaway `RELIABURGER_HOME`
so it can't touch yours:

```sh
RELIABURGER_HOME=~/.rbtour scripts/demo/tour.sh \
  --record docs/website/assets/tour.cast --install v0.2.1
RELIABURGER_HOME=~/.rbtour ~/.rbtour/bin/relish local destroy --yes
RELIABURGER_HOME=~/.rbtour ~/.rbtour/bin/relish uninstall --yes
rm -rf ~/.rbtour
```

`--install` runs the install line for real, with `RELIABURGER_VERSION` set so
it fetches that release even before this page's default points at it. To
record a build that isn't published yet, pass `--setup target/tour-bins`
instead, with `bun` and `relish` for Linux in `target/tour-bins` (built
`--release --features ebpf`) and the host `relish` on `PATH` or in `RELISH`;
the recording then shows `relish setup --quickstart --development-binaries`
where the tour says `curl … | sh`, and says so on screen.

If another laptop cluster already holds the default ports, give the tour's
cluster its own with `--api-port`, `--ingress-port` and `--registry-port` (for
example 29117, 28080 and 25050). `--install` and `--setup` pass them to
`relish setup --quickstart`, and the tour sends its requests to that ingress
port. The recording says which ports it used and shows the commands with them.

`--record` runs the tour inside `asciinema rec` (110x32, idle time cut to two
seconds) and then plays the setup step four times faster, because setup redraws
its timers several times a second and idle trimming can't shorten it. The
narration says both, and every wait prints how long it really took. It applies
the demo URL when it answers, and `examples/kubernetes/podinfo.yaml` (with a
note) when it doesn't; likewise it copies `examples/demo/burger` unless the
published tarball holds exactly that directory. Until a promotion's site
change deploys, the live tarball is the previous release's, so a re-record
always shows the repository copy, and says so on screen. The build step runs in a scratch directory, so
`burger/` never lands in the checkout. The published recording was made with
`--install v0.2.1 --api-port 29117 --ingress-port 28080 --registry-port 25050`,
beside a laptop cluster on the default ports.
`asciinema play docs/website/assets/tour.cast` plays the result in a terminal.

`--record` writes the installed version into the cast's title (`Reliaburger
0.2.1: the five-minute tour`), and the caption under the player names it too.
`tests/suite/website.rs` checks that both match `install.sh`'s default, so a
promotion that forgets the re-record fails CI.

The "Try it in five minutes" section keeps its heading and the recording in
plain view; people didn't notice it when the whole section sat in one collapsed
box. Only the step-by-step command list is folded, in a `<details
id="tour-commands">` whose summary reads "Show the commands", so it opens and
closes without script. Chromium and Safari open a closed `<details>` when
find-in-page matches inside it, so Ctrl-F still reaches every command, and
`assets/copy.js` opens it for a `#tour-commands` link. `tests/suite/website.rs`
checks that the recording isn't inside a `<details>` and that the commands are.
Each command in the tour carries a `data-tour` attribute.
`tests/suite/website.rs` parses every one of those with the real `relish`
command-line definition, and does the same for the manual's copy of the tour
(`docs/manual/08_five-minute-tour.md`), so edit both together.
`scripts/ci/select-jobs.sh` treats this page as code for that reason.

The tour applies `https://reliaburger.com/demo/podinfo.yaml` and builds from
`https://reliaburger.com/demo/burger.tar.gz`. Neither is committed here: the
Pages workflow copies `examples/kubernetes/podinfo.yaml` into `demo/` and packs
`examples/demo/burger` as `demo/burger.tar.gz` at deploy time, both taken from
the tag `install.sh` installs. Someone running the tour has that release, and
`main`'s demo may need features it doesn't have yet. To preview them locally, make them the same way
(`demo/` is ignored by version control).

Preview from the repository root:

```sh
python3 -m http.server 8080 --bind 127.0.0.1 --directory docs/website
```

Open `http://127.0.0.1:8080/`. Asset paths are relative so the page also works
under a GitHub project-site path.

GitHub is the source of truth for documentation. The whitepaper PDF link points
at `https://github.com/reliaburger/reliaburger/releases/latest/download/reliaburger-whitepaper.pdf`:
every promoted release carries the four PDFs, and promotion marks it latest.
The Build & Release workflow's `reliaburger-pdfs` artefact has newer copies
from `main`; artefacts require GitHub sign-in and have limited retention.

### Soak charts

"How a release is tested" (`#release-testing`) shows the released version's
soak charts: Bun memory, open fds and CPU from the app soak, and the same plus
accepted jobs a second from the job soak. They are copies of the SVGs in
`docs/qualification/<record>-charts/`, kept under the same directory names in
`assets/soak/`, because the Pages workflow publishes only this directory.
Each is an `<img>` linked to its own file, so a click opens it full size with
the `<title>` on every line and mark, and `loading="lazy"` keeps the folded
results from costing a visitor anything until they open them. The SVGs carry
their own colours and a light surface; this page is light only
(`color-scheme: light`), and an SVG drawn as an image follows the page's
scheme, so the charts stay light here too. At 720 px they're wider than a
phone and shrink with the column.

At a promotion, replace them with the new release's
([releasing step 9](../releasing.md#staging-a-candidate)):

```sh
rm -r docs/website/assets/soak
mkdir docs/website/assets/soak
cp -R docs/qualification/<app soak record>-charts docs/qualification/<job soak record>-charts \
  docs/website/assets/soak/
```

then point the `<img>` and record links at them. `tests/suite/website.rs`
fails while a chart differs from its record's copy, comes from another
release, lacks alt text, a size or lazy loading, or sits in `assets/soak/`
without being shown, and while the section misses any of the seven charts.

### Commands the installed release doesn't have

The page and the READMEs are written on the development branch, but a visitor
runs what the installer fetched. `scripts/release/released-cli.tsv` records
every command, positional, option and listed option value of that release,
written from the published binary's own help by
`scripts/release/released_cli.py VERSION > scripts/release/released-cli.tsv`.
`tests/suite/website.rs` checks every `relish …` command on this page, in the
README and in the tour chapter against it. The tour may only use released
commands. Anything else has to say which release adds it, in the words
"0.3.0, not in 0.2.0" (the build's version, then the installer's), in the
same paragraph, `<details>` or section. The README's generated command list
labels such commands itself. When the installer's default changes, regenerate
the snapshot; the test refuses a snapshot for another version.

The page leads with the one-line install now that releases are published;
building from source is the second path. Update the release-status paragraph
(version, date and release-notes link) and the install section's release-notes
link when you promote a release. The source quickstart intentionally matches
`examples/phase-1/proc-first-run.toml` and `examples/node/process-runtime.toml`.

`install.sh` is a small HTTPS bootstrap for the versioned release installer.
Both it and the generated installer are POSIX sh, so `curl … | sh` works where
`sh` is dash or busybox, not only bash. `scripts/release/test_package.py` runs
them under every POSIX shell it finds and under `shellcheck -s sh` when present.
It installs `v0.2.0` unless `RELIABURGER_VERSION` names another release; bump
that default when you promote a newer one. If the requested release isn't
published, it fails with a message saying so. The generated installer itself
lives in the GitHub release, with native CLI checksums supplied by release
packaging.

### Comparison pages

`compare/index.html` links six practical comparisons; `compare/methodology.html`
explains the evidence and version boundaries. Public pages foreground Reliaburger’s
strengths and honest trade-offs. Adaptation recommendations belong in the separate
[Markdown research writeup](../research/2026-10-15-competitor-review.md).
Each public page is hand-maintained static HTML,
using `style.css` and the small `comparisons.css` extension. There is no generated
copy or build dependency. The homepage links them under the stable `compare` id.

Keep evidence adjacent to factual claims. Pin source-code links to the inspected
commit or tag; date moving documentation. Distinguish the public installer release,
the reviewed train implementation and planned features. Learning-curve judgements
are qualitative, and source inspection is not a benchmark or security certification.
When refreshing a comparison, review commands, installation assumptions and both
application and platform upgrade procedures. Preserve section anchors.

Serve the directory with the preview command above and open `/compare/` or
`/compare/methodology.html`. Check all local links and fragments, desktop and narrow
layouts, and the command examples with the current CLI's `RELISH_PARSE_ONLY=1`.
The comparison pages use only `assets/copy.js`; they don't load the tour player.
