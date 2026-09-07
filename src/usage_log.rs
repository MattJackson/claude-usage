//! Long-form usage history: an append-only NDJSON log of per-tick per-account
//! snapshots, rotated monthly, pruned after six months. Feeds the menu-bar
//! sparkline and pace estimator, and is the substrate for future burn-rate /
//! cost-tracking features.
//!
//! Path layout: `~/.config/claude-usage/history.YYYY-MM.ndjson`. One JSON
//! object per line, keyed by (`provider`, `account`), always in UTC.
//!
//! The append path is O_APPEND-atomic per line (single small `writeln!`), so
//! concurrent writers from the daemon and menu-bar can never interleave a
//! record. Writes are batched-fsynced (every 24 rows, or when the appender is
//! dropped) so a crash loses at most a couple of poll cycles.
//!
//! All I/O is best-effort from the caller's perspective — the poll loop must
//! never fail because history logging failed. Errors bubble up so callers can
//! log them, but the surrounding `refresh_usage_cache` swallows them.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::store;

/// One tick of cached-usage state for one account, keyed by (provider, account).
/// The whole point of the history log — every field is here so downstream
/// analyses (sparkline, pace, burn rate, cost) never have to re-derive them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub ts: DateTime<Utc>,
    pub provider: String,
    pub account: String,
    #[serde(default)]
    pub session_pct: Option<f32>,
    #[serde(default)]
    pub weekly_pct: Option<f32>,
    #[serde(default)]
    pub active_model: Option<String>,
}

/// Identity of an account within a provider — the row key in the history log.
/// Cheap to construct at any callsite (menu-bar rendering, pace estimator,
/// upcoming burn/cost modules). String-typed for portability across the state
/// v1 (email-keyed) and future v2 (bucket-keyed) layouts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AccountKey {
    pub provider: String,
    pub account: String,
}

impl AccountKey {
    pub fn new(provider: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            account: account.into(),
        }
    }

    fn matches(&self, s: &Snapshot) -> bool {
        s.provider == self.provider && s.account == self.account
    }
}

/// Weighted linear regression on `weekly_pct` within the current window.
/// `slope_pct_per_hour` is percentage points per hour; `confidence` is R² of
/// the fit (0..=1); `sample_count` is the number of samples used.
#[derive(Debug, Clone)]
pub struct PaceEstimate {
    pub slope_pct_per_hour: f64,
    pub confidence: f64,
    pub sample_count: usize,
}

/// Batch fsyncs every this many appended rows. The daemon polls on the order
/// of once every couple of minutes, so 24 rows is ~1 hour of history at 5 accounts
/// per tick, or ~1 day of history at a single account — a crash loses at most
/// that much before the next fsync (or the `Drop` fsync at process exit).
const FSYNC_BATCH: u32 = 24;

/// Retain monthly history files this many days by mtime. Six months is enough
/// for a full weekly-reset trend to be visible and short enough to bound disk
/// use to a few MB per account.
const RETAIN_DAYS: i64 = 6 * 31;

// ---------------------------------------------------------------------------
// Global appender + per-boot rotation
// ---------------------------------------------------------------------------

static APPENDER: OnceLock<Mutex<Option<Appender>>> = OnceLock::new();
static BOOT_ROTATED: AtomicBool = AtomicBool::new(false);

fn appender_slot() -> &'static Mutex<Option<Appender>> {
    APPENDER.get_or_init(|| Mutex::new(None))
}

/// The default log directory: `~/.config/claude-usage`. All public entry
/// points route through here so a single place decides where history lives.
fn log_dir() -> Result<PathBuf> {
    store::config_dir()
}

/// `history.YYYY-MM.ndjson` under `dir` for the UTC year/month of `ts`.
fn month_path(dir: &Path, ts: DateTime<Utc>) -> PathBuf {
    month_path_ym(dir, ts.year(), ts.month())
}

fn month_path_ym(dir: &Path, year: i32, month: u32) -> PathBuf {
    dir.join(format!("history.{year:04}-{month:02}.ndjson"))
}

/// Append `snap` to the current month's history file. Rotation-pruning runs
/// once per process (first append), month-boundary handoff is automatic (the
/// appender reopens when the target path changes). O_APPEND-atomic per line.
pub fn append(snap: &Snapshot) -> Result<()> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).context("creating history dir")?;
    // Prune stale month files on the first append per process.
    if !BOOT_ROTATED.swap(true, Ordering::SeqCst) {
        let _ = rotate_files(&dir, Utc::now());
    }
    let path = month_path(&dir, snap.ts);
    let mut guard = appender_slot().lock().unwrap();
    let reopen = guard.as_ref().map(|a| a.path != path).unwrap_or(true);
    if reopen {
        // Dropping the old appender flushes any un-fsynced rows.
        *guard = None;
        *guard = Some(Appender::open(&path)?);
    }
    guard.as_mut().unwrap().write_snap(snap)
}

/// Explicit rotation entry point: prune history files older than
/// `RETAIN_DAYS` and mark this process as having rotated. Cheap; safe to call
/// repeatedly. `now` is threaded through so tests can pin a moment in time.
pub fn rotate_if_needed(now: DateTime<Utc>) -> Result<()> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).context("creating history dir")?;
    rotate_files(&dir, now)?;
    BOOT_ROTATED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Snapshots for `account` within the last `days` days (UTC-anchored), sorted
/// ascending by timestamp. Reads only the month files that could overlap the
/// window, so a long history stays cheap to sample.
pub fn last_n_days(account: &AccountKey, days: u32) -> Vec<Snapshot> {
    let Ok(dir) = log_dir() else {
        return Vec::new();
    };
    let now = Utc::now();
    read_range(&dir, account, now - Duration::days(days as i64), now)
}

/// Most recent snapshot for `account` in the current + previous month's
/// history files, or `None` if this account has no rows yet. Used by the
/// notifications module to derive a "prev" for the pair-based `evaluate`.
pub fn last_snapshot(account: &AccountKey) -> Option<Snapshot> {
    let dir = log_dir().ok()?;
    let now = Utc::now();
    // Read enough history to survive a month boundary just after midnight UTC.
    let snaps = read_range(&dir, account, now - Duration::days(35), now);
    snaps.into_iter().next_back()
}

/// Seven-character sparkline of the last seven local days' peak weekly usage
/// for `account`, oldest bucket on the left. Missing day = `·`. Cutoffs at
/// 0/20/40/60/80 map to ▁▂▄▇█.
pub fn sparkline_7d(account: &AccountKey) -> String {
    let snaps = last_n_days(account, 7);
    sparkline_from_snaps(&snaps, Local::now().date_naive())
}

/// Pace of weekly-percent growth within the current window (i.e. since the
/// most recent reset), as slope + R² confidence + sample count. Returns None
/// if the current window has fewer than 6 samples or the slope is not
/// strictly positive.
pub fn pace(account: &AccountKey) -> Option<PaceEstimate> {
    let snaps = last_n_days(account, 8);
    pace_from_snaps(&snaps)
}

// ---------------------------------------------------------------------------
// Appender (private)
// ---------------------------------------------------------------------------

struct Appender {
    file: File,
    path: PathBuf,
    rows_since_fsync: u32,
}

impl Appender {
    fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).ok();
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Appender {
            file,
            path: path.to_path_buf(),
            rows_since_fsync: 0,
        })
    }

    fn write_snap(&mut self, snap: &Snapshot) -> Result<()> {
        let line = serde_json::to_string(snap).context("serializing snapshot")?;
        writeln!(self.file, "{line}").context("appending history line")?;
        self.rows_since_fsync += 1;
        if self.rows_since_fsync >= FSYNC_BATCH {
            let _ = self.file.sync_data();
            self.rows_since_fsync = 0;
        }
        Ok(())
    }
}

impl Drop for Appender {
    fn drop(&mut self) {
        if self.rows_since_fsync > 0 {
            let _ = self.file.sync_data();
        }
    }
}

// ---------------------------------------------------------------------------
// Directory helpers (pure / dir-parameterized for tests)
// ---------------------------------------------------------------------------

/// Prune history files under `dir` whose mtime is older than `RETAIN_DAYS`
/// days before `now`. Non-history files and unparsable names are left alone.
fn rotate_files(dir: &Path, now: DateTime<Utc>) -> Result<()> {
    let cutoff = now - Duration::days(RETAIN_DAYS);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if !(n.starts_with("history.") && n.ends_with(".ndjson")) {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        let Ok(mt) = md.modified() else { continue };
        let mt_utc: DateTime<Utc> = mt.into();
        if mt_utc < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Read every snapshot for `account` whose `ts` falls in `[from, to]` from
/// the month files under `dir` that overlap that range. Sorted ascending.
fn read_range(
    dir: &Path,
    account: &AccountKey,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Snapshot> {
    let mut out = Vec::new();
    let mut y = from.year();
    let mut m = from.month();
    let end = (to.year(), to.month());
    // Bound loop iterations by the range so a malformed `from > to` can't spin.
    loop {
        let path = month_path_ym(dir, y, m);
        if let Ok(f) = File::open(&path) {
            let rdr = BufReader::new(f);
            // `filter_map(|r| r.ok())` (not `map_while`) so a single corrupt or
            // invalid-UTF-8 line in the middle of the file doesn't halt the
            // iterator and silently drop every valid snapshot after it — a
            // partial/interleaved write or a crash mid-line would otherwise
            // make `last_snapshot`/`sparkline_7d`/`pace` return stale prefixes
            // with zero indication that data was truncated.
            for line in rdr.lines().filter_map(|r| r.ok()) {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(snap) = serde_json::from_str::<Snapshot>(&line) {
                    if snap.ts >= from && snap.ts <= to && account.matches(&snap) {
                        out.push(snap);
                    }
                }
            }
        }
        if (y, m) == end {
            break;
        }
        // Step forward one month, bounded so we don't run past `to`.
        if m == 12 {
            y += 1;
            m = 1;
        } else {
            m += 1;
        }
        // Defensive stop for the pathological `from > to` case.
        if y > end.0 || (y == end.0 && m > end.1) {
            break;
        }
    }
    out.sort_by_key(|s| s.ts);
    out
}

/// Pure: bucket `snaps` into the seven local days ending on `today_local` and
/// emit a 7-char sparkline (oldest on the left). Missing day = `·`.
fn sparkline_from_snaps(snaps: &[Snapshot], today_local: NaiveDate) -> String {
    // Peak weekly_pct per bucket (index 0 = 6 days ago, index 6 = today).
    let mut buckets: [Option<f32>; 7] = [None; 7];
    for s in snaps {
        let Some(pct) = s.weekly_pct else { continue };
        let local_day = s.ts.with_timezone(&Local).date_naive();
        let age = (today_local - local_day).num_days();
        if !(0..7).contains(&age) {
            continue;
        }
        let idx = 6 - age as usize;
        buckets[idx] = Some(buckets[idx].map(|prev| prev.max(pct)).unwrap_or(pct));
    }
    // Half-open cutoffs: [0,20) [20,40) [40,60) [60,80) [80,∞).
    const GLYPHS: [char; 5] = ['▁', '▂', '▄', '▇', '█'];
    let mut out = String::new();
    for b in buckets {
        match b {
            None => out.push('·'),
            Some(v) => {
                let idx = if v < 20.0 {
                    0
                } else if v < 40.0 {
                    1
                } else if v < 60.0 {
                    2
                } else if v < 80.0 {
                    3
                } else {
                    4
                };
                out.push(GLYPHS[idx]);
            }
        }
    }
    out
}

/// Pure: fit a line to weekly_pct within the current window (samples after
/// the most recent significant drop, which we interpret as a weekly reset).
/// Returns None if fewer than 6 samples remain, or the slope is not strictly
/// positive (nothing to pace when usage is flat or declining).
fn pace_from_snaps(snaps: &[Snapshot]) -> Option<PaceEstimate> {
    // Detect the last "reset": a >5pp drop between consecutive samples with
    // weekly_pct set. Every sample from that reset forward is the window.
    let mut window_start = 0usize;
    let mut last_pct: Option<f32> = None;
    for (i, s) in snaps.iter().enumerate() {
        if let (Some(prev), Some(cur)) = (last_pct, s.weekly_pct) {
            if cur + 5.0 < prev {
                window_start = i;
            }
        }
        if s.weekly_pct.is_some() {
            last_pct = s.weekly_pct;
        }
    }
    let window: Vec<&Snapshot> = snaps[window_start..]
        .iter()
        .filter(|s| s.weekly_pct.is_some())
        .collect();
    if window.len() < 6 {
        return None;
    }
    // Ordinary least squares over (hours since first sample, weekly_pct).
    let t0 = window[0].ts.timestamp() as f64;
    let xs: Vec<f64> = window
        .iter()
        .map(|s| (s.ts.timestamp() as f64 - t0) / 3600.0)
        .collect();
    let ys: Vec<f64> = window
        .iter()
        .map(|s| s.weekly_pct.unwrap() as f64)
        .collect();
    let n = xs.len() as f64;
    let mx: f64 = xs.iter().sum::<f64>() / n;
    let my: f64 = ys.iter().sum::<f64>() / n;
    let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    let syy: f64 = ys.iter().map(|y| (y - my).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    if slope <= 0.0 {
        return None;
    }
    // R² of the fit; 0 if y is constant (avoid division by zero).
    let r2 = if syy == 0.0 {
        0.0
    } else {
        (sxy * sxy) / (sxx * syy)
    };
    Some(PaceEstimate {
        slope_pct_per_hour: slope,
        confidence: r2,
        sample_count: window.len(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDateTime};
    use std::time::{Duration as StdDuration, SystemTime};

    fn snap(provider: &str, account: &str, ts: DateTime<Utc>, weekly: Option<f32>) -> Snapshot {
        Snapshot {
            ts,
            provider: provider.to_string(),
            account: account.to_string(),
            session_pct: None,
            weekly_pct: weekly,
            active_model: None,
        }
    }

    fn write_snap(dir: &Path, s: &Snapshot) {
        let path = month_path(dir, s.ts);
        let mut a = Appender::open(&path).unwrap();
        a.write_snap(s).unwrap();
    }

    // --- rotation ---

    #[test]
    fn rotate_removes_files_older_than_retention() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("history.2024-01.ndjson");
        let recent = dir.path().join("history.2026-08.ndjson");
        std::fs::write(&old, b"").unwrap();
        std::fs::write(&recent, b"").unwrap();

        // Force the old file's mtime to well past the retention window.
        let very_old = SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_700_000_000);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(very_old)
            .unwrap();

        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        rotate_files(dir.path(), now).unwrap();

        assert!(!old.exists(), "6-month-old history should be pruned");
        assert!(recent.exists(), "current history must not be touched");
    }

    #[test]
    fn rotate_ignores_non_history_files() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("state.json");
        std::fs::write(&keep, b"{}").unwrap();
        // Old mtime — but not a history file, so it must not be pruned.
        File::options()
            .write(true)
            .open(&keep)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_700_000_000))
            .unwrap();
        rotate_files(dir.path(), Utc::now()).unwrap();
        assert!(keep.exists());
    }

    // --- month boundary handoff ---

    #[test]
    fn append_writes_into_month_specific_files() {
        let dir = tempfile::tempdir().unwrap();
        let s1 = snap(
            "claude",
            "a@x.io",
            Utc.with_ymd_and_hms(2026, 7, 31, 23, 55, 0).unwrap(),
            Some(10.0),
        );
        let s2 = snap(
            "claude",
            "a@x.io",
            Utc.with_ymd_and_hms(2026, 8, 1, 0, 5, 0).unwrap(),
            Some(11.0),
        );
        write_snap(dir.path(), &s1);
        write_snap(dir.path(), &s2);
        assert!(dir.path().join("history.2026-07.ndjson").exists());
        assert!(dir.path().join("history.2026-08.ndjson").exists());

        // Reading a 2-day window crossing the boundary sees both records.
        let key = AccountKey::new("claude", "a@x.io");
        let all = read_range(
            dir.path(),
            &key,
            Utc.with_ymd_and_hms(2026, 7, 31, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap(),
        );
        assert_eq!(all.len(), 2);
        assert!(all[0].ts < all[1].ts);
    }

    // --- empty-log paths ---

    #[test]
    fn empty_log_returns_no_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let key = AccountKey::new("claude", "nobody@x.io");
        let out = read_range(dir.path(), &key, Utc::now() - Duration::days(7), Utc::now());
        assert!(out.is_empty());
    }

    #[test]
    fn empty_log_sparkline_is_all_dots() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();
        let out = sparkline_from_snaps(&[], today);
        assert_eq!(out, "·······");
        assert_eq!(out.chars().count(), 7);
    }

    #[test]
    fn empty_log_pace_is_none() {
        assert!(pace_from_snaps(&[]).is_none());
    }

    // --- sparkline glyph cutoffs ---

    #[test]
    fn sparkline_maps_percent_ranges_to_expected_glyphs() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();
        // Build one sample per bucket at noon UTC (well away from DST windows
        // so the local day matches the UTC day at every listed local zone).
        let mk = |days_ago: i64, pct: f32| {
            let day = today - Duration::days(days_ago);
            let dt = NaiveDateTime::new(day, chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap());
            snap(
                "claude",
                "a@x.io",
                Local
                    .from_local_datetime(&dt)
                    .single()
                    .unwrap()
                    .with_timezone(&Utc),
                Some(pct),
            )
        };
        let snaps = vec![
            mk(6, 5.0),   // ▁
            mk(5, 25.0),  // ▂
            mk(4, 45.0),  // ▄
            mk(3, 65.0),  // ▇
            mk(2, 85.0),  // █
            mk(1, 100.0), // █
                          // day 0 missing -> ·
        ];
        let out = sparkline_from_snaps(&snaps, today);
        assert_eq!(out, "▁▂▄▇██·");
    }

    #[test]
    fn sparkline_takes_max_per_local_day() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();
        let noon = |days_ago: i64, pct: f32| {
            let day = today - Duration::days(days_ago);
            let dt = NaiveDateTime::new(day, chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap());
            snap(
                "claude",
                "a@x.io",
                Local
                    .from_local_datetime(&dt)
                    .single()
                    .unwrap()
                    .with_timezone(&Utc),
                Some(pct),
            )
        };
        // Two samples for the same day: the higher must win.
        let snaps = vec![noon(3, 10.0), noon(3, 85.0)];
        let out = sparkline_from_snaps(&snaps, today);
        // day 3 -> index 3 (7-1-3 = 3). 85 -> █.
        assert_eq!(&out[..], "···█···".chars().collect::<String>().as_str());
    }

    // --- DST spring-forward: still 7 buckets ---

    #[test]
    fn sparkline_seven_buckets_across_dst_spring_forward() {
        // US/Eastern spring-forward: 2026-03-08 (Sunday) skips 02:00->03:00.
        // Anchor "today" as the day after so 7 buckets straddle the gap.
        let today = NaiveDate::from_ymd_opt(2026, 3, 14).unwrap();
        // Fabricate one sample per local calendar day at noon UTC — noon UTC is
        // well away from any transition so local_day == day-shifted-by-tz.
        let mut snaps = Vec::new();
        for days_ago in 0..7 {
            let day = today - Duration::days(days_ago);
            let dt =
                NaiveDateTime::new(day, chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap());
            // Explicit fixed offset (UTC-5) avoids depending on the host TZ.
            let ts = FixedOffset::west_opt(5 * 3600)
                .unwrap()
                .from_local_datetime(&dt)
                .single()
                .unwrap()
                .with_timezone(&Utc);
            snaps.push(snap("claude", "a@x.io", ts, Some(50.0)));
        }
        let out = sparkline_from_snaps(&snaps, today);
        // Every bucket has data — no dots, exactly 7 glyphs regardless of DST.
        assert_eq!(out.chars().count(), 7);
        assert!(!out.contains('·'), "no missing days across DST: {out}");
    }

    // --- pace: linear-fit fixture ---

    #[test]
    fn pace_fits_perfectly_linear_fixture() {
        // Ten samples at 1h intervals, weekly_pct = 10 + 2*i.
        let t0 = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let snaps: Vec<Snapshot> = (0..10)
            .map(|i| {
                snap(
                    "claude",
                    "a@x.io",
                    t0 + Duration::hours(i),
                    Some(10.0 + 2.0 * i as f32),
                )
            })
            .collect();
        let pe = pace_from_snaps(&snaps).expect("linear fixture must produce a pace");
        assert!(
            (pe.slope_pct_per_hour - 2.0).abs() < 1e-6,
            "slope {} != 2.0",
            pe.slope_pct_per_hour
        );
        assert!(
            (pe.confidence - 1.0).abs() < 1e-9,
            "R² {} not 1.0",
            pe.confidence
        );
        assert_eq!(pe.sample_count, 10);
    }

    #[test]
    fn pace_uses_only_current_window_after_reset() {
        // Cycle 1: 3 samples climbing from 40->50. Cycle 2 (after reset): 8
        // samples climbing from 5 at slope 3pct/h. The reset detector must
        // discard cycle 1 so only cycle 2 counts.
        let t0 = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let mut snaps: Vec<Snapshot> = Vec::new();
        for i in 0..3 {
            snaps.push(snap(
                "claude",
                "a@x.io",
                t0 + Duration::hours(i),
                Some(40.0 + 5.0 * i as f32),
            ));
        }
        // Reset marker: big drop.
        for i in 0..8 {
            snaps.push(snap(
                "claude",
                "a@x.io",
                t0 + Duration::hours(10 + i),
                Some(5.0 + 3.0 * i as f32),
            ));
        }
        let pe = pace_from_snaps(&snaps).unwrap();
        assert_eq!(pe.sample_count, 8, "reset must trim earlier samples");
        assert!((pe.slope_pct_per_hour - 3.0).abs() < 1e-6);
    }

    #[test]
    fn pace_none_for_fewer_than_six_samples() {
        let t0 = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let snaps: Vec<Snapshot> = (0..5)
            .map(|i| {
                snap(
                    "claude",
                    "a@x.io",
                    t0 + Duration::hours(i),
                    Some(10.0 + i as f32),
                )
            })
            .collect();
        assert!(pace_from_snaps(&snaps).is_none());
    }

    #[test]
    fn pace_none_for_non_positive_slope() {
        let t0 = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        // Flat: slope 0 -> None.
        let flat: Vec<Snapshot> = (0..6)
            .map(|i| snap("claude", "a@x.io", t0 + Duration::hours(i), Some(50.0)))
            .collect();
        assert!(pace_from_snaps(&flat).is_none());
        // Declining without a discrete reset: slope negative -> None.
        let dec: Vec<Snapshot> = (0..6)
            .map(|i| {
                snap(
                    "claude",
                    "a@x.io",
                    t0 + Duration::hours(i),
                    Some(50.0 - i as f32),
                )
            })
            .collect();
        assert!(pace_from_snaps(&dec).is_none());
    }

    // --- filtering by account key ---

    #[test]
    fn read_range_filters_by_account_key() {
        let dir = tempfile::tempdir().unwrap();
        let t = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        write_snap(dir.path(), &snap("claude", "a@x.io", t, Some(1.0)));
        write_snap(
            dir.path(),
            &snap("claude", "b@x.io", t + Duration::minutes(1), Some(2.0)),
        );
        write_snap(
            dir.path(),
            &snap("codex", "a@x.io", t + Duration::minutes(2), Some(3.0)),
        );
        let key = AccountKey::new("claude", "a@x.io");
        let out = read_range(
            dir.path(),
            &key,
            t - Duration::hours(1),
            t + Duration::hours(1),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].account, "a@x.io");
        assert_eq!(out[0].provider, "claude");
    }
}
