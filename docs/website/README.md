# Landing page

Static HTML and CSS for reliaburger.com. No build step, framework, external
fonts, analytics or third-party requests. Publish the contents of this directory
with the GitHub Pages workflow (`.github/workflows/static.yml`); workflow and
custom-domain configuration are managed separately.

JavaScript: none today. The one planned exception (decision D2 in
`docs/plans/2026-09-23-zero-to-cluster.md`) is a vendored copy of the
asciinema player, served from this directory, to embed the recording of the
tour. Everything else, including the tour itself, must keep working with
scripts disabled. Don't add other scripts.

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
