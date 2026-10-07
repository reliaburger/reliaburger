# Large cron steps panic in debug and schedule every minute in release

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Current library/debug and exact-source/release reproduced.

### Problem


`CronSchedule::parse` parses a positive step as `u8` and increments a `u8` without checking overflow. `schedule = "59/255 * * * *"` is accepted syntax and should match minute 59 once per hour. Debug builds panic. Release builds wrap and parse all minutes 0–59, so an expensive or destructive hourly task runs every minute.

The cron expression is parsed inside `begin_deploy -> register_scheduled_jobs`, on the agent command loop. A debug-build apply can panic the long-lived agent task. Registered schedules are parsed again at startup, so persisted bad schedules can also affect restart.

### Verified reproduction


- Exact current-library call, caught with `catch_unwind`: `CronSchedule::parse("59/255 * * * *")` panics at [src/meat/cron.rs:180](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L180), `attempt to add with overflow`.
- Compiled exact source with `-C opt-level=3 -C overflow-checks=off`: it matched all 60 minutes. Probe `evidence/cron_release.rs`.

### Evidence / fix


- [src/meat/cron.rs:153](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L153): step `u8`.
- [src/meat/cron.rs:177–180](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L177-L180): unchecked increment.
- [src/bun/agent/job_runs.rs:322](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/job_runs.rs#L322): parsing on deploy loop.
- [src/bun/agent/records.rs:414](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/records.rs#L414): parsing persisted schedule on recovery.

Use wider/checking arithmetic and stop once the next step is beyond the bounded field. Validate schedules before admitting apply. Test maximal positive steps, boundary starts, lists/ranges, both build profiles, and prove the exact intended firing times.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/meat/cron.rs:150–160](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L150-L160)

```rust
    for term in token.split(',') {
        let (base, step) = match term.split_once('/') {
            Some((base, step)) => {
                let step: u8 = step.parse().map_err(|_| malformed())?;
                if step == 0 {
                    return Err(malformed());
                }
                (base, step)
            }
            None => (term, 1),
        };
```

[src/meat/cron.rs:173–184](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L173-L184)

```rust

        if start > end {
            return Err(malformed());
        }
        let mut v = start;
        while v <= end {
            values.insert(normalise(field, v));
            v += step;
        }
    }

    Ok(CronField { any: false, values })
```

[src/bun/agent/job_runs.rs:313–328](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/job_runs.rs#L313-L328)

```rust
            let namespace = spec
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let key = (name.clone(), namespace.clone());
            let Some(expression) = spec.schedule.as_deref() else {
                next.remove(&key);
                continue;
            };
            let schedule = crate::meat::cron::CronSchedule::parse(expression)
                .map_err(|error| BunError::ScheduleState(error.to_string()))?;
            let last_fired_minute = next
                .get(&key)
                .and_then(|existing| existing.last_fired_minute);
            next.insert(
                key,
```
