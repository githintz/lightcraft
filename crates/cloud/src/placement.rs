//! Choosing which account(s) store a blob. A pure function of the accounts' state, so the same
//! inputs always give the same answer (ties go to the smaller account id).
//!
//! Port of the Storage Distributor placement engine (its `src/placement/engine.ts`, PLAN.md §5.3):
//! keep only accounts that accept uploads, have room, allow the size and are fast enough for the
//! requested [`Availability`], then rank them by
//!
//! ```text
//! space · free share after the upload  +  speed · fit to the level's ideal speed
//! + reliability · recent success rate  +  priority · user preference
//! − overkill · how much faster than needed the account is
//! ```
//!
//! Redundant copies go to accounts on *different* providers, so one provider closing an account
//! or going down can't lose the file.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Availability, SpeedClass};

/// Most copies a request may ask for (more is clamped).
pub const MAX_COPIES: u8 = 8;

/// The share of an account's real quota used when the user set no cap: leaves room for the
/// provider's own use (e.g. mail on a mail-and-drive account).
pub const DEFAULT_CAP_SHARE: f64 = 0.9;

/// Everything placement needs to know about one account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacementAccount {
    pub id: String,
    /// Which provider runs the account (`"s3"`, `"webdav"`, `"folder"`…): copies of one blob go
    /// to different providers.
    pub provider: String,
    /// Bytes still usable, see [`usable_free`].
    pub free_bytes: u64,
    /// Bytes this library may use on the account in total (the cap, or the quota share).
    pub capacity_bytes: u64,
    pub speed: SpeedClass,
    /// 0–1, from the recent error / rate-limit rate.
    pub reliability: f64,
    /// 0–1, the user's preference.
    pub priority: f64,
    pub accepts_uploads: bool,
    /// The provider's per-file limit, if it has one.
    pub max_file_size: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRequest {
    pub size: u64,
    pub level: Availability,
    /// Copies on different providers; 0 counts as 1, more than [`MAX_COPIES`] as that.
    pub copies: u8,
}

impl PlacementRequest {
    pub fn new(size: u64, level: Availability) -> Self {
        PlacementRequest { size, level, copies: 1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Weights {
    pub space: f64,
    pub speed: f64,
    pub reliability: f64,
    pub priority: f64,
    pub overkill: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Weights { space: 1.0, speed: 0.5, reliability: 0.5, priority: 0.3, overkill: 0.6 }
    }
}

/// Why nothing could be chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum PlacementError {
    #[error("No connected account accepts uploads. Connect an account first.")]
    NoAccounts,
    #[error("This file is larger than any account allows.")]
    TooLarge,
    #[error("No account has enough free space for this file.")]
    NoSpace,
    /// `suggest` is the fastest slower level that would work.
    #[error("No account is fast enough for “{}”; “{}” would work.", .requested.label(), .suggest.label())]
    TooSlow { requested: Availability, suggest: Availability },
    #[error("Not enough different providers to store the requested number of copies.")]
    NotEnoughProviders,
}

/// Bytes still usable on an account: the smaller of what the provider has left and what the
/// user's cap leaves for this library. Without a cap, [`DEFAULT_CAP_SHARE`] of the quota.
/// `quota_total: None` (a provider that doesn't say) leaves only the cap; no cap either → 0,
/// so nothing is written to an account of unknown size until the user sets a cap.
pub fn usable_free(quota_total: Option<u64>, quota_used: u64, cap: Option<u64>, used_by_library: u64) -> u64 {
    let provider_left = quota_total.map(|t| t.saturating_sub(quota_used));
    let cap = cap.or_else(|| quota_total.map(|t| (t as f64 * DEFAULT_CAP_SHARE) as u64));
    let cap_left = cap.map(|c| c.saturating_sub(used_by_library));
    match (provider_left, cap_left) {
        (Some(p), Some(c)) => p.min(c),
        (None, Some(c)) => c,
        (Some(_), None) | (None, None) => 0,
    }
}

/// A 0–1 input from metrics or settings; NaN and out-of-range values are clamped.
fn unit(x: f64) -> f64 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}

fn eligible(a: &PlacementAccount, size: u64, level: Availability) -> bool {
    a.accepts_uploads && a.free_bytes >= size && a.max_file_size.is_none_or(|max| size <= max) && a.speed.rank() >= level.min_speed().rank()
}

/// How well `a` suits the request; higher is better. Finite for any input.
pub fn score(a: &PlacementAccount, req: &PlacementRequest, w: &Weights) -> f64 {
    let rank = f64::from(a.speed.rank());
    let ideal = f64::from(req.level.ideal_speed().rank());
    let free_after = a.free_bytes.saturating_sub(req.size);
    let free_ratio = if a.capacity_bytes > 0 { unit(free_after as f64 / a.capacity_bytes as f64) } else { 0.0 };
    let speed_fit = if rank >= ideal { 1.0 } else { rank / ideal };
    let overkill = (rank - ideal).max(0.0) / 3.0;
    let s = w.space * free_ratio + w.speed * speed_fit + w.reliability * unit(a.reliability) + w.priority * unit(a.priority) - w.overkill * overkill;
    if s.is_finite() { s } else { f64::MIN }
}

/// The account(s) that should store a blob of `req.size` bytes, best first.
pub fn choose<'a>(accounts: &'a [PlacementAccount], req: &PlacementRequest, w: &Weights) -> Result<Vec<&'a PlacementAccount>, PlacementError> {
    let copies = usize::from(req.copies.clamp(1, MAX_COPIES));
    let open: Vec<&PlacementAccount> = accounts.iter().filter(|a| a.accepts_uploads).collect();
    if open.is_empty() {
        return Err(PlacementError::NoAccounts);
    }

    let mut ranked: Vec<(&PlacementAccount, f64)> =
        open.iter().copied().filter(|a| eligible(a, req.size, req.level)).map(|a| (a, score(a, req, w))).collect();
    if ranked.is_empty() {
        if open.iter().all(|a| a.max_file_size.is_some_and(|max| req.size > max)) {
            return Err(PlacementError::TooLarge);
        }
        if !open.iter().any(|a| a.free_bytes >= req.size) {
            return Err(PlacementError::NoSpace);
        }
        let mut level = req.level.slower();
        while let Some(l) = level {
            if open.iter().any(|a| eligible(a, req.size, l)) {
                return Err(PlacementError::TooSlow { requested: req.level, suggest: l });
            }
            level = l.slower();
        }
        return Err(PlacementError::NoSpace);
    }

    ranked.sort_by(|(x, sx), (y, sy)| sy.total_cmp(sx).then_with(|| x.id.cmp(&y.id)));
    let mut chosen: Vec<&PlacementAccount> = Vec::with_capacity(copies);
    for (a, _) in ranked {
        if chosen.len() == copies {
            break;
        }
        if copies > 1 && chosen.iter().any(|c| c.provider == a.provider) {
            continue;
        }
        chosen.push(a);
    }
    if chosen.len() < copies {
        return Err(PlacementError::NotEnoughProviders);
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn acct(id: &str) -> PlacementAccount {
        PlacementAccount {
            id: id.into(),
            provider: id.into(),
            free_bytes: 10 * GB,
            capacity_bytes: 15 * GB,
            speed: SpeedClass::Standard,
            reliability: 1.0,
            priority: 0.5,
            accepts_uploads: true,
            max_file_size: None,
        }
    }

    fn with(id: &str, f: impl FnOnce(&mut PlacementAccount)) -> PlacementAccount {
        let mut a = acct(id);
        f(&mut a);
        a
    }

    fn ids(r: Result<Vec<&PlacementAccount>, PlacementError>) -> Vec<String> {
        r.map(|v| v.iter().map(|a| a.id.clone()).collect()).unwrap_or_default()
    }

    fn req(size: u64, level: Availability) -> PlacementRequest {
        PlacementRequest::new(size, level)
    }

    // The ten cases of the Storage Distributor's engine.test.ts, same inputs and answers.

    #[test]
    fn fails_without_accounts() {
        assert_eq!(choose(&[], &req(1, Availability::Standard), &Weights::default()), Err(PlacementError::NoAccounts));
    }

    #[test]
    fn prefers_more_free_space() {
        let a = [with("a", |a| a.free_bytes = 2 * GB), with("b", |a| a.free_bytes = 12 * GB)];
        assert_eq!(ids(choose(&a, &req(GB, Availability::Standard), &Weights::default())), ["b"]);
    }

    #[test]
    fn respects_caps_through_free_bytes() {
        let a = [with("a", |a| a.free_bytes = GB / 2)];
        assert_eq!(choose(&a, &req(GB, Availability::Standard), &Weights::default()), Err(PlacementError::NoSpace));
    }

    #[test]
    fn instant_uses_only_fast_accounts() {
        let a = [
            with("slow", |a| (a.speed, a.free_bytes) = (SpeedClass::Slow, 14 * GB)),
            with("fast", |a| (a.speed, a.free_bytes) = (SpeedClass::Fast, 3 * GB)),
        ];
        assert_eq!(ids(choose(&a, &req(GB, Availability::Instant), &Weights::default())), ["fast"]);
    }

    #[test]
    fn keeps_fast_accounts_free_for_archive_files() {
        let a = [with("fast", |a| a.speed = SpeedClass::Fast), with("cold", |a| a.speed = SpeedClass::Archive)];
        assert_eq!(ids(choose(&a, &req(GB, Availability::Archive), &Weights::default())), ["cold"]);
    }

    #[test]
    fn suggests_a_slower_level() {
        let a = [with("a", |a| a.speed = SpeedClass::Slow)];
        assert_eq!(
            choose(&a, &req(GB, Availability::Instant), &Weights::default()),
            Err(PlacementError::TooSlow { requested: Availability::Instant, suggest: Availability::Slow })
        );
    }

    #[test]
    fn honours_per_file_limits() {
        let a = [with("box", |a| a.max_file_size = Some(250 * 1024 * 1024))];
        assert_eq!(choose(&a, &req(GB, Availability::Standard), &Weights::default()), Err(PlacementError::TooLarge));
    }

    #[test]
    fn skips_accounts_closed_to_uploads() {
        let a = [with("a", |a| (a.accepts_uploads, a.free_bytes) = (false, 14 * GB)), acct("b")];
        assert_eq!(ids(choose(&a, &req(GB, Availability::Standard), &Weights::default())), ["b"]);
    }

    #[test]
    fn copies_go_to_different_providers() {
        let a = [
            with("g1", |a| (a.provider, a.free_bytes) = ("gdrive".into(), 14 * GB)),
            with("g2", |a| (a.provider, a.free_bytes) = ("gdrive".into(), 13 * GB)),
            with("d1", |a| (a.provider, a.free_bytes) = ("dropbox".into(), 2 * GB)),
        ];
        let r = PlacementRequest { copies: 2, ..req(GB, Availability::Standard) };
        assert_eq!(ids(choose(&a, &r, &Weights::default())), ["g1", "d1"]);
    }

    #[test]
    fn too_few_providers_for_the_copies() {
        let a = [with("g1", |a| a.provider = "gdrive".into()), with("g2", |a| a.provider = "gdrive".into())];
        let r = PlacementRequest { copies: 2, ..req(GB, Availability::Standard) };
        assert_eq!(choose(&a, &r, &Weights::default()), Err(PlacementError::NotEnoughProviders));
    }

    // LightCraft additions: hostile numbers and the free-space rule.

    #[test]
    fn hostile_numbers_never_panic_or_win() {
        let a = [
            with("nan", |a| (a.reliability, a.priority) = (f64::NAN, f64::INFINITY)),
            with("zero", |a| a.capacity_bytes = 0),
            with("huge", |a| (a.free_bytes, a.capacity_bytes) = (u64::MAX, 1)),
            acct("ok"),
        ];
        let w = Weights { space: f64::NAN, ..Weights::default() };
        for level in Availability::ALL {
            for size in [0, 1, GB, u64::MAX] {
                let _ = choose(&a, &PlacementRequest { size, level, copies: u8::MAX }, &w);
                for x in &a {
                    assert!(score(x, &PlacementRequest::new(size, level), &Weights::default()).is_finite());
                }
            }
        }
        // a NaN reliability counts as 0, not as the best account
        let b = [with("nan", |a| a.reliability = f64::NAN), acct("ok")];
        assert_eq!(ids(choose(&b, &req(GB, Availability::Standard), &Weights::default())), ["ok"]);
    }

    #[test]
    fn ties_go_to_the_smaller_id() {
        let a = [acct("b"), acct("a")];
        assert_eq!(ids(choose(&a, &req(GB, Availability::Standard), &Weights::default())), ["a"]);
    }

    #[test]
    fn usable_free_takes_the_tighter_limit() {
        // 15 GB quota, 5 GB used by the provider's own data, no cap: 90 % share = 13.5 GB cap
        assert_eq!(usable_free(Some(15 * GB), 5 * GB, None, 0), 10 * GB);
        assert_eq!(usable_free(Some(15 * GB), 0, Some(4 * GB), GB), 3 * GB);
        assert_eq!(usable_free(None, 0, Some(4 * GB), 5 * GB), 0);
        assert_eq!(usable_free(None, 0, None, 0), 0, "unknown size and no cap: write nothing");
        assert_eq!(usable_free(Some(u64::MAX), u64::MAX, None, u64::MAX), 0);
    }
}
