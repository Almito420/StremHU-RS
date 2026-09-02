//! The Stremio addon protocol: the manifest, and turning a request for a title into a
//! list of playable sources.
//!
//! The ordering rules are here too, because deciding which source to offer first is part
//! of answering the request rather than a separate concern.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::app::*;
use crate::tracker::Torrent;
use crate::stremio::{self, MetaId};
use crate::http::{authorised, host_for_display};



pub(crate) async fn manifest(
    State(state): State<Arc<AppState>>,
    Path(api_key): Path<String>,
) -> Response {
    if !authorised(&state.config().await, &api_key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let m = stremio::manifest("StremHU rs", env!("CARGO_PKG_VERSION"));
    axum::Json(m).into_response()
}

/// Stremio requests `/stream/{type}/{id}.json`; the `.json` suffix arrives as part
/// of the last path segment.
pub(crate) async fn stream_list(
    State(state): State<Arc<AppState>>,
    Path((api_key, kind, id)): Path<(String, String, String)>,
) -> Response {
    let cfg = state.config().await;
    if !authorised(&cfg, &api_key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let raw_id = id.strip_suffix(".json").unwrap_or(&id);
    let Some(req) = stremio::parse_stream_id(raw_id) else {
        tracing::warn!(id = %raw_id, "unparseable stream id");
        return axum::Json(stremio::StreamsResponse { streams: vec![] }).into_response();
    };

    let plan = match build_search_plan(&state, &kind, &req).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, id = %raw_id, "cannot decide how to search");
            return axum::Json(stremio::StreamsResponse { streams: vec![] }).into_response();
        }
    };

    // The finished list for this title, from the cache when it is still warm.
    //
    // Stremio asks again for every episode of a series, and with the ladder behind it that is
    // no longer one request per ask. Only a list that answered is kept: a title nothing was
    // found for is asked again next time, because the reason may simply be that nobody had
    // uploaded it yet.
    let searched = std::time::Instant::now();
    let (found, rung) = match state.cached_search(&plan, &req).await {
        Some(hit) => (hit, "cache"),
        None => {
            let (found, rung) = run_ladder(&state, &plan, &req, &cfg.filters).await;
            if !found.is_empty() {
                state.cache_search(&plan, &req, &found).await;
            }
            (found, rung)
        }
    };
    let usable = rank_candidates(&found, &req, &cfg.filters);

    tracing::info!(
        id = %raw_id,
        imdb = ?plan.imdb,
        names = ?plan.names,
        rung,
        found = found.len(),
        usable = usable.len(),
        took_ms = searched.elapsed().as_millis() as u64,
        "search finished"
    );

    // Over HTTPS whenever the TLS listener is up, and this is not a nicety.
    //
    // Stremio in a browser is an HTTPS page, and a browser refuses to load plain HTTP media
    // into one: the request is blocked as mixed content before it reaches us, and what the
    // viewer sees is a stream that will not start, with no error that says why. The addon
    // itself is installed over HTTPS, so handing out HTTP stream URLs from it was asking the
    // browser to do the one thing it will not do. The native players on the desktop and the
    // television have no such rule, which is why this went unnoticed while they were used.
    let base = match state.https_host.read().await.clone() {
        Some(host) => format!("https://{host}:{}/{}", cfg.network.https_port, api_key),
        None => format!(
            "http://{}:{}/{}",
            host_for_display(&cfg),
            cfg.server.port,
            api_key
        ),
    };

    // What is already downloaded, so those rows can be marked. Choosing a copy that is
    // on the disk costs nothing; choosing a different one costs a second download and
    // fresh seed time on a private tracker.
    let have: Vec<String> = state
        .store
        .items()
        .await
        .into_iter()
        .filter(|i| !i.ncore_torrent_id.is_empty())
        // Keyed by tracker as well: the star means "this exact release is already on the disk",
        // and the same number on the other tracker is a different release.
        .map(|i| i.tracker().owed_key(&i.ncore_torrent_id))
        .collect();

    let mut streams = Vec::with_capacity(usable.len());
    for t in usable {
        if let Some(url) = &t.download_url {
            state
                .remember_source(t.tracker, &t.torrent_id, url, t.size_bytes)
                .await;
        }
        // The id in the URL carries the tracker, because both sites number their torrents from
        // one and a bare number would let a play request reach the wrong site's release.
        let play_id = t.tracker.play_id(&t.torrent_id);
        let play = match (req.season, req.episode) {
            (Some(s), Some(e)) => format!("{base}/play/{play_id}/{s}/{e}"),
            _ => format!("{base}/play/{play_id}"),
        };
        let release = t.title.as_deref().unwrap_or("(no name)");
        let listing = crate::media::listing(
            t.tracker.label(),
            release,
            &t.category,
            t.seeders,
            t.leechers,
            t.size_bytes,
            have.contains(&t.tracker.owed_key(&t.torrent_id)),
        );
        streams.push(stremio::Stream {
            name: listing.name.clone(),
            title: listing.description.clone(),
            url: play,
            behavior_hints: stremio::StreamBehaviorHints {
                // Shown by the player instead of the URL, and it is what a viewer
                // recognises when several sources are open.
                filename: release.to_string(),
                // Keeps Stremio playing the next episode at the same quality rather
                // than asking again after every one.
                binge_group: listing.binge_group,
            },
        });
    }

    tracing::info!(kind = %kind, id = %raw_id, count = streams.len(), "stream list");
    axum::Json(stremio::StreamsResponse { streams }).into_response()
}

/// Decides how nCore should be searched.
///
/// An IMDb id gives an exact match, so it is always preferred. A TMDB id has to be
/// translated first, and when the work has no IMDb entry the only remaining handle
/// is its title. That is not an edge case here: many Hungarian series exist on TMDB
/// and not on IMDb, and an IMDb-only design can never find them.
pub(crate) async fn build_search_plan(
    state: &AppState,
    kind: &str,
    req: &stremio::StreamRequest,
) -> Result<SearchPlan> {
    match &req.meta {
        // A bare IMDb id is all Stremio sends for anything with an IMDb entry, and it carries
        // no title. TMDB can turn one back into a title, which is what gives this request the
        // second rung of the ladder; without a key, or when the lookup fails, the ladder is
        // simply shorter and nothing else changes.
        MetaId::Imdb(id) => {
            let names = title_for_imdb(state, kind_of(kind, req), id).await;
            Ok(SearchPlan {
                imdb: Some(id.clone()),
                names,
            })
        }
        MetaId::Tmdb(id) => {
            // The read guard is held across the lookups; it only blocks a settings
            // save, which is rare and can wait.
            let guard = state.tmdb.read().await;
            let tmdb = guard
                .as_ref()
                .context("tmdb.api_key is not set, so tmdb: ids cannot be resolved")?;

            // A series exposes its IMDb id only through /external_ids, a film carries
            // it in the details, so the two are fetched differently.
            let title = if kind == "series" || req.is_episode() {
                tmdb.series(id).await?
            } else {
                tmdb.movie(id).await?
            };

            // Both, not one or the other. The IMDb id is the exact handle and is tried
            // first, but nCore carries no IMDb id on a great many Hungarian uploads, and for
            // those the title is the only thing that will ever find them. Keeping only the id
            // here is what made an IMDb search that came up short the end of the road.
            let names = title.search_terms();
            let plan = SearchPlan {
                imdb: title.imdb_id.clone(),
                names,
            };
            if plan.is_empty() {
                anyhow::bail!("TMDB {id} has neither an IMDb id nor a usable title");
            }
            tracing::info!(tmdb = %id, imdb = ?plan.imdb, names = ?plan.names, "search plan");
            Ok(plan)
        }
    }
}

/// Whether this request is for a series, however it arrived.
fn kind_of(kind: &str, req: &stremio::StreamRequest) -> bool {
    kind == "series" || req.is_episode()
}

/// The titles for an IMDb id, so a request that arrived as one still has a name to fall back
/// on. An empty list when TMDB is not configured or does not know it: the ladder then has one
/// rung fewer per tracker, which is exactly the behaviour there was before.
async fn title_for_imdb(state: &AppState, series: bool, imdb: &str) -> Vec<String> {
    let guard = state.tmdb.read().await;
    let Some(tmdb) = guard.as_ref() else {
        return Vec::new();
    };
    match tmdb.find_by_imdb(imdb, series).await {
        Ok(Some(title)) => title.search_terms(),
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!(error = %e, imdb = %imdb, "could not turn the IMDb id into a title");
            Vec::new()
        }
    }
}

/// How many result pages one search may walk.
///
/// Measured against the live trackers rather than guessed. nCore answers a hundred rows a
/// page: an IMDb search is small (twenty-three rows for a series with eight seasons, one
/// page), but a title search is not, and "House" alone returns six and a half thousand rows
/// across sixty-seven pages. BitHUmen answers fifteen rows a page, and forty-three for
/// X-Faktor, which is three pages. Twenty pages covers every real case on both and still puts
/// a ceiling on a title so common that walking all of it would be traffic for nothing.
const MAX_SEARCH_PAGES: u32 = 20;

/// How many pages in a row may add no new copy of the wanted episode before the walk stops.
///
/// A release and its re-encodes sit next to each other in any ordering a tracker offers, so
/// once two pages running have added nothing new for this episode there is nothing more to
/// find. Stopping at the first hit instead would take away the choice between a 720p and a
/// 2160p copy of the same episode; walking to the ceiling every time would be twenty requests
/// for an answer that was complete after two.
const PAGES_WITHOUT_NEW: u32 = 2;

/// The episode a request is for, when it is for one.
pub(crate) fn wanted_episode(req: &stremio::StreamRequest) -> Option<crate::series::SeasonEpisode> {
    match (req.season, req.episode) {
        (Some(season), Some(episode)) => Some(crate::series::SeasonEpisode { season, episode }),
        _ => None,
    }
}

/// How many of these releases name the wanted episode outright.
fn exact_copies(found: &[Torrent], want: Option<crate::series::SeasonEpisode>) -> usize {
    let Some(se) = want else {
        return 0;
    };
    found
        .iter()
        .filter(|t| {
            crate::series::match_episode(t.title.as_deref().unwrap_or(""), se)
                == Some(crate::series::Match::Exact)
        })
        .count()
}

/// Whether this rung of the ladder produced something the viewer could press play on.
///
/// Deliberately measured on the finished, filtered list rather than on the raw hit count.
/// Three results for the right series but the wrong episode leave the viewer with the same
/// empty screen as no results at all, so they must not stop the ladder. This is the rule the
/// second tracker was already gated on; the ladder now applies it at every step.
fn answers_the_request(
    found: &[Torrent],
    req: &stremio::StreamRequest,
    filters: &crate::config::Filters,
) -> bool {
    !rank_candidates(found, req, filters).is_empty()
}

/// The narrowed query for an episode request: the title and its season.
///
/// Measured on nCore: "House" returns six thousand six hundred rows across sixty-seven pages,
/// "House S05" returns thirty-four on one. The season is as far as it can usefully go, because
/// "House S05E12" returns nothing at all: the episode lives inside a season pack whose name
/// never carries the episode number. So the season is the narrow end of what actually works.
fn narrowed(term: &str, want: Option<crate::series::SeasonEpisode>) -> Option<String> {
    let se = want?;
    Some(format!("{term} S{:02}", se.season))
}

/// One search term walked across pages for as long as it is worth it.
///
/// The walk stops when the tracker runs out, when the ceiling is reached, or when the pages
/// stop adding copies of the wanted episode.
async fn walk<F, Fut>(
    label: &'static str,
    term: &str,
    want: Option<crate::series::SeasonEpisode>,
    mut fetch: F,
) -> Vec<Torrent>
where
    F: FnMut(String, u32) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<crate::ncore::SearchPage>>,
{
    let mut out: Vec<Torrent> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut barren = 0u32;
    let mut page = 1u32;

    loop {
        let found = match fetch(term.to_string(), page).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, tracker = label, term = %term, page, "search failed");
                break;
            }
        };
        let next = found.next_page;
        let before = exact_copies(&out, want);
        // A tracker that clamps an over-large page number serves one it has already given us.
        // Counting only what is new makes that look like the end of the list, which it is.
        let fresh: Vec<Torrent> = found
            .torrents
            .into_iter()
            .filter(|t| seen.insert(t.tracker.owed_key(&t.torrent_id)))
            .collect();
        if fresh.is_empty() {
            break;
        }
        out.extend(fresh);

        // Only an episode request has a reason to want another page. A film's copies are all
        // uploaded within days of each other and sit together wherever the tracker puts them.
        if want.is_none() {
            break;
        }
        if exact_copies(&out, want) > before {
            barren = 0;
        } else {
            barren += 1;
            if barren >= PAGES_WITHOUT_NEW {
                break;
            }
        }
        match next {
            Some(n) if page < MAX_SEARCH_PAGES => page = n,
            _ => break,
        }
    }
    if page > 1 {
        tracing::info!(tracker = label, term = %term, pages = page, hits = out.len(), "walked");
    }
    out
}

/// Runs the ladder: the exact handle first, the title second, and the second tracker only
/// after both of those came up short on the first.
///
/// Each rung is reached only because the one before it did not answer the request, so nothing
/// here is speculative traffic. The narrowed round in front of each title walk is the one
/// exception: one request, and when it lands it saves the twenty behind it.
///
/// Returns which rung answered, because that is the thing worth having in the log when
/// somebody asks why a particular episode could or could not be played.
pub(crate) async fn run_ladder(
    state: &AppState,
    plan: &SearchPlan,
    req: &stremio::StreamRequest,
    filters: &crate::config::Filters,
) -> (Vec<Torrent>, &'static str) {
    let want = wanted_episode(req);

    // nCore, by IMDb id. Small and exact: measured at twenty-three rows for an eight-season
    // series, so this is one request in the ordinary case.
    if let Some(imdb) = &plan.imdb {
        let found = walk("ncore", imdb, want, |term, page| async move {
            state
                .ncore
                .read()
                .await
                .search(crate::ncore::SEARCH_BY_IMDB, &term, page)
                .await
        })
        .await;
        if answers_the_request(&found, req, filters) {
            return (found, "ncore/imdb");
        }
    }

    // nCore, by title. Many Hungarian uploads carry no IMDb id at all, so this is not a
    // fallback for odd cases: for a Hungarian series it is usually the rung that answers.
    for term in &plan.names {
        if let Some(narrow) = narrowed(term, want) {
            let found = walk("ncore", &narrow, want, |term, page| async move {
                state
                    .ncore
                    .read()
                    .await
                    .search(crate::ncore::SEARCH_BY_NAME, &term, page)
                    .await
            })
            .await;
            if answers_the_request(&found, req, filters) {
                return (found, "ncore/name-narrow");
            }
        }
        let found = walk("ncore", term, want, |term, page| async move {
            state
                .ncore
                .read()
                .await
                .search(crate::ncore::SEARCH_BY_NAME, &term, page)
                .await
        })
        .await;
        if answers_the_request(&found, req, filters) {
            return (found, "ncore/name");
        }
    }

    // BitHUmen, and only now. The rule is the owner's and it is not a preference: the account
    // with fifteen years of history on it is asked first, and the second tracker is for the
    // title it does not have.
    if state.bithumen.read().await.is_none() {
        return (Vec::new(), "nothing");
    }
    // It does answer an IMDb id, measured: eleven rows for tt0412142, every one of them
    // carrying that id. So this rung is worth having and not a formality.
    if let Some(imdb) = &plan.imdb {
        let found = walk("bithumen", imdb, want, |term, page| async move {
            bithumen_page(state, &term, page).await
        })
        .await;
        if answers_the_request(&found, req, filters) {
            return (found, "bithumen/imdb");
        }
    }
    for term in &plan.names {
        if let Some(narrow) = narrowed(term, want) {
            let found = walk("bithumen", &narrow, want, |term, page| async move {
                bithumen_page(state, &term, page).await
            })
            .await;
            if answers_the_request(&found, req, filters) {
                return (found, "bithumen/name-narrow");
            }
        }
        let found = walk("bithumen", term, want, |term, page| async move {
            bithumen_page(state, &term, page).await
        })
        .await;
        if answers_the_request(&found, req, filters) {
            return (found, "bithumen/name");
        }
    }

    (Vec::new(), "nothing")
}

/// One page of BitHUmen, in the shape the walk expects.
async fn bithumen_page(
    state: &AppState,
    term: &str,
    page: u32,
) -> anyhow::Result<crate::ncore::SearchPage> {
    let guard = state.bithumen.read().await;
    let client = guard
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("BitHUmen is switched off"))?;
    client.search_page(term, page).await
}

/// Keeps what can actually be played and orders it.
///
/// For an episode the release name has to name that episode, or be the pack for its
/// season, since a finished season is uploaded as `S01` while individual episodes go
/// up as `S01E04` during the run. An episode request must never be answered with a
/// different episode, so anything unrecognised is dropped rather than offered.
///
/// Order after that is by preference, not by popularity. Sorting on seeders alone puts
/// the most-shared copy on top, and on this tracker that is reliably the smallest
/// re-encode, so the first thing offered would be the worst one available.
pub(crate) fn rank_candidates<'a>(
    found: &'a [Torrent],
    req: &stremio::StreamRequest,
    filters: &crate::config::Filters,
) -> Vec<&'a Torrent> {
    let want = match (req.season, req.episode) {
        (Some(season), Some(episode)) => Some(crate::series::SeasonEpisode { season, episode }),
        _ => None,
    };

    let mut usable: Vec<Ranked<'a>> = found
        .iter()
        .filter(|t| t.download_url.is_some() && t.seeders >= filters.min_seeders)
        .filter_map(|t| {
            let exactness = match want {
                None => 0,
                Some(se) => {
                    let name = t.title.as_deref().unwrap_or("");
                    match crate::series::match_episode(name, se) {
                        // The exact episode is a better answer than a whole season.
                        Some(crate::series::Match::Exact) => 0,
                        Some(crate::series::Match::Pack) => 1,
                        None => return None,
                    }
                }
            };
            Some(Ranked {
                torrent: t,
                exactness,
                preference: preference_key(t, filters),
            })
        })
        .collect();

    usable.sort_by(|a, b| {
        a.exactness
            .cmp(&b.exactness)
            .then(a.preference.cmp(&b.preference))
            // Among equally suitable copies, the better-seeded one starts faster.
            .then(b.torrent.seeders.cmp(&a.torrent.seeders))
    });

    let mut out: Vec<&Torrent> = usable.into_iter().map(|r| r.torrent).collect();
    if filters.only_best_match {
        out.truncate(1);
    }
    out
}

pub(crate) struct Ranked<'a> {
    torrent: &'a Torrent,
    /// 0 for the episode itself, 1 for the season pack containing it.
    exactness: u8,
    /// Preference positions in the order the configuration says to weigh them.
    preference: Vec<usize>,
}

/// The sort key built from the configured orders.
///
/// Which order matters most is itself configurable, because there is no universally
/// right answer: a viewer who wants Hungarian audio would rather have 720p Hungarian
/// than 4K English, and a viewer chasing picture quality would not.
pub(crate) fn preference_key(t: &Torrent, filters: &crate::config::Filters) -> Vec<usize> {
    let attrs = crate::media::Attributes::parse(
        t.title.as_deref().unwrap_or(""),
        &t.category,
    );

    let mut key = Vec::with_capacity(filters.priority.len());
    for aspect in &filters.priority {
        let list = match aspect.trim().to_ascii_lowercase().as_str() {
            "language" => &filters.language_order,
            "resolution" => &filters.resolution_order,
            "source" => &filters.source_order,
            // An unknown name in the priority list must not silently reorder anything.
            _ => continue,
        };
        key.push(attrs.rank_in(list));
    }
    key
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Filters;
    // The key generator lives with the startup code that uses it; only the tests, which
    // check that it is unguessable, reach across for it.
    use crate::http::random_key;

    fn torrent(id: &str, seeders: u64, title: &str, dl: bool) -> Torrent {
        Torrent {
            tracker: crate::tracker::Tracker::Ncore,
            torrent_id: id.into(),
            seeders,
            leechers: 0,
            size_bytes: 1_000_000_000,
            download_url: dl.then(|| "https://ncore.pro/x".to_string()),
            category: "hd_hun".into(),
            imdb_id: None,
            title: Some(title.into()),
        }
    }

    fn filters() -> Filters {
        Filters {
            min_seeders: 1,
            only_best_match: false,
            ..Default::default()
        }
    }

    fn named(id: &str, seeders: u64, title: &str) -> Torrent {
        torrent(id, seeders, title, true)
    }

    /// The reason preference ordering exists: on this tracker the smallest re-encode is
    /// reliably the best seeded, so sorting on popularity offers the worst copy first.
    #[test]
    fn the_preferred_resolution_beats_a_better_seeded_lower_one() {
        let found = vec![
            named("sd", 300, "Film.2014.HUN.WEB-DL.H264"),
            named("uhd", 20, "Film.2014.HUN.2160p.UHD.BluRay.x265"),
            named("hd", 150, "Film.2014.HUN.1080p.BluRay.x264"),
        ];
        let out = rank_candidates(&found, &movie_request(), &filters());
        let ids: Vec<&str> = out.iter().map(|t| t.torrent_id.as_str()).collect();
        assert_eq!(ids, vec!["uhd", "hd", "sd"]);
    }

    /// Language outranks resolution by default: a film in a language you do not speak
    /// is not improved by being sharper.
    #[test]
    fn language_outranks_resolution_by_default() {
        let found = vec![
            named("eng-4k", 500, "Film.2014.ENG.2160p.UHD.BluRay.x265"),
            named("hun-sd", 5, "Film.2014.HUN.480p.BDRip.x264"),
        ];
        let out = rank_candidates(&found, &movie_request(), &filters());
        assert_eq!(out[0].torrent_id, "hun-sd");
    }

    /// And that can be turned around, because it is a preference, not a fact.
    #[test]
    fn the_priority_order_is_configurable() {
        let found = vec![
            named("eng-4k", 500, "Film.2014.ENG.2160p.UHD.BluRay.x265"),
            named("hun-sd", 5, "Film.2014.HUN.480p.BDRip.x264"),
        ];
        let f = Filters {
            priority: vec!["resolution".into(), "language".into()],
            ..filters()
        };
        let out = rank_candidates(&found, &movie_request(), &f);
        assert_eq!(out[0].torrent_id, "eng-4k");
    }

    /// A preference must not act as a filter: something unlisted still gets offered,
    /// just last.
    #[test]
    fn an_unlisted_quality_is_offered_last_not_dropped() {
        let found = vec![
            named("hdtv", 900, "Sorozat.HUN.HDTV.XviD"),
            named("bluray", 10, "Sorozat.HUN.1080p.BluRay.x264"),
        ];
        let out = rank_candidates(&found, &movie_request(), &filters());
        assert_eq!(out.len(), 2, "nothing is hidden by a preference");
        assert_eq!(out[0].torrent_id, "bluray");
    }

    /// Seeders still decide between copies that are otherwise equally suitable.
    #[test]
    fn seeders_break_a_tie() {
        let found = vec![
            named("few", 5, "Film.2014.HUN.1080p.BluRay.x264-A"),
            named("many", 200, "Film.2014.HUN.1080p.BluRay.x264-B"),
        ];
        let out = rank_candidates(&found, &movie_request(), &filters());
        assert_eq!(out[0].torrent_id, "many");
    }

    /// Exactness comes before every preference: the wanted episode in a poor quality
    /// still beats a season pack in a good one, because the pack is a bigger download
    /// for the same viewing.
    #[test]
    fn the_exact_episode_still_outranks_a_better_quality_pack() {
        let found = vec![
            named("pack", 400, "Exek.csataja.S01.COMPLETE.HUN.2160p.BluRay"),
            named("ep", 3, "Exek.csataja.S01E04.HUN.480p.WEB-DL"),
        ];
        let out = rank_candidates(&found, &episode_request(1, 4), &filters());
        assert_eq!(out[0].torrent_id, "ep");
    }

    /// A nonsense entry in the priority list must not reorder anything by accident.
    #[test]
    fn an_unknown_priority_name_is_ignored() {
        let found = vec![
            named("sd", 300, "Film.2014.HUN.480p.WEB-DL"),
            named("hd", 10, "Film.2014.HUN.1080p.BluRay"),
        ];
        let f = Filters {
            priority: vec!["colour".into(), "resolution".into()],
            ..filters()
        };
        let out = rank_candidates(&found, &movie_request(), &f);
        assert_eq!(out[0].torrent_id, "hd");
    }

    /// Empty preference lists leave seeders in charge, which is the old behaviour and
    /// has to keep working for anyone who empties them.
    #[test]
    fn empty_preferences_fall_back_to_seeders() {
        let found = vec![
            named("sd", 300, "Film.2014.HUN.480p.WEB-DL"),
            named("hd", 10, "Film.2014.HUN.1080p.BluRay"),
        ];
        let f = Filters {
            resolution_order: Vec::new(),
            source_order: Vec::new(),
            language_order: Vec::new(),
            ..filters()
        };
        let out = rank_candidates(&found, &movie_request(), &f);
        assert_eq!(out[0].torrent_id, "sd");
    }

    fn movie_request() -> stremio::StreamRequest {
        stremio::StreamRequest {
            meta: MetaId::Imdb("tt1".into()),
            season: None,
            episode: None,
        }
    }

    fn episode_request(season: u32, episode: u32) -> stremio::StreamRequest {
        stremio::StreamRequest {
            meta: MetaId::Tmdb("294663".into()),
            season: Some(season),
            episode: Some(episode),
        }
    }

    #[test]
    fn a_hit_without_a_download_url_is_unusable() {
        let found = vec![
            torrent("1", 10, "Film.2014.1080p", false),
            torrent("2", 5, "Film.2014.2160p", true),
        ];
        let out = rank_candidates(&found, &movie_request(), &filters());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].torrent_id, "2");
    }

    #[test]
    fn seeder_threshold_is_applied() {
        let found = vec![torrent("1", 0, "Film.2014", true)];
        let f = Filters {
            min_seeders: 1,
            ..filters()
        };
        assert!(rank_candidates(&found, &movie_request(), &f).is_empty());
    }

    #[test]
    fn films_are_ordered_by_seeders() {
        let found = vec![
            torrent("low", 3, "Film.A", true),
            torrent("high", 30, "Film.B", true),
        ];
        let out = rank_candidates(&found, &movie_request(), &filters());
        assert_eq!(out[0].torrent_id, "high");
    }

    /// The core of the Hungarian-series case: an episode request has to accept both
    /// the individual episode and the finished season pack.
    #[test]
    fn an_episode_request_accepts_the_episode_and_its_season_pack() {
        let found = vec![
            torrent("ep", 5, "Exek.csataja.S01E04.HUN.WEB-DL", true),
            torrent("pack", 40, "Exek.csataja.S01.COMPLETE.HUN", true),
        ];
        let out = rank_candidates(&found, &episode_request(1, 4), &filters());
        assert_eq!(out.len(), 2);
        // Exact wins over the pack even though the pack has far more seeders.
        assert_eq!(out[0].torrent_id, "ep");
        assert_eq!(out[1].torrent_id, "pack");
    }

    #[test]
    fn an_episode_request_never_offers_a_different_episode() {
        let found = vec![
            torrent("wrong-ep", 99, "Exek.csataja.S01E05.HUN", true),
            torrent("wrong-season", 99, "Exek.csataja.S02.COMPLETE", true),
            torrent("unrelated", 99, "Valami.mas.2014.1080p", true),
        ];
        assert!(
            rank_candidates(&found, &episode_request(1, 4), &filters()).is_empty(),
            "offering the wrong episode is worse than offering nothing"
        );
    }

    #[test]
    fn only_best_match_truncates_after_ranking() {
        let found = vec![
            torrent("ep", 5, "Show.S01E04", true),
            torrent("pack", 40, "Show.S01.COMPLETE", true),
        ];
        let f = Filters {
            only_best_match: true,
            ..filters()
        };
        let out = rank_candidates(&found, &episode_request(1, 4), &f);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].torrent_id, "ep", "the exact episode, not the pack");
    }

    #[test]
    fn a_hit_with_no_title_cannot_satisfy_an_episode_request() {
        let mut t = torrent("1", 10, "", true);
        t.title = None;
        let found = vec![t];
        assert!(rank_candidates(&found, &episode_request(1, 4), &filters()).is_empty());
        // A film request has nothing to match against, so it stays usable.
        assert_eq!(rank_candidates(&found, &movie_request(), &filters()).len(), 1);
    }

    #[test]
    fn a_generated_key_is_hex_and_long_enough() {
        let k = random_key();
        assert_eq!(k.len(), 32);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_generated_keys_differ() {
        // A predictable key would leave the stream URLs effectively public.
        assert_ne!(random_key(), random_key());
    }
}
