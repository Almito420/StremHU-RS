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
    pub kept: Vec<(String, &'static str)>,
    /// Set when the run was abandoned instead of performed.
    pub abandoned: Option<String>,
}

impl Report {
    /// The run in one message, for a push notification.
    ///
    /// Sent whether anything went or not: "nothing was deleted" is the answer to the same
    /// question as "these went", and a nightly job that only speaks up sometimes is a job you
    /// end up checking by hand. The reasons are grouped rather than listed per download, so a
    /// library of fifty does not arrive as fifty lines on a phone.
    pub fn notification(&self, disks: &[String]) -> String {
        let mut out = String::new();
        if let Some(why) = &self.abandoned {
            out.push_str(&format!("A takarítás megállt: {why}\nSemmi nem törlődött."));
        } else if self.deleted.is_empty() {
            out.push_str(&format!(
                "Nem törölt semmit. {} letöltés van a nyilvántartásban.",
                self.considered
            ));
            let mut grouped: Vec<(&str, usize)> = Vec::new();
            for (_, why) in &self.kept {
                match grouped.iter_mut().find(|(w, _)| w == why) {
                    Some((_, n)) => *n += 1,
                    None => grouped.push((why, 1)),
                }
            }
            grouped.sort_by(|a, b| b.1.cmp(&a.1));
            for (why, n) in grouped {
                out.push_str(&format!("\n{n} db: {why}"));
            }
        } else {
            out.push_str(&format!(
                "Törölve {} db, felszabadult {}:",
                self.deleted.len(),
                crate::media::size_label(self.freed_bytes)
            ));
            for title in &self.deleted {
                out.push_str(&format!("\n{title}"));
            }
            if !self.kept.is_empty() {
                out.push_str(&format!("\nMegtartva: {} db.", self.kept.len()));
            }
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
    /// Torrent ids the tracker still expects seeding on. An error abandons the run.
    fn owed_torrent_ids(&self) -> impl Future<Output = Result<Vec<String>>> + Send;
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

    // Only worth asking the tracker when its answer can change an outcome.
    let owed: Vec<String> = if cfg.hit_and_run && cfg.enable_deletion {
        match world.owed_torrent_ids().await {
            Ok(ids) => ids,
            Err(e) => {
                // Deliberately give up rather than delete with an unknown answer.
                report.abandoned = Some(format!("could not read the tracker's list: {e}"));
                return report;
            }
        }
    } else {
        Vec::new()
    };

    let streaming = world.streaming_hashes().await;
    let mut doomed: Vec<Item> = Vec::new();

    for item in items {
        let candidate = Candidate {
            kept: item.keep,
            watched: item.watched(cfg.watched_position_percent, cfg.watched_min_served_percent),
            owed_to_tracker: !item.ncore_torrent_id.is_empty()
                && owed.iter().any(|id| *id == item.ncore_torrent_id),
            // The list was read at the top of this run, and a run whose read failed was
            // abandoned before reaching here, so absence from it is an answer of now.
            tracker_says_clear: !item.ncore_torrent_id.is_empty()
                && !owed.iter().any(|id| *id == item.ncore_torrent_id),
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
            Verdict::Keep(why) => report.kept.push((item.title.clone(), why)),
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
                report.deleted.push(item.title.clone());
            } else {
                report
                    .kept
                    .push((item.title.clone(), "a törlés nem sikerült"));
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
    for (title, reason) in &report.kept {
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

    world.check_disk_space().await;
    let disks = world.disk_lines().await;
    world
        .notify(&format!("{why}\n{}", report.notification(&disks)))
        .await;
    report
}

pub fn spawn<W: World + 'static>(world: Arc<W>, store: Arc<Store>) {
    tokio::spawn(async move {
        // Once at startup, after a pause: the torrents have just been re-opened and the first
        // announce has not happened yet, and asking the tracker before that would be asking
        // about a state we have not reported.
        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
        let settings = world.settings().await;
        let quiet_enough = state::now().saturating_sub(store.last_sweep_at().await) >= 3600;
        if settings.enable_deletion && quiet_enough {
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
        async fn owed_torrent_ids(&self) -> Result<Vec<String>> {
            match &self.owed {
                Some(ids) => Ok(ids.clone()),
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
        assert!(msg.contains("Törölve 1 db"), "{msg}");
        assert!(msg.contains("film h1"), "{msg}");
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
        assert!(msg.contains("Nem törölt semmit"), "{msg}");
        assert!(msg.contains("1 db: megtartásra jelölve"), "{msg}");
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
                "a tracker szerint még seedelni kell"
            )]
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
        assert_eq!(report.kept[0].1, "épp játszik");
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
        assert_eq!(report.kept[0].1, "az automatikus törlés ki van kapcsolva");
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
        assert_eq!(report.kept[0].1, "a törlés nem sikerült");
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
