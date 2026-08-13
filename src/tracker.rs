//! Which tracker a torrent came from.
//!
//! There are two, and they are not interchangeable. Both identify their torrents by a small
//! integer, so `12345` means one release on nCore and a different one on BitHUmen; both keep
//! their own hit-and-run list, so an obligation read from one says nothing about the other; and
//! a `.torrent` can only be fetched with the session that belongs to its own site.
//!
//! Everything that leaves the process therefore carries the tracker with it: the play URL, the
//! record in the state file, the row in the interface. The one place it is allowed to be absent
//! is a record written before this existed, and that one is nCore, because until now there was
//! nothing else.

/// The trackers this server can search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracker {
    Ncore,
    Bithumen,
}

impl Tracker {
    /// How it is written in the state file and in the configuration.
    pub fn id(self) -> &'static str {
        match self {
            Tracker::Ncore => "ncore",
            Tracker::Bithumen => "bithumen",
        }
    }

    /// How it is written for a person: their own spelling of their own name.
    pub fn label(self) -> &'static str {
        match self {
            Tracker::Ncore => "nCore",
            Tracker::Bithumen => "BitHUmen",
        }
    }

    /// An empty or unknown name is nCore.
    ///
    /// Records written before there was a second tracker have no name in them, and they are
    /// all nCore downloads. Reading them as anything else would take their obligations off the
    /// only list that knows about them.
    pub fn from_id(id: &str) -> Tracker {
        match id.trim() {
            "bithumen" => Tracker::Bithumen,
            _ => Tracker::Ncore,
        }
    }

    /// The prefix a torrent id carries in a play URL.
    ///
    /// nCore ids are left bare so URLs that already exist go on working: a bookmark, a stream
    /// list still open in Stremio, a record in the state file. Only the new tracker needs
    /// marking.
    pub fn prefix(self) -> &'static str {
        match self {
            Tracker::Ncore => "",
            Tracker::Bithumen => "bh:",
        }
    }

    /// The id as it appears in a play URL.
    pub fn play_id(self, torrent_id: &str) -> String {
        format!("{}{torrent_id}", self.prefix())
    }

    /// The other direction: which tracker, and its own id.
    pub fn from_play_id(play_id: &str) -> (Tracker, &str) {
        match play_id.strip_prefix(Tracker::Bithumen.prefix()) {
            Some(rest) => (Tracker::Bithumen, rest),
            None => (Tracker::Ncore, play_id),
        }
    }

    /// How an obligation is keyed, so one tracker's list can never answer for the other's
    /// torrent. Both number their torrents from one, and `12345` exists on both.
    pub fn owed_key(self, torrent_id: &str) -> String {
        format!("{}:{torrent_id}", self.id())
    }
}

/// A torrent as a tracker's search results describe it.
///
/// One shape for both trackers, because everything downstream of the search — the ranking, the
/// stream list, the disk decision, the play URL — asks the same questions of a hit whichever
/// site it came from. What differs is only how it was read out of the answer.
#[derive(Debug, Clone)]
pub struct Torrent {
    /// Where it was found, and therefore which session can fetch it and which list its
    /// obligation is on.
    pub tracker: Tracker,
    pub torrent_id: String,
    pub seeders: u64,
    pub leechers: u64,
    /// Total size in bytes, as the tracker reports it. Zero when it did not say.
    /// Worth having before anything is downloaded: it is what tells a 4K remux from a
    /// re-encode of the same resolution.
    pub size_bytes: u64,
    /// None when the tracker offered no download link. Such an entry is still worth showing,
    /// just not streamable.
    pub download_url: Option<String>,
    /// The tracker's own category, used only when the release name does not say what the
    /// release is.
    pub category: String,
    pub imdb_id: Option<String>,
    pub title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record with no tracker in it predates the second tracker, so it is an nCore download.
    /// Reading it as BitHUmen would look for its obligation on a list that has never heard of
    /// it, and an obligation nobody claims is one nothing protects.
    #[test]
    fn an_unnamed_tracker_is_ncore() {
        assert_eq!(Tracker::from_id(""), Tracker::Ncore);
        assert_eq!(Tracker::from_id("ncore"), Tracker::Ncore);
        assert_eq!(Tracker::from_id("  bithumen "), Tracker::Bithumen);
        // Not a tracker we know. The cautious reading again, and it is also the only one that
        // can be acted on.
        assert_eq!(Tracker::from_id("filelist"), Tracker::Ncore);
    }

    /// nCore play URLs keep their bare id, so anything already pointing at one still works.
    #[test]
    fn play_ids_survive_the_round_trip() {
        assert_eq!(Tracker::Ncore.play_id("4207293"), "4207293");
        assert_eq!(Tracker::Bithumen.play_id("98765"), "bh:98765");

        assert_eq!(Tracker::from_play_id("4207293"), (Tracker::Ncore, "4207293"));
        assert_eq!(Tracker::from_play_id("bh:98765"), (Tracker::Bithumen, "98765"));
    }

    /// The same number on two trackers is two different releases, and an obligation from one
    /// must never be matched against the other.
    #[test]
    fn the_same_number_on_two_trackers_is_two_keys() {
        assert_ne!(
            Tracker::Ncore.owed_key("12345"),
            Tracker::Bithumen.owed_key("12345")
        );
        assert_eq!(Tracker::Ncore.owed_key("12345"), "ncore:12345");
    }
}
