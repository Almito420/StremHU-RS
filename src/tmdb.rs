//! TMDB lookups.
//!
//! This is what makes Hungarian titles work at all. nCore's search matches either an
//! IMDb id or a name, and many Hungarian series have no IMDb entry: `Exek csatája`
//! is TMDB 294663 with `imdb_id: null`, verified against the live API. A design that
//! only knows IMDb ids can never find those, no matter what catalogue addon is
//! installed, which is exactly why the existing Python implementation cannot.
//!
//! So a TMDB id resolves to three things, in order of usefulness for searching:
//! an IMDb id when one exists, the localised title, and the original title.
//!
//! Series and film differ: a film carries `imdb_id` in its details response, while a
//! series only exposes it through `/external_ids`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const BASE: &str = "https://api.themoviedb.org/3";

pub struct TmdbClient {
    http: reqwest::Client,
    api_key: String,
    language: String,
}

/// Everything we can use to find a release on a tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Title {
    /// Present only when the work actually has an IMDb entry.
    pub imdb_id: Option<String>,
    /// Localised title, in the configured language.
    pub name: String,
    /// Original-language title; for a Hungarian production these are the same.
    pub original_name: String,
    pub year: Option<u32>,
}

impl Title {
    /// Titles to try, most specific first, without duplicates.
    ///
    /// The accents are passed through as they are. nCore handles accented queries
    /// itself, and the client sends them as proper UTF-8 percent-encoding, so folding
    /// them here would only add searches that duplicate work.
    pub fn search_terms(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for title in [&self.name, &self.original_name] {
            let t = title.trim();
            if !t.is_empty() && !out.iter().any(|e: &String| e.eq_ignore_ascii_case(t)) {
                out.push(t.to_string());
            }
        }
        out
    }
}

#[derive(Debug, Deserialize)]
struct SeriesDetails {
    #[serde(default)]
    name: String,
    #[serde(default)]
    original_name: String,
    #[serde(default)]
    first_air_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MovieDetails {
    #[serde(default)]
    title: String,
    #[serde(default)]
    original_title: String,
    #[serde(default)]
    release_date: Option<String>,
    /// Films carry this directly; series do not.
    #[serde(default)]
    imdb_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExternalIds {
    #[serde(default)]
    imdb_id: Option<String>,
}

impl TmdbClient {
    pub fn new(api_key: &str, language: &str) -> Result<Self> {
        if api_key.trim().is_empty() {
            bail!("tmdb.api_key is not set");
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("building the tmdb http client")?;
        Ok(Self {
            http,
            api_key: api_key.trim().to_string(),
            language: if language.trim().is_empty() {
                "en-US".to_string()
            } else {
                language.trim().to_string()
            },
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        with_language: bool,
    ) -> Result<T> {
        // Built by hand so the API key and language are the only query parameters and
        // reqwest handles the percent-encoding of the path.
        let mut url = format!("{BASE}{path}?api_key={}", self.api_key);
        if with_language {
            url.push_str("&language=");
            url.push_str(&self.language);
        }

        let res = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;

        let status = res.status();
        let body = res.text().await.context("reading the tmdb body")?;
        if !status.is_success() {
            bail!("tmdb {path} returned {status}: {}", body.chars().take(200).collect::<String>());
        }
        serde_json::from_str(&body).with_context(|| format!("decoding {path}"))
    }

    /// Series titles plus the IMDb id when one exists.
    pub async fn series(&self, tmdb_id: &str) -> Result<Title> {
        let id = numeric_id(tmdb_id)?;
        let details: SeriesDetails = self.get_json(&format!("/tv/{id}"), true).await?;
        // A series only exposes its IMDb id here, and it is null for titles that have
        // no IMDb entry at all.
        let external: ExternalIds = self
            .get_json(&format!("/tv/{id}/external_ids"), false)
            .await?;

        Ok(Title {
            imdb_id: clean_imdb(external.imdb_id),
            name: details.name,
            original_name: details.original_name,
            year: year_of(details.first_air_date.as_deref()),
        })
    }

    pub async fn movie(&self, tmdb_id: &str) -> Result<Title> {
        let id = numeric_id(tmdb_id)?;
        let details: MovieDetails = self.get_json(&format!("/movie/{id}"), true).await?;

        Ok(Title {
            imdb_id: clean_imdb(details.imdb_id),
            name: details.title,
            original_name: details.original_title,
            year: year_of(details.release_date.as_deref()),
        })
    }
}

/// TMDB ids are numeric; anything else is a malformed request and must not be
/// pasted into a URL.
fn numeric_id(raw: &str) -> Result<u64> {
    raw.trim()
        .parse::<u64>()
        .with_context(|| format!("{raw:?} is not a numeric TMDB id"))
}

/// TMDB returns null, and sometimes an empty string, for a missing IMDb id. Both
/// have to collapse to None, otherwise an empty id would be used for an exact search
/// that can only fail.
fn clean_imdb(raw: Option<String>) -> Option<String> {
    let value = raw?;
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.starts_with("tt") {
        return None;
    }
    Some(trimmed.to_string())
}

fn year_of(date: Option<&str>) -> Option<u32> {
    date?.get(..4)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_imdb_id_collapses_to_none() {
        // The measured shape for a Hungarian-only series.
        assert_eq!(clean_imdb(None), None);
        assert_eq!(clean_imdb(Some(String::new())), None);
        assert_eq!(clean_imdb(Some("   ".into())), None);
        // Anything not shaped like an IMDb id is unusable for an exact search.
        assert_eq!(clean_imdb(Some("12345".into())), None);
        assert_eq!(clean_imdb(Some("tt1951266".into())), Some("tt1951266".into()));
        assert_eq!(clean_imdb(Some("  tt123  ".into())), Some("tt123".into()));
    }

    #[test]
    fn the_year_comes_from_the_date_prefix() {
        assert_eq!(year_of(Some("2025-06-18")), Some(2025));
        assert_eq!(year_of(Some("2015-11-18")), Some(2015));
        assert_eq!(year_of(Some("")), None);
        assert_eq!(year_of(Some("abcd-01-01")), None);
        assert_eq!(year_of(None), None);
    }

    #[test]
    fn non_numeric_ids_are_refused_before_reaching_a_url() {
        assert!(numeric_id("294663").is_ok());
        assert!(numeric_id(" 294663 ").is_ok());
        assert!(numeric_id("tt1951266").is_err());
        assert!(numeric_id("../secret").is_err());
        assert!(numeric_id("").is_err());
    }

    /// A Hungarian production has the same localised and original title, so the
    /// search must not try it twice.
    #[test]
    fn identical_titles_are_offered_once() {
        let t = Title {
            imdb_id: None,
            name: "Exek csatája".into(),
            original_name: "Exek csatája".into(),
            year: Some(2025),
        };
        assert_eq!(t.search_terms(), vec!["Exek csatája".to_string()]);
    }

    #[test]
    fn a_localised_title_is_tried_before_the_original() {
        let t = Title {
            imdb_id: Some("tt1951266".into()),
            name: "Az éhezők viadala: A kiválasztott - 2. rész".into(),
            original_name: "The Hunger Games: Mockingjay - Part 2".into(),
            year: Some(2015),
        };
        let terms = t.search_terms();
        assert_eq!(terms.len(), 2);
        assert!(terms[0].starts_with("Az éhezők"));
        assert!(terms[1].starts_with("The Hunger"));
    }

    #[test]
    fn an_empty_localised_title_does_not_produce_an_empty_search() {
        let t = Title {
            imdb_id: None,
            name: String::new(),
            original_name: "Original".into(),
            year: None,
        };
        assert_eq!(t.search_terms(), vec!["Original".to_string()]);
    }

    #[test]
    fn an_empty_api_key_is_refused_at_construction() {
        assert!(TmdbClient::new("", "hu-HU").is_err());
        assert!(TmdbClient::new("   ", "hu-HU").is_err());
        assert!(TmdbClient::new("key", "").is_ok(), "language falls back");
    }
}
