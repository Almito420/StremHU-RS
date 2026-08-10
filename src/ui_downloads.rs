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
        let (verdict, verdict_short) = match decision {
            crate::config::Verdict::Delete => (
                "a következő körben törlődik".to_string(),
                "következő kör".to_string(),
            ),
            crate::config::Verdict::Keep(why) => (format!("megtartva: {why}"), short_reason(why)),
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

        // The tracker's figures, or a plain dash so an empty cell is not mistaken for a
        // zero. A zero and "we have not asked yet" mean very different things here.
        let known = item.tracker_figures_at.is_some();
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
            added: crate::webui::human_ago(item.age(now)),
            watched,
            owed_label: owed_label(&item, owes, asked_now).0.to_string(),
            owed_class: owed_label(&item, owes, asked_now).1,
            owed_detail: owed_detail(
                &item,
                owes,
                asked_now,
                owed_entry.and_then(|e| e.remaining_secs),
                now,
            ),
            downloaded: if known {
                crate::webui::human_size(item.tracker_downloaded_bytes)
            } else {
                "-".into()
            },
            uploaded: if known {
                crate::webui::human_size(item.tracker_uploaded_bytes)
            } else {
                "-".into()
            },
            ratio: if known && !item.tracker_ratio.is_empty() {
                item.tracker_ratio.clone()
            } else {
                "-".into()
            },
            figures_age: match item.tracker_figures_at {
                Some(at) => crate::webui::human_ago(now.saturating_sub(at)),
                None => "még nem kérdeztük".into(),
            },
            keep: item.keep,
            verdict,
            verdict_short,
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

    html(crate::webui::page(crate::webui::PageState::Downloads {
        rows,
        tracker_note,
        history,
        message,
    }))
}

/// The reason a download survives, short enough for a table cell. The full sentence
/// stays available as the cell's tooltip.
pub(crate) fn short_reason(why: &str) -> String {
    match why {
        "automatic deletion is off" => "kikapcsolva",
        "being played right now" => "épp játszik",
        "marked to keep" => "megtartva",
        "not watched yet" => "nem nézted meg",
        "the tracker still expects seeding" => "seedelni kell",
        "has not seeded long enough" => "még seedel",
        "still within the retention window" => "túl friss",
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
