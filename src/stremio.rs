//! Stremio addon protocol surface.
//!
//! We only implement the `stream` resource. Catalogue and metadata come from
//! whatever addon the user already has installed (TMDB, Cinemeta), and Stremio
//! asks every addon that advertises `stream` for playable sources of an id. That
//! is why there is no catalogue here: building one would duplicate what a metadata
//! addon already does better.
//!
//! Ids arrive as `metaId` for a film and `metaId:season:episode` for an episode.
//! `metaId` is `tt…` from Cinemeta or `tmdb:…` from the TMDB addon.

use serde::Serialize;

/// Which id space a request came from. nCore matches exactly on IMDb ids, so a
/// TMDB id has to be translated before it is any use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaId {
    Imdb(String),
    Tmdb(String),
}

impl MetaId {
}

/// A parsed stream request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRequest {
    pub meta: MetaId,
    /// Present for an episode, absent for a film.
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

impl StreamRequest {
    pub fn is_episode(&self) -> bool {
        self.season.is_some() && self.episode.is_some()
    }
}

/// Parses the id Stremio puts in the stream request path.
///
/// The colon is both the TMDB prefix separator and the season/episode separator,
/// so splitting has to be done from the right, on exactly the trailing numeric
/// parts. Splitting from the left would turn `tmdb:12345` into meta `tmdb` and a
/// season of 12345.
pub fn parse_stream_id(raw: &str) -> Option<StreamRequest> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let parts: Vec<&str> = raw.split(':').collect();

    // Trailing `:season:episode`, but only when both are numbers.
    let (head, season, episode) = if parts.len() >= 3 {
        let last = parts[parts.len() - 1].parse::<u32>().ok();
        let before = parts[parts.len() - 2].parse::<u32>().ok();
        match (before, last) {
            (Some(s), Some(e)) => (parts[..parts.len() - 2].join(":"), Some(s), Some(e)),
            _ => (raw.to_string(), None, None),
        }
    } else {
        (raw.to_string(), None, None)
    };

    if head.is_empty() {
        return None;
    }

    let meta = match head.strip_prefix("tmdb:") {
        Some(rest) if !rest.is_empty() => MetaId::Tmdb(rest.to_string()),
        Some(_) => return None,
        None => MetaId::Imdb(head),
    };

    Some(StreamRequest {
        meta,
        season,
        episode,
    })
}

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    pub resources: Vec<String>,
    pub types: Vec<String>,
    /// Empty: catalogues are somebody else's job, but the field is required.
    pub catalogs: Vec<serde_json::Value>,
    #[serde(rename = "idPrefixes")]
    pub id_prefixes: Vec<String>,
    #[serde(rename = "behaviorHints")]
    pub behavior_hints: BehaviorHints,
}

#[derive(Debug, Serialize)]
pub struct BehaviorHints {
    /// Declares that streams come from a peer-to-peer source.
    pub p2p: bool,
    pub configurable: bool,
}

pub fn manifest(name: &str, version: &str) -> Manifest {
    Manifest {
        id: "hu.stremhu.rs".to_string(),
        version: version.to_string(),
        name: name.to_string(),
        description:
            "nCore streaming from a self-hosted libtorrent engine. Metadata comes from your \
             installed catalogue addon."
                .to_string(),
        resources: vec!["stream".to_string()],
        types: vec!["movie".to_string(), "series".to_string()],
        catalogs: Vec::new(),
        // Without `tmdb:` here Stremio would never route TMDB-sourced titles to us.
        id_prefixes: vec!["tt".to_string(), "tmdb:".to_string()],
        behavior_hints: BehaviorHints {
            p2p: true,
            configurable: false,
        },
    }
}

/// One playable source offered back to Stremio.
///
/// `name` is the badge on the left, so it carries the resolution and the picture
/// quality: that is what the eye picks a row by. `title` is the detail block under it.
#[derive(Debug, Serialize, PartialEq)]
pub struct Stream {
    pub name: String,
    pub title: String,
    pub url: String,
    #[serde(rename = "behaviorHints")]
    pub behavior_hints: StreamBehaviorHints,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct StreamBehaviorHints {
    /// The name the player shows for the file, instead of the URL.
    pub filename: String,
    /// Sources sharing a group count as one series run, so the following episode
    /// plays from the same place without asking again.
    #[serde(rename = "bingeGroup")]
    pub binge_group: String,
}

#[derive(Debug, Serialize)]
pub struct StreamsResponse {
    pub streams: Vec<Stream>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_film_id_has_no_season_or_episode() {
        let r = parse_stream_id("tt1392170").expect("parses");
        assert_eq!(r.meta, MetaId::Imdb("tt1392170".into()));
        assert!(!r.is_episode());
    }

    #[test]
    fn an_episode_id_splits_off_the_trailing_numbers() {
        let r = parse_stream_id("tt0898266:9:17").expect("parses");
        assert_eq!(r.meta, MetaId::Imdb("tt0898266".into()));
        assert_eq!((r.season, r.episode), (Some(9), Some(17)));
        assert!(r.is_episode());
    }

    /// The colon means two different things, so a left-to-right split would read
    /// `tmdb:12345` as meta `tmdb` with season 12345.
    #[test]
    fn a_tmdb_film_id_is_not_mistaken_for_an_episode() {
        let r = parse_stream_id("tmdb:12345").expect("parses");
        assert_eq!(r.meta, MetaId::Tmdb("12345".into()));
        assert!(!r.is_episode(), "12345 is the id, not a season");
    }

    #[test]
    fn a_tmdb_episode_id_keeps_both_halves() {
        let r = parse_stream_id("tmdb:1399:2:9").expect("parses");
        assert_eq!(r.meta, MetaId::Tmdb("1399".into()));
        assert_eq!((r.season, r.episode), (Some(2), Some(9)));
    }

    #[test]
    fn malformed_ids_are_rejected() {
        assert!(parse_stream_id("").is_none());
        assert!(parse_stream_id("   ").is_none());
        assert!(parse_stream_id("tmdb:").is_none());
    }

    #[test]
    fn non_numeric_trailing_parts_stay_part_of_the_id() {
        // Nothing to split, so the whole thing is the meta id.
        let r = parse_stream_id("tt123:abc:def").expect("parses");
        assert_eq!(r.meta, MetaId::Imdb("tt123:abc:def".into()));
        assert!(!r.is_episode());
    }

    #[test]
    fn the_manifest_routes_both_id_spaces_to_us() {
        let m = manifest("stremhu-rs", "0.1.0");
        assert!(m.id_prefixes.contains(&"tt".to_string()));
        assert!(
            m.id_prefixes.contains(&"tmdb:".to_string()),
            "without this, TMDB titles never reach us"
        );
        assert_eq!(m.resources, vec!["stream"]);
        assert!(m.behavior_hints.p2p);
    }

    #[test]
    fn manifest_serialises_with_the_names_stremio_expects() {
        let json = serde_json::to_string(&manifest("x", "1.0.0")).expect("serialises");
        assert!(json.contains("\"idPrefixes\""));
        assert!(json.contains("\"behaviorHints\""));
        assert!(json.contains("\"catalogs\":[]"));
    }







}
