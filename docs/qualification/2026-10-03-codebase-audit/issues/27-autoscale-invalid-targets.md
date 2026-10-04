# Autoscaling accepts nonpositive and nonfinite targets, disabling or corrupting scaling decisions

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Current Config validation accepts all four invalid values.

### Problem


Autoscale target parsing only checks whether Rust can parse an `f64`. `target="0%"`, `"-20%"`, `"NaN"`, and `"inf"` all pass `Config::validate` and apply validation.

Zero/negative targets make `compute_desired` return the current count forever. NaN produces NaN ratios/casts, then fails the hysteresis comparisons, normally preserving the current count. An infinite target yields a zero finite-load ratio and can scale down to the minimum regardless of load. These invalid control parameters look like successfully enabled autoscaling but either disable it or produce incorrect control decisions. Do not reject finite targets above 100% automatically: utilization is measured against a request, so a target above a request can be legitimate.

### Verified reproduction / evidence


Current-library `Config::validate` probe printed `true` for each listed target.

- [src/meat/autoscaler.rs:159–163](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L159-L163): only parse failure rejected.
- [src/meat/autoscaler.rs:385–392](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L385-L392): returns any parsed float.
- [src/meat/autoscaler.rs:307–326](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L307-L326): zero/negative short circuit; unchecked nonfinite ratio/hysteresis.
- [src/config/validate.rs:387–392](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/config/validate.rs#L387-L392): delegates to that parser for validation.

Reject nonfinite or nonpositive targets with the existing clear config error. Defensively refuse nonfinite collected metrics too. Test invalid target values through lint/apply and valid fraction/percentage targets, including a legitimate >100% utilization target. This is separate from closed #299's `min=0` problem.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/meat/autoscaler.rs:159–166](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L159-L166)

```rust
        let target =
            parse_percentage(&spec.target).ok_or_else(|| AutoscaleConfigError::InvalidTarget {
                target: spec.target.clone(),
            })?;
        if spec.max == 0 {
            return Err(AutoscaleConfigError::ZeroMax);
        }
        // Scale-to-zero would need a wake-up signal that exists without a
```

[src/meat/autoscaler.rs:385–394](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L385-L394)

```rust
fn parse_percentage(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        pct.trim().parse::<f64>().ok().map(|v| v / 100.0)
    } else {
        // Try as a raw fraction
        s.parse::<f64>().ok()
    }
}

```

[src/meat/autoscaler.rs:307–330](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L307-L330)

```rust
fn compute_desired(current: u32, metric: f64, config: &AutoscaleConfig) -> u32 {
    if config.target <= 0.0 || current == 0 {
        return current;
    }

    let ratio = metric / config.target;
    let raw_desired = (current as f64 * ratio).ceil() as u32;

    // Hysteresis: only scale down if metric is well below target
    let desired = if raw_desired < current {
        if metric < config.target * config.scale_down_threshold {
            raw_desired
        } else {
            current // not low enough to scale down
        }
    } else {
        raw_desired
    };

    desired.clamp(config.min, config.max)
}

/// Manage autoscale state for all apps.
#[derive(Debug, Default)]
```
