//! Pool semantics — a port of hermes `credential_pool.py` +
//! `fallback_cooldown.py` reduced to API-key refs (no OAuth refresh, no
//! singleton auth stores; gray resolves those through its own plugins).
//!
//! One entry = one key reference. A failure marks the entry `exhausted`
//! with a TTL; `select` skips hot entries and auto-clears expired ones.
//! `dead` is sticky (operator-set, cleared only by `clear`).

use serde::{Deserialize, Serialize};

pub const STRATEGY_FILL_FIRST: &str = "fill_first";
pub const STRATEGY_ROUND_ROBIN: &str = "round_robin";
pub const STRATEGY_RANDOM: &str = "random";
pub const STRATEGY_LEAST_USED: &str = "least_used";
pub const STRATEGIES: &[&str] = &[
    STRATEGY_FILL_FIRST,
    STRATEGY_ROUND_ROBIN,
    STRATEGY_RANDOM,
    STRATEGY_LEAST_USED,
];

pub const STATUS_OK: &str = "ok";
pub const STATUS_EXHAUSTED: &str = "exhausted";
pub const STATUS_DEAD: &str = "dead";

/// Cooldown seconds by HTTP status (hermes `EXHAUSTED_TTL_*`).
pub const TTL_401_SECONDS: f64 = 5.0 * 60.0;
pub const TTL_429_SECONDS: f64 = 60.0 * 60.0;
pub const TTL_DEFAULT_SECONDS: f64 = 60.0 * 60.0;
/// A pool with nothing to rotate to gets only a brief bench for transient
/// failures — a spent-key block (billing) still keeps the full TTL.
pub const TTL_SOLE_CREDENTIAL_SECONDS: f64 = 60.0;
/// Resolution failures (env unset, file gone, cmd failing) get a short
/// bench: they are deterministic config problems, not upstream throttles.
pub const TTL_RESOLVE_SECONDS: f64 = 60.0;

pub const FAILURE_BILLING: &str = "billing";
pub const FAILURE_RATE_LIMIT: &str = "rate_limit";
pub const FAILURE_AUTH: &str = "auth";
pub const FAILURE_RESOLVE: &str = "resolve_error";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PooledKey {
    /// Short hex id (`add` mints, `mark`/`remove` match on it).
    pub id: String,
    /// Display label — defaults to the ref, user-overridable. Refs are
    /// safe to show; material never is.
    #[serde(default)]
    pub label: String,
    /// `env:` / `file:` / `cmd:` / `auth:` reference (see [`crate::keys`]).
    pub key_ref: String,
    /// Selection order, low first. `round_robin` rewrites these.
    #[serde(default)]
    pub priority: u32,
    /// ok | exhausted | dead.
    #[serde(default = "default_status")]
    pub status: String,
    /// Epoch seconds the status was last set.
    #[serde(default)]
    pub status_at: Option<f64>,
    #[serde(default)]
    pub last_error_code: Option<u16>,
    #[serde(default)]
    pub last_error_reason: Option<String>,
    #[serde(default)]
    pub last_error_message: Option<String>,
    /// Provider-declared reset instant (Retry-After / reset headers).
    /// Overrides the computed TTL while in the future.
    #[serde(default)]
    pub reset_at: Option<f64>,
    /// Selections served — `least_used`'s baseline.
    #[serde(default)]
    pub request_count: u64,
}

fn default_status() -> String {
    STATUS_OK.to_string()
}

impl PooledKey {
    pub fn new(key_ref: &str, label: Option<String>, priority: u32) -> Self {
        Self {
            id: crate::hex_id(),
            label: label.unwrap_or_else(|| key_ref.to_string()),
            key_ref: key_ref.to_string(),
            priority,
            status: STATUS_OK.to_string(),
            status_at: None,
            last_error_code: None,
            last_error_reason: None,
            last_error_message: None,
            reset_at: None,
            request_count: 0,
        }
    }
}

/// Cooldown seconds for an HTTP status — hermes `_exhausted_ttl`.
///
/// `sole`: the pool has one entry total; transient failures get the short
/// cap so a lone key keeps retrying, while billing keeps the full bench.
/// `reason`: classifier verdict; "billing" forces the full TTL.
pub fn exhausted_ttl(code: Option<u16>, sole: bool, reason: Option<&str>) -> f64 {
    if code == Some(401) {
        return TTL_401_SECONDS;
    }
    let base = if code == Some(429) {
        TTL_429_SECONDS
    } else {
        TTL_DEFAULT_SECONDS
    };
    let billing = code == Some(402) || reason == Some(FAILURE_BILLING);
    if sole && !billing {
        return base.min(TTL_SOLE_CREDENTIAL_SECONDS);
    }
    base
}

/// Epoch until which an entry stays benched — `reset_at` wins over the
/// computed TTL (provider knows better), else `status_at + ttl`.
pub fn exhausted_until(entry: &PooledKey, sole: bool) -> Option<f64> {
    if entry.status != STATUS_EXHAUSTED {
        return None;
    }
    if entry.reset_at.is_some_and(|t| t > 0.0) {
        return entry.reset_at;
    }
    entry.status_at.map(|at| {
        at + exhausted_ttl(entry.last_error_code, sole, entry.last_error_reason.as_deref())
    })
}

/// Reason string from a status code when the caller gave none.
pub fn classify_reason(code: Option<u16>) -> &'static str {
    match code {
        Some(401) | Some(403) => FAILURE_AUTH,
        Some(402) => FAILURE_BILLING,
        Some(429) => FAILURE_RATE_LIMIT,
        _ => "error",
    }
}

/// Statuses that justify rotating to another key: auth/quota/throttle and
/// transient upstream 5xx. 400/404/422 are request problems — rotating
/// replays the same failure.
pub fn is_rotatable_status(code: Option<u16>) -> bool {
    match code {
        Some(401) | Some(402) | Some(403) | Some(408) | Some(409) | Some(429) => true,
        Some(s) if s >= 500 => true,
        // Transport failure (no HTTP status): another key rarely helps but
        // retrying the same one never does either; rotate so one flakey
        // credential does not pin the pool.
        None => true,
        _ => false,
    }
}

fn sole(pool: &crate::store::PoolFile) -> bool {
    pool.entries.iter().filter(|e| e.status != STATUS_DEAD).count() <= 1
}

/// Lift expired cooldowns in place. Returns true when anything changed
/// (caller persists).
pub fn clear_expired(pool: &mut crate::store::PoolFile, now: f64) -> bool {
    let is_sole = sole(pool);
    let mut changed = false;
    for e in &mut pool.entries {
        if e.status == STATUS_EXHAUSTED
            && exhausted_until(e, is_sole).is_some_and(|until| now >= until)
        {
            e.status = STATUS_OK.to_string();
            e.status_at = Some(now);
            e.reset_at = None;
            changed = true;
        }
    }
    changed
}

/// Cold = not dead, not inside its cooldown window. Crate-internal.
pub fn cold_indices(pool: &crate::store::PoolFile, now: f64) -> Vec<usize> {
    let is_sole = sole(pool);
    pool.entries
        .iter()
        .enumerate()
        .filter(|(_, e)| match e.status.as_str() {
            s if s == STATUS_DEAD => false,
            s if s == STATUS_EXHAUSTED => exhausted_until(e, is_sole)
                .is_none_or(|until| now >= until),
            _ => true,
        })
        .map(|(i, _)| i)
        .collect()
}

/// Pick the next cold entry by strategy, bump `request_count`, set
/// `last_selected_id`. Caller persists the store. `now` = epoch seconds.
///
/// round_robin reprioritizes the pick to the back so order advances even
/// across processes (the pool file is the shared memory).
pub fn select(pool: &mut crate::store::PoolFile, now: f64) -> Option<usize> {
    clear_expired(pool, now);
    let cold = cold_indices(pool, now);
    if cold.is_empty() {
        pool.last_selected_id = None;
        return None;
    }
    let pick = match pool.strategy.as_str() {
        STRATEGY_RANDOM => cold[(crate::now().to_bits() as usize ^ cold.len().wrapping_mul(2654435761)) % cold.len()],
        STRATEGY_LEAST_USED => *cold
            .iter()
            .min_by_key(|&&i| (pool.entries[i].request_count, pool.entries[i].priority))
            .unwrap(),
        _ => *cold.iter().min_by_key(|&&i| pool.entries[i].priority).unwrap(),
    };
    pool.entries[pick].request_count += 1;
    pool.entries[pick].status_at.get_or_insert(now);
    pool.last_selected_id = Some(pool.entries[pick].id.clone());
    if pool.strategy == STRATEGY_ROUND_ROBIN && pool.entries.len() > 1 {
        let back = pool.entries.iter().map(|e| e.priority).max().unwrap_or(0) + 1;
        pool.entries[pick].priority = back;
        let mut order: Vec<usize> = (0..pool.entries.len()).collect();
        order.sort_by_key(|&i| pool.entries[i].priority);
        for (new_prio, &i) in order.iter().enumerate() {
            pool.entries[i].priority = new_prio as u32;
        }
    }
    Some(pick)
}

/// Identify the entry that served a failed request: explicit `id`, else
/// the key *material* that failed (`key_hint` — matched against resolved
/// refs, never stored), else `last_selected_id`.
pub fn find_failed(
    pool: &crate::store::PoolFile,
    id: Option<&str>,
    key_hint: Option<&str>,
) -> Option<usize> {
    if let Some(id) = id {
        if let Some(i) = pool
            .entries
            .iter()
            .position(|e| e.id == id || e.label == id)
        {
            return Some(i);
        }
    }
    if let Some(hint) = key_hint {
        if let Some(i) = pool
            .entries
            .iter()
            .position(|e| crate::keys::resolve(&e.key_ref).is_ok_and(|k| k == hint))
        {
            return Some(i);
        }
    }
    pool.last_selected_id
        .as_deref()
        .and_then(|id| pool.entries.iter().position(|e| e.id == id))
}

/// Mark an entry hot. `reset_at` (provider-declared instant) overrides the
/// status-derived TTL. `dead` benches permanently until `clear`.
pub fn mark(
    pool: &mut crate::store::PoolFile,
    index: usize,
    code: Option<u16>,
    reason: Option<&str>,
    message: Option<String>,
    reset_at: Option<f64>,
    now: f64,
    dead: bool,
) {
    let e = &mut pool.entries[index];
    e.status = if dead { STATUS_DEAD } else { STATUS_EXHAUSTED }.to_string();
    e.status_at = Some(now);
    e.last_error_code = code;
    e.last_error_reason = Some(
        reason
            .map(str::to_string)
            .unwrap_or_else(|| classify_reason(code).to_string()),
    );
    e.last_error_message = message;
    e.reset_at = reset_at;
}

/// `mark` + pick the next cold entry. Returns the replacement index —
/// `None` when the pool is drained or the only candidate is the entry
/// just benched (hermes' no-recovery guard).
pub fn mark_and_rotate(
    pool: &mut crate::store::PoolFile,
    index: usize,
    code: Option<u16>,
    reason: Option<&str>,
    message: Option<String>,
    reset_at: Option<f64>,
    now: f64,
) -> Option<usize> {
    mark(pool, index, code, reason, message, reset_at, now, false);
    match select(pool, now) {
        Some(next) if next != index => Some(next),
        _ => {
            pool.last_selected_id = None;
            None
        }
    }
}

/// Earliest instant any benched entry comes back; `None` = something is
/// already cold (or the pool is empty).
pub fn next_available_at(pool: &crate::store::PoolFile, now: f64) -> Option<f64> {
    let is_sole = sole(pool);
    if cold_indices(pool, now).is_empty() {
        pool.entries
            .iter()
            .filter_map(|e| exhausted_until(e, is_sole))
            .reduce(f64::min)
    } else {
        None
    }
}

/// Rotate without marking anything (operator-driven `rotate`): demote the
/// current pick to the back of the priority order, then re-select.
pub fn rotate(pool: &mut crate::store::PoolFile, now: f64) -> Option<usize> {
    if let Some(id) = pool.last_selected_id.clone() {
        if let Some(i) = pool.entries.iter().position(|e| e.id == id) {
            let back = pool.entries.iter().map(|e| e.priority).max().unwrap_or(0) + 1;
            pool.entries[i].priority = back;
        }
    }
    select(pool, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PoolFile;

    fn pool_with(refs: &[&str]) -> PoolFile {
        PoolFile {
            entries: refs
                .iter()
                .enumerate()
                .map(|(i, r)| PooledKey {
                    priority: i as u32,
                    ..PooledKey::new(r, None, i as u32)
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn fill_first_picks_lowest_priority() {
        let mut p = pool_with(&["env:A", "env:B", "env:C"]);
        let i = select(&mut p, 100.0).unwrap();
        assert_eq!(p.entries[i].key_ref, "env:A");
        // fill_first keeps picking A until it is marked.
        assert_eq!(select(&mut p, 101.0), Some(0));
    }

    #[test]
    fn exhausted_entry_is_skipped_and_cleared() {
        let mut p = pool_with(&["env:A", "env:B"]);
        let now = 1000.0;
        mark(&mut p, 0, Some(429), None, None, None, now, false);
        let i = select(&mut p, now).unwrap();
        assert_eq!(p.entries[i].key_ref, "env:B");
        // 429 TTL = 1h; still hot at +30m.
        assert!(exhausted_until(&p.entries[0], false).unwrap() > now + 1800.0);
        // Cooled off at +3700s: select re-admits A and clears the mark.
        let i = select(&mut p, now + 3700.0).unwrap();
        assert_eq!(p.entries[i].key_ref, "env:A");
        assert_eq!(p.entries[0].status, STATUS_OK);
    }

    #[test]
    fn reset_at_overrides_ttl() {
        let mut p = pool_with(&["env:A", "env:B"]);
        let now = 1000.0;
        mark(&mut p, 0, Some(429), None, None, Some(now + 5.0), now, false);
        assert_eq!(exhausted_until(&p.entries[0], false), Some(now + 5.0));
        // After the declared reset the key is cold again.
        let i = select(&mut p, now + 6.0).unwrap();
        assert_eq!(p.entries[i].key_ref, "env:A");
    }

    #[test]
    fn sole_credential_caps_ttl() {
        let mut p = pool_with(&["env:A"]);
        let now = 0.0;
        mark(&mut p, 0, Some(429), None, None, None, now, false);
        // Sole + non-billing: 60s cap, not 1h.
        assert_eq!(exhausted_until(&p.entries[0], true), Some(now + 60.0));
        // Billing keeps the full bench.
        mark(&mut p, 0, Some(402), Some(FAILURE_BILLING), None, None, now, false);
        assert_eq!(exhausted_until(&p.entries[0], true), Some(now + 3600.0));
    }

    #[test]
    fn round_robin_rotates() {
        let mut p = pool_with(&["env:A", "env:B", "env:C"]);
        p.strategy = STRATEGY_ROUND_ROBIN.into();
        let mut picks = Vec::new();
        for t in 0..4 {
            let idx = select(&mut p, 100.0 + t as f64).unwrap();
            picks.push(p.entries[idx].key_ref.clone());
        }
        assert_eq!(picks, vec!["env:A", "env:B", "env:C", "env:A"]);
    }

    #[test]
    fn least_used_balances() {
        let mut p = pool_with(&["env:A", "env:B"]);
        p.strategy = STRATEGY_LEAST_USED.into();
        p.entries[0].request_count = 5;
        let i = select(&mut p, 0.0).unwrap();
        assert_eq!(p.entries[i].key_ref, "env:B");
    }

    #[test]
    fn drained_pool_reports_next_available() {
        let mut p = pool_with(&["env:A", "env:B"]);
        let now = 500.0;
        mark(&mut p, 0, Some(429), None, None, None, now, false);
        mark(&mut p, 1, Some(401), None, None, None, now, false);
        assert_eq!(select(&mut p, now), None);
        // 401 (300s) reopens before 429 (3600s).
        assert_eq!(next_available_at(&p, now), Some(now + 300.0));
    }

    #[test]
    fn mark_and_rotate_refuses_self_recovery() {
        let mut p = pool_with(&["env:A"]);
        let now = 0.0;
        // After marking the sole entry, its capped TTL still benches it:
        // rotation must report no-recovery, not re-hand the same key.
        assert_eq!(mark_and_rotate(&mut p, 0, Some(402), Some(FAILURE_BILLING), None, None, now), None);
    }

    #[test]
    fn find_failed_prefers_explicit_id() {
        let mut p = pool_with(&["env:A", "env:B"]);
        p.last_selected_id = Some(p.entries[0].id.clone());
        let id = p.entries[1].id.clone();
        assert_eq!(find_failed(&p, Some(&id), None), Some(1));
        assert_eq!(find_failed(&p, None, None), Some(0));
    }
}
