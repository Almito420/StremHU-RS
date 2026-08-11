//! The downloads page and the actions on it.
//!
//! Split out of `ui` because it is a separate job with separate state: `ui` is about who is
//! logged in and what the settings are, this is about what is on the disk. The two only share
//! the session check and the page shell.

use std::sync::Arc;

use axum::extract::{Form, State};
use axum::http::HeaderMap;
use axum::response::Response;

use crate::app::*;
use crate::ui::{cookie_header, html, require_login};

/// The by-hand watched flag.
#[derive(serde::Deserialize)]
pub(crate) struct WatchedForm {
    key: String,
    watched: String,
}

/// Marks one file watched, or takes that back.
///
/// Deliberately per file, like everything else about a pack: marking the episode you watched
/// elsewhere starts its retention clock without touching its neighbours. It does not delete
/// anything by itself; the seeding obligation still has to be paid off first.
pub(crate) async fn ui_set_watched(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<WatchedForm>,
) -> Response {
    if let Some(page) = require_login(&state, cookie_header(&headers)).await {
        return page;
    }
    let watched = form.watched == "1";
    let message = if state.store.set_watched(&form.key, watched).await {
        let _ = state.store.flush().await;
        if watched {
            "Megnézettnek jelölve. A törléshez ezen felül a seedelési kötelezettségnek is le \
             kell telnie erre a fájlra."
                .to_string()
        } else {
            "Már nincs megnézettnek jelölve.".to_string()
        }
    } else {
        "Ez a letöltés nincs a nyilvántartásban.".to_string()
    };
    downloads_page(&state, Some(message)).await
}

/// What to call a torrent on the page.
///
/// The folder its files were written into, which libtorrent names after the torrent itself for
/// anything with more than one file. A single-file torrent has no folder of its own, and then the
/// caller falls back to the file's name, which is the whole of what it is anyway.
fn release_name(items: &[crate::state::Item], hash: &str) -> Option<String> {
    let item = items.iter().find(|i| i.info_hash == hash)?;
    let path = std::path::Path::new(&item.save_path);
    let parent = path.parent()?;
    let name = parent.file_name()?.to_string_lossy().to_string();
    // The download folder itself is not a release name.
    let root = std::path::Path::new(&item.save_path)
        .parent()
        .and_then(|p| p.parent())
        .is_some();
    (root && name.to_lowercase() != "downloads").then_some(name)
}

/// The seeding obligation as one word and a colour.
///
/// Three states on purpose. Presence on the tracker's hit-and-run list means seeding is still
/// owed; absence from it, once the list has actually been read, means nothing is owed. Never
/// having read the list is neither, and showing that as "nem" would be a green light nobody
/// gave: the sweep would still refuse to delete, so the page would be contradicting the
/// behaviour.
fn owed_label(
    item: &crate::state::Item,
    owes: bool,
    asked_now: bool,
) -> (&'static str, &'static str) {
    if item.ncore_torrent_id.is_empty() {
        return ("?", "owed-unknown");
    }
    if !asked_now && item.owed_checked_at.is_none() {
        return ("?", "owed-unknown");
    }
    // The same standard the deletion decision uses. Without figures the tracker has no record of
    // this torrent, and a green "no" next to a download the rules will not release is the page
    // contradicting the behaviour.
    if !owes && item.tracker_figures_at.is_none() {
        return ("?", "owed-unknown");
    }
    if owes {
        ("igen", "owed-yes")
    } else {
        ("nem", "owed-no")
    }
}

/// What goes under that word: the seeding still wanted, or where the answer came from.
fn owed_detail(
    item: &crate::state::Item,
    owes: bool,
    asked_now: bool,
    remaining_now: Option<u64>,
    now: crate::state::Unix,
) -> String {
    if item.ncore_torrent_id.is_empty() {
        return "nincs tracker azonosító".into();
    }
    let remaining = if asked_now {
        remaining_now
    } else {
        item.owed_remaining_secs
    };
    if owes {
        return match remaining {
            Some(secs) => format!("még {}", crate::webui::human_duration(secs)),
            None => "a hátralévő időt nem írta ki".into(),
        };
    }
    if !owes && item.tracker_figures_at.is_none() {
        return "a tracker még nem tud erről a torrentről".into();
    }
    match (asked_now, item.owed_checked_at) {
        (true, _) => "épp most kérdeztük".into(),
        (false, Some(at)) => format!(
            "utoljára kérdezve: {}",
            crate::webui::human_ago(now.saturating_sub(at))
        ),
        (false, None) => "még nem kérdeztük".into(),
    }
}

/// The downloads page, with the reason each item is still there.
/// Renders the page. Every caller has already checked the session; doing it again here
/// would need the request's cookies, and the version of this that passed `None` for them
/// turned the page into a permanent redirect back to the login screen.
pub(crate) async fn downloads_page(state: &AppState, message: Option<String>) -> Response {
    let cfg = state.config().await;
    let snapshot = state.owed.read().await.clone();
    let streaming = state.lib.streaming_hashes().await;
    let now = crate::state::now();

    let all = state.store.items().await;
    // How many files each torrent is serving, and how many of those are finished with. Worked
    // out once rather than per row: a pack with ten episodes would otherwise walk the list ten
    // times to say the same thing.
    let mut per_torrent: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();
    for item in &all {
        let entry = per_torrent.entry(item.info_hash.clone()).or_insert((0, 0));
        entry.0 += 1;
        if item.watched(
            cfg.maintenance.watched_position_percent,
            cfg.maintenance.watched_min_served_percent,
        ) {
            entry.1 += 1;
        }
    }

    let all_items = all.clone();
    // One pass over the records rather than one per row: keeper_key walks the whole list, and
    // calling it inside the loop made the page quadratic in the number of downloads.
    let keepers: std::collections::HashMap<String, String> = per_torrent
        .keys()
        .filter_map(|hash| {
            crate::state::keeper_key(&all_items, hash).map(|key| (hash.clone(), key))
        })
        .collect();
    let mut rows = Vec::new();
    for item in all {
        // This run's reading if there is one, otherwise what was stored from the last: the
        // obligation is a fact about the download, so it survives a restart.
        let owed_entry = snapshot
            .entries
            .iter()
            .find(|e| !item.ncore_torrent_id.is_empty() && e.torrent_id == item.ncore_torrent_id);
        let asked_now = snapshot.fetched_at.is_some() && snapshot.error.is_none();
        let owes = if asked_now {
            owed_entry.is_some()
        } else {
            item.owed_to_tracker
        };

        let candidate = crate::config::Candidate {
            kept: item.keep,
            watched: item.watched(
                cfg.maintenance.watched_position_percent,
                cfg.maintenance.watched_min_served_percent,
            ),
            owed_to_tracker: owes,
            // A stored answer counts, but only if nothing has been taken from the torrent
            // since it was given: a later download is a new obligation the answer predates.
            tracker_says_clear: !item.ncore_torrent_id.is_empty()
                && item.tracker_figures_at.is_some()
                && !owes
                && (asked_now
                    || item.owed_checked_at.is_some_and(|at| {
                        all_items
                            .iter()
                            .filter(|o| o.info_hash == item.info_hash)
                            .filter_map(|o| o.completed_at)
                            .max()
                            .is_none_or(|newest| newest <= at)
                    })),
            partial: item.partial,
            streaming: streaming.contains(&item.info_hash),
            // The torrent's clock, not this file's: the debt is the torrent's.
            seeded_secs: item.torrent_seeded_for(&all_items, now),
            // The file's own account, and whether it is the one holding the torrent open.
            is_keeper: keepers
                .get(&item.info_hash)
                .is_none_or(|k| *k == item.key()),
            file_bytes: item.file_len,
            file_seeded_secs: item.file_seeded_for(now),
            // Per torrent, which is what the obligation is attached to.
            figures_known: item.tracker_figures_at.is_some(),
            tracker_downloaded: item.tracker_downloaded_bytes,
            tracker_uploaded: item.tracker_uploaded_bytes,
        };
        let decision = cfg.maintenance.verdict(&candidate);
        // The duration that belongs under the reason. Worked out from the same Candidate the
        // verdict came from, so the number and the sentence above it cannot disagree; where the
        // reason has no clock on it, the record's age stands there instead, and says so.
        let verdict_note = match decision {
            crate::config::Verdict::Keep(scope, why) => crate::config::remaining_for_reason(
                &cfg.maintenance,
                &candidate,
                scope,
                why,
                owed_entry.and_then(|e| e.remaining_secs),
            )
            .map(|secs| match scope {
                crate::config::Scope::File => {
                    format!("még {} ezzel a fájllal", crate::webui::human_duration(secs))
                }
                crate::config::Scope::Torrent => {
                    format!("még {} a torrenttel", crate::webui::human_duration(secs))
                }
            }),
            crate::config::Verdict::Delete => None,
        }
        .unwrap_or_else(|| format!("hozzáadva: {}", crate::webui::human_ago(item.age(now))));

        let (verdict, verdict_short) = match decision {
            crate::config::Verdict::Delete => (
                "a következő körben törlődik".to_string(),
                "következő kör".to_string(),
            ),
            crate::config::Verdict::Keep(_, why) => {
                (format!("megtartva: {why}"), short_reason(why))
            }
        };

        // Distinct parts of the file that were sent, from the coverage map.
        //
        // Two earlier versions of this column were wrong in the same direction, and both are
        // worth stating because the number sits next to a deletion decision. The first showed
        // the furthest byte reached, which reads 100% within seconds: a player asks for the
        // end of the file to find the seek index before it plays a frame. The second showed
        // the running total of bytes served, which counts the same minute again every time a
        // player re-requests it, so it passed 90% around the middle of a film. This is the
        // same measure the watched decision uses, so the column and the verdict can no longer
        // disagree.
        let watched = if candidate.watched {
            "megnézve".to_string()
        } else if item.play_count == 0 {
            "nem indult el".to_string()
        } else {
            // Three possible values in this column and no more: watched, never started, or a
            // percentage. A playback from before the coverage map existed therefore reads 0%,
            // which is what the measure honestly says about it: nothing was measured.
            format!("{}%", item.served_percent().min(99))
        };

        rows.push(crate::webui::DownloadRow {
            key: item.key(),
            watched_by_hand: item.watched_manually,
            pack_summary: match per_torrent.get(&item.info_hash) {
                Some((files, watched)) if *files > 1 => {
                    let keeper = keepers
                        .get(&item.info_hash)
                        .is_some_and(|k| *k == item.key());
                    let role = if keeper {
                        ", ez tartja életben a torrentet"
                    } else {
                        ""
                    };
                    format!("ez a torrent {files} fájlt szolgál ki, {watched} megnézve{role}")
                }
                _ => String::new(),
            },
            title: item.title.clone(),
            size: crate::webui::human_size(item.file_len),
            watched,
            owed_label: owed_label(&item, owes, asked_now).0.to_string(),
            owed_class: owed_label(&item, owes, asked_now).1,
            keep: item.keep,
            verdict,
            verdict_short,
            verdict_note,
        });
    }
    // Newest first: that is what someone is looking for after watching something.
    rows.reverse();

    // The oldest stored answer, so the note can say how fresh what is on screen is even
    // when this run of the server has not asked anything yet.
    let stored_answer_age = state
        .store
        .items()
        .await
        .iter()
        .filter_map(|i| i.owed_checked_at)
        .max()
        .map(|at| crate::webui::human_ago(now.saturating_sub(at)));

    let tracker_note = match (&snapshot.fetched_at, &snapshot.error) {
        (None, _) => match stored_answer_age {
            Some(age) => format!(
                "Ebben a munkamenetben még nem kérdeztük meg. A táblázatban a tárolt válasz \
                 látszik, kora: {age}."
            ),
            None => "A trackert még soha nem kérdeztük meg.".to_string(),
        },
        (Some(at), Some(err)) => format!(
            "Nem sikerült beolvasni a listát ({err}), {} próbáltuk. Amíg ez így van, semmi nem törlődik.",
            crate::webui::human_ago(now.saturating_sub(*at))
        ),
        (Some(at), None) => format!(
            "{} nyitott kötelezettség, {} kérdeztük meg.",
            snapshot.entries.len(),
            crate::webui::human_ago(now.saturating_sub(*at))
        ),
    };

    // What was watched, newest first, as a date and a title.
    let history = state
        .store
        .history(40)
        .await
        .into_iter()
        .map(|e| {
            let when = chrono::DateTime::from_timestamp(e.at as i64, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| "?".to_string());
            (when, e.title)
        })
        .collect();

    // Grouped by torrent, in the order the rows already came in, so the newest download leads.
    let mut groups: Vec<crate::webui::TorrentGroup> = Vec::new();
    for row in rows {
        let hash = row.key.split(':').next().unwrap_or("").to_string();
        match groups.iter_mut().find(|g| g.hash == hash) {
            Some(g) => g.rows.push(row),
            None => groups.push(crate::webui::TorrentGroup {
                hash,
                title: String::new(),
                summary: String::new(),
                figures: String::new(),
                owed_label: String::new(),
                owed_class: "owed-unknown",
                owed_detail: String::new(),
                open: false,
                rows: vec![row],
            }),
        }
    }
    // The group's own line: what the torrent is, and what it owes. The obligation is the
    // torrent's, so it is said once here rather than repeated on every file.
    for g in groups.iter_mut() {
        let first = &g.rows[0];
        g.title = release_name(&all_items, &g.hash)
            .unwrap_or_else(|| first.title.clone());
        let files = g.rows.len();
        let watched = g
            .rows
            .iter()
            .filter(|r| r.watched == "megnézve")
            .count();
        let bytes: u64 = all_items
            .iter()
            .filter(|i| i.info_hash == g.hash)
            .map(|i| i.file_len)
            .sum();
        g.summary = if files == 1 {
            crate::webui::human_size(bytes)
        } else {
            format!(
                "{files} fájl, {watched} megnézve, {}",
                crate::webui::human_size(bytes)
            )
        };
        g.owed_label = first.owed_label.clone();
        g.owed_class = first.owed_class;
        // The tracker's figures, once, from any record of this torrent: they are the torrent's,
        // so every record carries the same copy of them.
        g.figures = match all_items.iter().find(|i| i.info_hash == g.hash) {
            Some(i) if i.tracker_figures_at.is_some() => format!(
                "letöltve {} · vissza {} · arány {} ({})",
                crate::webui::human_size(i.tracker_downloaded_bytes),
                crate::webui::human_size(i.tracker_uploaded_bytes),
                if i.tracker_ratio.is_empty() {
                    "-".to_string()
                } else {
                    i.tracker_ratio.clone()
                },
                match i.tracker_figures_at {
                    Some(at) => crate::webui::human_ago(now.saturating_sub(at)),
                    None => "még nem kérdeztük".into(),
                }
            ),
            _ => "a trackertől még nincs adat".to_string(),
        };
        // And how long that obligation has left, said here and nowhere else. It is one debt per
        // torrent, so the file rows carry the word "igen" and this line carries the clock.
        g.owed_detail = match all_items.iter().find(|i| i.info_hash == g.hash) {
            Some(i) => {
                let entry = snapshot.entries.iter().find(|e| {
                    !i.ncore_torrent_id.is_empty() && e.torrent_id == i.ncore_torrent_id
                });
                let asked_now = snapshot.fetched_at.is_some() && snapshot.error.is_none();
                let owes = if asked_now {
                    entry.is_some()
                } else {
                    i.owed_to_tracker
                };
                owed_detail(i, owes, asked_now, entry.and_then(|e| e.remaining_secs), now)
            }
            None => String::new(),
        };
        // Opened when something in it is about to go, so a deletion is never hidden behind a
        // closed row.
        g.open = g.rows.iter().any(|r| r.verdict_short == "következő kör");
    }

    html(crate::webui::page(crate::webui::PageState::Downloads {
        groups,
        tracker_note,
        history,
        message,
    }))
}

/// The reason a download survives, short enough for a table cell. The full sentence
/// stays available as the cell's tooltip.
pub(crate) fn short_reason(why: &str) -> String {
    // Every seeding reason reads the same here. Which of the six rules is the one holding a
    // file is a question for the tooltip; what the column answers is whether this file may go
    // yet, and "a tracker szerint még seedelni kell" and "ennek a fájlnak még hátravan a
    // seedelése" were two ways of writing the same no. The duration underneath says whose
    // seeding it is and how long it has to run.
    if crate::config::is_about_seeding(why) {
        return "seedelés szükséges".to_string();
    }
    match why {
        "az automatikus törlés ki van kapcsolva" => "kikapcsolva",
        "megtartásra jelölve" => "megtartva",
        "még nem néztük meg" => "nem nézted meg",
        other => other,
    }
    .to_string()
}

pub(crate) async fn ui_downloads(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(page) = require_login(&state, cookie_header(&headers)).await {
        return page;
    }
    downloads_page(&state, None).await
}

#[derive(serde::Deserialize)]
pub(crate) struct KeepForm {
    key: String,
    keep: String,
}

pub(crate) async fn ui_set_keep(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<KeepForm>,
) -> Response {
    if let Some(page) = require_login(&state, cookie_header(&headers)).await {
        return page;
    }
    let keep = form.keep == "1";
    let message = if state.store.set_keep(&form.key, keep).await {
        let _ = state.store.flush().await;
        Some(if keep {
            "Megtartásra jelölve. Az automatikus törlés soha nem viszi el.".to_string()
        } else {
            "Már nincs megtartásra jelölve, újra a szokásos szabályok érvényesek.".to_string()
        })
    } else {
        Some("Ez a letöltés már nincs a listán.".to_string())
    };
    downloads_page(&state, message).await
}

#[derive(serde::Deserialize)]
pub(crate) struct DeleteForm {
    key: String,
}

/// Deletion by hand.
///
/// Deliberately not subject to the automatic rules: the watched requirement, the
/// retention window and the keep flag exist to stop the *server* from removing
/// something unasked. When the person who owns the files says delete, that is the
/// answer. The one thing worth saying out loud is what it costs on a private tracker,
/// so a warning goes into the log and into what the page reports.
pub(crate) async fn ui_delete_download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<DeleteForm>,
) -> Response {
    if let Some(page) = require_login(&state, cookie_header(&headers)).await {
        return page;
    }

    let Some(item) = state.store.get(&form.key).await else {
        return downloads_page(&state, Some("Ez a letöltés már nincs meg.".into())).await;
    };

    // Whether the tracker still wants it seeded, from the last answer we have.
    let owed = {
        let snapshot = state.owed.read().await;
        !item.ncore_torrent_id.is_empty()
            && snapshot
                .entries
                .iter()
                .any(|e| e.torrent_id == item.ncore_torrent_id)
    };

    let message = match state.delete_download(&item).await {
        Ok(()) => {
            state.store.remove(&form.key).await;
            let _ = state.store.flush().await;
            if owed {
                tracing::warn!(
                    title = %item.title,
                    "deleted by hand while the tracker still expected seeding"
                );
                format!(
                    "{} törölve. Ezen a tracker szerint még seedelnünk kellett volna.",
                    item.title
                )
            } else {
                format!("{} törölve.", item.title)
            }
        }
        Err(e) => format!("Nem sikerült törölni: {e}"),
    };
    downloads_page(&state, Some(message)).await
}

/// Answers "what would tonight's run remove", without removing anything.
///
/// The same code down the same path as the real run, including asking the tracker, so what
/// it reports is what would be done rather than a second guess at it. Deletion being
/// switched off is ignored for the purpose, because deciding whether to switch it on is the
/// reason to ask.
/// Runs the deletion round now, exactly as the evening one does.
///
/// The same code path as the schedule and the one at startup, so what it does here is what it
/// will do unattended. Only the trigger differs, and the message says which.
pub(crate) async fn ui_sweep_now(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(page) = require_login(&state, cookie_header(&headers)).await {
        return page;
    }
    let world = ServerWorld {
        state: state.clone(),
    };
    let report = crate::maintenance::run_once(&world, &state.store, "Kézi takarítás").await;
    state.store.set_last_sweep_at(crate::state::now()).await;

    let message = if let Some(why) = &report.abandoned {
        format!("A kör megállt: {why}. Ilyenkor semmi nem törlődik.")
    } else if report.deleted.is_empty() {
        format!(
            "Lefutott, egyet sem törölt a {} letöltésből.",
            report.considered
        )
    } else {
        format!(
            "Törölve {} db, felszabadult {}: {}.",
            report.deleted.len(),
            crate::media::size_label(report.freed_bytes),
            report.deleted.join(", ")
        )
    };
    downloads_page(&state, Some(message)).await
}

pub(crate) async fn ui_dry_run(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(page) = require_login(&state, cookie_header(&headers)).await {
        return page;
    }
    let cfg = state.config().await;
    let world = ServerWorld {
        state: state.clone(),
    };
    let report = crate::maintenance::sweep_with(
        &world,
        &state.store,
        &cfg.maintenance,
        crate::state::now(),
        crate::maintenance::Mode::DryRun,
    )
    .await;

    let message = if let Some(why) = &report.abandoned {
        format!("A próbafutás megállt: {why}. Ilyenkor a valódi kör sem törölne semmit.")
    } else if report.deleted.is_empty() {
        format!(
            "Próbafutás: a {} letöltésből egyet sem törölne.",
            report.considered
        )
    } else {
        format!(
            "Próbafutás: ezt a {} letöltést törölné: {}.",
            report.deleted.len(),
            report.deleted.join(", ")
        )
    };
    downloads_page(&state, Some(message)).await
}

pub(crate) async fn ui_refresh_tracker(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(page) = require_login(&state, cookie_header(&headers)).await {
        return page;
    }
    let message = match state.refresh_owed().await {
        Ok(entries) => format!("A tracker szerint {} nyitott kötelezettség van.", entries.len()),
        Err(e) => format!("Nem sikerült beolvasni a tracker listáját: {e}"),
    };
    downloads_page(&state, Some(message)).await
}
