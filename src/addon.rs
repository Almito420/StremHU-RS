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
use crate::ncore::NcoreTorrent;
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

    let found = run_search(&state, &plan).await;
    let usable = rank_candidates(&found, &req, &cfg.filters);
    tracing::info!(
        id = %raw_id,
        plan = ?plan,
        found = found.len(),
        usable = usable.len(),
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
        .map(|i| i.ncore_torrent_id)
        .filter(|id| !id.is_empty())
        .collect();

    let mut streams = Vec::with_capacity(usable.len());
    for t in usable {
        if let Some(url) = &t.download_url {
            state.remember_source(&t.torrent_id, url, t.size_bytes).await;
        }
        let play = match (req.season, req.episode) {
            (Some(s), Some(e)) => format!("{base}/play/{}/{s}/{e}", t.torrent_id),
            _ => format!("{base}/play/{}", t.torrent_id),
        };
        let release = t.title.as_deref().unwrap_or("(no name)");
        let listing = crate::media::listing(
            "nCore",
            release,
            &t.category,
            t.seeders,
            t.leechers,
            t.size_bytes,
            have.contains(&t.torrent_id),
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
        MetaId::Imdb(id) => Ok(SearchPlan::Imdb(id.clone())),
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

            match &title.imdb_id {
                Some(imdb) => {
                    tracing::info!(tmdb = %id, imdb = %imdb, "resolved to an IMDb id");
                    Ok(SearchPlan::Imdb(imdb.clone()))
                }
                None => {
                    let terms = title.search_terms();
                    if terms.is_empty() {
                        anyhow::bail!("TMDB {id} has neither an IMDb id nor a usable title");
                    }
                    tracing::info!(tmdb = %id, ?terms, "no IMDb entry, searching by name");
                    Ok(SearchPlan::Names(terms))
                }
            }
        }
    }
}

/// Runs the plan, stopping at the first search that returns anything. Trying every
/// name even after a hit would only add unrelated results from a looser title.
pub(crate) async fn run_search(state: &AppState, plan: &SearchPlan) -> Vec<NcoreTorrent> {
    match plan {
        SearchPlan::Imdb(imdb) => match state
            .ncore
            .read()
            .await
            .search(crate::ncore::SEARCH_BY_IMDB, imdb, 1)
            .await
        {
            Ok(r) => r.torrents,
            Err(e) => {
                tracing::warn!(error = %e, imdb = %imdb, "nCore imdb search failed");
                Vec::new()
            }
        },
        SearchPlan::Names(terms) => {
            for term in terms {
                match state
                    .ncore
                    .read()
                    .await
                    .search(crate::ncore::SEARCH_BY_NAME, term, 1)
                    .await
                {
                    Ok(r) if !r.torrents.is_empty() => return r.torrents,
                    Ok(_) => tracing::info!(term = %term, "no hits, trying the next title"),
                    Err(e) => tracing::warn!(error = %e, term = %term, "nCore name search failed"),
                }
            }
            Vec::new()
        }
    }
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
    found: &'a [NcoreTorrent],
    req: &stremio::StreamRequest,
    filters: &crate::config::Filters,
) -> Vec<&'a NcoreTorrent> {
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

    let mut out: Vec<&NcoreTorrent> = usable.into_iter().map(|r| r.torrent).collect();
    if filters.only_best_match {
        out.truncate(1);
    }
    out
}

pub(crate) struct Ranked<'a> {
    torrent: &'a NcoreTorrent,
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
pub(crate) fn preference_key(t: &NcoreTorrent, filters: &crate::config::Filters) -> Vec<usize> {
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

    fn torrent(id: &str, seeders: u64, title: &str, dl: bool) -> NcoreTorrent {
        NcoreTorrent {
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

    fn named(id: &str, seeders: u64, title: &str) -> NcoreTorrent {
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
