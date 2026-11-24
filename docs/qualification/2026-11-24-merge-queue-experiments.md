# Merge queue experiments, 24 November 2026

[#2264](https://github.com/reliaburger/reliaburger/issues/2264), the rest of
[#1053](https://github.com/reliaburger/reliaburger/issues/1053). Two questions:
what does a train's merge queue do with a compatible pair of PRs and with a
conflicting pair, and should its build concurrency be 1?

No live experiment was run, and no PR was opened to test the queue. The 0.3.3
train's queue answered both questions on its own: 27 entries went through it
on 24 November, four of them conflicting with an entry ahead. Everything below
is read back from the GitHub API. All times are UTC, which is London time in
November.

## What was read

- The ruleset: `gh api repos/reliaburger/reliaburger/rulesets/25015244`.
- Every PR into `release-1-3-1`, `release-1-3-2` and `release-1-3-3`, with
  its `AddedToMergeQueueEvent`, `RemovedFromMergeQueueEvent` (and `reason`)
  and `MergedEvent` from the GraphQL timeline.
- Every `merge_group` run of `ci.yml` (`gh run list --event merge_group`),
  each group commit's parents (`gh api repos/.../commits/<sha>`), and the jobs
  of all 332 workflow runs created on 24 November, with each job's
  `created_at`, `started_at` and `completed_at`.
- The live queue: `mergeQueue(branch: "release-1-3-3") { entries { position
  state headCommit baseCommit } }`.
- Each conflict, replayed locally with `git merge-tree --write-tree` on the
  exact commits the queue had.

The queue's events all fall on 24 November: 0.3.3's train opened that night,
and `release-1-3-2`'s last queue merge was at 23:50 on 22 November. The window
is 02:18, the first entry, to 14:24, when the record was taken.

## The settings

| Parameter | Value |
|---|---|
| `max_entries_to_build` (build concurrency) | 8 (raised from 4 on 7 November, #1724) |
| `min_entries_to_merge` / `max_entries_to_merge` | 1 / 8 |
| `min_entries_to_merge_wait_minutes` | 5 |
| `grouping_strategy` | `ALLGREEN` ("only merge non-failing pull requests") |
| `check_response_timeout_minutes` | 240 |
| `merge_method` | `MERGE` |

## What the queue did

| | 24 Nov, `release-1-3-3` |
|---|---|
| Entries the queue built (one `ci.yml` run each) | 27 |
| Merged | 21 |
| Rebuilt after a failure ahead | 0 |
| Dropped for failed checks or a timeout | 0 |
| Dropped for `merge_conflict` | 2 (#2234, #2268) |
| Removed by hand to update the branch (`manual`) | 3 (#2268, #2266, #2255) |
| Held unbuilt as `UNMERGEABLE` at 14:24 | 3 (#2269, #2272, #2276) |

For comparison, from 20 to 22 November the 0.3.1 and 0.3.2 queues dropped five
entries for `merge_conflict` (#2141, #2172 and #2166 together at 00:34 on 21
November, #2191 twice, #2224) and three for failed checks (#2140, #2153,
#2194). Only the failures made the queue rebuild anything.

### Every group is one PR

Each merge group's branch is `gh-readonly-queue/<train>/pr-<N>-<base sha>`,
and in all 53 groups of the 0.3.2 and 0.3.3 trains among the last 300
merge-group runs, the group commit's first
parent is that base: the head commit of the entry ahead. So every entry is
built alone, stacked on the entry in front, and the "group size" of 8 never
puts two PRs in one build. GitHub's documentation says so too: "Merge limits
do not combine `merge_group` builds". What `max_entries_to_merge` does is let
one push land several entries that have all passed. That happened twice: six
PRs at 09:12 and two at 11:13.

### (a) A compatible pair

#2249, #2244, #2247 and #2243 were queued within ten seconds of each other at
02:27, none conflicting with another. The queue built all four at once,
each on the one ahead, and merged each as soon as it and everything ahead had
passed: 04:00, 04:27, 04:30 and 04:44. Nothing was re-tested, and nothing
waited for a slower PR behind it.

The cost of `ALLGREEN` shows when the slow one is in front. #2234's group
started at 05:20 and took 231 minutes, 65 of them with no job running at all.
The five entries behind it had passed long before: #2258 at 07:11, #2248 at
07:45, #2257 at 08:10, #2246 at 08:25 and #2259 at 09:07. All six landed
together at 09:12, the moment #2234's own run finished, about five PR-hours of
waiting for a commit that was already proven by the run behind it.

### (a) A conflicting pair

Four times on 24 November a PR was queued behind an entry that edited the same
lines:

| PR | Conflicts with | File | Queued | Built? | Left the queue |
|---|---|---|---|---|---|
| #2234 | #2249, ahead | `docs/flakes.md` | 02:52 | no | 04:44, `merge_conflict` |
| #2268 | #2234, ahead | `docs/flakes.md` | 07:05 | no | 09:39, `merge_conflict`; queued again 09:39:13 unchanged; 10:21, removed by hand |
| #2266 | #2234, ahead | `docs/flakes.md` | 07:46 | no | 10:21, removed by hand |
| #2255 | #2253, ahead | `CHANGELOG.md`, `docs/testing/concept-budget.json` | 08:44 | no | 10:21, removed by hand |

What the queue does with a conflicting entry, from these four and the live
queue:

1. **It never builds it.** No run exists for any of the four until a later,
   fixed push. The live queue shows the state: an entry that won't merge onto
   the one ahead is `UNMERGEABLE`, with a `baseCommit` and no `headCommit`.
2. **It skips it.** The entries behind are built on the entry ahead of the
   conflicting one, so nothing behind is invalidated or re-tested. #2251,
   #2262, #2253, #2270 and #2250 were all built straight past #2268.
3. **It says nothing until the entry reaches the head,** then drops it for
   `merge_conflict`. #2234 waited 1 h 52 min for that, #2268 2 h 34 min. The PR
   page shows it queued the whole time.
4. **Queueing it again doesn't help** while the conflict stands: #2268 went
   back in ten seconds after it was dropped, unchanged, and sat unbuilt again.
5. **Only a person gets it out early.** The three `manual` removals, by
   seeker89 between 10:21:45 and 10:21:49, were followed within 25 seconds
   by a merge of the train into each branch, and #2268 went back in at 11:42
   and was built at once.

Every one of the four merged cleanly onto the train itself when it was queued
(`git merge-tree` onto the train's tip then: clean; onto the head of the last
built entry: conflict). The conflict was always with a PR still in the queue.
So a check against the train can't catch it; a check against the queue's tip
catches all four.

The live queue at 14:24 holds three more: #2269 (`CHANGELOG.md` and
`src/grill/mock.rs`), #2272 (`docs/flakes.md`) and #2276 (`CHANGELOG.md`) are
`UNMERGEABLE` and conflict with the train's tip as well. Each will be dropped
when it reaches the head unless its branch is updated first.

### (b) Runners: waiting versus running

The account ran 14 to 31 hosted jobs at once all day, while 35 to 94 more
waited for a runner. The queue had up to 8 groups building at once (07:00),
and 4 to 7 most of the day.

| Per merge group (the 25 finished by 14:24) | Median |
|---|---|
| Queued to its last job finishing | 136 min (shortest full run 70, longest 240) |
| Job-minutes waiting for a runner | 633 |
| Job-minutes running | 276 |
| Minutes with none of its jobs running | 37 |
| Queued to merged (21 merged PRs) | 199 min |

So a queue job waited more than twice as long for a runner as it ran. The
waits fell on the short jobs: `optimized contract boundaries` waited a median
53 minutes to run for one, `lint` 47 for 6, `doctests and feature tests` 47
for 7. Portable macOS, which has its own five runners, waited a median 6
minutes and ran 62.

Runner-minutes used from 02:00 to 14:20 on 24 November:

| Event and workflow | Runner-minutes |
|---|---|
| `merge_group` CI | 5,884 |
| `pull_request` CI | 5,187 |
| `pull_request` Appliance image | 1,525 |
| `pull_request` Mutants | 1,317 |
| `push` CI | 542 |
| everything else | 851 |

The longest group, #2250's, ran from 08:49:13 to 12:49:19, the length of
`check_response_timeout_minutes`. It merged, so either GitHub's clock starts
later than the group or it had seconds to spare; either way the margin is gone.
A timeout drops the entry and rebuilds every entry behind it.

## Answers

**(a)** A compatible pair is built as two stacked, one-PR groups at once, and
each lands as soon as it and everything ahead of it have passed. With
`ALLGREEN`, a slow entry in front holds every passed entry behind it. A
conflicting pair is caught at once and costs no runners: the second PR is
marked `UNMERGEABLE`, never built, and skipped. But it's silent, the PR keeps
its place until it reaches the head, and only then is it dropped, an hour or
two later.

**(b) Not 1. Lower it from 8 to 3.** Build concurrency doesn't change how
many runner-minutes the queue uses: every entry is built once whatever the
setting (27 builds, 21 merges, the other six in flight), and a rebuild only
follows a failure ahead. What it changes is how many groups compete for
runners at once and how much a failure throws away.

- **1 is too slow.** A group with runners to spare takes about 70 to 80
  minutes (#2254: 70, #2235: 79), so one at a time lands at most about 0.8 PRs
  an hour. The 0.3.3 queue took 27 entries in 12 hours, 2.25 an hour, and
  landed 21. At concurrency 1 the backlog would have grown by about 17 entries
  over the day.
- **8 is more than the runners can serve.** Eight groups ask for 144 jobs
  against about 20 runners. The queue's own groups and the train PRs that feed
  it wait on each other, a group takes 136 minutes instead of 75, and the
  slowest reached the 240-minute timeout. A failure at the front throws away up
  to seven groups behind it, about 1,900 runner-minutes; at 3 it's two groups,
  about 550.
- **3 matches the work.** By Little's law, 2.25 entries an hour at about 75
  minutes each needs about 2.8 groups in flight. Three keeps up with this
  week's arrival rate at an uncontended group's length, caps the queue at 54
  jobs, and leaves the rest of the runners to the train PRs.

## Recommendations

1. **Ruleset (maintainer):** `max_entries_to_build` 8 → 3 and
   `check_response_timeout_minutes` 240 → 360. Keep `ALLGREEN`: it is what
   makes "every commit on a train passed every job" true, and whether turning
   it off would land a passed entry while the one ahead is still running isn't
   documented (see the live test below). Keep `max_entries_to_merge` 8, which
   only lets passed entries land in one push, and `min_entries_to_merge` 1
   with its 5-minute wait. Only an admin can make the change:

   ```
   gh api repos/reliaburger/reliaburger/rulesets/25015244 \
     | jq '{name, target, enforcement, conditions, bypass_actors,
            rules: (.rules | map(if .type == "merge_queue"
              then .parameters.max_entries_to_build = 3
                 | .parameters.check_response_timeout_minutes = 360
              else . end))}' \
     | gh api -X PUT repos/reliaburger/reliaburger/rulesets/25015244 --input -
   gh api repos/reliaburger/reliaburger/rulesets/25015244 \
     | jq '.rules[] | select(.type == "merge_queue") | .parameters'
   ```

2. **Check against the queue's tip before queueing:**
   `python3 scripts/ci/queue_conflicts.py <PR>` (added with this record). It
   merges the PR's head onto the head of the last built entry with `git
   merge-tree`, and when that conflicts names the first entry that brought the
   conflict in. Replayed on the four PRs above it refuses all four, naming
   #2249, #2234, #2234 and #2253. A check against the train refuses none of
   them. It also refuses a PR the queue already marks `UNMERGEABLE`. Run it
   before `gh pr merge --merge` and after every queue event; a PR it refuses
   waits for the named entry to land, then merges the train in.
3. **No `merge=union`** for `docs/flakes.md` or `CHANGELOG.md`; see below.
4. **No serialising** of `CHANGELOG.md` and `docs/flakes.md` edits beyond the
   check. Every conflict this week was found and skipped without wasting a
   runner; what it cost was the hours before anyone knew, which the check
   removes. Batching changelog lines into one PR per train would move the
   conflict, not remove it, and lose the line from the PR that earned it.

## Why not `merge=union`

`merge=union` resolves a textual conflict by keeping both sides' lines, ours
first. It's safe for a file whose changes are only ever appended lines in an
order nobody reads. Neither file is that.

**`docs/flakes.md`** rows change as often as they're added: of the 43
non-merge commits to it on the train since 10 November, 24 changed or removed
existing lines, mostly a row's status going from Open to Fixed. Replayed with
`git merge-file --union` on this week's real pairs, #2268 against #2234 and
#2266 against #2234 come out right, but #2234 against #2249 doesn't. Both
sides carried #2236's `job_recovery::killed_bun_retires_api_exec_and_adopts_the_original_workload`
row: #2249 had rewritten it to "Fixed", while #2234 still had the "Open" one.
Union keeps both, so the register would have listed one test twice, once
still open with a closed issue, and nothing in the queue's checks reads that
file's rows (the register check runs on PRs and nightly). The real resolution,
on the train at `95c81937`, has the row once.

**`CHANGELOG.md`** is mostly appends (19 commits, +645 −17), and #2255
against #2253, its real conflict, comes out right: both bullets under
"Added". But a changelog's lines belong to the heading above them, and union
doesn't know that. In synthetic cases (`scripts/ci/test_queue_conflicts.py`):

- one side adds a new `### Fixed` subsection at the end while the other
  appends a bullet to the last subsection: union files the bullet under
  Fixed;
- one side rewords a bullet while the other adds the next one: union keeps
  both wordings.

`test_changelog.py` catches a duplicated heading, but not a bullet under the
wrong one or a stale copy. The second shape is common in an Unreleased section
that grows a subsection at a time. A conflict a person resolves in a minute is
better than a wrong changelog line nobody sees until the release notes.

Whether GitHub's server-side merge even reads `.gitattributes` for the queue
wasn't tested; with both files ruled out, it doesn't matter.
`test_queue_conflicts.py` pins that neither file has a union driver, with the
three cases above.

## A live test for after 0.3.3

One question history couldn't answer: with "only merge non-failing pull
requests" off (`HEADGREEN`), does the queue land an entry whose group passed
while the entry ahead is still running, as #2258 could have at 07:11? If it
does, a slow head stops holding the queue, at the price of commits on the
train whose own group never finished. To find out, once 0.3.3 is out and its
queue is empty:

1. The maintainer sets `grouping_strategy` to `HEADGREEN` on the ruleset for
   the next train.
2. Queue two real, reviewed PRs for that train, a code PR first and a
   docs-only PR second; the docs-only group skips the code jobs, so it passes
   about an hour before the one ahead.
3. Record when each lands, then set `ALLGREEN` back unless the maintainer
   decides otherwise.

No throwaway PRs and no disruption to a queue carrying a release.
