# Release log

One paragraph per release: when it shipped, how long qualifying it took, and
what the soaks found on the way. It's the track record behind the
[release gate](releasing.md#staging-a-candidate); the evidence is in the
[qualification records](qualification/) each paragraph links, and what each
release contains is in the [roadmap](roadmap.md#releases-after-010). A new
paragraph goes at the top when a release is promoted
([release gate, step 9](releasing.md#staging-a-candidate)).

## 0.2.1: cluster safety

Promoted on 21 October 2026 as tag `v0.2.1` at `abec2c45`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/39575597952)).
Qualification ran from 17 to 21 October 2026 (UTC) and took nine candidates
([closure record](qualification/2026-10-21-v0.2.1-release-closure.md)), the
first on the shorter gate: one 8-hour jobs final in place of 0.2.0's separate
V02 and job finals ([#991](https://github.com/reliaburger/reliaburger/issues/991)).
Candidate 1 never got past its staged install: 0.2.1 refuses a health timeout
longer than its interval, and the quickstart's own `hello` app had one
([#1019](https://github.com/reliaburger/reliaburger/issues/1019)). Candidate 2
couldn't launch a fresh runc job
([#1021](https://github.com/reliaburger/reliaburger/issues/1021)). Candidate 3
found that `relish local stop` power-cut every VM, because Lima kills a guest
that ignores its stop request after 30 s
([#1023](https://github.com/reliaburger/reliaburger/issues/1023)), and that
cut left a Raft store redb couldn't open
([#1024](https://github.com/reliaburger/reliaburger/issues/1024)). Candidates 3
and 4 both found a leader that couldn't prove its leadership forwarding API
calls to itself until it ran out of memory
([#1069](https://github.com/reliaburger/reliaburger/issues/1069)).
Candidate 5 found a health-probe publication holding the agent loop for 5 s
([#1100](https://github.com/reliaburger/reliaburger/issues/1100)). From
candidate 6 on, the V02 fast tier passed and the failures moved to the job
soaks: a failed redeploy that marked a released version failed for good
([#1110](https://github.com/reliaburger/reliaburger/issues/1110)), a stale
follower answering a batch cancel with 404
([#1192](https://github.com/reliaburger/reliaburger/issues/1192)), and Bun
exiting after a self-upgrade because a forking child held its upload lock
([#1198](https://github.com/reliaburger/reliaburger/issues/1198)), plus a
handful of harness bugs. Candidate 9 passed both staged installs, the V02 fast
tier and the jobs fast tier at 3,520 accepted jobs a second
([V02 fast](qualification/2026-10-20-v0.2.1-sustained-v02-fast.md),
[jobs fast](qualification/2026-10-20-v0.2.1-job-qualification-fast.md)). Its
first jobs final ran the full 8 hours at 3,840 accepted jobs a second and
failed on one 3.95 s agent-loop turn: a health-probe journal write caught in a
host disk stall ([#1255](https://github.com/reliaburger/reliaburger/issues/1255)).
The maintainer re-ran it on the same candidate, and accepted slow turns from
#1255 as a known issue for 0.2.1, provided everything else passed. Run 2 ran
the full 8 hours at 3,764 accepted jobs a second, with every fault settled,
nothing lost and Bun's memory within target
([record](qualification/2026-10-21-v0.2.1-job-qualification.md)). Its only
failures were slow agent-loop turns: #1255 again, which the checker counted
twice ([#1261](https://github.com/reliaburger/reliaburger/issues/1261)), and
the same fsync'd discovery journal write at a second call site
([#1266](https://github.com/reliaburger/reliaburger/issues/1266)), which the
maintainer accepted too. The power-cut and reboot lanes then passed 25 of 25 in
every lane ([record](qualification/2026-10-21-power-cut-reboot.md)).
0.2.1 ships with #1255 and #1266 (a slow disk can still hold a node's agent
loop for seconds; the fixes are in 0.2.2), and with RUSTSEC-2026-0381, -0382
and -0383, published after the gate: its rustls-webpki and webpki-roots trust
the distrusted emSign intermediates on outbound HTTPS, as 0.2.0's do. Cluster
mTLS isn't affected, `main` has the patched versions, and 0.2.2 is the first
release built with them
([#1269](https://github.com/reliaburger/reliaburger/issues/1269)). It's also
the last release with an Intel macOS build
([#1018](https://github.com/reliaburger/reliaburger/issues/1018)).

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
