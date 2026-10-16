# Release log

One paragraph per release: when it shipped, how long qualifying it took, and
what the soaks found on the way. It's the track record behind the
[release gate](releasing.md#staging-a-candidate); the evidence is in the
[qualification records](qualification/) each paragraph links, and what each
release contains is in the [roadmap](roadmap.md#releases-after-010). A new
paragraph goes at the top when a release is promoted
([release gate, step 9](releasing.md#staging-a-candidate)).

## 0.2.0: A million jobs

Promoted on 16 October 2026 as tag `v0.2.0` at `cc5f6439`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/38944559201)).
Qualification ran from 12 to 16 October 2026 (UTC) and took eight
candidates ([closure record](qualification/2026-10-16-v0.2.0-release-closure.md)).
All eight installed cleanly; the soaks, now carrying a job campaign alongside
the apps, failed the first seven. Candidates 1 and 2 never got past setup:
Bun crash-looped when the soak switched it from runc to the mixed runtime
([#823](https://github.com/reliaburger/reliaburger/issues/823),
[#839](https://github.com/reliaburger/reliaburger/issues/839)). Candidates 3
to 7 found the API node's Bun at over 1 GiB
([#892](https://github.com/reliaburger/reliaburger/issues/892),
[#945](https://github.com/reliaburger/reliaburger/issues/945)), a launch
failure that was never retried, a killed shell reported as a clean exit,
stale executor slots after a Bun kill, and a replica refused for capacity and
never moved, plus a dozen harness bugs in the new job checks. Candidate 8
passed the 8-hour V02 final tier cleanly and the jobs fast tier at 5,001
accepted jobs a second. Its first 12-hour jobs final failed after six hours:
not Bun, but Lima 2.1's gRPC port forwarder leaking a socket per forwarded
request until the API node's guest agent held 1 GiB and the kernel killed Bun
([#982](https://github.com/reliaburger/reliaburger/issues/982),
[lima-vm/lima#5210](https://github.com/lima-vm/lima/issues/5210)). The rerun
forwarded ports over SSH instead
([#986](https://github.com/reliaburger/reliaburger/pull/986)) and ran the full
12 hours at 4,600 accepted jobs a second, with Bun's memory within target. Its
15 failures all came from the harness: SSH forwarding that Lima never retried
after a reboot, cutting a healthy node off from the host, and the job
controller running out of its run budget after 10.5 hours. The maintainer
counted both as known harness artefacts, and 0.2.1 removes them
([#983](https://github.com/reliaburger/reliaburger/issues/983),
[#987](https://github.com/reliaburger/reliaburger/issues/987),
[#989](https://github.com/reliaburger/reliaburger/issues/989);
[record](qualification/2026-10-17-v0.2.0-job-qualification.md)). Every
power-cut and reboot lane passed 25 of 25
([record](qualification/2026-10-16-power-cut-reboot.md)).
0.2.0 ships with the quickstart still pinning Lima 2.1.0, so with that leak
and with CVE-2026-53657 (root in a QEMU-driven VM through the guest agent's
socket); 0.2.1 bundles Lima 2.2.1
([#983](https://github.com/reliaburger/reliaburger/issues/983)).

## 0.1.6: operations security

Promoted on 7 October 2026, the day after 0.1.5. One candidate passed
everything first time: the
[staged install](qualification/2026-10-06-v0.1.6-staged-install-apple-silicon.md),
the [compressed](qualification/2026-10-06-v0.1.6-soak-compressed.md) and
[full](qualification/2026-10-06-v0.1.6-soak-full.md) CI soaks, and a clean
[8-hour final tier](qualification/2026-10-06-v0.1.6-sustained-v02-final.md).

## 0.1.5: bug fixes

Promoted on 6 October 2026 after two candidates. The first one's 8-hour final
tier found a node crash-looping after Lima's guest agent stepped the guest
clock, because a clock step made its adopted processes look like strangers
([#607](https://github.com/reliaburger/reliaburger/issues/607)). The fix
([#609](https://github.com/reliaburger/reliaburger/pull/609)) went into a
re-cut, which passed the
[staged install](qualification/2026-10-05-v0.1.5-staged-install-apple-silicon.md),
the [compressed](qualification/2026-10-05-v0.1.5-soak-compressed.md) and
[full](qualification/2026-10-05-v0.1.5-soak-full.md) CI soaks and the
[8-hour final tier](qualification/2026-10-05-v0.1.5-sustained-v02-final.md).
It also fixed the three bugs 0.1.4 shipped with.

## 0.1.4: cross-node logs and events, security fixes

Promoted on 3 October 2026, the same day as 0.1.3. The
[staged install](qualification/2026-10-03-v0.1.4-staged-install-apple-silicon.md)
passed, and the [compressed](qualification/2026-10-03-v0.1.4-soak-compressed.md)
and [full](qualification/2026-10-03-v0.1.4-soak-full.md) CI soaks found three
bugs that it shipped with and 0.1.5 fixed:
consumer syncs over the agent loop's turn budget
([#505](https://github.com/reliaburger/reliaburger/issues/505)), a pooled peer
connection presenting an expired leaf
([#509](https://github.com/reliaburger/reliaburger/issues/509)), and the
leader resigning for disk pressure over unexported logs
([#510](https://github.com/reliaburger/reliaburger/issues/510)).

## 0.1.3: agent loop, observability and test quality

Promoted on 3 October 2026, two days after 0.1.2. It went through the release
path twice: after the first candidates, every known bug fix found since was
folded in (data safety, council recovery, the scheduler and API audit) and the
release re-cut ([#470](https://github.com/reliaburger/reliaburger/pull/470)).
The flakes and turn-budget misses found qualifying the first candidates were
fixed on the way. The [release notes](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.3)
record two cold installs (377 s and 379 s on Apple silicon), then the fast
tier and an 8-hour soak with no failures and, for the first time, no
agent-loop turn over 1 s.

## 0.1.2: images

Promoted on 1 October 2026, a day after 0.1.1. The
[release notes](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.2)
record two cold installs (240 s and 250 s on Apple silicon), then the fast tier
and an 8-hour soak with no failures. Only the x86_64 half of a multi-platform
image ran under runc in CI; both architectures haven't run in one cluster yet.

## 0.1.1: bug fixes

Promoted on 30 September 2026, a day after 0.1.0, with every bug known from
0.1.0's soak, early users and the static review. The
[release notes](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.1)
record two cold installs (191 s and 183 s on Apple silicon), then the fast tier
and an 8-hour soak, both with no failures.

## 0.1.0: the first release

Promoted on 29 September 2026 after five days and 13 candidates of
qualification, from 25 September
([closure record](qualification/2026-09-27-v0.1.0-release-closure.md)). The
soaks found most of what that push fixed: a
deploy that wedged every retry, Bun's memory climbing from 240 to 734 MB in an
hour because Lima 2.1's guest agent never closed forwarded connections, slow
health ticks starving the agent loop, and a stopped volume app restarting on
an empty volume. Candidate 13 passed the
[fast](qualification/2026-09-29-v02-fast-tier-7d7dfc0.md) and
[8-hour final](qualification/2026-09-29-sustained-v02.md) tiers cleanly.
