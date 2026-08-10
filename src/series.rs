//! Reading season and episode numbers out of release names.
//!
//! This has to cope with how Hungarian trackers actually label things, which is a
//! wider set of shapes than the international `S01E04` convention:
//!
//!   * `S01E04`, `s1.e4`, and episode ranges such as `S01E04-E06`
//!   * `1x05` and `1x05-x07`
//!   * `Season 1 Episode 4`
//!   * `1. évad 4. rész`, the Hungarian form, which is the one that matters most here
//!   * `4. rész` or `ep4` on their own, when the season is implied
//!   * season packs: `S01`, and multi-season packs like `S01+S02` or `1-3. évad`
//!
//! A result carries *lists*, because one release can cover several seasons or
//! episodes. A pack that covers what was asked for is a valid source; the individual
//! file inside it is chosen later.
//!
//! The dangerous case is a bare number. Release names are full of them: `2160p`,
//! `x265`, `2014`, `1920x1080`, `7.1`. Those are stripped before any bare number is
//! considered, and even then a bare number only counts when no better shape matched.
//! Answering an episode request with the wrong episode is worse than answering with
//! nothing.

use std::sync::LazyLock;

use regex::Regex;

/// What a release name says about its contents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SeriesInfo {
    pub seasons: Vec<u32>,
    /// Empty for a season pack.
    pub episodes: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeasonEpisode {
    pub season: u32,
    pub episode: u32,
}

/// How well a release answers a request for one episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// The name names this very episode.
    Exact,
    /// A pack that contains it; the file is picked from inside the torrent.
    Pack,
}

impl SeriesInfo {

    /// None when this release cannot serve the wanted episode.
    /// True when the name covers whole seasons rather than one episode.
    ///
    /// Only the tests ask this; the matcher itself answers the sharper question of whether
    /// a given episode is covered.
    #[cfg(test)]
    pub fn is_pack(&self) -> bool {
        self.episodes.is_empty() && !self.seasons.is_empty()
    }

    pub fn covers(&self, want: SeasonEpisode) -> Option<Match> {
        if !self.seasons.contains(&want.season) {
            return None;
        }
        if self.episodes.is_empty() {
            return Some(Match::Pack);
        }
        if self.episodes.contains(&want.episode) {
            return Some(Match::Exact);
        }
        None
    }
}

// Ordered from most specific to least. The first shape that matches wins, so a name
// containing both `S01E04` and a stray number cannot be misread.
static SXXEXX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"s(\d{1,2})[ ._-]?e(\d{1,4})(?:[ ._-]?-[ ._-]?e?(\d{1,4}))?").expect("valid")
});
static NXNN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^\d])(\d{1,2})x(\d{1,4})(?:-x?(\d{1,4}))?").expect("valid"));
static WORDY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"season[ ._-]?(\d{1,2})[ ._-]?(?:episode|ep)[ ._-]?(\d{1,4})(?:[ ._-]?-[ ._-]?(?:episode|ep)?[ ._-]?(\d{1,4}))?")
        .expect("valid")
});
/// `1. évad 4. rész`, with the dots and spacing optional.
static HUNGARIAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{1,2})\.?\s*(?:évad|evad).*?(\d{1,4})\.?\s*(?:rész|resz)").expect("valid")
});
/// A season range or list, in either language: `seasons 1-3`, `s01+s02`, `1-3. évad`.
static SEASON_RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:seasons?|évad|evad)[ ._-]*(\d{1,2})\s*(?:-|to|,|&|\+|and)\s*(\d{1,2})")
        .expect("valid")
});
static SEASON_RANGE_HU: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{1,2})\s*(?:-|to|,|&|\+|and)\s*(\d{1,2})\.?\s*(?:évad|evad)").expect("valid")
});
static MULTI_S: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"s(\d{1,2})(?:[ ._+&-]+s(\d{1,2}))+").expect("valid"));
static EVERY_S: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"s(\d{1,2})").expect("valid"));
/// A lone season marker: `S01` or `1. évad`, with no episode anywhere.
static SEASON_ONLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^a-z\d])s(\d{1,2})(?:[^a-z\d]|$)|(\d{1,2})\.?\s*(?:évad|evad)")
        .expect("valid")
});
/// An episode with no season: `E04`, `ep 4`, `episode 4`, `4. rész`.
static LONE_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^a-z\d])(?:e|ep|episode)[ ._-]?(\d{1,4})(?:[^a-z\d]|$)|(\d{1,4})\.?\s*(?:rész|resz)")
        .expect("valid")
});
/// Everything that looks like a number but is not an episode: resolutions, frame
/// dimensions, years, and audio channel counts.
static NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{3,4}\s*[xх]\s*\d{3,4}|\b(?:2160|1080|720|576|540|480|264|265|19\d{2}|20\d{2})\b|\b\d\.\d\b")
        .expect("valid")
});

/// Folds Hungarian accents to plain ASCII.
///
/// This matters in two places. A title from TMDB carries accents (`Exek csatája`)
/// while tracker release names are almost always plain (`Exek.csataja.S01E04.HUN`),
/// so a search has to be tried both ways or it finds nothing. And a release name may
/// spell the season and episode words either way, so folding first means the
/// patterns below only ever have to handle the ASCII spelling.
pub fn fold_accents(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'ő' => 'o',
            'ú' | 'ù' | 'û' | 'ü' | 'ű' => 'u',
            'Á' | 'À' | 'Â' | 'Ä' => 'A',
            'É' | 'È' | 'Ê' | 'Ë' => 'E',
            'Í' | 'Ì' | 'Î' | 'Ï' => 'I',
            'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Ő' => 'O',
            'Ú' | 'Ù' | 'Û' | 'Ü' | 'Ű' => 'U',
            other => other,
        })
        .collect()
}

fn num(m: Option<regex::Match<'_>>) -> Option<u32> {
    m?.as_str().parse().ok()
}

/// Expands `from..=to` when the pair makes sense, otherwise just `from`.
fn range(from: u32, to: Option<u32>) -> Vec<u32> {
    match to {
        // A bounded range only: a name claiming episodes 1 to 900 is a parse
        // accident, not a real pack.
        Some(to) if to > from && to - from <= 200 => (from..=to).collect(),
        _ => vec![from],
    }
}

/// Reads what a release name says about seasons and episodes.
pub fn parse(name: &str) -> Option<SeriesInfo> {
    // Folded first, so `ÉVAD`, `évad` and `evad` all reach the patterns identically.
    let lower = fold_accents(name).to_lowercase();

    if let Some(c) = SXXEXX.captures(&lower) {
        let season = num(c.get(1))?;
        let first = num(c.get(2))?;
        return Some(SeriesInfo {
            seasons: vec![season],
            episodes: range(first, num(c.get(3))),
        });
    }
    if let Some(c) = NXNN.captures(&lower) {
        let season = num(c.get(1))?;
        let first = num(c.get(2))?;
        return Some(SeriesInfo {
            seasons: vec![season],
            episodes: range(first, num(c.get(3))),
        });
    }
    if let Some(c) = WORDY.captures(&lower) {
        let season = num(c.get(1))?;
        let first = num(c.get(2))?;
        return Some(SeriesInfo {
            seasons: vec![season],
            episodes: range(first, num(c.get(3))),
        });
    }
    if let Some(c) = HUNGARIAN.captures(&lower) {
        let season = num(c.get(1))?;
        let episode = num(c.get(2))?;
        return Some(SeriesInfo {
            seasons: vec![season],
            episodes: vec![episode],
        });
    }

    // Season packs, widest first so a range is not read as a single season.
    for re in [&*SEASON_RANGE, &*SEASON_RANGE_HU] {
        if let Some(c) = re.captures(&lower) {
            let from = num(c.get(1))?;
            let to = num(c.get(2))?;
            if to >= from && to - from <= 50 {
                return Some(SeriesInfo {
                    seasons: (from..=to).collect(),
                    episodes: Vec::new(),
                });
            }
        }
    }
    if MULTI_S.is_match(&lower) {
        let seasons: Vec<u32> = EVERY_S
            .captures_iter(&lower)
            .filter_map(|c| num(c.get(1)))
            .collect();
        if seasons.len() > 1 {
            return Some(SeriesInfo {
                seasons,
                episodes: Vec::new(),
            });
        }
    }
    if let Some(c) = SEASON_ONLY.captures(&lower) {
        if let Some(season) = num(c.get(1)).or_else(|| num(c.get(2))) {
            return Some(SeriesInfo {
                seasons: vec![season],
                episodes: Vec::new(),
            });
        }
    }

    // Nothing named a season. Strip the numeric noise before trusting any number,
    // so `2160p` and `2014` cannot become episode numbers.
    let cleaned = NOISE.replace_all(&lower, " ");
    if let Some(c) = LONE_EPISODE.captures(&cleaned) {
        if let Some(episode) = num(c.get(1)).or_else(|| num(c.get(2))) {
            return Some(SeriesInfo {
                seasons: Vec::new(),
                episodes: vec![episode],
            });
        }
    }
    None
}

/// Whether a release can serve one episode. A name with an episode but no season is
/// accepted for that episode number, since single-season shows are often labelled
/// that way.
pub fn match_episode(name: &str, want: SeasonEpisode) -> Option<Match> {
    let info = parse(name)?;
    if info.seasons.is_empty() {
        return info
            .episodes
            .contains(&want.episode)
            .then_some(Match::Exact);
    }
    info.covers(want)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn want(season: u32, episode: u32) -> SeasonEpisode {
        SeasonEpisode { season, episode }
    }

    #[test]
    fn standard_numbering() {
        let i = parse("Show.S01E04.1080p.WEB-DL").expect("parses");
        assert_eq!(i.seasons, vec![1]);
        assert_eq!(i.episodes, vec![4]);
        assert_eq!(parse("Show.s1.e4.HUN").unwrap().episodes, vec![4]);
        assert_eq!(parse("A.hegyi.doktor.S19E08.HUN").unwrap().seasons, vec![19]);
    }

    #[test]
    fn episode_ranges_expand() {
        let i = parse("Show.S01E04-E06.HUN").expect("parses");
        assert_eq!(i.seasons, vec![1]);
        assert_eq!(i.episodes, vec![4, 5, 6]);
        assert_eq!(match_episode("Show.S01E04-E06", want(1, 5)), Some(Match::Exact));
        assert_eq!(match_episode("Show.S01E04-E06", want(1, 9)), None);
    }

    #[test]
    fn x_format_with_range() {
        assert_eq!(parse("Show.1x05.HUN").unwrap().episodes, vec![5]);
        assert_eq!(parse("Show.12x03").unwrap().seasons, vec![12]);
        assert_eq!(parse("Show.1x05-x07").unwrap().episodes, vec![5, 6, 7]);
    }

    #[test]
    fn wordy_english_form() {
        let i = parse("Show.Season.1.Episode.4.WEB").expect("parses");
        assert_eq!((i.seasons, i.episodes), (vec![1], vec![4]));
    }

    /// The form that matters most for this tracker, and the one the first version of
    /// this parser could not read at all.
    #[test]
    fn hungarian_numbering() {
        let i = parse("Exek csatája 1. évad 4. rész").expect("parses");
        assert_eq!((i.seasons.clone(), i.episodes.clone()), (vec![1], vec![4]));

        let j = parse("Valami.2.evad.10.resz.HUN").expect("parses");
        assert_eq!((j.seasons, j.episodes), (vec![2], vec![10]));
    }

    #[test]
    fn hungarian_episode_without_a_season() {
        let i = parse("Exek csatája 7. rész 1080p").expect("parses");
        assert!(i.seasons.is_empty());
        assert_eq!(i.episodes, vec![7]);
        // A single-season show labelled this way still answers an episode request.
        assert_eq!(
            match_episode("Exek csatája 7. rész 1080p", want(1, 7)),
            Some(Match::Exact)
        );
    }

    #[test]
    fn season_packs() {
        let i = parse("Exek.csataja.S01.COMPLETE.HUN").expect("parses");
        assert_eq!(i.seasons, vec![1]);
        assert!(i.is_pack());
        assert_eq!(parse("Show.1. évad.WEB-DL").unwrap().seasons, vec![1]);
    }

    #[test]
    fn multi_season_packs() {
        let i = parse("Show.S01+S02.COMPLETE").expect("parses");
        assert_eq!(i.seasons, vec![1, 2]);
        assert!(i.is_pack());

        let r = parse("Show.Seasons.1-3.1080p").expect("parses");
        assert_eq!(r.seasons, vec![1, 2, 3]);

        let hu = parse("Show.1-3. évad.HUN").expect("parses");
        assert_eq!(hu.seasons, vec![1, 2, 3]);
    }

    #[test]
    fn a_pack_answers_any_episode_of_its_season() {
        assert_eq!(
            match_episode("Exek.csataja.S01.COMPLETE", want(1, 4)),
            Some(Match::Pack)
        );
        assert_eq!(
            match_episode("Show.Seasons.1-3.1080p", want(2, 17)),
            Some(Match::Pack)
        );
        assert_eq!(match_episode("Exek.csataja.S01.COMPLETE", want(2, 4)), None);
    }

    /// Numbers in release names are mostly not episodes; reading one as an episode
    /// would send back the wrong video.
    #[test]
    fn numeric_noise_is_not_read_as_an_episode() {
        for name in [
            "Film.2014.2160p.UHD.HDR.BluRay.TrueHD.7.1.x265.HuN-TRiNiTY",
            "Film.1920x1080.x264",
            "Film.2026.WEBRip.1080p",
        ] {
            assert_eq!(parse(name), None, "misread: {name}");
        }
    }

    #[test]
    fn accents_are_folded_before_matching() {
        assert_eq!(fold_accents("Exek csatája"), "Exek csataja");
        assert_eq!(fold_accents("ÉVAD ŰRHAJÓ ÖSSZES"), "EVAD URHAJO OSSZES");
        assert_eq!(fold_accents("plain ascii"), "plain ascii");
    }

    /// Both spellings of the Hungarian words have to parse identically, including the
    /// accented uppercase form.
    #[test]
    fn accented_and_plain_hungarian_parse_the_same() {
        let accented = parse("Exek csatája 1. évad 4. rész").expect("parses");
        let plain = parse("Exek csataja 1. evad 4. resz").expect("parses");
        assert_eq!(accented, plain);

        let shouty = parse("EXEK CSATÁJA 1. ÉVAD 4. RÉSZ").expect("parses");
        assert_eq!(shouty, plain);
    }

    #[test]
    fn a_wrong_episode_is_never_matched() {
        assert_eq!(match_episode("Show.S01E05.HUN", want(1, 4)), None);
        assert_eq!(match_episode("Show.S02.COMPLETE", want(1, 4)), None);
        assert_eq!(match_episode("Unrelated.Film.2014.1080p", want(1, 4)), None);
        assert_eq!(match_episode("", want(1, 4)), None);
    }

    #[test]
    fn the_most_specific_shape_wins() {
        // Both an episode and a season marker are present; the episode has to win so
        // the release is not treated as a whole-season pack.
        let i = parse("Show.S01.E04.Something.S02").expect("parses");
        assert_eq!(i.episodes, vec![4]);
        assert!(!i.is_pack());
    }

    #[test]
    fn an_implausible_range_collapses_to_one_episode() {
        // A parse accident must not claim hundreds of episodes.
        let i = parse("Show.S01E01-E900").expect("parses");
        assert_eq!(i.episodes, vec![1]);
    }
}
