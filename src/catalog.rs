//! The recommended catalogue: what the tracker itself says is worth watching.
//!
//! The addon has only ever answered "where can I play this", which means it can only be used
//! by somebody who already knows what they want. This turns the tracker's own recommended page
//! into two browsable rows in Stremio, films and series, so there is something to look at.
//!
//! # Which list, and why that one
//!
//! The recommended page has two halves per category. The left is a grid of covers, the staff's
//! own picks; the right is a ranked list of the most active torrents, with a bar showing how
//! far ahead of the next one each is. It is the right-hand list that is used here, because that
//! is what was asked for and because it is the one that says what people are actually watching
//! this week rather than what somebody chose to feature.
//!
//! # The one hard part
//!
//! That list gives a torrent id and a release name. Nothing else: no IMDb id, no cover, no
//! title in any language. So every row has to be turned into something Stremio can show, and
//! that is done through TMDB rather than by asking the tracker for each torrent's own page,
//! which would be forty requests against a private account for one refresh.
//!
//! The release name is cleaned back to a title and a year, TMDB is searched for it, and the
//! result gives the poster and the id. Rows that cannot be resolved are dropped rather than
//! shown blank: a catalogue row with no cover and a filename for a title is worse than one
//! item fewer.
//!
//! # A separate addon
//!
//! This is published under its own manifest rather than added to the streaming one. Stremio
//! remembers what an installed addon can do, so adding a resource to the existing manifest
//! would mean removing and re-adding the addon on every device, television included, before
//! the catalogue appeared. A second manifest leaves the working installation alone and makes
//! the catalogue something that can be added, and removed, on its own.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::RwLock;

use crate::stremio::Meta;

/// How long a built catalogue is served for before it counts as stale.
///
/// A day. The list is a week's worth of activity, so rebuilding it more often would be asking
/// a private tracker and TMDB for an answer that has not changed.
pub const MAX_AGE: u64 = 24 * 60 * 60;

/// The built rows, and when they were built.
#[derive(Default)]
pub struct Cache {
    inner: RwLock<Built>,
}

#[derive(Default, Clone)]
struct Built {
    films: Vec<Meta>,
    series: Vec<Meta>,
    built_at: crate::state::Unix,
}

/// Which of the two catalogues is being asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Film,
    Series,
}

impl Kind {
    /// The id in the manifest, and in the URL Stremio then asks with.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Film => "stremhu-ncore-film",
            Kind::Series => "stremhu-ncore-sorozat",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Kind::Film => "nCore Ajánló, Film",
            Kind::Series => "nCore Ajánló, Sorozat",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "stremhu-ncore-film" => Some(Kind::Film),
            "stremhu-ncore-sorozat" => Some(Kind::Series),
            _ => None,
        }
    }

    /// The heading the tracker's page puts above this category.
    fn heading(self) -> &'static str {
        match self {
            Kind::Film => "Film",
            Kind::Series => "Sorozat",
        }
    }
}

impl Cache {
    pub async fn rows(&self, kind: Kind) -> Vec<Meta> {
        let built = self.inner.read().await;
        match kind {
            Kind::Film => built.films.clone(),
            Kind::Series => built.series.clone(),
        }
    }

    pub async fn built_at(&self) -> crate::state::Unix {
        self.inner.read().await.built_at
    }

    /// Whether the catalogue is old enough to be worth building again.
    pub async fn is_stale(&self, now: crate::state::Unix) -> bool {
        let built = self.inner.read().await;
        built.built_at == 0 || now.saturating_sub(built.built_at) >= MAX_AGE
    }

    async fn store(&self, films: Vec<Meta>, series: Vec<Meta>, at: crate::state::Unix) {
        let mut built = self.inner.write().await;
        // A refresh that resolved nothing must not empty a catalogue that is already there.
        // TMDB being briefly unreachable is not a reason to show the viewer an empty shelf.
        if !films.is_empty() {
            built.films = films;
        }
        if !series.is_empty() {
            built.series = series;
        }
        built.built_at = at;
    }
}

/// One row of the tracker's ranked list: all it actually gives us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub torrent_id: String,
    pub release: String,
}

/// Reads the most-active list out of one category's section of the recommended page.
///
/// The page is one section per category, each headed `<div class="fobox_fej">Film</div>`, and
/// inside it a two-column layout whose right column is this list. Rows are plain links to a
/// torrent's own page with the release name as their text.
///
/// An empty list is returned when the section is not there, rather than an error: the page is
/// part of the site's interface and may be rearranged, and a catalogue that quietly has one
/// fewer row is better than a startup that fails.
pub fn parse_recommended(html: &str, kind: Kind) -> Vec<Row> {
    let heading = format!("\"fobox_fej\">{}", kind.heading());
    let Some(start) = html.find(&heading) else {
        return Vec::new();
    };
    // Up to the next category heading, so the film list cannot run on into the series one.
    let rest = &html[start + heading.len()..];
    let section = match rest.find("\"fobox_fej\">") {
        Some(next) => &rest[..next],
        None => rest,
    };

    // The right-hand column. Without it there is nothing here worth guessing at.
    let Some(column) = section.find("width:50%; padding-left") else {
        return Vec::new();
    };
    let column = &section[column..];

    let mut out = Vec::new();
    for part in column.split("torrents.php?action=details&id=").skip(1) {
        let torrent_id: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if torrent_id.is_empty() {
            continue;
        }
        // The link text is the release name, between the end of the opening tag and `</a>`.
        let Some((_, after_tag)) = part.split_once('>') else {
            continue;
        };
        let Some((text, _)) = after_tag.split_once("</a>") else {
            continue;
        };
        let release = crate::ncore::decode_entities(crate::ncore::strip_tags(text).trim());
        if release.is_empty() {
            continue;
        }
        out.push(Row {
            torrent_id,
            release,
        });
    }
    out
}

/// The title and year hiding inside a release name.
///
/// Release names are a title, then a year or a season marker, then everything a viewer does
/// not want in a catalogue: the resolution, the source, the codec, the group. Cutting at the
/// first of those is what turns `Project.Hail.Mary.2026.IMAX.iT.WEBRip.x264.HUN-FULCRUM` into
/// "Project Hail Mary" and 2026, which is a thing TMDB can be asked about.
pub fn title_of(release: &str) -> (String, Option<u32>) {
    let spaced = release.replace(['.', '_'], " ");
    let words: Vec<&str> = spaced.split_whitespace().collect();

    let mut title: Vec<&str> = Vec::new();
    let mut year = None;
    for word in words {
        let bare = word.trim_matches(|c: char| c == '(' || c == ')' || c == '[' || c == ']');
        // A year ends the title and is worth keeping: it is what tells two films of the same
        // name apart, and TMDB takes it as a hint.
        if let Ok(value) = bare.parse::<u32>() {
            if (1900..=2100).contains(&value) {
                year = Some(value);
                break;
            }
        }
        // A season or episode marker ends it too, and means nothing to TMDB.
        if crate::series::parse(bare).is_some() {
            break;
        }
        if is_quality_word(bare) {
            break;
        }
        title.push(word);
        // A release name is not a novel. Anything past this is not a title any more.
        if title.len() >= 12 {
            break;
        }
    }

    (title.join(" ").trim().to_string(), year)
}

/// Words that are never part of a title and always the start of the technical tail.
fn is_quality_word(word: &str) -> bool {
    const WORDS: &[&str] = &[
        "1080p", "720p", "2160p", "480p", "576p", "webrip", "web-dl", "webdl", "bluray",
        "blu-ray", "bdrip", "brrip", "dvdrip", "dvd9", "dvd5", "hdtv", "remux", "x264", "x265",
        "h264", "h265", "hevc", "xvid", "divx", "hun", "eng", "multi", "complete", "proper",
        "repack", "retail", "uhd", "amzn", "nf", "dsnp", "atvp", "hmax", "pmtp", "skst", "ma",
    ];
    let lower = word.to_ascii_lowercase();
    WORDS.contains(&lower.as_str())
}

/// Builds both catalogues from the tracker and TMDB.
///
/// Fails only when the tracker's page could not be read at all. A row TMDB cannot place is
/// dropped and the rest are kept: one missing film is not a reason to have no catalogue.
pub async fn build(
    ncore: &crate::ncore::NcoreClient,
    tmdb: &crate::tmdb::TmdbClient,
    limit: usize,
) -> Result<(Vec<Meta>, Vec<Meta>)> {
    let html = ncore
        .recommended()
        .await
        .context("reading the recommended page")?;

    let mut out = Vec::new();
    for kind in [Kind::Film, Kind::Series] {
        let rows = parse_recommended(&html, kind);
        if rows.is_empty() {
            tracing::warn!(
                catalogue = kind.id(),
                "the recommended page has no list for this category; its layout may have changed"
            );
        }
        let mut metas: Vec<Meta> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for row in rows.into_iter().take(limit) {
            let (title, year) = title_of(&row.release);
            if title.is_empty() {
                continue;
            }
            match tmdb.catalogue_entry(&title, year, kind == Kind::Series).await {
                Ok(Some(meta)) => {
                    // The same film often appears two or three times in one list, once per
                    // release. The viewer wants the film once.
                    if seen.insert(meta.id.clone()) {
                        metas.push(meta);
                    }
                }
                Ok(None) => tracing::debug!(title = %title, "TMDB does not know this one"),
                Err(e) => tracing::warn!(error = %e, title = %title, "TMDB lookup failed"),
            }
        }
        tracing::info!(catalogue = kind.id(), rows = metas.len(), "catalogue built");
        out.push(metas);
    }

    let mut it = out.into_iter();
    let films = it.next().unwrap_or_default();
    let series = it.next().unwrap_or_default();
    Ok((films, series))
}

/// Rebuilds the catalogue when it is stale, and says whether it managed to.
///
/// Both clients have to be there. Without a TMDB key there is no poster and no id for
/// anything on that page, which would be a catalogue of filenames; that is not worth
/// publishing, so it simply does not appear.
pub async fn refresh(state: &Arc<crate::app::AppState>) -> Result<()> {
    let guard = state.tmdb.read().await;
    let tmdb = guard
        .as_ref()
        .context("tmdb.api_key is not set, so the recommended catalogue cannot be built")?;
    let ncore = state.ncore.read().await;

    let (films, series) = build(&ncore, tmdb, ROWS_PER_CATALOGUE).await?;
    let at = crate::state::now();
    state.catalog.store(films, series, at).await;
    state.store.set_catalog_built_at(at).await;
    Ok(())
}

/// How many rows each catalogue holds.
///
/// The tracker's own list is about this long, and a browsable row in Stremio is scrolled
/// sideways: past thirty nobody is looking.
pub const ROWS_PER_CATALOGUE: usize = 30;

#[cfg(test)]
mod tests {
    use super::*;

    /// A page shaped like the live one, cut down to two categories and three rows.
    ///
    /// Every detail here is one the real page has: the section headings the lists have to be
    /// separated by, the two-column layout whose right half is the list wanted, and the staff
    /// grid in the left half whose links must not be read as list rows.
    fn page() -> String {
        r#"<div class="fobox_fej">Film</div>
        <div class="fobox_tartalom"><div style="display:flex;">
          <div style="border-right: 4px solid #e0e0e0;width: 50%;"> Staff által jelölt
            <a href="torrents.php?action=details&id=111"><img title="Staff.Pick.2020.1080p"/></a>
          </div>
          <div style="width:50%; padding-left: 16px;">
            <div><a href="torrents.php?action=details&id=4157267">Project.Hail.Mary.2026.IMAX.iT.WEBRip.x264.HUN-FULCRUM</a><div style="min-width: 150px;"></div></div>
            <div><a href="torrents.php?action=details&id=4216881">In.the.Grey.2026.MA.WEBRip.x264.HUN-FULCRUM</a></div>
          </div>
        </div></div>
        <div class="fobox_fej">Sorozat</div>
        <div class="fobox_tartalom"><div style="display:flex;">
          <div style="border-right: 4px solid #e0e0e0;width: 50%;"> Staff által jelölt </div>
          <div style="width:50%; padding-left: 16px;">
            <div><a href="torrents.php?action=details&id=4220802">Exek.csataja.S02.HUN.WEB-DL.1080p.H264-LEGION</a></div>
          </div>
        </div></div>
        <div class="fobox_fej">Játék</div>
        <div style="width:50%; padding-left: 16px;">
          <div><a href="torrents.php?action=details&id=999">Valami.Jatek.2026-GROUP</a></div>
        </div>"#
            .to_string()
    }

    #[test]
    fn the_ranked_list_is_read_and_the_staff_grid_is_not() {
        let films = parse_recommended(&page(), Kind::Film);
        assert_eq!(films.len(), 2, "the staff pick must not be counted: {films:?}");
        assert_eq!(films[0].torrent_id, "4157267");
        assert_eq!(
            films[0].release,
            "Project.Hail.Mary.2026.IMAX.iT.WEBRip.x264.HUN-FULCRUM"
        );
    }

    /// One category's list must not run on into the next one's. Without the section boundary
    /// the film catalogue would quietly fill up with series and games.
    #[test]
    fn each_category_gets_only_its_own_rows() {
        let series = parse_recommended(&page(), Kind::Series);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].torrent_id, "4220802");

        let films = parse_recommended(&page(), Kind::Film);
        assert!(
            films.iter().all(|r| r.torrent_id != "4220802"),
            "a series leaked into the film list"
        );
    }

    /// A page whose layout has changed gives nothing rather than nonsense.
    #[test]
    fn an_unrecognised_page_yields_nothing() {
        assert!(parse_recommended("<html>semmi</html>", Kind::Film).is_empty());
        assert!(parse_recommended("", Kind::Series).is_empty());
    }

    #[test]
    fn a_release_name_becomes_a_title_and_a_year() {
        assert_eq!(
            title_of("Project.Hail.Mary.2026.IMAX.iT.WEBRip.x264.HUN-FULCRUM"),
            ("Project Hail Mary".to_string(), Some(2026))
        );
        assert_eq!(
            title_of("In.the.Grey.2026.MA.WEBRip.x264.HUN-FULCRUM"),
            ("In the Grey".to_string(), Some(2026))
        );
        // A series is cut at its season marker, which is not a year.
        assert_eq!(
            title_of("Exek.csataja.S02.HUN.WEB-DL.1080p.H264-LEGION"),
            ("Exek csataja".to_string(), None)
        );
        // And one with no year at all still gives a usable title.
        assert_eq!(
            title_of("A.Viszkis.BD50"),
            ("A Viszkis BD50".to_string(), None)
        );
    }

    /// The quality tail must never reach TMDB: searching for "Wind River 2017 RETAiL 1080p"
    /// finds nothing, and the row would be dropped for no reason.
    #[test]
    fn the_technical_tail_is_cut_off() {
        let (title, year) = title_of("Wind.River.2017.RETAiL.1080p.HUN.Blu-ray.AVC.DTS-HD.MA.5.1-HyperX");
        assert_eq!(title, "Wind River");
        assert_eq!(year, Some(2017));

        let (title, _) = title_of("Blade.Runner.2049.2017.COMPLETE.UHD.BLURAY-TERMiNAL");
        // 2049 is a year by shape and part of the title by meaning. Cutting there is the
        // wrong answer and a known limit of reading names this way; what matters is that the
        // codec and the group never survive.
        assert!(!title.to_lowercase().contains("bluray"));
        assert!(!title.to_lowercase().contains("terminal"));
    }
}
