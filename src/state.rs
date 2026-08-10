//! What the server remembers between restarts: what was downloaded, how much of it
//! was actually watched, and what must not be deleted.
//!
//! A JSON file rather than a database. The implementation being replaced uses SQLite,
//! but it also stores the `.torrent` bytes and libtorrent resume blobs in tables,
//! which is what makes a database convenient there. Here those stay files on disk and
//! only the bookkeeping lives in this store: a few hundred rows at most in a
//! household, written a handful of times per viewing. A JSON file can be opened and
//! read by the person who owns it, needs no schema migration, and is one file to
//! delete.
//!
//! Writes are atomic through a temporary file and a rename, so losing power mid-write
//! cannot leave a truncated store behind, and they are batched by a flusher rather
//! than done per range request.
//!
//! # What counts as watched
//!
//! An addon never learns the playback position. Stremio keeps that to itself and
//! syncs it to the viewer's own account; the addon's whole contract is handing over a
//! URL, after which it sees nothing but HTTP range requests. So "watched" is inferred
//! from bytes: the furthest offset a player asked for, and how much was delivered in
//! total.
//!
//! Both are needed. The furthest offset alone would count a jump to the end as a full
//! viewing, and the total alone would miss someone who skipped the recap. Together
//! they mean "got to the credits, having actually streamed the thing".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Seconds since the epoch. Stored as a plain number so the file stays readable and
/// no timezone can be baked into it.
pub type Unix = u64;

/// One bit of the coverage map per megabyte.
///
/// A megabyte is the size of one served chunk, so the map lines up with what is actually
/// handed over, and a three-hour 4K film needs about three kilobytes.
const COVERAGE_CHUNK: u64 = 1024 * 1024;

/// The coverage map as hex in the state file.
///
/// A byte array would serialise as a list of hundreds of numbers, which would bury the rest
/// of the record. This file is meant to be readable by the person who owns it.
mod coverage_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        s.serialize_str(&hex)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        Ok(text
            .as_bytes()
            .chunks(2)
            .filter_map(|pair| {
                let s = std::str::from_utf8(pair).ok()?;
                u8::from_str_radix(s, 16).ok()
            })
            .collect())
    }
}

/// A quiet period long enough to call the next request a separate sitting.
///
/// Fifteen minutes: longer than any pause inside one viewing that still holds the
/// connection pattern together, and shorter than the gap between two people sitting down
/// to watch the same thing.
const VIEWING_GAP_SECS: Unix = 15 * 60;

pub fn now() -> Unix {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Which file of a torrent is the one to keep while the others are rotated off the disk.
///
/// The newest **finished** file. Newest because it is the one the swarm is most likely to still
/// want, and finished because that is what makes the tracker see us as a seeder at all: with a
/// half-downloaded file still wanted, the client announces that it has something left to fetch,
/// and the seeding clock is not something to gamble on in that state.
///
/// `None` when no file of the torrent has finished, and then nothing is rotated: there is
/// nothing that could safely hold the torrent open.
pub fn keeper_key(items: &[Item], info_hash: &str) -> Option<String> {
    items
        .iter()
        .filter(|i| i.info_hash == info_hash && i.completed_at.is_some())
        // Newest first, and the file index only to make a tie repeatable.
        .max_by_key(|i| (i.completed_at.unwrap_or(0), i.file_index))
        .map(|i| i.key())
}

/// How a served file is addressed, in the records and in the library alike.
pub fn item_key(info_hash: &str, file_index: usize) -> String {
    format!("{info_hash}:{file_index}")
}

/// One downloaded item.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Item {
    pub info_hash: String,
    /// The tracker's own id for this torrent. Needed because the hit-and-run list
    /// identifies torrents by id, not by info hash, so without it an obligation
    /// cannot be matched to a local download.
    pub ncore_torrent_id: String,
    /// What to show a human: the release name.
    pub title: String,
    /// The one file being served out of the torrent.
    pub file_name: String,
    /// Its index inside the torrent, so the same file is selected again after a
    /// restart without having to work out which episode it was.
    pub file_index: usize,
    pub file_len: u64,
    /// Where the data was written, so deletion knows what to remove.
    pub save_path: String,
    /// Path of the saved `.torrent`, removed along with the data.
    pub torrent_file: String,
    pub added_at: Unix,
    pub first_played_at: Option<Unix>,
    pub last_played_at: Option<Unix>,
    pub play_count: u32,
    /// Highest byte offset any player has asked for.
    pub furthest_byte: u64,

    /// Which parts of the file have actually been sent, one bit per megabyte.
    ///
    /// This is what decides whether something was watched, because it cannot be inflated by
    /// re-reading: a minute served twice sets the same bits twice. A three-hour 4K film needs
    /// about three kilobytes of it.
    #[serde(default, with = "coverage_hex")]
    pub served_map: Vec<u8>,
    /// What the tracker says has been downloaded and sent for this torrent, and when it
    /// said so.
    ///
    /// Taken from the tracker rather than counted locally, because the tracker's figure
    /// is the one that decides whether an obligation is met. A local counter would also
    /// restart from zero on every restart, while the obligation does not.
    pub tracker_downloaded_bytes: u64,
    pub tracker_uploaded_bytes: u64,
    /// As the tracker prints it, so no rounding of ours can disagree with their page.
    pub tracker_ratio: String,
    /// When those three were read. Kept so the interface can say how fresh they are
    /// rather than presenting a day-old number as current.
    pub tracker_figures_at: Option<Unix>,
    /// True when files inside the torrent were deliberately left out, which is the normal
    /// case for one episode taken from a season pack.
    ///
    /// Worth recording because a torrent that is not complete is a different thing to the
    /// tracker than one that is: it has no way to become a finished seed, so whatever it
    /// says about the obligation cannot be waited out.
    pub partial: bool,
    /// Whether the tracker's hit-and-run list still expected seeding on this the last time
    /// it was read.
    ///
    /// Stored rather than held in memory because it is a fact about the download, not about
    /// this run of the program: a restart used to leave the column blank until somebody
    /// pressed the button, which reads as "no obligation" when it actually means "we have not
    /// looked". The sweep still asks the tracker live before deleting anything; this is what
    /// the interface shows.
    pub owed_to_tracker: bool,
    /// Seed time the tracker still wanted, when it said. None when it did not say, or when
    /// nothing is owed.
    pub owed_remaining_secs: Option<Unix>,
    /// When the list was last read. None means never, which is not the same as "nothing owed".
    pub owed_checked_at: Option<Unix>,
    /// When every piece of this file arrived, if it has.
    ///
    /// The moment the file's own seeding clock starts. Not the same as `added_at`: a seven
    /// gigabyte episode is added in a moment and finishes hours later, and counting from the
    /// wrong end of that would have us throw it away hours early.
    pub completed_at: Option<Unix>,
    /// Marked watched by hand in the interface.
    ///
    /// A separate flag rather than doctored measurements. Somebody may have watched the episode
    /// on another device, or want the retention clock to start now, and the honest way to record
    /// that is as a statement by a person: the measured coverage stays what it was measured to
    /// be, and the two can be told apart afterwards.
    pub watched_manually: bool,
    /// Set by hand in the interface. Nothing automatic ever removes a kept item.
    pub keep: bool,
}

impl Item {
    /// How this record is addressed: the torrent and the file inside it.
    pub fn key(&self) -> String {
        item_key(&self.info_hash, self.file_index)
    }

    /// Whether the viewer got far enough for this to count as seen.
    ///
    /// Two conditions, both from what was actually sent to a player. The furthest position
    /// alone would count a jump to the end, and a player reads the tail before it plays a
    /// frame, so on its own it reads as complete within seconds. Distinct coverage alone
    /// would count someone who watched the first half twice.
    pub fn watched(&self, position_percent: u8, min_served_percent: u8) -> bool {
        // Somebody said so. That is a stronger statement than any measurement, and it is the
        // only way to account for an episode watched somewhere else.
        if self.watched_manually {
            return true;
        }
        if self.file_len == 0 || self.play_count == 0 {
            return false;
        }
        let reached = percent_of(self.furthest_byte, self.file_len) >= position_percent as u64;
        let covered = self.served_percent() >= min_served_percent as u64;
        reached && covered
    }

    /// How much of the file has been sent at least once, as a percentage.
    pub fn served_percent(&self) -> u64 {
        if self.file_len == 0 {
            return 0;
        }
        let chunks = self.file_len.div_ceil(COVERAGE_CHUNK);
        if chunks == 0 {
            return 0;
        }
        let seen: u64 = self.served_map.iter().map(|b| b.count_ones() as u64).sum();
        (seen.min(chunks) * 100 / chunks).min(100)
    }

    /// Marks the bytes from `offset` for `len` as sent.
    pub fn mark_served(&mut self, offset: u64, len: u64) {
        if self.file_len == 0 || len == 0 {
            return;
        }
        let chunks = self.file_len.div_ceil(COVERAGE_CHUNK);
        let needed = (chunks.div_ceil(8)) as usize;
        if self.served_map.len() < needed {
            self.served_map.resize(needed, 0);
        }

        let first = offset / COVERAGE_CHUNK;
        let last = (offset + len - 1) / COVERAGE_CHUNK;
        for chunk in first..=last.min(chunks.saturating_sub(1)) {
            let byte = (chunk / 8) as usize;
            if let Some(slot) = self.served_map.get_mut(byte) {
                *slot |= 1 << (chunk % 8);
            }
        }
    }

    /// Seconds since this was added.
    pub fn age(&self, now: Unix) -> u64 {
        now.saturating_sub(self.added_at)
    }

    /// How long it has been seeding, taken as its age. libtorrent is not asked,
    /// because a restart resets its counters while the obligation does not.
    /// How long this file has been finished, which is how long it has been paying its own way.
    ///
    /// Falls back to when it was added for a record written before this was tracked, or for a
    /// file that never finished. Both fall on the cautious side only by a matter of hours.
    pub fn file_seeded_for(&self, now: Unix) -> u64 {
        now.saturating_sub(self.completed_at.unwrap_or(self.added_at))
    }

    /// How long the torrent behind this record has been seeding, across every file taken from
    /// it.
    ///
    /// The obligation is the torrent's, and so is the clock: watching a second episode out of a
    /// pack raises what is owed but does not restart the time already served. Measured from the
    /// first file taken from that torrent, which is when it started announcing.
    pub fn torrent_seeded_for(&self, all: &[Item], now: Unix) -> u64 {
        let earliest = all
            .iter()
            .filter(|other| other.info_hash == self.info_hash)
            .map(|other| other.added_at)
            .min()
            .unwrap_or(self.added_at);
        now.saturating_sub(earliest)
    }

}

/// Drops a leading byte order mark.
///
/// Windows text editors and PowerShell's own redirection write one by default, and both
/// JSON and TOML parsers reject it as an unexpected character at line 1 column 1. These
/// files are meant to be editable by hand, so refusing to start over an invisible
/// character the editor added would be a poor way to treat that.
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Percentage of `total` that `part` represents, saturating and integer-only.
fn percent_of(part: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    ((part as u128 * 100) / total as u128).min(u64::MAX as u128) as u64
}

/// One sitting in front of something, for the history list.
///
/// Separate from the per-item counters because those answer "may this be deleted" while
/// this answers "what did we watch, and when". Someone looking for the episode they were
/// on last week wants the second question.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PlayEvent {
    pub at: Unix,
    /// Which record this sitting belongs to: `info_hash:file_index`.
    ///
    /// The alias keeps a history written when this was the info hash alone readable. Those
    /// entries then belong to no current record, which costs nothing: they are shown by title
    /// and dropped with the item they name.
    #[serde(alias = "info_hash")]
    pub key: String,
    pub title: String,
}

/// How many sittings are kept. Enough to cover a season or two of evenings, and small
/// enough that the file stays something a person can open and read.
const HISTORY_LIMIT: usize = 500;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct State {
    /// Keyed by torrent and file, `info_hash:file_index`.
    ///
    /// One record per served file rather than per torrent, because a season pack is one
    /// torrent holding many episodes and each episode is watched, seeded and deleted on its
    /// own. Keyed by the torrent alone, a second episode overwrote the first one's record.
    pub items: BTreeMap<String, Item>,
    /// Newest last.
    pub history: Vec<PlayEvent>,
    /// Date the sweep last ran, `YYYY-MM-DD` local time, so a restart cannot make it
    /// run a second time on the same day.
    pub last_sweep_date: String,
    /// When it last ran, to the second.
    ///
    /// The date alone cannot throttle the run that happens at startup: restarting the server
    /// six times in an afternoon would mean asking the tracker six times and six notifications.
    pub last_sweep_at: Unix,
}

pub struct Store {
    path: PathBuf,
    state: RwLock<State>,
    dirty: AtomicBool,
}

impl Store {
    /// Loads the store, starting empty when the file does not exist yet.
    ///
    /// A file that exists but cannot be parsed is kept, not overwritten: it is the
    /// only record of what is on disk, and losing it silently would orphan every
    /// download. The error is returned so startup can complain loudly instead.
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        let state = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let text = strip_bom(&text);
            if text.trim().is_empty() {
                State::default()
            } else {
                serde_json::from_str(text)
                    .with_context(|| format!("parsing {}", path.display()))?
            }
        } else {
            State::default()
        };

        // Re-keyed from the items themselves, so a file written when records were keyed by
        // info hash alone loads without a migration step and without losing anything. An item
        // with no info hash keeps whatever key it had rather than being dropped.
        let mut state = state;
        let rekeyed: BTreeMap<String, Item> = state
            .items
            .into_iter()
            .map(|(old_key, item)| {
                let key = if item.info_hash.is_empty() {
                    old_key
                } else {
                    item.key()
                };
                (key, item)
            })
            .collect();
        state.items = rekeyed;

        Ok(Arc::new(Self {
            path: path.to_path_buf(),
            state: RwLock::new(state),
            dirty: AtomicBool::new(false),
        }))
    }

    pub async fn items(&self) -> Vec<Item> {
        self.state.read().await.items.values().cloned().collect()
    }

    /// The keys of every record that came from this tracker id.
    ///
    /// Keys only, deliberately. This runs on every range request a player makes, and cloning
    /// the whole record set for it means copying every coverage map in the library several
    /// times a second while a film is playing.
    pub async fn keys_for_tracker_id(&self, torrent_id: &str) -> Vec<String> {
        if torrent_id.is_empty() {
            return Vec::new();
        }
        self.state
            .read()
            .await
            .items
            .iter()
            .filter(|(_, item)| item.ncore_torrent_id == torrent_id)
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// One record, by `info_hash:file_index`.
    pub async fn get(&self, key: &str) -> Option<Item> {
        self.state.read().await.items.get(key).cloned()
    }

    /// Records a download, or refreshes the parts that can change without disturbing
    /// what has been watched.
    pub async fn upsert(&self, item: Item) {
        let mut state = self.state.write().await;
        let key = item.key();
        match state.items.get_mut(&key) {
            Some(existing) => {
                // Viewing history and the keep flag survive: the same torrent can be
                // handed out again for a second episode or a re-watch.
                existing.ncore_torrent_id = item.ncore_torrent_id;
                existing.title = item.title;
                existing.file_name = item.file_name;
                existing.file_index = item.file_index;
                existing.file_len = item.file_len;
                existing.save_path = item.save_path;
                existing.torrent_file = item.torrent_file;
            }
            None => {
                state.items.insert(key, item);
            }
        }
        drop(state);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// A player opened the stream.
    ///
    /// One viewing is many requests. A player asks for the container header, then the
    /// index at the end of the file, then a range per few seconds of video, and it opens
    /// a fresh connection every time the viewer seeks: a single test of one film produced
    /// twenty-one of these. Counting each one as a viewing makes the figure meaningless,
    /// so requests close together in time are treated as the same sitting and only a gap
    /// starts a new one.
    pub async fn record_play(&self, key: &str, at: Unix) {
        let mut state = self.state.write().await;
        if let Some(item) = state.items.get_mut(key) {
            let new_sitting = match item.last_played_at {
                None => true,
                Some(last) => at.saturating_sub(last) >= VIEWING_GAP_SECS,
            };
            let title = item.title.clone();
            if new_sitting {
                item.play_count = item.play_count.saturating_add(1);
            }
            item.last_played_at = Some(at);
            if item.first_played_at.is_none() {
                item.first_played_at = Some(at);
            }

            // One entry per sitting, for the same reason the counter works that way.
            if new_sitting {
                state.history.push(PlayEvent {
                    at,
                    key: key.to_string(),
                    title,
                });
                let excess = state.history.len().saturating_sub(HISTORY_LIMIT);
                if excess > 0 {
                    state.history.drain(..excess);
                }
            }
        }
        drop(state);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// The most recent sittings, newest first.
    pub async fn history(&self, limit: usize) -> Vec<PlayEvent> {
        let state = self.state.read().await;
        state
            .history
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Bytes were delivered from `offset`. This is the only signal there is about how
    /// far a viewer got.
    pub async fn record_served(&self, key: &str, offset: u64, len: u64, at: Unix) {
        let mut state = self.state.write().await;
        if let Some(item) = state.items.get_mut(key) {
            item.mark_served(offset, len);
            item.furthest_byte = item.furthest_byte.max(offset.saturating_add(len));
            item.last_played_at = Some(at);
        }
        drop(state);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Records the tracker's hit-and-run answer against every download.
    ///
    /// `owed` is what the list contained: a tracker id and the seed time still wanted. Every
    /// item with a tracker id is stamped, whether it appeared on the list or not, because
    /// absence from the list is the answer "nothing is owed on this" and is exactly as much
    /// information as presence. An item with no tracker id cannot be matched either way and
    /// is left alone, so the interface can say so instead of implying an answer.
    pub async fn record_obligations(&self, owed: &[(String, Option<u64>)], at: Unix) -> usize {
        let mut state = self.state.write().await;
        let mut updated = 0;
        for item in state.items.values_mut() {
            if item.ncore_torrent_id.is_empty() {
                continue;
            }
            let found = owed.iter().find(|(id, _)| *id == item.ncore_torrent_id);
            item.owed_to_tracker = found.is_some();
            item.owed_remaining_secs = found.and_then(|(_, remaining)| *remaining);
            item.owed_checked_at = Some(at);
            updated += 1;
        }
        drop(state);
        if updated > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        updated
    }

    /// Records what the tracker says about a torrent's transfer, matched by its
    /// tracker id. Returns how many items were updated.
    pub async fn record_tracker_figures(
        &self,
        torrent_id: &str,
        uploaded: u64,
        downloaded: u64,
        ratio: &str,
        at: Unix,
    ) -> usize {
        let mut state = self.state.write().await;
        let mut updated = 0;
        for item in state.items.values_mut() {
            if item.ncore_torrent_id != torrent_id || torrent_id.is_empty() {
                continue;
            }
            item.tracker_uploaded_bytes = uploaded;
            item.tracker_downloaded_bytes = downloaded;
            item.tracker_ratio = ratio.to_string();
            item.tracker_figures_at = Some(at);
            updated += 1;
        }
        drop(state);
        if updated > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        updated
    }

    /// Records which file inside the torrent is the one being served.
    /// Corrects which file inside the torrent a record refers to.
    ///
    /// The index is part of the key, so the record moves rather than being edited in place.
    /// This runs at startup, where a record written before the index was tracked has it
    /// resolved from the file name instead.
    pub async fn set_file_index(&self, key: &str, index: usize) {
        let mut state = self.state.write().await;
        let Some(mut item) = state.items.get(key).cloned() else {
            return;
        };
        if item.file_index == index {
            return;
        }
        item.file_index = index;
        let new_key = item.key();
        state.items.remove(key);
        state.items.insert(new_key, item);
        drop(state);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Notes that a file finished downloading, the first time it is seen finished.
    pub async fn mark_completed(&self, key: &str, at: Unix) -> bool {
        let mut state = self.state.write().await;
        let changed = match state.items.get_mut(key) {
            Some(item) if item.completed_at.is_none() => {
                item.completed_at = Some(at);
                true
            }
            _ => false,
        };
        drop(state);
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        changed
    }

    /// Marks a record watched, or takes that back.
    pub async fn set_watched(&self, key: &str, watched: bool) -> bool {
        let mut state = self.state.write().await;
        let found = match state.items.get_mut(key) {
            Some(item) => {
                item.watched_manually = watched;
                true
            }
            None => false,
        };
        drop(state);
        if found {
            self.dirty.store(true, Ordering::Relaxed);
        }
        found
    }

    pub async fn set_keep(&self, key: &str, keep: bool) -> bool {
        let mut state = self.state.write().await;
        let found = match state.items.get_mut(key) {
            Some(item) => {
                item.keep = keep;
                true
            }
            None => false,
        };
        drop(state);
        if found {
            self.dirty.store(true, Ordering::Relaxed);
        }
        found
    }

    /// Forgets a download, and the sittings that belong to it.
    ///
    /// The history follows the item rather than outliving it. Stremio keeps its own record
    /// of what has been watched and syncs it to the viewer's account, so a second copy here
    /// of something no longer on the disk serves nobody and grows without limit. What this
    /// history is for is the shorter question: what is here, and when did we last watch it.
    pub async fn remove(&self, key: &str) -> Option<Item> {
        let mut state = self.state.write().await;
        let gone = state.items.remove(key);
        let before = state.history.len();
        state.history.retain(|e| e.key != key);
        let dropped = before - state.history.len();
        drop(state);
        if gone.is_some() || dropped > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        gone
    }

    pub async fn last_sweep_date(&self) -> String {
        self.state.read().await.last_sweep_date.clone()
    }

    pub async fn last_sweep_at(&self) -> Unix {
        self.state.read().await.last_sweep_at
    }

    pub async fn set_last_sweep_at(&self, at: Unix) {
        self.state.write().await.last_sweep_at = at;
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub async fn set_last_sweep_date(&self, date: &str) {
        self.state.write().await.last_sweep_date = date.to_string();
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Writes the file when something changed. Atomic, so an interrupted write cannot
    /// destroy the record of what is on disk.
    pub async fn flush(&self) -> Result<()> {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let text = {
            let state = self.state.read().await;
            serde_json::to_string_pretty(&*state).context("serialising the state")?
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, text) {
            // Put the flag back so the next tick tries again rather than losing it.
            self.dirty.store(true, Ordering::Relaxed);
            return Err(e).with_context(|| format!("writing {}", tmp.display()));
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            self.dirty.store(true, Ordering::Relaxed);
            return Err(e).with_context(|| format!("replacing {}", self.path.display()));
        }
        Ok(())
    }
}

/// Writes the store out periodically. A range request arrives for every megabyte of
/// video, so saving on each change would mean rewriting the file constantly for no
/// benefit; losing a few seconds of "how far did I get" to a crash costs nothing.
pub fn spawn_flusher(store: Arc<Store>, every: Duration) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            if let Err(e) = store.flush().await {
                tracing::warn!(error = %e, "could not write the state file");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seeding clock is the torrent's, not the file's: a second episode raises what is
    /// owed, and must not throw away the time already served.
    #[tokio::test]
    async fn the_seeding_clock_belongs_to_the_torrent() {
        let now = 1_000_000u64;
        let first = Item {
            info_hash: "pack".into(),
            file_index: 1,
            added_at: now - 100 * 3600,
            ..Item::default()
        };
        let second = Item {
            info_hash: "pack".into(),
            file_index: 2,
            added_at: now - 2 * 3600,
            ..Item::default()
        };
        let elsewhere = Item {
            info_hash: "other".into(),
            added_at: now - 900 * 3600,
            ..Item::default()
        };
        let all = vec![first.clone(), second.clone(), elsewhere];

        assert_eq!(second.age(now) / 3600, 2, "this file is two hours old");
        assert_eq!(
            second.torrent_seeded_for(&all, now) / 3600,
            100,
            "but its torrent has been seeding for a hundred"
        );
        assert_eq!(first.torrent_seeded_for(&all, now) / 3600, 100);
    }

    /// The keeper is the newest finished file, and a torrent with nothing finished keeps
    /// everything: a half-downloaded file cannot hold the torrent open, because the client then
    /// announces that it still wants something and the tracker stops seeing a seeder.
    #[test]
    fn the_keeper_is_the_newest_finished_file() {
        let pack = |index: usize, completed: Option<Unix>| Item {
            info_hash: "pack".into(),
            file_index: index,
            completed_at: completed,
            ..Item::default()
        };

        let items = vec![
            pack(1, Some(1_000)),
            pack(2, Some(3_000)),
            pack(3, Some(2_000)),
            pack(4, None),
            Item {
                info_hash: "other".into(),
                file_index: 9,
                completed_at: Some(9_999),
                ..Item::default()
            },
        ];
        assert_eq!(keeper_key(&items, "pack").as_deref(), Some("pack:2"));
        assert_eq!(keeper_key(&items, "other").as_deref(), Some("other:9"));
        assert_eq!(keeper_key(&items, "absent"), None);

        // Nothing finished: no keeper, so nothing may be rotated away.
        let unfinished = vec![pack(1, None), pack(2, None)];
        assert_eq!(keeper_key(&unfinished, "pack"), None);

        // A tie is broken by the file index rather than left to the map's order.
        let tied = vec![pack(5, Some(7_000)), pack(6, Some(7_000))];
        assert_eq!(keeper_key(&tied, "pack").as_deref(), Some("pack:6"));
    }

    /// The file's own clock runs from when it finished, not from when it was added: a seven
    /// gigabyte episode takes hours to arrive, and counting from the wrong end would throw it
    /// away early.
    #[tokio::test]
    async fn a_files_clock_runs_from_when_it_finished() {
        let path = temp_path("completed-at.json");
        let store = Store::load(&path).expect("loads");
        store
            .upsert(Item {
                info_hash: "c".into(),
                added_at: 1_000,
                ..Item::default()
            })
            .await;

        let before = store.get("c:0").await.expect("there");
        assert_eq!(before.completed_at, None);
        assert_eq!(before.file_seeded_for(4_600), 3_600, "falls back to added_at");

        assert!(store.mark_completed("c:0", 4_000).await);
        let after = store.get("c:0").await.expect("there");
        assert_eq!(after.completed_at, Some(4_000));
        assert_eq!(after.file_seeded_for(4_600), 600);

        // Only the first time: a restart re-checking the files must not move the clock.
        assert!(!store.mark_completed("c:0", 9_000).await);
        assert_eq!(
            store.get("c:0").await.expect("there").completed_at,
            Some(4_000)
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Marking by hand has to satisfy the deletion rule on its own: that is the point of it.
    /// It must not touch the measurement, so the two can still be told apart.
    #[tokio::test]
    async fn marking_watched_by_hand_counts_as_watched() {
        let path = temp_path("manual-watched.json");
        let store = Store::load(&path).expect("loads");
        store
            .upsert(Item {
                info_hash: "m".into(),
                file_len: 1_000_000,
                ..Item::default()
            })
            .await;

        let before = store.get("m:0").await.expect("there");
        assert!(!before.watched(90, 50), "never played, so not watched");

        assert!(store.set_watched("m:0", true).await);
        let after = store.get("m:0").await.expect("there");
        assert!(after.watched(90, 50));
        assert_eq!(after.served_percent(), 0, "the measurement is untouched");
        assert_eq!(after.play_count, 0);

        assert!(store.set_watched("m:0", false).await);
        assert!(!store.get("m:0").await.expect("there").watched(90, 50));
        assert!(!store.set_watched("nothing:0", true).await);

        let _ = std::fs::remove_file(&path);
    }

    /// The point of keying by file: a season pack is one torrent holding many episodes, and
    /// each episode is watched, seeded and deleted on its own. Keyed by the torrent alone, the
    /// second episode overwrote the first one's record and the first one's viewing history.
    #[tokio::test]
    async fn two_episodes_of_one_torrent_are_two_records() {
        let path = temp_path("season-pack.json");
        let store = Store::load(&path).expect("loads");

        for (index, name) in [(3usize, "S02E04.mkv"), (5, "S02E06.mkv")] {
            store
                .upsert(Item {
                    info_hash: "packhash".into(),
                    ncore_torrent_id: "555".into(),
                    file_index: index,
                    file_name: name.into(),
                    title: name.into(),
                    file_len: 1_000_000,
                    ..Item::default()
                })
                .await;
        }
        assert_eq!(store.items().await.len(), 2, "one record per episode");

        // Watched separately: what happens to one must not show up on the other.
        store.record_play("packhash:3", 1_000_000).await;
        store.record_served("packhash:3", 0, 900_000, 1_000_000).await;
        let fourth = store.get("packhash:3").await.expect("there");
        let sixth = store.get("packhash:5").await.expect("there");
        assert_eq!(fourth.play_count, 1);
        assert_eq!(sixth.play_count, 0, "the other episode was not touched");
        assert!(fourth.served_percent() >= 89);
        assert_eq!(sixth.served_percent(), 0);

        // And deleted separately.
        assert!(store.remove("packhash:3").await.is_some());
        assert!(store.get("packhash:5").await.is_some(), "the other episode stays");
        assert_eq!(store.items().await.len(), 1);

        // The tracker's answer applies to the torrent, so it reaches every episode of it.
        store.record_obligations(&[("555".into(), Some(7200))], 500).await;
        let left = store.get("packhash:5").await.expect("there");
        assert!(left.owed_to_tracker);
        assert_eq!(left.owed_remaining_secs, Some(7200));

        let _ = std::fs::remove_file(&path);
    }

    /// Absence from the tracker's list is an answer, and it has to be recorded as one:
    /// leaving the previous "still owed" in place would keep a finished torrent forever.
    #[tokio::test]
    async fn falling_off_the_tracker_list_clears_the_obligation() {
        let dir = std::env::temp_dir().join("stremhu-obligations");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");
        let _ = std::fs::remove_file(&path);
        let store = Store::load(&path).expect("loads");

        store
            .upsert(Item {
                info_hash: "aaaa".into(),
                ncore_torrent_id: "111".into(),
                file_len: 1000,
                ..Default::default()
            })
            .await;
        store
            .upsert(Item {
                info_hash: "bbbb".into(),
                ncore_torrent_id: "222".into(),
                file_len: 1000,
                ..Default::default()
            })
            .await;
        // No tracker id: nothing can be said about it either way.
        store
            .upsert(Item {
                info_hash: "cccc".into(),
                ncore_torrent_id: String::new(),
                file_len: 1000,
                ..Default::default()
            })
            .await;

        let owed = vec![("111".to_string(), Some(3600u64))];
        assert_eq!(store.record_obligations(&owed, 500).await, 2);

        let items = store.items().await;
        let by = |hash: &str| items.iter().find(|i| i.info_hash == hash).cloned().unwrap();

        let a = by("aaaa");
        assert!(a.owed_to_tracker);
        assert_eq!(a.owed_remaining_secs, Some(3600));
        assert_eq!(a.owed_checked_at, Some(500));

        let b = by("bbbb");
        assert!(!b.owed_to_tracker, "not on the list means nothing is owed");
        assert_eq!(b.owed_remaining_secs, None);
        assert_eq!(b.owed_checked_at, Some(500), "checked, and the answer was no");

        let c = by("cccc");
        assert_eq!(c.owed_checked_at, None, "no tracker id, so no answer either way");

        // And the next reading, with the obligation now met, must clear it.
        assert_eq!(store.record_obligations(&[], 900).await, 2);
        let items = store.items().await;
        let a = items.iter().find(|i| i.info_hash == "aaaa").unwrap();
        assert!(!a.owed_to_tracker);
        assert_eq!(a.owed_remaining_secs, None);
        assert_eq!(a.owed_checked_at, Some(900));

        let _ = std::fs::remove_file(&path);
    }


    /// A realistic film. Sizes here are in gigabytes rather than kilobytes on purpose: the
    /// coverage map has one bit per megabyte, so a ten kilobyte test file is a single bit and
    /// cannot express "half watched" at all.
    fn film(len: u64) -> Item {
        Item {
            info_hash: "abc".into(),
            file_len: len,
            play_count: 1,
            ..Item::default()
        }
    }

    const GB: u64 = 1024 * 1024 * 1024;

    /// Watching a film through: this is the case that should count, and 90% is where end
    /// credits usually start.
    #[test]
    fn getting_to_the_credits_counts_as_watched() {
        let mut f = film(20 * GB);
        f.furthest_byte = 19 * GB;
        f.mark_served(0, 19 * GB);
        assert!(f.watched(90, 50));
        assert_eq!(f.served_percent(), 95);
    }

    /// The case that was going wrong in practice. A player reads the tail of the file before
    /// it shows a frame, so the furthest position reads as complete within seconds; without a
    /// coverage floor a film glanced at for a moment counted as watched.
    #[test]
    fn skipping_straight_to_the_end_is_not_watched() {
        let mut f = film(20 * GB);
        f.furthest_byte = 20 * GB; // the player probed the end
        f.mark_served(0, 200 * 1024 * 1024); // and only the opening came out
        assert!(!f.watched(90, 50));
        assert_eq!(f.served_percent(), 0, "200 MB of 20 GB rounds to nothing");
    }

    /// The fault this replaces: a running total counts the same minute twice whenever a
    /// player re-requests a range, and they do that constantly. Coverage cannot be inflated
    /// that way.
    #[test]
    fn re_reading_the_same_part_does_not_add_up_to_watched() {
        let mut f = film(20 * GB);
        f.furthest_byte = 20 * GB;
        // The first quarter served four times over: twenty gigabytes handed out, a quarter of
        // the film actually seen.
        for _ in 0..4 {
            f.mark_served(0, 5 * GB);
            // What the old running total would have counted, kept as a comment because it is the
        // reason this measure exists: 20 GB served on a 20 GB file, of which 15 GB was the
        // same quarter re-read four times.
        }
        assert_eq!(f.served_percent(), 25, "and the honest answer is a quarter");
        assert_eq!(f.served_percent(), 25, "the honest answer");
        assert!(!f.watched(90, 50));
    }

    /// Someone who skips the recap and the credits still watched the episode.
    #[test]
    fn skipping_the_intro_still_counts() {
        let mut f = film(20 * GB);
        f.furthest_byte = 19 * GB;
        f.mark_served(GB, 18 * GB); // the first gigabyte skipped
        assert!(f.watched(90, 50));
    }

    #[test]
    fn stopping_halfway_is_not_watched() {
        let mut f = film(20 * GB);
        f.furthest_byte = 10 * GB;
        f.mark_served(0, 10 * GB);
        assert!(!f.watched(90, 50), "the credits were never reached");
    }

    #[test]
    fn a_download_that_was_never_opened_is_not_watched() {
        let mut f = film(20 * GB);
        f.play_count = 0;
        f.furthest_byte = 20 * GB;
        f.mark_served(0, 20 * GB);
        assert!(!f.watched(90, 50));
    }

    #[test]
    fn a_zero_length_file_cannot_be_watched() {
        assert!(!film(0).watched(90, 50));
        assert_eq!(film(0).served_percent(), 0);
    }

    /// A 48 GB 4K release is a realistic size here, so the arithmetic must not overflow or
    /// lose precision, and the map must stay small.
    #[test]
    fn the_figures_hold_at_realistic_sizes() {
        let len = 48_560_000_000u64;
        let mut f = film(len);
        f.furthest_byte = len / 100 * 91;
        f.mark_served(0, len / 100 * 91);
        assert!(f.watched(90, 50));
        // One bit per megabyte: a 48 GB film costs about six kilobytes to track.
        assert!(f.served_map.len() < 8 * 1024, "map is {} bytes", f.served_map.len());

        let mut f = film(len);
        f.furthest_byte = len / 100 * 89;
        f.mark_served(0, len / 100 * 89);
        assert!(!f.watched(90, 50), "89% has not reached the credits");
    }

    /// The map survives a trip through the state file, or a restart mid-film would lose what
    /// had been watched.
    #[test]
    fn the_coverage_map_round_trips_through_the_file() {
        let mut f = film(20 * GB);
        f.mark_served(0, 12 * GB);
        let before = f.served_percent();

        let text = serde_json::to_string(&f).expect("serialises");
        assert!(text.contains("\"served_map\":\""), "stored as a hex string: {text:.120}");
        let back: Item = serde_json::from_str(&text).expect("parses");

        assert_eq!(back.served_map, f.served_map);
        assert_eq!(back.served_percent(), before);
    }

    #[test]
    fn percent_of_is_saturating() {
        assert_eq!(percent_of(0, 0), 0);
        assert_eq!(percent_of(50, 100), 50);
        assert_eq!(percent_of(u64::MAX, u64::MAX), 100);
        assert_eq!(percent_of(1, 3), 33);
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("stremhu-rs-state-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn a_missing_file_starts_empty_and_round_trips() {
        let path = temp_path("round-trip.json");
        let store = Store::load(&path).expect("loads");
        assert!(store.items().await.is_empty());

        store
            .upsert(Item {
                info_hash: "hash1".into(),
                ncore_torrent_id: "4207293".into(),
                title: "A hegyi doktor S19E08".into(),
                file_len: 1000,
                added_at: 100,
                ..Item::default()
            })
            .await;
        store.record_play("hash1:0", 200).await;
        store.record_served("hash1:0", 0, 950, 210).await;
        store.flush().await.expect("writes");

        let back = Store::load(&path).expect("reloads");
        let item = back.get("hash1:0").await.expect("kept");
        assert_eq!(item.ncore_torrent_id, "4207293");
        assert_eq!(item.play_count, 1);
        assert_eq!(item.furthest_byte, 950);
        assert!(item.watched(90, 50));
        let _ = std::fs::remove_file(&path);
    }

    /// Re-adding a torrent must not wipe what is known about it, or a season pack
    /// handed out for a second episode would forget the first viewing.
    #[tokio::test]
    async fn re_adding_keeps_the_history_and_the_keep_flag() {
        let path = temp_path("re-add.json");
        let store = Store::load(&path).expect("loads");
        store
            .upsert(Item {
                info_hash: "h".into(),
                title: "old".into(),
                file_len: 100,
                added_at: 10,
                ..Item::default()
            })
            .await;
        store.record_play("h:0", 20).await;
        store.set_keep("h:0", true).await;

        store
            .upsert(Item {
                info_hash: "h".into(),
                title: "new title".into(),
                file_len: 200,
                added_at: 999,
                ..Item::default()
            })
            .await;

        let item = store.get("h:0").await.expect("still there");
        assert_eq!(item.title, "new title");
        assert_eq!(item.file_len, 200);
        assert_eq!(item.play_count, 1, "the viewing survived");
        assert!(item.keep, "the keep flag survived");
        assert_eq!(item.added_at, 10, "the original download date survived");
        let _ = std::fs::remove_file(&path);
    }

    /// Measured on a real television session: one test of one film produced twenty-one
    /// body requests. Counting each as a viewing makes the number meaningless.
    #[tokio::test]
    async fn one_sitting_counts_once_however_many_requests_it_makes() {
        let path = temp_path("sittings.json");
        let store = Store::load(&path).expect("loads");
        store
            .upsert(Item {
                info_hash: "h".into(),
                file_len: 10_000,
                ..Item::default()
            })
            .await;

        // A player opening the header, the tail, then ranges, all within a minute.
        let start = 1_000_000;
        for offset in [0, 1, 2, 30, 45, 60] {
            store.record_play("h:0", start + offset).await;
        }
        assert_eq!(store.get("h:0").await.expect("there").play_count, 1);

        // A pause inside the same viewing, still one sitting.
        store.record_play("h:0", start + 10 * 60).await;
        assert_eq!(store.get("h:0").await.expect("there").play_count, 1);

        // Coming back the next evening is a second one.
        store.record_play("h:0", start + 86_400).await;
        assert_eq!(store.get("h:0").await.expect("there").play_count, 2);

        let item = store.get("h:0").await.expect("there");
        assert_eq!(item.first_played_at, Some(start), "the first is remembered");
        assert_eq!(item.last_played_at, Some(start + 86_400));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn the_furthest_byte_never_goes_backwards() {
        let path = temp_path("furthest.json");
        let store = Store::load(&path).expect("loads");
        store
            .upsert(Item {
                info_hash: "h".into(),
                file_len: 10_000,
                ..Item::default()
            })
            .await;
        store.record_served("h:0", 8_000, 500, 1).await;
        store.record_served("h:0", 100, 500, 2).await; // viewer seeks back
        let item = store.get("h:0").await.expect("there");
        assert_eq!(item.furthest_byte, 8_500);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn flushing_is_skipped_when_nothing_changed() {
        let path = temp_path("dirty.json");
        let store = Store::load(&path).expect("loads");
        store.flush().await.expect("no-op");
        assert!(!path.exists(), "an untouched store writes no file");

        store
            .upsert(Item {
                info_hash: "h".into(),
                ..Item::default()
            })
            .await;
        store.flush().await.expect("writes");
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    /// A corrupt file is the only record of what sits on disk; it must not be
    /// silently replaced with an empty one.
    #[tokio::test]
    async fn a_corrupt_file_is_reported_not_overwritten() {
        let path = temp_path("corrupt.json");
        std::fs::write(&path, "{ this is not json").expect("writes");
        assert!(Store::load(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("still there"),
            "{ this is not json"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A file saved by a Windows editor or by PowerShell redirection starts with a byte
    /// order mark. Refusing to start because of an invisible character the editor added
    /// would be a poor way to treat a file meant to be edited by hand.
    #[tokio::test]
    async fn a_byte_order_mark_does_not_stop_the_store_loading() {
        let path = temp_path("bom.json");
        let json = r#"{"items":{"h":{"info_hash":"h","title":"Film","keep":true}}}"#;
        std::fs::write(&path, format!("\u{feff}{json}")).expect("writes");

        let store = Store::load(&path).expect("loads despite the mark");
        let item = store.get("h:0").await.expect("read back");
        assert_eq!(item.title, "Film");
        assert!(item.keep);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stripping_the_mark_leaves_everything_else_alone() {
        assert_eq!(strip_bom("\u{feff}{}"), "{}");
        assert_eq!(strip_bom("{}"), "{}");
        assert_eq!(strip_bom(""), "");
        // Only a leading one, and only one.
        assert_eq!(strip_bom("x\u{feff}y"), "x\u{feff}y");
        assert_eq!(strip_bom("\u{feff}\u{feff}x"), "\u{feff}x");
    }

    #[tokio::test]
    async fn an_empty_file_is_treated_as_an_empty_store() {
        let path = temp_path("empty.json");
        std::fs::write(&path, "   \n").expect("writes");
        let store = Store::load(&path).expect("loads");
        assert!(store.items().await.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn removing_an_item_forgets_it() {
        let path = temp_path("remove.json");
        let store = Store::load(&path).expect("loads");
        store
            .upsert(Item {
                info_hash: "h".into(),
                ..Item::default()
            })
            .await;
        assert!(store.remove("h:0").await.is_some());
        assert!(store.get("h:0").await.is_none());
        assert!(store.remove("h:0").await.is_none(), "twice is not an error");
        let _ = std::fs::remove_file(&path);
    }

    /// A deleted film takes its own history with it. Stremio keeps the real viewing record,
    /// so a second copy here of something no longer on disk only grows the file.
    #[tokio::test]
    async fn deleting_a_download_clears_its_history_but_not_the_others() {
        let path = temp_path("history-cleanup.json");
        let store = Store::load(&path).expect("loads");
        for hash in ["gone", "stays"] {
            store
                .upsert(Item {
                    info_hash: hash.into(),
                    title: hash.into(),
                    file_len: 100,
                    ..Item::default()
                })
                .await;
        }
        // Two sittings each, spaced far enough apart to count separately.
        let start = 2_000_000;
        for offset in [0, 86_400] {
            store.record_play("gone:0", start + offset).await;
            store.record_play("stays:0", start + offset).await;
        }
        assert_eq!(store.history(100).await.len(), 4);

        store.remove("gone:0").await;

        let left = store.history(100).await;
        assert_eq!(left.len(), 2, "only the surviving download's sittings remain");
        assert!(left.iter().all(|e| e.key == "stays:0"));

        // And it is written out, not just dropped in memory.
        store.flush().await.expect("writes");
        let reloaded = Store::load(&path).expect("reloads");
        assert_eq!(reloaded.history(100).await.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// The list cannot grow without limit, however long the server runs.
    #[tokio::test]
    async fn the_history_is_capped() {
        let path = temp_path("history-cap.json");
        let store = Store::load(&path).expect("loads");
        store
            .upsert(Item {
                info_hash: "h".into(),
                title: "Film".into(),
                file_len: 100,
                ..Item::default()
            })
            .await;
        // Each an hour apart, so every one is its own sitting.
        for i in 0..(HISTORY_LIMIT as u64 + 25) {
            store.record_play("h:0", 3_000_000 + i * 3600).await;
        }
        let all = store.history(usize::MAX).await;
        assert_eq!(all.len(), HISTORY_LIMIT);
        // The newest are the ones kept.
        assert_eq!(all[0].at, 3_000_000 + (HISTORY_LIMIT as u64 + 24) * 3600);
        let _ = std::fs::remove_file(&path);
    }

}
