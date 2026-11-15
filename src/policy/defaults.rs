//! Fill-only defaults (#1118).
//!
//! A policy's `default` table names app fields and the values to give
//! them. [`fill`] copies a value in only where the app leaves the field
//! out, and never touches one the app sets, so filling twice is the same
//! as filling once. Tables merge key by key: a default `env.TEAM` adds
//! `TEAM` to an app's own `env` and leaves its other variables alone.
//!
//! A field counts as left out when the app's spec, serialised, doesn't
//! hold it or holds `null`. That's every optional field and every empty
//! list or map. `replicas` always has a value, so it can't be defaulted.

use serde_json::{Map, Value};

use crate::config::AppSpec;
use crate::meat::types::AppId;

/// One field a policy's `default` filled in.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Filled {
    /// The policy whose default it was.
    pub policy: String,
    /// The app it was filled in on.
    pub app: AppId,
    /// The field, as a dotted path (`env.TEAM`, `deploy.max_unavailable`).
    pub field: String,
    /// The value it was given.
    pub value: Value,
}

impl std::fmt::Display for Filled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "policy {} fills {} = {} on app {}/{}",
            self.policy, self.field, self.value, self.app.namespace, self.app.name
        )
    }
}

/// Copy every value in `fragment` into `target` where `target` leaves it
/// out, recursing into tables both hold. Pushes the dotted path and value
/// of each leaf it fills onto `filled`. A value `target` already holds,
/// of whatever type, wins.
pub(super) fn fill(
    target: &mut Map<String, Value>,
    fragment: &Map<String, Value>,
    prefix: &str,
    filled: &mut Vec<(String, Value)>,
) {
    for (key, value) in fragment {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let absent = matches!(target.get(key), None | Some(Value::Null));
        match (target.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(inner)) => {
                fill(existing, inner, &path, filled);
            }
            (_, Value::Object(inner)) if absent && !inner.is_empty() => {
                // Fill a missing table leaf by leaf, so each filled field
                // is reported on its own.
                let mut table = Map::new();
                fill(&mut table, inner, &path, filled);
                target.insert(key.clone(), Value::Object(table));
            }
            _ if absent => {
                target.insert(key.clone(), value.clone());
                filled.push((path, value.clone()));
            }
            _ => {}
        }
    }
}

/// `spec` with `fragment` filled in, and the fields filled, as dotted
/// paths with their values. When nothing was filled the spec comes back
/// unchanged. Fails when the filled spec isn't a valid app spec, which
/// the commit checks make rare: a default of the right type always fits.
pub(super) fn apply_default(
    spec: &AppSpec,
    fragment: &Map<String, Value>,
) -> Result<(AppSpec, Vec<(String, Value)>), String> {
    let Value::Object(mut json) = serde_json::to_value(spec).map_err(|error| error.to_string())?
    else {
        return Err("an app spec didn't serialise to a table".to_string());
    };
    let mut filled = Vec::new();
    fill(&mut json, fragment, "", &mut filled);
    if filled.is_empty() {
        return Ok((spec.clone(), filled));
    }
    let defaulted =
        serde_json::from_value(Value::Object(json)).map_err(|error| error.to_string())?;
    Ok((defaulted, filled))
}

/// The dotted path of the first leaf of `fragment` that `json` doesn't
/// hold, if any: a field serde dropped because no app keeps it.
pub(super) fn first_dropped(
    json: &Map<String, Value>,
    fragment: &Map<String, Value>,
    prefix: &str,
) -> Option<String> {
    fragment.iter().find_map(|(key, value)| {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match (json.get(key), value) {
            (None | Some(Value::Null), _) => Some(path),
            (Some(Value::Object(kept)), Value::Object(inner)) => first_dropped(kept, inner, &path),
            _ => None,
        }
    })
}
