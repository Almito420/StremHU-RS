//! Reading a release name into the things a viewer actually chooses between.
//!
//! A list of raw release names is not a choice a person can make quickly. What matters
//! is the resolution, whether it has HDR, what the audio is, and how many seeders it
//! has. So the name is parsed into groups, and each group is rendered with its own
//! marker.
//!
//! The patterns and the labels are taken from the implementation being replaced,
//! deliberately and character for character where it matters: those regexes have been
//! run against this tracker's naming habits for a long time, and inventing new ones
//! would only reintroduce mistakes that have already been fixed once. Two of its
//! groups were left out at first, editions and 3D, on the grounds that nothing here comes in
//! more than one edition often enough to earn a line. Both are back, and 3D for a sharper
//! reason than completeness: starting a side-by-side file by accident on a television that is
//! not set up for it is a bad minute, and the label costs nothing.
//!
//! A ninth group is ours rather than the original's: which streaming service a release was
//! taken from, read from the tag the tracker puts in the name. It answers a question the
//! original never asked, and it is the one a viewer with subscriptions asks first: is this
//! already somewhere I pay for, so I need not download it at all?
//!
//! Order inside a group is the order of the table, so the strongest match is listed
//! first and single-valued groups take the first hit rather than an arbitrary one.

use std::sync::LazyLock;

use regex::Regex;

/// Which choice a tag belongs to. The marker is what precedes it on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Language,
    Resolution,
    VideoQuality,
    Source,
    VideoCodec,
    AudioQuality,
    AudioChannels,
    AudioSpatial,
    /// Cut or release variant: IMAX, Extended, the director's cut.
    Edition,
    /// Side-by-side and over-under stereoscopic releases.
    ThreeD,
    /// The streaming service the release was taken from.
    Service,
}

impl Group {
    /// The marker shown in front of the group.
    pub fn marker(self) -> &'static str {
        match self {
            Group::Language => "🌐",
            Group::Resolution => "📺",
            Group::VideoQuality => "✨",
            Group::Source => "💿",
            Group::VideoCodec => "🎞️",
            Group::AudioQuality => "🔊",
            Group::AudioChannels => "📻",
            Group::AudioSpatial => "🌌",
            // The original's own marker for this group.
            Group::Edition => "🏷️",
            // Goggles: the pull request that brought this to the original names this emoji in
            // as many words, and it reads as stereoscopic at a glance in a way a label does not.
            Group::ThreeD => "🥽",
            // Not a logo. A Stremio stream row is text, so a real service mark cannot go there;
            // popcorn plus the service name is as close as the medium allows.
            Group::Service => "🍿",
        }
    }

    /// False when only the best match is worth showing. A release has one resolution
    /// and one video codec; it can carry several audio tracks and several sources.
    fn multiple(self) -> bool {
        match self {
            // A release is in one stereoscopic layout or none, and comes from one service.
            Group::Resolution
            | Group::VideoCodec
            | Group::AudioChannels
            | Group::ThreeD
            | Group::Service => false,
            _ => true,
        }
    }
}

struct Tag {
    group: Group,
    /// Short, stable name used in the configuration's ordering lists. Kept separate
    /// from the label so the text on screen can be reworded without invalidating
    /// somebody's settings file.
    id: &'static str,
    label: &'static str,
    pattern: &'static str,
    /// Labels this one makes redundant once it has matched.
    ///
    /// The reference implementation expresses this with negative lookahead, as in
    /// `dts(?![-_. ]?(?:hd|x))`, which Rust's regex engine does not support: it
    /// guarantees linear-time matching and lookaround cannot be done in linear time.
    /// Stating the precedence outright does the same job and is easier to read than a
    /// lookahead anyway, since "DTS-HD MA wins over DTS" is the actual intent.
    supersedes: &'static [&'static str],
}

/// The table, most specific first within each group.
///
/// Specificity matters: `HDR10+` has to be tried before `HDR10`, and `DTS-HD` before
/// `DTS`, or the looser pattern claims the match and the label understates what the
/// release actually is.
static TAGS: &[Tag] = &[
    // Stereoscopic. The pattern is the original's, character for character: `3d` on its own,
    // and the three abbreviations that actually appear in release names. Anchored on word
    // boundaries so a group called "3DTV" or a codec string cannot claim it.
    Tag {
        group: Group::ThreeD,
        id: "3d",
        label: "3D",
        pattern: r"(?i)\b(3d|hsbs|hou|half[-_. ]?(?:sbs|ou))\b",
        supersedes: &[],
    },
    // Streaming services, as this tracker writes them: the tag sits between dots in the
    // release name, `The.Whisper.Man.2026.2160p.NF.WEB-DL`. The separators are required on
    // both sides rather than a bare word boundary, because two and four letter abbreviations
    // are exactly what a release group name is made of, and `NF` inside one is not Netflix.
    //
    // Which services appear here was measured against the tracker rather than copied from a
    // list of everything that exists.
    Tag {
        group: Group::Service,
        id: "netflix",
        label: "Netflix",
        pattern: r"(?i)[-_. ]nf[-_. ]",
        supersedes: &[],
    },
    Tag {
        group: Group::Service,
        id: "hbo",
        label: "HBO Max",
        pattern: r"(?i)[-_. ](?:hbo|hmax)[-_. ]",
        supersedes: &[],
    },
    Tag {
        group: Group::Service,
        id: "disney",
        label: "Disney+",
        pattern: r"(?i)[-_. ]dsnp[-_. ]",
        supersedes: &[],
    },
    Tag {
        group: Group::Service,
        id: "amazon",
        label: "Amazon",
        pattern: r"(?i)[-_. ]amzn[-_. ]",
        supersedes: &[],
    },
    Tag {
        group: Group::Service,
        id: "appletv",
        label: "Apple TV+",
        pattern: r"(?i)[-_. ]atvp[-_. ]",
        supersedes: &[],
    },
    Tag {
        group: Group::Service,
        id: "paramount",
        label: "Paramount+",
        pattern: r"(?i)[-_. ]pmtp[-_. ]",
        supersedes: &[],
    },
    Tag {
        group: Group::Service,
        id: "skyshowtime",
        label: "SkyShowtime",
        pattern: r"(?i)[-_. ]skst[-_. ]",
        supersedes: &[],
    },
    // Editions, most specific first. The labels are Hungarian because this is what the viewer
    // reads, and the ids are the original's so an ordering written against them still applies.
    Tag {
        group: Group::Edition,
        id: "imax",
        label: "IMAX",
        pattern: r"(?i)\bimax\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Edition,
        id: "directors-cut",
        label: "Rendezői változat",
        pattern: r"(?i)\b(directors?[-_. ]?cut|dc)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Edition,
        id: "extended",
        label: "Bővített",
        pattern: r"(?i)\bextended\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Edition,
        id: "remastered",
        label: "Felújított",
        pattern: r"(?i)\bremastered\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Edition,
        id: "uncut",
        label: "Vágatlan",
        pattern: r"(?i)\b(uncut|unrated)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Edition,
        id: "open-matte",
        label: "Open Matte",
        pattern: r"(?i)\bopen[-_. ]?matte\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Edition,
        id: "theatrical",
        label: "Mozis",
        pattern: r"(?i)\btheatrical\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Edition,
        id: "special-edition",
        label: "Különkiadás",
        pattern: r"(?i)\b(special|collectors?|limited|ultimate|definitive|anniversary)[-_. ]?edition\b",
        supersedes: &[],
    },
    // Languages.
    Tag {
        group: Group::Language,
        id: "hun",
        label: "Hun",
        pattern: r"(?i)\b(hun(?:[-_. ]?dub)?|magyar|hungarian)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Language,
        id: "eng",
        label: "Eng",
        pattern: r"(?i)\b(eng(?:[-_. ]?dub)?|english)\b",
        supersedes: &[],
    },
    // Resolutions.
    Tag {
        group: Group::Resolution,
        id: "2160p",
        label: "UHD (4K)",
        pattern: r"(?i)(2160p|4k[-_. ](?:UHD|HEVC|BD)|(?:UHD|HEVC|BD)[-_. ]4k|\b4k\b|COMPLETE.UHD|UHD.COMPLETE)",
        supersedes: &[],
    },
    Tag {
        group: Group::Resolution,
        id: "1080p",
        label: "Full HD (1080p)",
        pattern: r"(?i)(1080(i|p)|1920x1080)",
        supersedes: &[],
    },
    Tag {
        group: Group::Resolution,
        id: "720p",
        label: "HD (720p)",
        pattern: r"(?i)(720(i|p)|1280x720|960p)",
        supersedes: &[],
    },
    Tag {
        group: Group::Resolution,
        id: "576p",
        label: "SD (576p)",
        pattern: r"(?i)(576(i|p))",
        supersedes: &[],
    },
    Tag {
        group: Group::Resolution,
        id: "540p",
        label: "SD (540p)",
        pattern: r"(?i)(540(i|p))",
        supersedes: &[],
    },
    Tag {
        group: Group::Resolution,
        id: "480p",
        label: "SD (480p)",
        pattern: r"(?i)(480(i|p)|640x480|848x480)",
        supersedes: &[],
    },
    // Picture quality.
    Tag {
        group: Group::VideoQuality,
        id: "dv",
        label: "Dolby Vision",
        pattern: r"(?i)\b(dolby[-_. ]?vision|dovi|dv)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::VideoQuality,
        id: "hdr10plus",
        label: "HDR10+",
        pattern: r"(?i)(?:^|[^a-zA-Z0-9])(hdr10(?:plus|p|\+))(?:$|[^a-zA-Z0-9])",
        supersedes: &["HDR10"],
    },
    Tag {
        group: Group::VideoQuality,
        id: "hdr10",
        label: "HDR10",
        pattern: r"(?i)(?:^|[^a-zA-Z0-9])(hdr(?:10)?)(?:$|[^a-zA-Z0-9+])",
        supersedes: &[],
    },
    Tag {
        group: Group::VideoQuality,
        id: "hlg",
        label: "HLG",
        pattern: r"(?i)\b(hlg)\b",
        supersedes: &[],
    },
    // Where it came from.
    Tag {
        group: Group::Source,
        id: "remux",
        label: "Remux",
        pattern: r"(?i)\b(remux)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Source,
        id: "uhd",
        label: "UHD",
        pattern: r"(?i)\b(uhd)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Source,
        id: "bluray",
        label: "Blu-ray",
        pattern: r"(?i)\b(blu[-_. ]?ray)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Source,
        id: "bdrip",
        label: "BDRip",
        pattern: r"(?i)\b(bdrip)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Source,
        id: "webrip",
        label: "WebRip",
        pattern: r"(?i)\b(webrip|web[-_. ]rip)\b",
        // A WebRip is a re-encode of a stream, not the stream itself, so it must not
        // also be announced as the better thing.
        supersedes: &["Web-DL"],
    },
    Tag {
        group: Group::Source,
        id: "web-dl",
        label: "Web-DL",
        pattern: r"(?i)\b(web[-_. ]?dl|web)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Source,
        id: "dvdrip",
        label: "DVD Rip",
        pattern: r"(?i)\b(dvdrip)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Source,
        id: "hdtv",
        label: "TV Rip",
        pattern: r"(?i)\b(hdtv|pdtv|dvb|satrip)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::Source,
        // Narrower than the reference, on purpose. Its pattern also accepts bare `ts`,
        // `md` and `scr`, which occur inside ordinary release names and would label a
        // Blu-ray as a camera recording. A wrong label here is worse than a missing
        // one, and nothing on this tracker is a cam release anyway.
        id: "cam",
        label: "Cam",
        pattern: r"(?i)\b(cam|hdcam|telesync|dvdscr|mdhun|mdub)\b",
        supersedes: &[],
    },
    // Video codec.
    Tag {
        group: Group::VideoCodec,
        id: "av1",
        label: "AV1",
        pattern: r"(?i)\b(av1)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::VideoCodec,
        id: "x265",
        label: "x265",
        pattern: r"(?i)\b(x265|h\.?265|hevc)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::VideoCodec,
        id: "x264",
        label: "x264",
        pattern: r"(?i)\b(x264|h\.?264|avc)\b",
        supersedes: &[],
    },
    // Audio format.
    Tag {
        group: Group::AudioQuality,
        id: "truehd",
        label: "TrueHD",
        pattern: r"(?i)\b(truehd)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::AudioQuality,
        id: "dts-hd",
        label: "DTS-HD MA",
        pattern: r"(?i)\b(dts[-_. ]?hd(?:[-_. ]?ma)?|dtshdma)\b",
        supersedes: &["DTS"],
    },
    Tag {
        group: Group::AudioQuality,
        id: "flac",
        label: "FLAC",
        pattern: r"(?i)\b(flac)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::AudioQuality,
        id: "ddp",
        label: "Dolby Digital Plus",
        pattern: r"(?i)\b(ddp[-_. ]?(?:2\.0|5\.1|7\.1)?|dd\+[-_. ]?(?:2\.0|5\.1|7\.1)?|e[-_. ]?ac[-_. ]?3)",
        supersedes: &["Dolby Digital"],
    },
    Tag {
        group: Group::AudioQuality,
        id: "dts",
        label: "DTS",
        pattern: r"(?i)\b(dts[-_. x]*)",
        supersedes: &[],
    },
    Tag {
        group: Group::AudioQuality,
        id: "dd",
        label: "Dolby Digital",
        pattern: r"(?i)\b(dd[-_. ]?(?:2\.0|5\.1|7\.1)?|ac[-_. ]?3)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::AudioQuality,
        id: "aac",
        label: "AAC",
        pattern: r"(?i)\b(aac[-_. ]?(?:2\.0|5\.1)?|mp3)\b",
        supersedes: &[],
    },
    // Object-based surround.
    Tag {
        group: Group::AudioSpatial,
        id: "dtsx",
        label: "DTS:X",
        pattern: r"(?i)\b(dts[-_. ]?x)\b",
        supersedes: &[],
    },
    Tag {
        group: Group::AudioSpatial,
        id: "atmos",
        label: "Dolby Atmos",
        pattern: r"(?i)\b(atmos)\b",
        supersedes: &[],
    },
    // Channel count.
    Tag {
        group: Group::AudioChannels,
        id: "7.1",
        label: "7.1",
        pattern: r"(7\.1|7ch)",
        supersedes: &[],
    },
    Tag {
        group: Group::AudioChannels,
        id: "5.1",
        label: "5.1",
        pattern: r"(5\.1|6ch)",
        supersedes: &[],
    },
    Tag {
        group: Group::AudioChannels,
        id: "2.0",
        label: "2.0",
        pattern: r"(?i)(2\.0|2ch|stereo)",
        supersedes: &[],
    },
];

/// Compiled once. A bad pattern here is a programming error, not a runtime condition,
/// so it fails loudly at first use rather than silently matching nothing.
static COMPILED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    TAGS.iter()
        .map(|tag| {
            Regex::new(tag.pattern)
                .unwrap_or_else(|e| panic!("bad pattern for {}: {e}", tag.label))
        })
        .collect()
});

/// Everything recognised in one release name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attributes {
    /// Group and label, in table order.
    pub tags: Vec<(Group, &'static str)>,
}

impl Attributes {
    /// Reads a release name. The category is consulted only for the resolution, and
    /// only when the name itself does not say: nCore's `hdser` and `xvidser` buckets
    /// hold both 720p and 1080p releases, so the name is always the better source.
    pub fn parse(release_name: &str, category: &str) -> Self {
        let mut tags: Vec<(Group, &'static str)> = Vec::new();
        let mut beaten: Vec<&'static str> = Vec::new();

        for (tag, regex) in TAGS.iter().zip(COMPILED.iter()) {
            if !regex.is_match(release_name) {
                continue;
            }
            // A single-valued group keeps its first, most specific match.
            if !tag.group.multiple() && tags.iter().any(|(g, _)| *g == tag.group) {
                continue;
            }
            beaten.extend_from_slice(tag.supersedes);
            tags.push((tag.group, tag.label));
        }

        // `DTS-HD MA` and `DTS` both match a DTS-HD release; only the precise one is
        // worth showing. Applied after the sweep so precedence does not depend on
        // which pattern happens to sit earlier in the table.
        tags.retain(|(_, label)| !beaten.contains(label));

        if !tags.iter().any(|(g, _)| *g == Group::Resolution) {
            if let Some(label) = resolution_from_category(category) {
                tags.push((Group::Resolution, label));
            }
        }
        if !tags.iter().any(|(g, _)| *g == Group::Language) {
            if let Some(label) = language_from_category(category) {
                tags.push((Group::Language, label));
            }
        }

        Self { tags }
    }

    pub fn of(&self, group: Group) -> Vec<&'static str> {
        self.tags
            .iter()
            .filter(|(g, _)| *g == group)
            .map(|(_, label)| *label)
            .collect()
    }

    /// Configuration ids of everything matched, for the ordering lists.
    pub fn ids(&self) -> Vec<&'static str> {
        self.tags
            .iter()
            .filter_map(|(_, label)| id_for_label(label))
            .collect()
    }

    /// Position of this release in a preference list, lower being better.
    ///
    /// Anything the list does not mention sorts after everything it does. That is the
    /// useful behaviour: a list of `["2160p", "1080p"]` means "4K first, then 1080p,
    /// then whatever is left", not "hide the rest".
    pub fn rank_in(&self, preferences: &[String]) -> usize {
        let mine = self.ids();
        preferences
            .iter()
            .position(|wanted| {
                mine.iter()
                    .any(|id| id.eq_ignore_ascii_case(wanted.trim()))
            })
            .unwrap_or(preferences.len())
    }

    /// One group as `marker label, label`, or None when nothing matched.
    /// Whether anything in this group matched.
    pub fn has(&self, group: Group) -> bool {
        self.tags.iter().any(|(g, _)| *g == group)
    }

    pub fn render(&self, group: Group) -> Option<String> {
        let labels = self.of(group);
        if labels.is_empty() {
            return None;
        }
        Some(format!("{} {}", group.marker(), labels.join(", ")))
    }
}

fn id_for_label(label: &str) -> Option<&'static str> {
    TAGS.iter()
        .find(|tag| tag.label == label)
        .map(|tag| tag.id)
}

/// Every id the configuration's ordering lists may name. Used by the test that keeps
/// the shipped defaults honest.
#[cfg(test)]
pub fn known_ids() -> Vec<&'static str> {
    TAGS.iter().map(|tag| tag.id).collect()
}

/// Last resort when the release name carries no resolution.
///
/// nCore's categories only distinguish high definition from standard, so this can say
/// no more than that. `hdser`/`hd` covers 720p and 1080p alike, and calling it 720p
/// would be a guess presented as fact, so it stays at the honest coarse label.
fn resolution_from_category(category: &str) -> Option<&'static str> {
    let c = category.to_ascii_lowercase();
    // The second tracker names the resolution outright in its category — `Film/Hun/1080p` —
    // which is worth more than any guess from the words around it.
    for (needle, label) in [
        ("2160p", "UHD (4K)"),
        ("1080p", "Full HD (1080p)"),
        ("720p", "HD (720p)"),
        ("480p", "SD (480p)"),
    ] {
        if c.contains(needle) {
            return Some(label);
        }
    }
    if c.contains("hd") {
        Some("HD")
    } else if c.contains("xvid") || c.contains("dvd") || c.contains("sd") {
        // "sd" is how the second tracker labels its standard-definition categories, and a
        // release from there may carry nothing in its name that says so.
        Some("SD")
    } else {
        None
    }
}

/// The spoken language from the tracker's category, for the same reason as the resolution: an
/// old upload named in Hungarian with no `hun` in the filename is still a Hungarian audio
/// release, and the category is where the tracker says so.
fn language_from_category(category: &str) -> Option<&'static str> {
    let c = category.to_ascii_lowercase();
    if c.contains("hun") || c.contains("magyar") {
        Some("Hun")
    } else if c.contains("eng") {
        Some("Eng")
    } else {
        None
    }
}

/// Binary units with two decimals, as trackers write sizes.
pub fn size_label(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// What a source looks like in the list.
pub struct Listing {
    /// The badge: resolution and picture quality, which is what the eye goes to.
    pub name: String,
    /// Three lines of detail.
    pub description: String,
    /// Identifies the kind of release, so bingeing continues with a matching one.
    pub binge_group: String,
}

/// A key describing the release profile rather than the release.
///
/// Stremio uses this to keep playing "the same sort of thing" through a season: when
/// the viewer picks a source, the next episode auto-plays from the source whose group
/// matches. So the key has to be stable across episodes and distinct between
/// qualities. Keying it by torrent id, as the implementation being replaced does,
/// makes it unique per file and therefore never match the next episode at all, which
/// silently disables the feature it exists for.
fn binge_group(indexer: &str, attrs: &Attributes) -> String {
    let mut parts = vec![indexer.to_ascii_lowercase()];
    for group in [
        Group::Resolution,
        Group::VideoCodec,
        Group::Language,
        Group::VideoQuality,
    ] {
        let labels = attrs.of(group);
        if !labels.is_empty() {
            parts.push(labels.join("+").to_ascii_lowercase().replace(' ', ""));
        }
    }
    parts.join("-")
}

/// Builds the two blocks Stremio shows for one source.
///
/// `already_have` marks a release that is already on this disk, which saves choosing a
/// second copy of something that is downloaded and paid for in seed time.
#[allow(clippy::too_many_arguments)]
pub fn listing(
    indexer: &str,
    release_name: &str,
    category: &str,
    seeders: u64,
    leechers: u64,
    size_bytes: u64,
    already_have: bool,
) -> Listing {
    let attrs = Attributes::parse(release_name, category);

    let mut badge: Vec<String> = Vec::new();
    if already_have {
        badge.push("⭐".to_string());
    }
    badge.extend(attrs.render(Group::Resolution));
    badge.extend(attrs.render(Group::VideoQuality));
    // Both of these belong in front of the click rather than in the detail lines. 3D because
    // starting a side-by-side file on a television that is not set up for it is a bad minute,
    // and the service because the whole point of knowing is to decide not to download at all.
    badge.extend(attrs.render(Group::ThreeD));
    badge.extend(attrs.render(Group::Service));
    // Never leave the badge empty: an unlabelled row cannot be told from its neighbour.
    if badge.is_empty() {
        badge.push(indexer.to_string());
    }

    // Leechers alongside seeders, because the pair says something the seeder count alone
    // does not: four hundred seeders with no leechers is a quiet swarm that will give you
    // everything, while the same number with a hundred leechers is a busy one.
    let first = [
        Some(format!("🧲 {indexer}")),
        Some(match leechers {
            0 => format!("👥 {seeders}"),
            _ => format!("👥 {seeders} / {leechers}"),
        }),
        (size_bytes > 0).then(|| format!("💾 {}", size_label(size_bytes))),
    ];
    let second = [
        attrs.render(Group::Language),
        attrs.render(Group::AudioQuality),
        attrs.render(Group::AudioSpatial),
        attrs.render(Group::AudioChannels),
    ];
    let third = [
        attrs.render(Group::Edition),
        attrs.render(Group::Source),
        attrs.render(Group::VideoCodec),
    ];

    let lines: Vec<String> = [
        join(first.into_iter()),
        join(second.into_iter()),
        join(third.into_iter()),
    ]
    .into_iter()
    .flatten()
    .collect();

    Listing {
        name: badge.join(" | "),
        description: lines.join("\n"),
        binge_group: binge_group(indexer, &attrs),
    }
}

/// Joins the present parts, or None when the line would be empty.
fn join(parts: impl Iterator<Item = Option<String>>) -> Option<String> {
    let present: Vec<String> = parts.flatten().collect();
    if present.is_empty() {
        None
    } else {
        Some(present.join(" | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names on the badge, so a test can say what the viewer sees before clicking.
    fn badge_of(name: &str) -> String {
        listing("nCore", name, "hdser_hun", 10, 0, 1_000_000_000, false).name
    }

    fn details_of(name: &str) -> String {
        listing("nCore", name, "hdser_hun", 10, 0, 1_000_000_000, false).description
    }

    /// Stereoscopic releases, in the shapes the original's pattern covers.
    #[test]
    fn stereoscopic_releases_are_labelled() {
        for name in [
            "Avatar.2009.3D.1080p.BluRay.HUN",
            "Gravity.2013.1080p.HSBS.BluRay.HUN",
            "Titanic.1997.1080p.HOU.BluRay.HUN",
            "Dune.2021.1080p.Half-SBS.BluRay.HUN",
            "Dune.2021.1080p.Half.OU.BluRay.HUN",
        ] {
            assert!(
                badge_of(name).contains("3D"),
                "not labelled as 3D: {name}"
            );
        }
    }

    /// And the things that must not be read as 3D. A flat release with a number in it, or a
    /// group whose name happens to start with the same letters.
    #[test]
    fn flat_releases_are_not_labelled_3d() {
        for name in [
            "Film.2009.1080p.BluRay.x264-D3D",
            "Film.2160p.HDR10.WEB-DL.HUN",
            "Sorozat.S03D01.HUN.DVDRip",
        ] {
            assert!(
                !badge_of(name).contains("3D"),
                "wrongly labelled as 3D: {name}"
            );
        }
    }

    /// The streaming service, which is the whole reason this group exists: seeing it before
    /// clicking is what stops a download of something already paid for.
    #[test]
    fn the_streaming_service_is_on_the_badge() {
        for (name, service) in [
            ("The.Whisper.Man.2026.2160p.NF.WEB-DL.DDP5.1.Atmos.H.264-FULCRUM", "Netflix"),
            ("The.Gabby.Petito.Story.2022.1080p.HBO.WEB-DL.DDP.2.0.H264", "HBO Max"),
            ("MAO.S01E22.1080p.DSNP.WEB-DL.AAC2.0.H.264-Kitsune", "Disney+"),
            ("Backrooms.2026.AMZN.WEBRip.x264.HUN-FULCRUM", "Amazon"),
            ("Film.2025.1080p.ATVP.WEB-DL.HUN", "Apple TV+"),
            ("Film.2025.1080p.PMTP.WEB-DL.HUN", "Paramount+"),
            ("Film.2025.1080p.SKST.WEB-DL.HUN", "SkyShowtime"),
        ] {
            let badge = badge_of(name);
            assert!(badge.contains(service), "{service} missing from {badge:?} for {name}");
        }
    }

    /// A two-letter tag inside a group name is not a service. This is the case the dotted
    /// pattern exists for: `NF` is Netflix between separators and nothing at all inside a word.
    #[test]
    fn a_service_tag_inside_a_word_is_not_a_service() {
        for name in [
            "Film.2026.1080p.WEB-DL.HUN-NFHD",
            "Film.2026.1080p.WEB-DL.HUN.READ.NFO",
            "Sorozat.S01.HUN.WEBRip-AMZNIA",
        ] {
            assert!(
                !badge_of(name).contains("🍿"),
                "wrongly read as a streaming service: {name}"
            );
        }
    }

    /// Editions land in the detail lines, where the original puts them, not on the badge.
    #[test]
    fn editions_are_shown_in_the_details() {
        for (name, label) in [
            ("Dune.2021.IMAX.2160p.WEB-DL.HUN", "IMAX"),
            ("LOTR.2001.Extended.1080p.BluRay.HUN", "Bővített"),
            ("Blade.Runner.1982.Directors.Cut.1080p.HUN", "Rendezői változat"),
            ("Film.1997.REMASTERED.1080p.BluRay.HUN", "Felújított"),
            ("Film.2010.UNRATED.1080p.BluRay.HUN", "Vágatlan"),
            ("Film.2012.Open.Matte.1080p.WEB-DL.HUN", "Open Matte"),
            ("Film.2015.Special.Edition.1080p.BluRay.HUN", "Különkiadás"),
        ] {
            let details = details_of(name);
            assert!(details.contains(label), "{label} missing from {details:?}");
            assert!(
                !badge_of(name).contains(label),
                "{label} belongs in the details, not the badge"
            );
        }
    }

    /// Every pattern has to compile; a typo would otherwise silently match nothing.
    #[test]
    fn every_pattern_compiles() {
        assert_eq!(COMPILED.len(), TAGS.len());
    }

    /// A real nCore release name, taken from a live search.
    #[test]
    fn a_hungarian_web_dl_episode_is_read_correctly() {
        let a = Attributes::parse("Exek.csataja.S02E01.HUN.WEB-DL.1080p.H264-LEGION", "hdser_hun");
        assert_eq!(a.of(Group::Resolution), vec!["Full HD (1080p)"]);
        assert_eq!(a.of(Group::Language), vec!["Hun"]);
        assert_eq!(a.of(Group::Source), vec!["Web-DL"]);
        assert_eq!(a.of(Group::VideoCodec), vec!["x264"]);
        assert!(a.of(Group::VideoQuality).is_empty(), "no HDR claimed");
    }

    /// The 4K release that is on this disk, from the live tracker.
    #[test]
    fn a_uhd_hdr_remux_is_read_correctly() {
        let a = Attributes::parse(
            "The.Hunger.Games.Mockingjay.Part.1.2014.2160p.UHD.HDR.BluRay.TrueHD.7.1.x265.HuN-TRiNiTY",
            "hd_hun",
        );
        assert_eq!(a.of(Group::Resolution), vec!["UHD (4K)"]);
        assert_eq!(a.of(Group::VideoQuality), vec!["HDR10"]);
        assert_eq!(a.of(Group::AudioQuality), vec!["TrueHD"]);
        assert_eq!(a.of(Group::AudioChannels), vec!["7.1"]);
        assert_eq!(a.of(Group::VideoCodec), vec!["x265"]);
        assert_eq!(a.of(Group::Language), vec!["Hun"]);
        assert!(a.of(Group::Source).contains(&"Blu-ray"));
        assert!(a.of(Group::Source).contains(&"UHD"));
    }

    /// The looser pattern must not claim a match the specific one deserves. This is
    /// what the reference does with negative lookahead, which this engine cannot do.
    #[test]
    fn the_more_specific_tag_wins() {
        let plus = Attributes::parse("Film.2160p.HDR10+.BluRay", "hd");
        assert_eq!(
            plus.of(Group::VideoQuality),
            vec!["HDR10+"],
            "HDR10 must not be listed alongside HDR10+"
        );

        let dv = Attributes::parse("Film.2160p.DV.HDR10.BluRay.DTS-HD.MA.5.1", "hd");
        assert!(dv.of(Group::VideoQuality).contains(&"Dolby Vision"));
        assert_eq!(
            dv.of(Group::AudioQuality),
            vec!["DTS-HD MA"],
            "a DTS-HD release must not read as plain DTS as well"
        );

        // Measured on a live listing: this exact release used to show "DTS-HD MA, DTS".
        let live = Attributes::parse(
            "The.Hunger.Games.Mockingjay.Part.1.2014.1080p.BluRay.DTS-HD.MA.5.1.x264.HuN",
            "hd_hun",
        );
        assert_eq!(live.of(Group::AudioQuality), vec!["DTS-HD MA"]);

        // Plain DTS still reports itself.
        let plain = Attributes::parse("Film.1080p.BluRay.DTS.5.1.x264", "hd");
        assert_eq!(plain.of(Group::AudioQuality), vec!["DTS"]);

        let ddp = Attributes::parse("Film.1080p.WEB-DL.DDP5.1.x264", "hd");
        assert_eq!(
            ddp.of(Group::AudioQuality),
            vec!["Dolby Digital Plus"],
            "Dolby Digital must not be listed as well"
        );
    }

    /// A re-encode of a stream is not the stream, and labelling it as both overstates
    /// what it is.
    #[test]
    fn a_webrip_is_not_also_reported_as_a_web_dl() {
        let rip = Attributes::parse("Film.2014.1080p.WEBRip.x264", "hd");
        assert_eq!(rip.of(Group::Source), vec!["WebRip"]);

        let dl = Attributes::parse("Film.2014.1080p.WEB-DL.x264", "hd");
        assert_eq!(dl.of(Group::Source), vec!["Web-DL"]);
    }

    /// A wrong label is worse than a missing one: the reference pattern accepts a bare
    /// `ts` and `md`, which appear inside ordinary release names.
    #[test]
    fn an_ordinary_release_is_never_labelled_a_cam() {
        for name in [
            "Film.2014.1080p.BluRay.TrueHD.7.1.x264-GROUP",
            "Sorozat.S01E01.HUN.HDTV.x264",
            "Film.2014.MULTi.1080p.BluRay.REMUX.AVC.DTS-HD.MA.5.1",
        ] {
            let a = Attributes::parse(name, "hd_hun");
            assert!(
                !a.of(Group::Source).contains(&"Cam"),
                "{name} must not read as a cam release"
            );
        }
        // A genuine cam still does.
        let cam = Attributes::parse("Film.2014.HDCAM.x264", "hd");
        assert!(cam.of(Group::Source).contains(&"Cam"));
    }

    /// A release has one resolution, so a name mentioning two must not show both.
    #[test]
    fn a_single_valued_group_keeps_only_its_best_match() {
        let a = Attributes::parse("Film.1080p.upscaled.from.720p.BluRay", "hd");
        assert_eq!(a.of(Group::Resolution), vec!["Full HD (1080p)"]);
        assert_eq!(a.of(Group::AudioChannels).len(), 0);
    }

    /// nCore's own buckets can only say high or standard definition, and saying more
    /// than that would be a guess dressed up as a fact.
    #[test]
    fn the_category_is_a_coarse_fallback_only() {
        // The name says nothing about resolution.
        let hd = Attributes::parse("Exek.csataja.S02E01.HUN.WEB-DL.H264-LEGION", "hdser_hun");
        assert_eq!(hd.of(Group::Resolution), vec!["HD"]);

        let sd = Attributes::parse("Valami.HUN.DVDRip", "xvidser_hun");
        assert_eq!(sd.of(Group::Resolution), vec!["SD"]);

        // And it never overrides what the name does say.
        let named = Attributes::parse("Film.1080p", "xvidser_hun");
        assert_eq!(named.of(Group::Resolution), vec!["Full HD (1080p)"]);

        assert_eq!(resolution_from_category("unknown_bucket"), None);
    }

    /// The whole point of the layout: resolution and seeders visible at a glance.
    #[test]
    fn the_listing_leads_with_resolution_and_shows_seeders() {
        let l = listing(
            "nCore",
            "Exek.csataja.S02E01.HUN.WEB-DL.1080p.H264-LEGION",
            "hdser_hun",
            420,
            11,
            1_889_668_652,
            false,
        );
        assert_eq!(l.name, "📺 Full HD (1080p)");

        let lines: Vec<&str> = l.description.lines().collect();
        assert_eq!(lines.len(), 3);
        // Seeders and leechers together: the pair says more than the seeder count alone.
        assert_eq!(lines[0], "🧲 nCore | 👥 420 / 11 | 💾 1.76 GiB");
        assert_eq!(lines[1], "🌐 Hun");
        assert_eq!(lines[2], "💿 Web-DL | 🎞️ x264");
    }

    #[test]
    fn a_uhd_listing_reads_like_the_reference() {
        let l = listing(
            "nCore",
            "The.Hunger.Games.Mockingjay.Part.1.2014.2160p.UHD.HDR.BluRay.TrueHD.7.1.x265.HuN-TRiNiTY",
            "hd_hun",
            47,
            0,
            27_798_000_000,
            false,
        );
        assert_eq!(l.name, "📺 UHD (4K) | ✨ HDR10");
        let lines: Vec<&str> = l.description.lines().collect();
        assert_eq!(lines[0], "🧲 nCore | 👥 47 | 💾 25.89 GiB");
        assert_eq!(lines[1], "🌐 Hun | 🔊 TrueHD | 📻 7.1");
        assert!(lines[2].starts_with("💿 "));
        assert!(lines[2].contains("🎞️ x265"));
    }

    /// Something already downloaded is worth marking: choosing it again costs nothing,
    /// choosing a different copy costs a second download and more seed time.
    #[test]
    fn a_release_already_on_disk_is_starred() {
        let l = listing("nCore", "Film.2014.1080p.BluRay", "hd", 10, 0, 1024, true);
        assert!(l.name.starts_with("⭐ | 📺 Full HD (1080p)"));
    }

    /// A name nothing matches must still produce a distinguishable row.
    #[test]
    fn an_unrecognisable_name_still_gets_a_badge() {
        let l = listing("nCore", "kissvideo", "weird", 3, 0, 0, false);
        assert_eq!(l.name, "nCore");
        // No size line when the tracker did not say.
        assert_eq!(l.description, "🧲 nCore | 👥 3");
    }

    /// Zero seeders is information, not an absence of it, so it is still shown.
    #[test]
    fn zero_seeders_is_still_printed() {
        let l = listing("nCore", "Film.1080p", "hd", 0, 0, 100, false);
        assert!(l.description.contains("👥 0"));
    }

    /// The point of a binge group: the same quality of the next episode has to land in
    /// the same group, and a different quality must not.
    #[test]
    fn the_binge_group_survives_the_next_episode_but_separates_qualities() {
        let e1 = listing(
            "nCore",
            "Exek.csataja.S02E01.HUN.WEB-DL.1080p.H264-LEGION",
            "hdser_hun",
            1, 0,
            1,
            false,
        );
        let e2 = listing(
            "nCore",
            "Exek.csataja.S02E02.HUN.WEB-DL.1080p.H264-LEGION",
            "hdser_hun",
            1, 0,
            1,
            false,
        );
        assert_eq!(
            e1.binge_group, e2.binge_group,
            "the next episode at the same quality must continue the run"
        );

        let e1_720 = listing(
            "nCore",
            "Exek.csataja.S02E01.HUN.WEB-DL.720p.H264-LEGION",
            "hdser_hun",
            1, 0,
            1,
            false,
        );
        assert_ne!(
            e1.binge_group, e1_720.binge_group,
            "a different resolution is a different choice"
        );
        assert_eq!(e1.binge_group, "ncore-fullhd(1080p)-x264-hun");
    }

    #[test]
    fn sizes_match_the_trackers_own_formatting() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(1024), "1.00 KiB");
        assert_eq!(size_label(506_166_968), "482.72 MiB");
        assert_eq!(size_label(1_889_668_652), "1.76 GiB");
        assert_eq!(size_label(27_798_000_000), "25.89 GiB");
    }

    /// An empty group leaves no stray marker or separator behind.
    #[test]
    fn empty_groups_leave_no_dangling_separators() {
        let l = listing("nCore", "Film.1080p", "hd", 5, 0, 1024, false);
        assert!(!l.description.contains("| |"));
        assert!(!l.description.ends_with('|'));
        for line in l.description.lines() {
            assert!(!line.is_empty(), "no blank line may be emitted");
            assert!(!line.starts_with('|'));
        }
    }
}
