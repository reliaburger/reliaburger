# Security Policy

## Reporting a vulnerability

Please don't report a vulnerability in a public issue, a discussion or a pull request. Report it privately through GitHub instead:

**[Report a vulnerability](https://github.com/reliaburger/reliaburger/security/advisories/new)**

That opens a private advisory only you and the maintainers can see. Tell us:

- the version (`relish --version`, and `bun --version` if the nodes run a different build);
- what an attacker can do, and what they need first (network access, a token, a node);
- a reproducer: the smallest steps, config or code that show it;
- what you didn't test.

The [AI-assistance policy](CONTRIBUTING.md#ai-assistance) applies here too. If an AI tool found the issue or wrote the report, say so, and check every claim yourself before you send it. A report with no reproducer that we can't confirm may be closed.

## What happens next

We'll acknowledge the report, confirm or rule it out, and agree a disclosure date with you. A fix ships in a new release, which a cluster takes with `relish upgrade`, and the advisory is published with it. We credit reporters by name in the advisory and the release notes, unless you'd rather we didn't.

## No bounty

There's no bug bounty, and no payment at any severity. Credit is what we offer. A cash reward invites volume over evidence, and [curl ended its bounty](https://daniel.haxx.se/blog/2026/01/26/the-end-of-the-curl-bug-bounty/) for that reason.

## Supported versions

Reliaburger is before 1.0.0, so fixes go into the next release only. Run the latest one.
