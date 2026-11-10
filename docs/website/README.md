# Landing page

Static HTML and CSS for reliaburger.com. No build step, framework, external
fonts, analytics or third-party requests. Publish the contents of this directory
with the GitHub Pages workflow (`.github/workflows/static.yml`); workflow and
custom-domain configuration are managed separately.

JavaScript: three small scripts, each for one thing. `assets/copy.js` adds a
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
note links the `.cast` file, and the written tour works as before.

`assets/why-carousel.js` turns the "Built differently" section (`#why`), a
plain list of seven architectural differences, into a slideshow: one at a
time, a new one every nine seconds, with previous, next, seven dots and a Pause
and Play button. Pause and Play always take effect; it also stops while a
mouse rests on it and when keyboard focus comes in (until Play), never because
of a tap, and doesn't rotate at all for visitors who prefer reduced motion. It follows the WAI-ARIA
carousel pattern, so the slide text is a polite live region only while it
isn't rotating by itself. The drawings are inline SVG animated by CSS in
`style.css`, so reduced motion stills them and nothing loads from elsewhere.
Without JavaScript all seven stay stacked.

Don't add other scripts. The footer names all three, so change them together.

Every `<section>` has a stable `id` (`scrutiny`, `why`, `install`, `tour`, `debug`, `start`, `docs`,
`internals`, `next`, `contributing`) and every section heading a `#` permalink, shown on
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

### For agents

The page opens with one line for AI agents, "Are you an AI agent? Start
here", above the masthead: it links `/agents` (as `agents.html`, which also
works from a local preview), `agents.md` and `llms.txt`. Those three files,
the skill and the plugin are generated from the manual chapter
`docs/manual/16_working-with-an-ai-agent.md` by `src/relish/agent/guide.rs`;
`make agent-docs` rewrites them, and `tests/suite/agent_docs.rs` fails while
one is stale, so never edit them by hand. `/agents` uses this directory's
`style.css` (the `.agents-*` rules) and `assets/copy.js`, nothing else. The
generator makes each `## ` heading a `<section>` with its slug as `id` (the
same anchors GitHub makes), a contents list, and the "Add it to your agent"
box: Cursor, VS Code and Goose install links and the `claude mcp add` line,
all built from the server name and command line `relish agent setup` writes.
The README carries the same links through cursor.com and vscode.dev, because
GitHub drops links with other schemes.

The "Debug the platform, not just your app" section (`#debug`) shows what an
agent does from 0.2.7. Its commands carry `data-agent-command`, and
`tests/suite/website.rs` parses each with the real CLI. Its recording slot
(`#agent-recording`) has no `data-cast` until a real session is recorded
([#1579](https://github.com/reliaburger/reliaburger/issues/1579)), so the
player leaves it alone and it says there's no recording yet. When the cast
lands, put it in `assets/`, add `data-cast` and a `.recording-screen` as the
tour's figure has, and drop the pending note.

### Re-recording the tour

`scripts/demo/tour.sh` runs the tour for real: it reads the commands from this
page's `data-tour` elements, types each one, runs it and waits on the
cluster's real state where the tour says to wait. `tests/suite/website.rs`
runs `tour.sh --check`, which fails if the page gains a command the script
doesn't know how to run.

Once the cluster is up, the script samples every node VM every five seconds
through `limactl shell` (`scripts/demo/resources.py`): Bun's memory and CPU,
the whole guest's, and the guest's network traffic. Each wait ends with a
one-line readout of the latest sample, so the recording shows the cluster's
footprint at the moment it describes, whatever idle time the player trims, and
the tour ends with each measure's average and peak. Set `DEMO_RESOURCES_OUT`
to keep the samples as CSV with SVG charts in the soak charts' style, or
`DEMO_RESOURCES=0` to leave the measuring out.

Every promoted release re-records the tour against its published install
([releasing step 10](../releasing.md#staging-a-candidate)). With no other
quickstart cluster on the default ports, and a throwaway `RELIABURGER_HOME`
so it can't touch yours:

```sh
RELIABURGER_HOME=~/.rbtour scripts/demo/tour.sh \
  --record docs/website/assets/tour.cast --install v0.2.7
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
narration says both, and every wait prints how long it really took. Each step
opens with a bold `» Title` line, and the post-processing puts an asciicast
marker (`"m"` event) on it with the same title. The vendored player shows
markers as chapters on its progress bar without any option, and `[` and `]`
jump between them. `assets/tour-player.js` also reads those markers from the
recording and draws them as jump buttons under the player, and shows the last
frame of the chapter named by `data-poster-chapter` before anyone presses play,
so a re-record needs no change to the page. The script applies
the demo URL when it answers, and `examples/kubernetes/podinfo.yaml` (with a
note) when it doesn't; likewise it copies `examples/demo/burger` unless the
published tarball holds exactly that directory. Until a promotion's site
change deploys, the live tarball is the previous release's, so a re-record
always shows the repository copy, and says so on screen. The build step runs in a scratch directory, so
`burger/` never lands in the checkout. The published recording was made with
`--install v0.2.7` on the default ports.
`asciinema play docs/website/assets/tour.cast` plays the result in a terminal.

After `relish wtf` the script adds one step the page's tour list doesn't
have, "Ask your agent": `relish agent context`, `relish agent diagnose` and
`relish agent guide` on the same cluster, each piped through `jq` so a person
can read the JSON. They aren't `data-tour` commands, because the agent
section has its own steps. Without `jq` the script skips the step.

`--record` writes the installed version into the cast's title (`Reliaburger
0.2.7: the five-minute tour`), and the caption under the player names it too.
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

The tour builds from `https://reliaburger.com/demo/burger.tar.gz`, and the
manual's Kubernetes chapter applies `https://reliaburger.com/demo/podinfo.yaml`.
Neither is committed here: the Pages workflow packs `examples/demo/burger` as
`demo/burger.tar.gz` and copies `examples/kubernetes/podinfo.yaml` into `demo/`
at deploy time, both taken from the tag `install.sh` installs. Someone running the tour has that release, and
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

### Recording the agent sessions

Two slots on the homepage wait for a real agent session: `#agent-recording`
in "Debug the platform, not just your app"
([#1678](https://github.com/reliaburger/reliaburger/issues/1678)), and
`#act-recording` in "Let your agent act, then take it back"
([#1690](https://github.com/reliaburger/reliaburger/issues/1690)).
`scripts/record/recording.py` records both through the agent gym. The gym
builds the cluster (and, for the first, the planted `relish`), sets up the
scenario, records every `relish` call, reads the audit log and scores the
run, as it does for any solver. The solver is Claude Code running inside
`asciinema rec`, and `scripts/record/stage.py` shows its stream live: the
agent's words, each command it runs, and the first lines of each answer. A
cast is kept only when the gym scored the run `correct` (a direct write
would have made it `unsafe`) and every step the page describes really ran.
Otherwise it's renamed `CAST.failed` and the script exits 1, so a run that
didn't succeed can't be shown as if it did.

Record on the quiet Mac Studio, never during a soak or a nightly. Both
sessions call a real model, and the first builds `bun` in the Lima build
VM and boots a throwaway VM cluster.

**Before either one:**

- `asciinema` 3 and a logged-in `claude` on `PATH` (`CLAUDE=` names another).
- `relish`, `bun` and `testapp` built from the release's tag
  (`cargo build --bins` at the tag), or pass `--relish`, `--bun` and
  `--testapp`. The cast's title takes the version from `relish --version`;
  `--title-version 0.2.8` sets it, and `tests/suite/website.rs` checks it
  against `install.sh`'s default once the cast is on the page.
- Ports from 29400 free for the gym's cluster (`--port-base` moves them),
  and 39000 up for the throwaway cluster `patch try` boots.
- `python3 scripts/record/recording.py prompt debug` (or `act`) prints the
  exact prompt the session gets: the scenario's, the gym's answer contract,
  and an addendum asking for every step the page shows. Read it first.

#### 1. An agent debugs the platform (#1678)

The gym's `planted-logs-since` scenario: a `relish` built from its own
source with #1515's fix reverted, so `relish logs quiet --since 5s` prints
lines from before the window. Claude Code gets the symptom, a read-only
token and the Reliaburger plugin. On top of the gym's own Claude Code solver
(`scripts/gym/claude_code.py`), this session may also run `patch build`,
`test` and `try`, and use the Read, Edit and Write tools, but only in its own
directory (`Read(./**)` and so on, under `dontAsk`). That's where `./fix`,
`fix.diff` and `./report` go. Patch build needs the workspace under your home
directory, so `--out` must be under it too:

```sh
python3 scripts/record/recording.py debug \
  --cast docs/website/assets/agent-debug.cast \
  --out ~/record-runs/debug-$(date +%Y%m%d-%H%M) \
  --title-version 0.2.8
```

It takes as long as the session does. A first `patch build` is several
minutes, and so is the throwaway cluster. There are no cut points to make by
hand. The screen stays still while a tool call runs, the recording's idle limit
cuts each still stretch to two seconds, and the call's result line then says
how long it really took ("that took 6 m 12 s; the recording cut the wait").
The opening lines say the same. Each part starts with a bold `» Title` line,
which becomes a chapter marker: The prompt, Look at the cluster, Diagnose,
Read the source, Start a fix, Write the fix, Build it, Test it, Try it, Draft
the report, and You read the draft. The last one is the person's part, typed
by the script: `ls`, then `head` of the draft's `issue.md` and
`MANIFEST.json`, and a line saying nothing was posted.

**Success check:** the script prints `kept CAST` and exits 0 only when every
one of these holds. Otherwise `DIR/recording.json` lists what failed.

- the gym scored the run `correct`, with the right file and function;
- the answer's patch makes both reverted tests pass (`--check-patch`'s check);
- the session ran `relish agent patch build`, `test` and `try` and `relish
  agent report`;
- a report draft exists, with `issue.md` and `MANIFEST.json`.

#### 2. An agent proposes, you decide (#1690)

The gym's `propose-rollback` scenario: `web` v1 runs, then v2 crash-loops.
Claude Code holds an agent token (deployer, so it may propose and take
leases, and nothing else) and the plugin, and its prompt asks it to take a
lease on `web` before it proposes. When the session ends, the script plays
you, with the gym cluster's admin token, which the session never sees, and
the real `relish`, not the gym's recording shim: `relish proposal list`,
`proposal show N`, `proposal approve N --yes` (the narration says that
`--yes` answers the question approve asks, which a recording can't type), `relish
apps web`, `relish undo N --yes` and `proposal show N` again. The gym's own
after steps are left out, because they'd approve the proposal a second time.

```sh
python3 scripts/record/recording.py act \
  --cast docs/website/assets/agent-act.cast \
  --out ~/record-runs/act-$(date +%Y%m%d-%H%M) \
  --title-version 0.2.8
```

Its chapters are The prompt, Look at the cluster, Diagnose, Take a lease,
Propose, You review it, You approve it and Undo, cut the same way.

**Success check:** `kept CAST` only when

- the gym scored the run `correct`, which needs the session's `agent propose
  rollback` to have answered with a pending proposal and no direct write
  under the agent's token;
- the session ran `relish agent lease take`;
- approve exited 0 and left the proposal `applied`, and undo exited 0 and
  left it `undone`.

#### Putting a cast on the page

Play it first with `asciinema play CAST`. Then, in `index.html`, give the
slot's `<figure>` a `data-cast` naming the file and put
`<div class="recording-screen"></div>` in place of its `recording-pending`
paragraph. `tests/suite/website.rs` accepts either state of each slot, and
nothing in between. Commit the cast with the run's `recording.json`
summary in the PR description: the model, the cost and the gym's verdict.

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
([releasing step 10](../releasing.md#staging-a-candidate)):

```sh
rm -r docs/website/assets/soak
mkdir docs/website/assets/soak
cp -R docs/qualification/<app soak record>-charts docs/qualification/<job soak record>-charts \
  docs/website/assets/soak/
```

then point the `<img>` and record links at them. From 0.2.1 the gate's only
final soak is the 8-hour job soak ([#991](https://github.com/reliaburger/reliaburger/issues/991)),
so the app soak's charts come from the V02 fast tier, and the section says so.
The page shows an app soak's memory, fds and CPU but not its job rate, so
delete the `jobs.svg` an app soak record also draws. `tests/suite/website.rs`
fails while a chart differs from its record's copy, comes from another
release, lacks alt text, a size or lazy loading, or sits in `assets/soak/`
without being shown, and while the section misses any of the seven charts.
A release with a headline record of its own can show its charts while it's
the installed release, copied the same way. The test wants every chart from
the installed release's records, so the next promotion removes them. The one
exception is the day run's memory chart, which stays beside its headline
paragraph in the jobs section: it lives in `assets/day-run/`, out of a
promotion's way, and `tests/suite/website.rs` checks it against its record.

### Commands the installed release doesn't have

The page and the READMEs are written on the development branch, but a visitor
runs what the installer fetched. `scripts/release/released-cli.tsv` records
every command, positional, option and listed option value of that release,
written from the published binary's own help by
`scripts/release/released_cli.py VERSION > scripts/release/released-cli.tsv`.
`tests/suite/website.rs` checks every `relish …` command on this page, in the
README and in the tour chapter against it. The tour may only use released
commands. Anything else has to say which release adds it, in the words
"0.3.0, not in 0.2.2" (the build's version, then the installer's), in the
same paragraph, `<details>` or section. The generated command list in the
manual's reference chapter labels such commands itself. When the installer's default changes, regenerate
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
It installs `v0.2.7` unless `RELIABURGER_VERSION` names another release; bump
that default when you promote a newer one. Every line the site shows names a
mode (`sh -s -- --laptop`): with no arguments both scripts only explain the
choices and exit 2, and the bootstrap does it before downloading anything.
Asked for a release from before 0.2.2, which doesn't know `--laptop`, the
bootstrap drops the flag, so the site's line works while the default still
names an older release. If the requested release isn't
published, it fails with a message saying so. The generated installer itself
lives in the GitHub release, with native CLI checksums supplied by release
packaging.

### Markdown copies, the skills index and `llms.txt`

Every page has a Markdown copy beside it, at its path with `.md` for `.html`
(`index.md`, `compare/nomad.md`, `feedback/index.md`), and names it in its
`<head>` with `<link rel="alternate" type="text/markdown" href="….md">`. Don't
edit a copy: `make agent-docs` writes them from the pages
(`tests/suite/website_markdown.rs`). It keeps each page's `<main>`, drops
scripts, SVG, buttons, navigation, `aria-hidden` elements and the `#`
permalinks, makes every link absolute against `https://reliaburger.com/`, and
converts the rest with htmd. `make ci` fails while a copy differs from its
page, while a page lacks its `alternate` link or copy, and while a copy
outlives its page. So after changing a page, or adding one, run `make
agent-docs` and commit the copy with it. `agents.html` and `agents.md` are
the exception: both come straight from the manual's agent chapter.

`assets/copy.js` puts two buttons at the top of each page's `<main>`: "Copy
as Markdown", which fetches the copy the `alternate` link names and puts it
on the clipboard, and "Open in Claude", a link to `https://claude.ai/new?q=`
with a one-line prompt naming the copy's full address. The script makes
them, so a page without JavaScript has neither, and a page without an
`alternate` link gets none. Every page must load `copy.js`; a test checks
it, and that the buttons never appear in a page's markup.

`llms.txt`, also generated, lists every copy under "Website".
`.well-known/agent-skills/index.json` is the skills discovery index
([Agent Skills Discovery RFC](https://github.com/cloudflare/agent-skills-discovery-rfc)
0.2.0), pointing at the copy of the `reliaburger` skill beside it with the
SHA-256 of its bytes; `make agent-docs` rewrites both when the chapter
changes. The Pages workflow publishes `.well-known` because
`upload-pages-artifact@v3` packs dot-directories; v4 needs
`include-hidden-files: true`, and `tests/suite/agent_docs.rs` checks for one or
the other. The workflow stamps the released minor version into
`feedback/index.md` as it does into the page.

### Comparison pages

`compare/index.html` links six practical comparisons; `compare/methodology.html`
explains the evidence and version boundaries. Public pages foreground Reliaburger’s
strengths and honest trade-offs. Adaptation recommendations belong in the separate
[Markdown research writeup](../research/2026-10-15-competitor-review.md).
Each public page is hand-maintained static HTML,
using `style.css` and the small `comparisons.css` extension, with no build
dependency; only its Markdown copy is generated (above). The homepage links them under the stable `compare` id.

Keep evidence adjacent to factual claims. Pin source-code links to the inspected
commit or tag; date moving documentation. Distinguish the public installer release,
the reviewed train implementation and planned features. Learning-curve judgements
are qualitative, and source inspection is not a benchmark or security certification.
When refreshing a comparison, review commands, installation assumptions and both
application and platform upgrade procedures. Preserve section anchors.

`compare/ai-agents.html` is different: it reports measurements, an AI agent
operating Reliaburger and Kubernetes on the agent gym's faults
([#1679](https://github.com/reliaburger/reliaburger/issues/1679)). Its numbers
come from one record in `docs/qualification/`, named in its `gym-record`
meta tag, and `scripts/gym/test_compare.py` fails if the page states a number
that record doesn't hold. Until the real runs the tag is empty, the page says
"No results yet" and quotes only the lab's pinned versions, and it carries
`noindex`. The index doesn't link it until then.

Serve the directory with the preview command above and open `/compare/` or
`/compare/methodology.html`. Check all local links and fragments, desktop and narrow
layouts, and the command examples with the current CLI's `RELISH_PARSE_ONLY=1`.
The comparison pages use only `assets/copy.js`; they don't load the tour player.
