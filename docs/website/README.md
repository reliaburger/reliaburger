# Landing page

Static HTML and CSS for reliaburger.com. No build step, framework, JavaScript,
external fonts or analytics. Publish the contents of this directory with the
GitHub Pages workflow (`.github/workflows/static.yml`); workflow and custom-domain
configuration are managed separately.

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

Update the release-status paragraph when the installer actually ships. The
source quickstart intentionally matches `examples/phase-1/proc-first-run.toml`.

`install.sh` is a small HTTPS bootstrap for the versioned release installer.
Both it and the generated installer are POSIX sh, so `curl … | sh` works where
`sh` is dash or busybox, not only bash. `scripts/release/test_package.py` runs
them under every POSIX shell it finds and under `shellcheck -s sh` when present.
It intentionally fails with a release-not-published message until v0.1.0 exists.
The page labels this path as pending; remove that label only after published
candidate qualification passes. The generated installer itself lives in the
GitHub release, with native CLI checksums supplied by release packaging.
