<!-- Generated from docs/website/compare/methodology.html by `make agent-docs`: edit the page, not this file. -->

COMPARISONS · EVIDENCE AND VERSIONS

# Claims you can check.

The comparisons explain where Reliaburger's integrated platform helps, with sources for the capabilities and boundaries that matter to your decision.

## How to read the comparisons

Reviewed on **15 October 2026**, and the Reliaburger side again on **21 October 2026** for 0.2.2. We read official documentation and inspected source where a capability needed a closer check. Each page links the evidence beside the relevant claim. Code links use the reviewed commit or release tag; official documentation links may change over time.

The pages compare deployment, diagnosis, replica changes, installation and upgrades. They distinguish documented behaviour from our judgement about which workflow fits a particular team. A shorter configuration or a bundled component is an observable difference; it is not a measurement of reliability, security or onboarding time.

We did not deploy the competing clusters for a comparative benchmark. Example commands follow the cited interfaces, with prerequisites stated on each page. Relish examples were checked against its CLI parser. No performance ranking is implied.

## Which Reliaburger

The reference is the [0.2.2 release](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2), which the public installer installs. Code and documentation links point at its tag, so a claim describes what you get when you install it. The first review compared the unreleased 0.2.1 train with the installed 0.1.6. Since then 0.2.1 has shipped its cluster-safety fixes, node-by-node rollouts among them, and 0.2.2 its UX polish, including the installer's `--laptop` mode and a Kubernetes import that attaches Services by selector and fails `--strict` on anything dropped. 0.2.2 also drops the Intel macOS build, so the laptop cluster needs an Apple silicon Mac or a Linux host. [0.2.1 milestone](https://github.com/reliaburger/reliaburger/milestone/16?closed=1) · [0.2.2 milestone](https://github.com/reliaburger/reliaburger/milestone/15?closed=1).

Pre-1.0 changes to protocol or state formats require a fresh cluster, and 0.2.2 makes one: a 0.2.1 cluster can't roll to it. Same-format rolling binary replacement is a narrower capability, not an in-place migration promise. [Format versions](https://github.com/reliaburger/reliaburger/blob/v0.2.2/src/compatibility.rs) · [Compatibility policy](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100).

Competitor versions and evidence limits are listed on each comparison page. Kubernetes, k3s, Nomad, Coolify and Uncloud represent five relevant platform approaches; Docker Compose supplies the single-host baseline. This selection reflects architectural relevance, not a market-share ranking.

## Keep the evidence useful

If a comparison is wrong or a capability changes, [open an issue](https://github.com/reliaburger/reliaburger/issues) with the page, affected version and primary source. [Return to the comparisons](https://reliaburger.com/compare/).

---

The Markdown copy of <https://reliaburger.com/compare/methodology>.
