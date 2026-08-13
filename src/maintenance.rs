//! The daily sweep: what may go, and what must stay.
//!
//! Once a day at a wall-clock time rather than on an interval. Deleting is never
//! urgent, and a fixed time is something a person can reason about: "it runs at eight"
//! is checkable, "every so often" is not.
//!
//! The order of business matters more than the schedule. Before anything is removed
//! the tracker is asked which torrents still owe seed time, and **if that question
//! cannot be answered the whole run is abandoned**. An unanswered question is not the
//! same as "nothing is owed": treating it that way is how an account that has been
//! open for fifteen years acquires its first hit and run. The implementation being
//! replaced does not make this distinction — its list-fetch failure is caught further
//! up and the sweep proceeds regardless.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{Local, Timelike};

use crate::config::{Candidate, Verdict};
use crate::state::{self, Item, Store};

/// One run's outcome, for the log and the interface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub considered: usize,
    pub deleted: Vec<String>,
    /// How much the deletions freed, so the message can say it without measuring the disks.
    pub freed_bytes: u64,
    /// Why each surviving item survived, so "why is this still here" is answerable.
    ///
    /// Title, the torrent it belongs to, the scope of the reason, and the reason.
    pub kept: Vec<(String, String, crate::config::Scope, &'static str)>,
    /// Paths the deletions removed, so the disks can be measured once they are really gone.
    pub deleted_paths: Vec<String>,
    /// Set when the run was abandoned instead of performed.
    pub abandoned: Option<String>,
}

impl Report {
    /// What is still being seeded, and anything held back for a different reason.
    ///
    /// Two sentences rather than a list of reasons with counts. The seeding obligation belongs to
    /// the torrent, so that line counts torrents and says how many files they come to; a file
    /// kept for a reason of its own is counted as a file, because that is what it is.
    fn append_seeding_line(&self, out: &mut String) {
        let mut torrents: Vec<&str> = Vec::new();
        let mut files = 0usize;
        let mut other: Vec<(&str, usize)> = Vec::new();
        for (_, hash, _, why) in &self.kept {
            if crate::config::is_about_seeding(why) {
                files += 1;
                if !torrents.contains(&hash.as_str()) {
                    torrents.push(hash);
                }
            } else {
                match other.iter_mut().find(|(w, _)| w == why) {
                    Some((_, n)) => *n += 1,
                    None => other.push((why, 1)),
                }
            }
        }
        if !torrents.is_empty() {
            out.push_str(&format!(
                "\nseedelendő: {} torrent ({files} fájl összesen)",
                torrents.len()
            ));
        }
        other.sort_by(|a, b| b.1.cmp(&a.1));
        for (why, n) in other {
            out.push_str(&format!("\negyéb okból megtartva: {n} fájl ({why})"));
        }
    }

    /// The run in one message, for a push notification.
    ///
    /// Sent whether anything went or not: "nothing was deleted" is the answer to the same
    /// question as "these went", and a nightly job that only speaks up sometimes is a job you
    /// end up checking by hand.
    pub fn notification(&self, disks: &[String]) -> String {
        let mut out = String::new();
        if let Some(why) = &self.abandoned {
            out.push_str(&format!("A takarítás megállt: {why}\nSemmi nem törlődött."));
        } else if self.deleted.is_empty() {
            out.push_str("törölt elemek száma: 0");
            self.append_seeding_line(&mut out);
        } else {
            out.push_str(&format!(
                "törölt elemek száma: {} (felszabadult {})",
                self.deleted.len(),
                crate::media::size_label(self.freed_bytes)
            ));
            for title in &self.deleted {
                out.push_str(&format!("\n- {title}"));
            }
            self.append_seeding_line(&mut out);
        }
        for line in disks {
            out.push_str(&format!("\n{line}"));
        }
        out
    }


    pub fn summary(&self) -> String {
        match &self.abandoned {
            Some(why) => format!("sweep abandoned: {why}"),
            None => format!(
                "sweep looked at {} downloads, deleted {}",
                self.considered,
                self.deleted.len()
            ),
        }
    }
}

/// What the sweep needs from the rest of the program.
///
/// A trait so the decision logic can be tested without a torrent session, a tracker
/// account or a clock: the parts that are hard to arrange are exactly the parts whose
/// behaviour matters most here.
/// The futures are `Send` explicitly, because the sweep runs on a spawned task and a
/// bare `async fn` in a trait promises nothing about which thread may hold it.
pub trait World: Send + Sync {
    /// The settings as they stand now. Read per run rather than captured once, so an
    /// edit in the interface takes effect at the next sweep without a restart.
    fn settings(&self) -> impl Future<Output = crate::config::Maintenance> + Send;
    /// Where the `.torrent` files live, and how long an orphan may linger.
    fn torrent_files_dir(&self) -> impl Future<Output = String> + Send;
    /// What is still owed, and to whom it was possible to ask. An error abandons the run.
    fn owed(&self) -> impl Future<Output = Result<Owed>> + Send;
    /// Info hashes with a reader attached right now.
    fn streaming_hashes(&self) -> impl Future<Output = Vec<String>> + Send;
    /// Removes several at once, and returns the keys it actually managed to remove.
    ///
    /// Batched because each deletion from a torrent that keeps running costs a recheck of that
    /// torrent, and rotating a pack an episode at a time would mean one recheck per episode
    /// with the torrent not seeding through any of them.
    fn delete_downloads(&self, items: &[Item]) -> impl Future<Output = Vec<String>> + Send;
    /// Looks at the download folders and reports anything running low.
    fn check_disk_space(&self) -> impl Future<Output = ()> + Send;
    /// What the disks look like right now, one line each, for a message.
    fn disk_lines(&self) -> impl Future<Output = Vec<String>> + Send;
    /// Pushes a message wherever the owner asked for it.
    fn notify(&self, message: &str) -> impl Future<Output = ()> + Send;
}

/// The quiet period between two rounds triggered by a shortage of space.
///
/// A player that is refused retries, and each retry arrives as a fresh request: without this,
/// one film nobody has room for would mean a tracker query and a full pass over the library
/// several times a minute. Ten minutes is long enough that a retry storm costs one round, and
/// short enough that a genuine second attempt later in the evening gets its own.
pub const FULL_DISK_QUIET_SECONDS: u64 = 600;

/// Whether a download that does not fit justifies a deletion round first.
///
/// `free` and `needed` are bytes on the folder the file would be written to, and
/// `since_last_sweep` is how long ago any round last ran, in seconds.
///
/// The margin is `warn_below_free_bytes`, which means the round happens while there is still
/// the owner's own reserve left rather than at the last possible byte. Filling a disk to
/// within a few megabytes is what makes everything else on the machine start failing.
pub fn sweep_before_download(
    cfg: &crate::config::Maintenance,
    free: u64,
    needed: u64,
    since_last_sweep: u64,
) -> bool {
    // Deletion switched off means nothing may be removed, whatever the reason. A round here
    // would ask the tracker, work everything out, and be able to act on none of it.
    if !cfg.sweep_when_full || !cfg.enable_deletion {
        return false;
    }
    let wanted = needed.saturating_add(cfg.warn_below_free_bytes);
    free < wanted && since_last_sweep >= FULL_DISK_QUIET_SECONDS
}

/// What the trackers said this round.
///
/// Two lists rather than one, and the second is what makes the first safe to act on. An
/// obligation missing from `keys` means nothing unless that tracker was actually asked: a
/// tracker switched off, unreachable, or without credentials produces exactly the same empty
/// answer as a tracker with nothing owed, and one of those two permits a deletion.
#[derive(Debug, Clone, Default)]
pub struct Owed {
    /// `<tracker>:<torrent id>` for every obligation still open.
    pub keys: Vec<String>,
    /// The trackers whose list was read. Anything else is unknown, not clear.
    pub asked: Vec<crate::tracker::Tracker>,
}

impl Owed {
    pub fn asked(&self, tracker: crate::tracker::Tracker) -> bool {
        self.asked.contains(&tracker)
    }
}

/// Which trackers have downloads of ours on the disk, and therefore which ones are worth
/// asking.
///
/// A tracker we hold nothing from has nothing to say about us, and asking it anyway is a
/// request against a private account for no reason at all: one more login, one more page
/// fetched, every round, for an answer that cannot change any outcome. With the second tracker
/// switched on but never used, that would have been a daily visit to a site we took nothing
/// from.
///
/// In the order the trackers first appear, so the account almost everything came from is asked
/// first and a failure there stops the round before the other one is touched.
pub fn trackers_to_ask(items: &[Item]) -> Vec<crate::tracker::Tracker> {
    let mut out: Vec<crate::tracker::Tracker> = Vec::new();
    for item in items {
        // Without an id there is no way to look this download up on any list, so it cannot be
        // the reason for asking.
        if item.ncore_torrent_id.is_empty() {
            continue;
        }
        let tracker = item.tracker();
        if !out.contains(&tracker) {
            out.push(tracker);
        }
    }
    out
}

/// How far a run goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Delete what qualifies.
    Delete,
    /// Work everything out and report it, but remove nothing.
    ///
    /// This exists so the answer to "what would happen tonight" can be had without
    /// finding out the hard way. It runs the same code down the same path, including
    /// asking the tracker, so what it reports is what would actually be done rather
    /// than a second implementation's opinion of it.
    DryRun,
}

/// Decides and acts. Returns what happened.
pub async fn sweep<W: World>(
    world: &W,
    store: &Store,
    cfg: &crate::config::Maintenance,
    now: state::Unix,
) -> Report {
    sweep_with(world, store, cfg, now, Mode::Delete).await
}

pub async fn sweep_with<W: World>(
    world: &W,
    store: &Store,
    cfg: &crate::config::Maintenance,
    now: state::Unix,
    mode: Mode,
) -> Report {
    let mut report = Report::default();

    // A dry run is asked in order to decide whether to switch deletion on, so it answers
    // that question rather than the trivial one. With the switch off every verdict would
    // be "deletion is off" and the run would say nothing useful about the actual rules.
    let forced;
    let cfg = if mode == Mode::DryRun && !cfg.enable_deletion {
        forced = crate::config::Maintenance {
            enable_deletion: true,
            ..cfg.clone()
        };
        &forced
    } else {
        cfg
    };

    let items = store.items().await;
    // Kept whole: the seeding clock of a pack is worked out across every file taken from it.
    let all_items = items.clone();
    report.considered = items.len();
    if items.is_empty() {
        return report;
    }

    // Only worth asking the trackers when their answer can change an outcome.
    let owed = if cfg.hit_and_run && cfg.enable_deletion {
        match world.owed().await {
            Ok(owed) => owed,
            Err(e) => {
                // Deliberately give up rather than delete with an unknown answer.
                report.abandoned = Some(format!("could not read the tracker's list: {e}"));
                return report;
            }
        }
    } else {
        Owed::default()
    };

    let streaming = world.streaming_hashes().await;
    let mut doomed: Vec<Item> = Vec::new();

    for item in items {
        // A download whose tracker could not be asked this round is left alone, whatever its
        // clock says. This is the same rule as an unreadable list, applied per tracker: the
        // second tracker being switched off is not the second tracker saying nothing is owed.
        if cfg.hit_and_run && cfg.enable_deletion && !owed.asked(item.tracker()) {
            report.kept.push((
                item.title.clone(),
                item.info_hash.clone(),
                crate::config::Scope::Torrent,
                "ezt a trackert nem kérdeztük meg",
            ));
            continue;
        }
        let owed_key = item.owed_key();
        let candidate = Candidate {
            kept: item.keep,
            watched: item.watched(cfg.watched_position_percent, cfg.watched_min_served_percent),
            owed_to_tracker: !item.ncore_torrent_id.is_empty()
                && owed.keys.iter().any(|key| *key == owed_key),
            // The list was read at the top of this run, and a run whose read failed was
            // abandoned before reaching here, so absence from it is an answer of now.
            //
            // What counts as proof that the tracker knows the torrent differs by tracker, and
            // that is the only difference. nCore's proof is its transfer figures — this branch
            // is exactly as it was and is deliberately left alone. BitHUmen publishes no
            // figures at all, so its proof is having been on the hit-and-run list at least
            // once: a torrent that was owed and no longer is has settled, while one that has
            // never been listed may simply not have been processed yet, and deleting on that
            // is how an account collects a hit and run.
            tracker_says_clear: !item.ncore_torrent_id.is_empty()
                && match item.tracker() {
                    crate::tracker::Tracker::Ncore => item.tracker_figures_at.is_some(),
                    crate::tracker::Tracker::Bithumen => item.tracker_known_at.is_some(),
                }
                && !owed.keys.iter().any(|key| *key == owed_key),
            partial: item.partial,
            streaming: streaming.iter().any(|h| *h == item.info_hash),
            // The torrent's clock, not this file's: the debt is the torrent's.
            seeded_secs: item.torrent_seeded_for(&all_items, now),
            // The file's own account, and whether it is the one holding the torrent open.
            is_keeper: crate::state::keeper_key(&all_items, &item.info_hash)
                .is_none_or(|k| k == item.key()),
            file_bytes: item.file_len,
            file_seeded_secs: item.file_seeded_for(now),
            // Per torrent, which is what the obligation is attached to.
            figures_known: item.tracker_figures_at.is_some(),
            tracker_downloaded: item.tracker_downloaded_bytes,
            tracker_uploaded: item.tracker_uploaded_bytes,
        };

        match cfg.verdict(&candidate) {
            Verdict::Keep(scope, why) => report.kept.push((
                item.title.clone(),
                item.info_hash.clone(),
                scope,
                why,
            )),
            // A dry run stops here: the verdict is the answer it was asked for.
            Verdict::Delete if mode == Mode::DryRun => {
                report.deleted.push(item.title.clone());
            }
            // Collected rather than deleted one at a time, so a pack costs one recheck.
            Verdict::Delete => doomed.push(item.clone()),
        }
    }

    if !doomed.is_empty() {
        let removed = world.delete_downloads(&doomed).await;
        for item in &doomed {
            if removed.contains(&item.key()) {
                // The record goes only for what actually left the disk: a failed deletion has
                // to stay on the books, or the data becomes an orphan nothing knows about.
                store.remove(&item.key()).await;
                report.freed_bytes = report.freed_bytes.saturating_add(item.file_len);
                report.deleted_paths.push(item.save_path.clone());
                report.deleted.push(item.title.clone());
            } else {
                report
                    .kept
                    .push((
                        item.title.clone(),
                        item.info_hash.clone(),
                        crate::config::Scope::File,
                        "a törlés nem sikerült",
                    ));
            }
        }
    }

    report
}

/// Runs the sweep once a day at the configured local time.
///
/// The wake-up interval is a minute, not the gap to the next run: the time is
/// editable while the server runs, so a schedule computed once would be stale. The
/// date of the last run is persisted, so a restart in the evening cannot make it run
/// twice, and a machine that was asleep at the appointed hour still runs when it wakes
/// up.
/// Runs one sweep and everything that belongs with it: the tidy-up of orphaned `.torrent`
/// files, the state flush, and the message saying what happened.
///
/// Factored out because it now happens from two places, on the daily schedule and once at
/// startup, and a housekeeping round that behaves differently depending on who called it is a
/// round nobody can reason about.
pub async fn run_once<W: World + 'static>(world: &W, store: &Arc<Store>, why: &str) -> Report {
    let maintenance = world.settings().await;
    let report = sweep(world, store, &maintenance, state::now()).await;
    tracing::info!(trigger = why, "{}", report.summary());
    for (title, _, _, reason) in &report.kept {
        tracing::debug!(title = %title, reason = %reason, "kept");
    }
    for title in &report.deleted {
        tracing::info!(title = %title, "deleted");
    }

    // Orphaned .torrent files go in the same run, so there is one moment when housekeeping
    // happens rather than several.
    let dir = world.torrent_files_dir().await;
    match clean_torrent_files(
        std::path::Path::new(&dir),
        store,
        maintenance.cache_retention_seconds,
        state::now(),
    )
    .await
    {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "removed orphaned .torrent files"),
        Err(e) => tracing::warn!(error = %e, "could not clean the .torrent folder"),
    }

    if let Err(e) = store.flush().await {
        tracing::warn!(error = %e, "could not write the state file after the sweep");
    }

    // Measured only once the files have actually gone.
    //
    // libtorrent deletes asynchronously: remove_torrent returns at once and its disk thread does
    // the work afterwards. Measuring straight away reported the space as it was before the run,
    // which read as a deletion that freed nothing: "freed 24.65 GiB" on one line and "10.51 GiB
    // free" on the next, with the true 35.29 GiB only showing up in the following run's message.
    wait_until_gone(&report.deleted_paths).await;

    world.check_disk_space().await;
    let disks = world.disk_lines().await;
    if maintenance.notify_sweep {
        world
            .notify(&format!("{why}\n{}", report.notification(&disks)))
            .await;
    }
    report
}

/// Waits for deleted files to leave the filesystem, up to a few seconds.
///
/// Not a fixed sleep: it asks the question it actually wants answered, and returns the moment
/// the answer is yes. A path that will not go away is not worth waiting on for ever either, so
/// the wait is bounded and the measurement that follows is simply taken as it stands.
async fn wait_until_gone(paths: &[String]) {
    const LIMIT: std::time::Duration = std::time::Duration::from_secs(10);
    const STEP: std::time::Duration = std::time::Duration::from_millis(250);
    if paths.is_empty() {
        return;
    }
    let deadline = std::time::Instant::now() + LIMIT;
    loop {
        let remaining = paths
            .iter()
            .filter(|p| std::path::Path::new(p.as_str()).exists())
            .count();
        if remaining == 0 {
            return;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                remaining,
                "some deleted files were still on the disk when the space was measured"
            );
            return;
        }
        tokio::time::sleep(STEP).await;
    }
}

pub fn spawn<W: World + 'static>(world: Arc<W>, store: Arc<Store>) {
    tokio::spawn(async move {
        // Once at startup, after a pause: the torrents have just been re-opened and the first
        // announce has not happened yet, and asking the tracker before that would be asking
        // about a state we have not reported.
        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
        let settings = world.settings().await;
        let quiet_enough = state::now().saturating_sub(store.last_sweep_at().await) >= 3600;
        if settings.enable_deletion && settings.sweep_on_start && quiet_enough {
            store.set_last_sweep_at(state::now()).await;
            run_once(&*world, &store, "Induláskori takarítás").await;
        }

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;

            let maintenance = world.settings().await;
            let (hour, minute) = maintenance.sweep_time();
            let now = Local::now();
            let today = now.format("%Y-%m-%d").to_string();

            if store.last_sweep_date().await == today {
                continue;
            }
            // Only once the appointed time has passed today.
            if !time_has_passed(now.hour(), now.minute(), hour, minute) {
                continue;
            }

            store.set_last_sweep_date(&today).await;
            store.set_last_sweep_at(state::now()).await;
            run_once(&*world, &store, &format!("Esti takarítás {hour:02}:{minute:02}")).await;

            // Space, in the same run. Deletion here is time-based, so nothing stops the
            // disks filling before anything is old enough to remove; saying so while there
            // is still room is the only useful moment to say it.
            world.check_disk_space().await;
        }
    });
}

/// Whether the appointed minute has arrived or gone by today.
fn time_has_passed(now_h: u32, now_m: u32, at_h: u32, at_m: u32) -> bool {
    (now_h, now_m) >= (at_h, at_m)
}

/// When the next run is due, for display. Today's time if it has not passed yet,
/// otherwise tomorrow's.
pub fn next_run_label(maintenance: &crate::config::Maintenance, last_sweep_date: &str) -> String {
    let (hour, minute) = maintenance.sweep_time();
    let now = Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let due_today = !time_has_passed(now.hour(), now.minute(), hour, minute)
        && last_sweep_date != today;
    let day = if due_today { "today" } else { "tomorrow" };
    format!("{day} at {hour:02}:{minute:02}")
}

/// Deletes `.torrent` files that no longer belong to anything, older than the
/// retention window.
///
/// These are kept so a torrent can be re-added after a restart without asking the
/// tracker for the file again. Once the download they describe is gone they are
/// litter, but they are tiny, so age is the only guard needed against removing one
/// that is about to be used.
pub async fn clean_torrent_files(
    dir: &std::path::Path,
    store: &Store,
    retention_secs: u64,
    now: state::Unix,
) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let in_use: Vec<String> = store
        .items()
        .await
        .iter()
        .map(|i| i.torrent_file.to_lowercase())
        .collect();

    // Resume files are named after the info hash, so an in-use torrent's resume file is
    // recognised by the same stem. Removing one would cost a full re-check at the next
    // start, which is exactly what it exists to prevent.
    let in_use_stems: Vec<String> = store
        .items()
        .await
        .iter()
        .map(|i| i.info_hash.to_lowercase())
        .collect();

    let mut removed = 0usize;
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if extension != "torrent" && extension != "resume" {
            continue;
        }
        if in_use.contains(&path.to_string_lossy().to_lowercase()) {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if in_use_stems.contains(&stem) {
            continue;
        }
        let age = match file_age_secs(&path, now) {
            Some(age) => age,
            None => continue,
        };
        if age < retention_secs {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "could not remove"),
        }
    }
    Ok(removed)
}

fn file_age_secs(path: &std::path::Path, now: state::Unix) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Maintenance;

    const GB: u64 = 1024 * 1024 * 1024;

    /// The round before the download: it runs when the file plus the owner's own reserve does
    /// not fit, and not otherwise.
    #[test]
    fn a_download_that_does_not_fit_asks_for_a_round_first() {
        let cfg = Maintenance {
            enable_deletion: true,
            sweep_when_full: true,
            warn_below_free_bytes: GB,
            ..Default::default()
        };
        let long_ago = FULL_DISK_QUIET_SECONDS;

        // 40 GiB free, a 50 GiB film: no.
        assert!(sweep_before_download(&cfg, 40 * GB, 50 * GB, long_ago));
        // 60 GiB free, the same film: room for it and for the reserve.
        assert!(!sweep_before_download(&cfg, 60 * GB, 50 * GB, long_ago));
        // Exactly the size of the film and nothing more. It would fit, and leave the disk with
        // nothing at all, which is the state the reserve exists to prevent.
        assert!(sweep_before_download(&cfg, 50 * GB, 50 * GB, long_ago));
    }

    /// Deletion switched off outranks it. A round then could not remove anything, and would
    /// still cost a tracker query and a pass over the library.
    #[test]
    fn nothing_happens_while_deletion_is_off_or_the_setting_is_off() {
        let base = Maintenance {
            enable_deletion: true,
            sweep_when_full: true,
            warn_below_free_bytes: GB,
            ..Default::default()
        };
        let short = (0, 50 * GB, FULL_DISK_QUIET_SECONDS);

        assert!(sweep_before_download(&base, short.0, short.1, short.2));
        assert!(!sweep_before_download(
            &Maintenance { enable_deletion: false, ..base.clone() },
            short.0,
            short.1,
            short.2
        ));
        assert!(!sweep_before_download(
            &Maintenance { sweep_when_full: false, ..base.clone() },
            short.0,
            short.1,
            short.2
        ));
    }

    /// A refused stream is retried by the player, and every retry is a fresh request. One film
    /// nobody has room for must cost one round, not one per retry.
    #[test]
    fn a_retrying_player_does_not_start_a_round_each_time() {
        let cfg = Maintenance {
            enable_deletion: true,
            sweep_when_full: true,
            warn_below_free_bytes: GB,
            ..Default::default()
        };
        assert!(sweep_before_download(&cfg, 0, 50 * GB, FULL_DISK_QUIET_SECONDS));
        assert!(!sweep_before_download(&cfg, 0, 50 * GB, FULL_DISK_QUIET_SECONDS - 1));
        assert!(!sweep_before_download(&cfg, 0, 50 * GB, 0));
    }

    struct Fake {
        notified: std::sync::Mutex<Vec<String>>,
        owed: Option<Vec<String>>,
        streaming: Vec<String>,
        deleted: std::sync::Mutex<Vec<String>>,
        fail_delete: bool,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                owed: Some(Vec::new()),
                streaming: Vec::new(),
                deleted: std::sync::Mutex::new(Vec::new()),
                notified: std::sync::Mutex::new(Vec::new()),
                fail_delete: false,
            }
        }
    }

    impl World for Fake {
        async fn settings(&self) -> Maintenance {
            // Deletion on, because a fake used to test what a run does has no business
            // testing the switch that stops runs happening.
            Maintenance {
                enable_deletion: true,
                ..Maintenance::default()
            }
        }
        async fn torrent_files_dir(&self) -> String {
            String::new()
        }
        async fn owed(&self) -> Result<Owed> {
            match &self.owed {
                Some(ids) => Ok(Owed {
                    keys: ids
                        .iter()
                        .map(|id| crate::tracker::Tracker::Ncore.owed_key(id))
                        .collect(),
                    asked: vec![crate::tracker::Tracker::Ncore],
                }),
                None => anyhow::bail!("nCore is unreachable"),
            }
        }
        async fn streaming_hashes(&self) -> Vec<String> {
            self.streaming.clone()
        }
        async fn delete_downloads(&self, items: &[Item]) -> Vec<String> {
            if self.fail_delete {
                return Vec::new();
            }
            let mut keys = Vec::new();
            for item in items {
                self.deleted.lock().unwrap().push(item.info_hash.clone());
                keys.push(item.key());
            }
            keys
        }
        async fn check_disk_space(&self) {}
        async fn disk_lines(&self) -> Vec<String> {
            vec!["elsődleges (D:/le): 100 GiB szabad".into()]
        }
        async fn notify(&self, message: &str) {
            self.notified.lock().unwrap().push(message.to_string());
        }
    }

    fn deleting() -> Maintenance {
        Maintenance {
            enable_deletion: true,
            ..Maintenance::default()
        }
    }

    async fn store_with(items: Vec<Item>) -> Arc<Store> {
        let dir = std::env::temp_dir().join("stremhu-rs-sweep-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(format!("sweep-{}.json", items.len()));
        let _ = std::fs::remove_file(&path);
        let store = Store::load(&path).expect("loads");
        for item in items {
            store.upsert(item).await;
        }
        store
    }

    /// A watched, expired, long-seeded download the tracker no longer wants.
    fn ripe(hash: &str, now: state::Unix) -> Item {
        let mut item = Item {
            info_hash: hash.into(),
            ncore_torrent_id: "4207293".into(),
            title: format!("film {hash}"),
            file_len: 1000,
            added_at: now - 30 * 24 * 3600,
            play_count: 1,
            furthest_byte: 950,
            ..Item::default()
        };
        // Coverage, not a running total, is what decides watched now, so the fixture has to
        // carry it the way a real viewing would.
        item.mark_served(0, 950);
        item
    }

    /// A tracker we hold nothing from is not asked at all.
    ///
    /// The point is the traffic: the sweep runs every evening, and a private site we took
    /// nothing from should not be getting a login and a page fetch out of it. The second
    /// tracker switched on and never used is exactly that case.
    #[test]
    fn only_the_trackers_we_have_downloads_from_are_asked() {
        let now = state::now();
        // Nothing on the disk: nobody to ask.
        assert!(trackers_to_ask(&[]).is_empty());

        // Only nCore downloads: BitHUmen is not asked, whatever the settings say.
        let ncore_only = vec![ripe("h1", now), ripe("h2", now)];
        assert_eq!(
            trackers_to_ask(&ncore_only),
            vec![crate::tracker::Tracker::Ncore]
        );

        // One download from the second tracker is enough to make its list worth reading.
        let mixed = vec![
            ripe("h1", now),
            Item {
                tracker: "bithumen".into(),
                ..ripe("h3", now)
            },
        ];
        assert_eq!(
            trackers_to_ask(&mixed),
            vec![
                crate::tracker::Tracker::Ncore,
                crate::tracker::Tracker::Bithumen
            ],
            "the tracker most of it came from is asked first"
        );

        // A record with no tracker id cannot be looked up on any list, so it is not a reason
        // to ask one.
        let no_id = vec![Item {
            ncore_torrent_id: String::new(),
            ..ripe("h4", now)
        }];
        assert!(trackers_to_ask(&no_id).is_empty());
    }

    /// A download from a tracker this round could not ask is left alone, even when everything
    /// else about it says it may go.
    ///
    /// This is the second tracker's version of the rule that already protects the first: an
    /// unreadable list is not an empty one. BitHUmen switched off, unreachable, or without
    /// credentials produces the same silence as BitHUmen with nothing owed, and one of those two
    /// is a hit and run waiting to happen.
    #[tokio::test]
    async fn a_tracker_that_was_not_asked_keeps_its_downloads() {
        let now = state::now();
        let from_bithumen = Item {
            tracker: "bithumen".into(),
            ..ripe("h9", now)
        };
        let store = store_with(vec![from_bithumen]).await;
        // The fake answers for nCore only, which is exactly the state of a server with the
        // second tracker switched off.
        let world = Arc::new(Fake::new());
        let report = sweep(&*world, &store, &deleting(), now).await;

        assert!(report.deleted.is_empty(), "nothing may go on an unasked list");
        assert_eq!(
            report.kept.first().map(|(_, _, _, why)| *why),
            Some("ezt a trackert nem kérdeztük meg")
        );

        // And the same download from nCore, whose list *was* read, does go: the rule is about
        // which tracker was asked, not about being cautious with everything.
        let store = store_with(vec![ripe("h8", now)]).await;
        let report = sweep(&*Arc::new(Fake::new()), &store, &deleting(), now).await;
        assert_eq!(report.deleted.len(), 1);
    }

    /// The nightly message is sent whether anything went or not: a job that only speaks up
    /// sometimes is a job you end up checking by hand.
    #[tokio::test]
    async fn the_run_says_what_it_did_either_way() {
        let store = store_with(vec![ripe("h1", state::now())]).await;
        let world = Arc::new(Fake::new());
        let report = run_once(&*world, &store, "Teszt").await;
        assert_eq!(report.deleted.len(), 1);

        let sent = world.notified.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "one message per run");
        let msg = &sent[0];
        assert!(msg.starts_with("Teszt"), "it says which run: {msg}");
        assert!(msg.contains("törölt elemek száma: 1"), "{msg}");
        assert!(msg.contains("- film h1"), "{msg}");
        assert!(msg.contains("szabad"), "and what the disks look like: {msg}");

        // Nothing to do: still a message, with the reasons grouped.
        let store = store_with(vec![Item {
            keep: true,
            ..ripe("h2", state::now())
        }])
        .await;
        let world = Arc::new(Fake::new());
        run_once(&*world, &store, "Teszt").await;
        let msg = world.notified.lock().unwrap()[0].clone();
        assert!(msg.contains("törölt elemek száma: 0"), "{msg}");
        // Counted as a file, because being marked to keep is a fact about the file and not
        // about the torrent's obligation.
        assert!(
            msg.contains("egyéb okból megtartva: 1 fájl (megtartásra jelölve)"),
            "{msg}"
        );
    }

    /// A reason that belongs to the torrent is counted in torrents. Three episodes of one pack
    /// held back because the pack owes seeding is one torrent, not three downloads, and the
    /// difference is what somebody reading the message on a phone needs.
    #[tokio::test]
    async fn a_torrents_reason_is_counted_in_torrents() {
        let now = state::now();
        let mut items = Vec::new();
        for index in 0..3 {
            let mut item = ripe("pack", now);
            item.file_index = index;
            item.title = format!("pack rész {index}");
            items.push(item);
        }
        let store = store_with(items).await;
        let world = Arc::new(Fake {
            // The tracker still wants seeding on that one torrent.
            owed: Some(vec!["4207293".to_string()]),
            ..Fake::new()
        });
        let report = run_once(&*world, &store, "Teszt").await;

        assert!(report.deleted.is_empty());
        assert_eq!(report.kept.len(), 3, "three files");
        let msg = world.notified.lock().unwrap()[0].clone();
        assert!(msg.contains("seedelendő: 1 torrent (3 fájl összesen)"), "{msg}");
        assert!(msg.contains("törölt elemek száma: 0"), "{msg}");
    }

    #[tokio::test]
    async fn a_ripe_download_is_deleted_and_forgotten() {
        let now = 2_000_000_000;
        let store = store_with(vec![ripe("h1", now)]).await;
        let world = Fake::new();

        let report = sweep(&world, &store, &deleting(), now).await;

        assert_eq!(report.deleted, vec!["film h1".to_string()]);
        assert_eq!(*world.deleted.lock().unwrap(), vec!["h1".to_string()]);
        assert!(store.get("h1:0").await.is_none(), "removed from the records too");
    }

    /// The case this whole module is careful about: an unanswerable tracker means no
    /// deletion at all, not deletion without protection.
    #[tokio::test]
    async fn an_unreachable_tracker_abandons_the_whole_run() {
        let now = 2_000_000_000;
        let store = store_with(vec![ripe("h1", now)]).await;
        let world = Fake {
            owed: None,
            ..Fake::new()
        };

        let report = sweep(&world, &store, &deleting(), now).await;

        assert!(report.abandoned.is_some());
        assert!(report.deleted.is_empty());
        assert!(world.deleted.lock().unwrap().is_empty());
        assert!(store.get("h1:0").await.is_some(), "nothing was forgotten");
    }

    #[tokio::test]
    async fn a_torrent_on_the_trackers_list_survives() {
        let now = 2_000_000_000;
        let store = store_with(vec![ripe("h1", now)]).await;
        let world = Fake {
            owed: Some(vec!["4207293".into()]),
            ..Fake::new()
        };

        let report = sweep(&world, &store, &deleting(), now).await;

        assert!(report.deleted.is_empty());
        assert_eq!(
            report.kept,
            vec![(
                "film h1".to_string(),
                "h1".to_string(),
                crate::config::Scope::Torrent,
                "a tracker szerint még seedelni kell"
            )],
            "and the reason is about the torrent, not this one file"
        );
    }

    /// The sweep runs in the evening, when someone is likely to be watching.
    #[tokio::test]
    async fn something_being_watched_right_now_survives() {
        let now = 2_000_000_000;
        let store = store_with(vec![ripe("h1", now)]).await;
        let world = Fake {
            streaming: vec!["h1".into()],
            ..Fake::new()
        };

        let report = sweep(&world, &store, &deleting(), now).await;
        assert!(report.deleted.is_empty());
        assert_eq!(report.kept[0].3, "épp játszik");
    }

    /// With deletion off the tracker is not even asked: no traffic for a question
    /// whose answer cannot change anything.
    /// A dry run has to reach the same verdict as the real thing and remove nothing.
    #[tokio::test]
    async fn a_dry_run_reports_what_would_go_without_touching_it() {
        let now = 2_000_000_000;
        let store = store_with(vec![ripe("h1", now)]).await;
        let world = Fake::new();

        let report = sweep_with(&world, &store, &deleting(), now, Mode::DryRun).await;

        assert_eq!(report.deleted, vec!["film h1".to_string()]);
        assert!(
            world.deleted.lock().unwrap().is_empty(),
            "a dry run must not delete anything"
        );
        assert!(store.get("h1:0").await.is_some(), "nor forget it");

        // And the real run then does what the dry run said it would.
        let report = sweep(&world, &store, &deleting(), now).await;
        assert_eq!(report.deleted, vec!["film h1".to_string()]);
        assert!(store.get("h1:0").await.is_none());
    }

    /// Asked in order to decide whether to switch deletion on, so the switch being off
    /// must not make the answer meaningless.
    #[tokio::test]
    async fn a_dry_run_answers_even_with_deletion_switched_off() {
        let now = 2_000_000_000;
        let store = store_with(vec![ripe("h1", now)]).await;
        let world = Fake::new();

        // The default has enable_deletion false.
        let report = sweep_with(&world, &store, &Maintenance::default(), now, Mode::DryRun).await;

        assert_eq!(
            report.deleted,
            vec!["film h1".to_string()],
            "the dry run has to answer about the rules, not about the switch"
        );
        assert!(world.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn nothing_is_asked_or_deleted_while_deletion_is_off() {
        let now = 2_000_000_000;
        let store = store_with(vec![ripe("h1", now)]).await;
        let world = Fake {
            owed: None, // would fail if it were consulted
            ..Fake::new()
        };

        let report = sweep(&world, &store, &Maintenance::default(), now).await;

        assert!(report.abandoned.is_none(), "the tracker was not consulted");
        assert!(report.deleted.is_empty());
        assert_eq!(report.kept[0].3, "az automatikus törlés ki van kapcsolva");
    }

    /// A file that cannot be removed must stay in the records, or the server would
    /// forget about data still sitting on the disk.
    #[tokio::test]
    async fn a_failed_deletion_keeps_the_record() {
        let now = 2_000_000_000;
        let store = store_with(vec![ripe("h1", now)]).await;
        let world = Fake {
            fail_delete: true,
            ..Fake::new()
        };

        let report = sweep(&world, &store, &deleting(), now).await;

        assert!(report.deleted.is_empty());
        assert_eq!(report.kept[0].3, "a törlés nem sikerült");
        assert!(store.get("h1:0").await.is_some());
    }

    #[tokio::test]
    async fn an_empty_library_is_not_a_reason_to_contact_the_tracker() {
        let store = store_with(vec![]).await;
        let world = Fake {
            owed: None,
            ..Fake::new()
        };
        let report = sweep(&world, &store, &deleting(), 2_000_000_000).await;
        assert_eq!(report.considered, 0);
        assert!(report.abandoned.is_none());
    }

    #[test]
    fn the_appointed_minute_is_recognised() {
        assert!(!time_has_passed(19, 59, 20, 0));
        assert!(time_has_passed(20, 0, 20, 0));
        assert!(time_has_passed(23, 30, 20, 0));
        assert!(time_has_passed(0, 0, 0, 0));
        assert!(!time_has_passed(7, 59, 8, 0));
    }

    /// The label the settings page shows, so a person can read off when it will next
    /// happen instead of working it out.
    #[test]
    fn the_next_run_label_names_a_day_and_a_time() {
        let m = Maintenance::default();
        let label = next_run_label(&m, "");
        assert!(label.ends_with("at 20:00"), "got: {label}");
        assert!(label.starts_with("today") || label.starts_with("tomorrow"));

        // Already run today: the next one can only be tomorrow.
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert!(next_run_label(&m, &today).starts_with("tomorrow"));
    }

    #[tokio::test]
    async fn orphaned_torrent_files_are_cleaned_up_but_used_ones_are_not() {
        let dir = std::env::temp_dir().join("stremhu-rs-torrentfile-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let used = dir.join("used.torrent");
        let orphan = dir.join("orphan.torrent");
        let other = dir.join("notes.txt");
        std::fs::write(&used, b"d4:infod4:name1:aee").expect("writes");
        std::fs::write(&orphan, b"d4:infod4:name1:bee").expect("writes");
        std::fs::write(&other, b"not a torrent").expect("writes");

        let store = store_with(vec![Item {
            info_hash: "h".into(),
            torrent_file: used.to_string_lossy().to_string(),
            ..Item::default()
        }])
        .await;

        // Far in the future, so both files count as old.
        let now = state::now() + 10 * 24 * 3600;
        let removed = clean_torrent_files(&dir, &store, 7 * 24 * 3600, now)
            .await
            .expect("cleans");

        assert_eq!(removed, 1);
        assert!(used.exists(), "a file still in use is not touched");
        assert!(!orphan.exists());
        assert!(other.exists(), "only torrent and resume files are considered");

        // Young orphans are left alone.
        std::fs::write(&orphan, b"d4:infod4:name1:bee").expect("writes");
        let removed = clean_torrent_files(&dir, &store, 7 * 24 * 3600, state::now())
            .await
            .expect("cleans");
        assert_eq!(removed, 0);
        assert!(orphan.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Resume data is named after the info hash rather than the `.torrent` path, so it
    /// has to be recognised as in use by the hash. Deleting a live one would cost the
    /// full re-check it exists to avoid.
    #[tokio::test]
    async fn a_live_resume_file_is_kept_and_an_orphaned_one_goes() {
        let dir = std::env::temp_dir().join("stremhu-rs-resume-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let live_hash = "aabbccddeeff00112233445566778899aabbccdd";
        let live_torrent = dir.join(format!("{live_hash}.torrent"));
        let live_resume = dir.join(format!("{live_hash}.resume"));
        let dead_resume = dir.join("0000000000000000000000000000000000000000.resume");
        for p in [&live_torrent, &live_resume, &dead_resume] {
            std::fs::write(p, b"x").expect("writes");
        }

        let store = store_with(vec![Item {
            info_hash: live_hash.into(),
            torrent_file: live_torrent.to_string_lossy().to_string(),
            ..Item::default()
        }])
        .await;

        let now = state::now() + 30 * 24 * 3600;
        let removed = clean_torrent_files(&dir, &store, 7 * 24 * 3600, now)
            .await
            .expect("cleans");

        assert_eq!(removed, 1);
        assert!(live_torrent.exists());
        assert!(live_resume.exists(), "the live resume file must survive");
        assert!(!dead_resume.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_missing_torrent_folder_is_not_an_error() {
        let store = store_with(vec![]).await;
        let missing = std::env::temp_dir().join("stremhu-rs-no-such-folder-xyz");
        let _ = std::fs::remove_dir_all(&missing);
        assert_eq!(
            clean_torrent_files(&missing, &store, 1, state::now())
                .await
                .expect("no error"),
            0
        );
    }

    #[test]
    fn the_report_reads_as_a_sentence() {
        let mut r = Report {
            considered: 3,
            deleted: vec!["a".into()],
            ..Report::default()
        };
        assert_eq!(r.summary(), "sweep looked at 3 downloads, deleted 1");
        r.abandoned = Some("tracker down".into());
        assert_eq!(r.summary(), "sweep abandoned: tracker down");
    }
}
