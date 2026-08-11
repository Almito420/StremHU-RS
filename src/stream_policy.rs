//! Deciding which pieces the engine must fetch next, and by when.
//!
//! This mirrors the strategy of the implementation being replaced, because that one
//! is proven on this tracker and the numbers in it are not arbitrary:
//!
//!   * the piece under the read head gets deadline 0, meaning "now"
//!   * every other piece gets `2000 + distance * 1000` milliseconds
//!   * only pieces that are actually missing are considered, so the window is never
//!     spent on data already on disk
//!   * the file's first and last piece get deadline 0 while missing, because a player
//!     reads the container header and the seek index before it will play anything
//!   * when fewer missing pieces are found between the head and the end of the file
//!     than the window allows, the search wraps to the start of the file, with the
//!     distance continuing past the end. That is what eventually completes a whole
//!     film while still always favouring the playhead.
//!
//! Deadlines expire, so the caller re-applies this plan continuously, and clears the
//! deadlines of pieces that have dropped out of the window: a stale deadline from an
//! earlier read position keeps competing for bandwidth nobody needs.

use std::collections::{BTreeMap, BTreeSet};

/// Tunables, all supplied by the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// How many missing pieces are given deadlines per reader.
    pub prefetch_pieces: u32,
    /// Deadline for the piece under the read head. Zero means "as soon as possible".
    pub head_deadline_ms: u32,
    /// Base added to every other piece's deadline.
    pub base_deadline_ms: u32,
    /// Added per piece of distance from the head.
    pub deadline_step_ms: u32,
    /// Pin the file's first and last piece while they are missing.
    pub pin_file_edges: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            prefetch_pieces: 8,
            head_deadline_ms: 0,
            base_deadline_ms: 2000,
            deadline_step_ms: 1000,
            pin_file_edges: true,
        }
    }
}

/// How many pieces a readahead of `readahead_bytes` comes to on this torrent.
///
/// The window is a quantity of video, not a count of pieces, and this is the correction of a
/// real fault. It used to be four pieces whatever the torrent, which measured on the actual
/// files here means:
///
/// | release                   | piece size | four pieces |
/// |---------------------------|-----------:|------------:|
/// | 1080p episode, 1.84 GiB   |   0.5 MiB  |      2 MiB  |
/// | 4K remux, 16.95 GiB       |    16 MiB  |     64 MiB  |
/// | 4K remux, 26.04 GiB       |     2 MiB  |      8 MiB  |
///
/// A 4K remux runs at roughly 8 MB a second, so on that last one the readahead was under a
/// second of video: it started, played what was buffered, and stopped. Counted in bytes the
/// same window is the same amount of film on every torrent, which is what a viewer
/// experiences.
///
/// At least four pieces, so a torrent with enormous pieces still gets a run at the file
/// rather than a single piece at a time.
pub fn prefetch_for_piece_size(piece_size: u64, readahead_bytes: u64) -> u32 {
    let piece_size = piece_size.max(1);
    let pieces = readahead_bytes.div_ceil(piece_size);
    pieces.clamp(4, u32::MAX as u64) as u32
}

/// What a torrent should be asked for at the moment a file is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// Every file in the torrent, at this priority. For a tracker that judges a download
    /// by the whole torrent rather than by what was taken from it.
    Everything(u8),
    /// Nothing in advance. Only a deadline raises a piece, so only what is played arrives.
    OnlyWhatIsPlayed,
    /// This file whole, every other file switched off.
    OnlyThisFile,
    /// This file as well, leaving alone whatever the torrent is already serving.
    AlsoThisFile,
}

/// The order the three settings resolve in when a file is opened.
///
/// `full` is the tracker's demand and outranks everything: if the whole torrent has to be
/// downloaded then no saving is allowed to interfere. `partial` comes next, because "fetch
/// only what is played" is a statement about pieces and applies whether or not a sibling
/// episode is open — the sibling's own deadlines keep its window alive. Only when neither is
/// set does it matter whether this is the first file out of the torrent.
pub fn wanted_at_open(full: bool, partial: bool, siblings: usize, idle_priority: u8) -> Wanted {
    if full {
        // Zero would mean the torrent never finishes, which is the opposite of the request.
        Wanted::Everything(idle_priority.max(1))
    } else if partial {
        Wanted::OnlyWhatIsPlayed
    } else if siblings == 0 {
        Wanted::OnlyThisFile
    } else {
        Wanted::AlsoThisFile
    }
}

/// Which other files of the torrent are worth picking up once the wanted one is on disk.
///
/// `sizes` is every file's length, `skip` the files already being served, and `limit` the
/// largest a file may be to come along. Returns the indices to switch on.
///
/// The decision is per file, and the threshold is the whole of the policy. A film's torrent
/// carries a sample and an nfo beside the film: without them the torrent can never be a
/// complete seed, and the tracker shows 98.94% for ever, so at ninety-odd megabytes they are
/// worth having. A season pack carries nine more episodes: those are what the one-file rule
/// exists to avoid, and they are far above any sensible threshold, so a pack ends up with the
/// episode being watched plus its nfo and whatever else is small, and nothing more.
///
/// `skip` is not only the file being served. Another episode of the same pack may be open in
/// its own right, and switching that one to a low priority would slow down somebody's playback
/// or, worse, look like a file that can be discarded.
pub fn extras_worth_completing(sizes: &[u64], skip: &[usize], limit: u64) -> Vec<usize> {
    if limit == 0 {
        return Vec::new();
    }
    // The largest file we are actually serving, as the yardstick.
    let wanted = skip.iter().filter_map(|i| sizes.get(*i)).copied().max().unwrap_or(0);
    (0..sizes.len())
        .filter(|i| {
            if skip.contains(i) {
                return false;
            }
            let size = sizes[*i];
            // Small in absolute terms, and small next to what we came for.
            //
            // The second test is what makes this safe on a pack of small files. Measured on a
            // real one: an XviD rip of a whole series, every episode 346 MB, so every episode
            // fell under a threshold meant for samples and nfos and the server fetched all
            // twenty-two of them, 7.44 GiB, to serve one. A sample is a percent or two of the
            // film it belongs to; a sibling episode is the same size as the one being watched.
            size <= limit && size.saturating_mul(4) <= wanted
        })
        .collect()
}

/// A file's piece span, inclusive on both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSpan {
    pub first_piece: u32,
    pub last_piece: u32,
}

impl FileSpan {
    pub fn from_offsets(base_offset: u64, file_len: u64, piece_size: u64) -> Self {
        let piece_size = piece_size.max(1);
        let first = base_offset / piece_size;
        let last_byte = base_offset + file_len.saturating_sub(1);
        Self {
            first_piece: first as u32,
            last_piece: (last_byte / piece_size) as u32,
        }
    }

}

/// Global piece index for a byte offset inside a file.
pub fn piece_of(base_offset: u64, offset_in_file: u64, piece_size: u64) -> u32 {
    ((base_offset + offset_in_file) / piece_size.max(1)) as u32
}

/// One active reader: where a player currently is within a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadHead {
    pub span: FileSpan,
    pub piece: u32,
}

/// What to tell the engine after read positions moved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Piece to deadline in milliseconds from now, sorted by piece index.
    pub set: BTreeMap<u32, u32>,
    /// Deadlines to clear because they are no longer wanted.
    pub reset: Vec<u32>,
}

impl Plan {
}

/// True when the piece is already downloaded.
fn have(bitmap: &[u8], piece: u32) -> bool {
    bitmap.get(piece as usize).copied().unwrap_or(0) == 1
}

/// Builds the deadline plan for every active reader on one torrent.
///
/// `have_bitmap` is one byte per piece, 1 for complete. `active` is what currently
/// carries a deadline, so the plan can clear whatever fell out of the window.
pub fn plan(
    policy: &Policy,
    heads: &[ReadHead],
    have_bitmap: &[u8],
    active: &BTreeSet<u32>,
) -> Plan {
    let mut wanted: BTreeMap<u32, u32> = BTreeMap::new();
    let window = policy.prefetch_pieces.max(1);

    for head in heads {
        let span = head.span;
        // Clamp into the file: a range request at the very end must not aim past it.
        let start = head.piece.clamp(span.first_piece, span.last_piece);
        let mut collected: Vec<(u32, u32)> = Vec::new();

        // Forward from the head to the end of the file.
        let mut piece = start;
        while piece <= span.last_piece && (collected.len() as u32) < window {
            if !have(have_bitmap, piece) {
                collected.push((piece, piece - start));
            }
            match piece.checked_add(1) {
                Some(next) => piece = next,
                None => break,
            }
        }

        // Still room? Wrap to the start of the file, continuing the distance past the
        // end, so the rest of the file is fetched without ever outranking the head.
        if (collected.len() as u32) < window {
            let distance_to_end = (span.last_piece - start) + 1;
            let mut piece = span.first_piece;
            while piece < start && (collected.len() as u32) < window {
                if !have(have_bitmap, piece) {
                    collected.push((piece, distance_to_end + (piece - span.first_piece)));
                }
                piece += 1;
            }
        }

        for (piece, distance) in collected {
            let deadline = if piece == start {
                policy.head_deadline_ms
            } else {
                policy
                    .base_deadline_ms
                    .saturating_add(policy.deadline_step_ms.saturating_mul(distance))
            };
            // With several readers the nearer one wins, so a second stream cannot push
            // back a piece the first one needs sooner.
            wanted
                .entry(piece)
                .and_modify(|d| *d = (*d).min(deadline))
                .or_insert(deadline);
        }

        if policy.pin_file_edges {
            // A player probes the container header and the seek index before playing,
            // so both ends are wanted immediately while they are missing.
            for edge in [span.first_piece, span.last_piece] {
                if !have(have_bitmap, edge) {
                    wanted.entry(edge).and_modify(|d| *d = 0).or_insert(0);
                }
            }
        }
    }

    let reset = active
        .iter()
        .copied()
        .filter(|p| !wanted.contains_key(p))
        .collect();

    Plan { set: wanted, reset }
}

/// Connection limit to apply: streaming wants more peers than idle seeding.
pub fn max_connections(streaming: bool, while_streaming: u32, while_idle: u32) -> u32 {
    if streaming { while_streaming } else { while_idle }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The precedence of the three settings, which is the whole of this decision.
    #[test]
    fn the_trackers_demand_outranks_every_saving() {
        // Full download on: nothing else may reduce what is fetched, partial download included.
        assert_eq!(wanted_at_open(true, true, 0, 4), Wanted::Everything(4));
        assert_eq!(wanted_at_open(true, false, 3, 4), Wanted::Everything(4));
        // Priority zero would mean the torrent never finishes, which is not what "download the
        // whole thing" can be allowed to mean.
        assert_eq!(wanted_at_open(true, false, 0, 0), Wanted::Everything(1));
    }

    #[test]
    fn partial_download_applies_whether_or_not_a_sibling_is_open() {
        assert_eq!(wanted_at_open(false, true, 0, 4), Wanted::OnlyWhatIsPlayed);
        // A second episode of the same pack does not change it: that episode's own deadlines
        // keep its window wanted, and nothing else is supposed to be.
        assert_eq!(wanted_at_open(false, true, 2, 4), Wanted::OnlyWhatIsPlayed);
    }

    /// The ordinary case, unchanged: the first file out of a torrent switches the rest off, a
    /// later one is added without disturbing what is already being served.
    #[test]
    fn without_either_setting_it_depends_on_the_siblings() {
        assert_eq!(wanted_at_open(false, false, 0, 4), Wanted::OnlyThisFile);
        assert_eq!(wanted_at_open(false, false, 1, 4), Wanted::AlsoThisFile);
    }

    /// The case this was written for, measured from the real torrent: a film, a sample and an
    /// nfo, where leaving the last two out is what kept the tracker reporting 98.94%.
    #[test]
    fn a_films_sample_and_nfo_are_worth_completing() {
        let sizes = [
            99_000_000,        // Sample/sample.mkv
            8_962_000_000,     // the film
            3_500,             // the nfo
        ];
        let extras = extras_worth_completing(&sizes, &[1], 512 * 1024 * 1024);
        assert_eq!(extras, vec![0, 2]);
    }

    /// A season pack: the episode being watched plus the small companion files, and not one
    /// byte of the other episodes.
    #[test]
    fn a_season_pack_gets_its_nfo_and_none_of_the_other_episodes() {
        let mut sizes = vec![1_970_000_000u64; 10];
        sizes.push(17_595); // the nfo
        let extras = extras_worth_completing(&sizes, &[3], 512 * 1024 * 1024);
        assert_eq!(extras, vec![10], "only the nfo");
    }

    /// The case that cost 7.44 GiB: an XviD rip of a whole series, every episode 346 MB. Each
    /// one fits under a threshold meant for samples, so the absolute limit alone pulled the
    /// entire series down to serve one episode.
    #[test]
    fn a_pack_of_small_episodes_is_not_mistaken_for_companion_files() {
        let mut sizes = vec![346 * 1024 * 1024u64; 22];
        sizes.push(2_400); // the nfo
        let extras = extras_worth_completing(&sizes, &[5], 512 * 1024 * 1024);
        assert_eq!(extras, vec![22], "the nfo, and none of the twenty-one other episodes");

        // The film case still works: a sample is a fraction of what it accompanies.
        let film = [99_000_000u64, 8_962_000_000, 3_500];
        assert_eq!(extras_worth_completing(&film, &[1], 512 * 1024 * 1024), vec![0, 2]);
    }

    /// A second episode already being served must not be touched: dropping it to a low
    /// priority would slow down a playback in progress.
    #[test]
    fn files_already_being_served_are_left_alone() {
        let sizes = [300_000_000u64, 300_000_000, 1_000];
        let extras = extras_worth_completing(&sizes, &[0, 1], 512 * 1024 * 1024);
        assert_eq!(extras, vec![2]);
    }

    #[test]
    fn completing_extras_can_be_switched_off_and_needs_something_to_do() {
        let sizes = [99_000_000u64, 8_962_000_000];
        assert!(extras_worth_completing(&sizes, &[1], 0).is_empty(), "zero means off");
        assert!(
            extras_worth_completing(&[8_962_000_000], &[0], u64::MAX).is_empty(),
            "a single-file torrent has no leftovers"
        );
        // Exactly on the limit is still worth it; a byte over is not.
        assert_eq!(extras_worth_completing(&sizes, &[1], 99_000_000), vec![0]);
        assert!(extras_worth_completing(&sizes, &[1], 98_999_999).is_empty());
    }


    fn span(first: u32, last: u32) -> FileSpan {
        FileSpan {
            first_piece: first,
            last_piece: last,
        }
    }

    fn policy(prefetch: u32) -> Policy {
        Policy {
            prefetch_pieces: prefetch,
            head_deadline_ms: 0,
            base_deadline_ms: 2000,
            deadline_step_ms: 1000,
            pin_file_edges: false,
        }
    }

    /// Nothing downloaded yet: the head is due now, the rest scale with distance.
    #[test]
    fn the_head_is_due_immediately_and_the_rest_scale() {
        let have = vec![0u8; 100];
        let heads = [ReadHead {
            span: span(0, 99),
            piece: 10,
        }];
        let p = plan(&policy(4), &heads, &have, &BTreeSet::new());

        assert_eq!(p.set[&10], 0, "the head must be fetched now");
        assert_eq!(p.set[&11], 3000);
        assert_eq!(p.set[&12], 4000);
        assert_eq!(p.set[&13], 5000);
        assert_eq!(p.set.len(), 4);
    }

    /// The window must not be spent on data already on disk, which is what a fixed
    /// window over all pieces would do.
    #[test]
    fn pieces_already_held_are_skipped() {
        let mut have = vec![0u8; 100];
        for i in 10..20 {
            have[i] = 1;
        }
        let heads = [ReadHead {
            span: span(0, 99),
            piece: 10,
        }];
        let p = plan(&policy(3), &heads, &have, &BTreeSet::new());

        // 10..19 are present, so the window starts at the first gap.
        assert_eq!(p.set.keys().copied().collect::<Vec<_>>(), vec![20, 21, 22]);
        // Distance is measured from the head, not from the first missing piece.
        assert_eq!(p.set[&20], 2000 + 10 * 1000);
    }

    #[test]
    fn a_fully_downloaded_file_needs_no_deadlines() {
        let have = vec![1u8; 50];
        let heads = [ReadHead {
            span: span(0, 49),
            piece: 0,
        }];
        let p = plan(&policy(8), &heads, &have, &BTreeSet::new());
        assert!(p.set.is_empty());
    }

    /// Once the tail is queued, the window wraps to the front so the whole file is
    /// eventually fetched. This is what completes a film the viewer started midway.
    #[test]
    fn the_window_wraps_to_the_start_of_the_file() {
        let have = vec![0u8; 10];
        let heads = [ReadHead {
            span: span(0, 9),
            piece: 8,
        }];
        let p = plan(&policy(6), &heads, &have, &BTreeSet::new());

        // Forward: 8, 9. Then wrapped: 0, 1, 2, 3.
        assert_eq!(
            p.set.keys().copied().collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 8, 9]
        );
        assert_eq!(p.set[&8], 0, "the head is still first");
        assert_eq!(p.set[&9], 3000);
        // distance_to_end = (9 - 8) + 1 = 2, so piece 0 sits at distance 2.
        assert_eq!(p.set[&0], 2000 + 2 * 1000);
        assert_eq!(p.set[&1], 2000 + 3 * 1000);
        assert!(p.set[&0] > p.set[&9], "wrapped pieces never outrank the tail");
    }

    #[test]
    fn the_window_never_reaches_outside_the_file() {
        let have = vec![0u8; 200];
        let heads = [ReadHead {
            span: span(50, 55),
            piece: 54,
        }];
        let p = plan(&policy(20), &heads, &have, &BTreeSet::new());
        // Only this file's pieces: the others may belong to a deselected file.
        assert!(p.set.keys().all(|piece| (50..=55).contains(piece)));
    }

    #[test]
    fn file_edges_are_pinned_while_missing() {
        let have = vec![0u8; 100];
        let p = Policy {
            pin_file_edges: true,
            ..policy(2)
        };
        let heads = [ReadHead {
            span: span(0, 99),
            piece: 50,
        }];
        let out = plan(&p, &heads, &have, &BTreeSet::new());

        assert_eq!(out.set[&0], 0, "the container header is needed to start");
        assert_eq!(out.set[&99], 0, "so is the seek index at the end");
        assert_eq!(out.set[&50], 0, "and the head itself");
    }

    #[test]
    fn a_held_edge_is_not_pinned_again() {
        let mut have = vec![0u8; 100];
        have[0] = 1;
        have[99] = 1;
        let p = Policy {
            pin_file_edges: true,
            ..policy(2)
        };
        let heads = [ReadHead {
            span: span(0, 99),
            piece: 50,
        }];
        let out = plan(&p, &heads, &have, &BTreeSet::new());
        assert!(!out.set.contains_key(&0));
        assert!(!out.set.contains_key(&99));
    }

    #[test]
    fn stale_deadlines_are_cleared_when_the_head_moves() {
        let have = vec![0u8; 100];
        let active: BTreeSet<u32> = [10, 11, 12].into_iter().collect();
        let heads = [ReadHead {
            span: span(0, 99),
            piece: 50,
        }];
        let p = plan(&policy(2), &heads, &have, &active);

        assert_eq!(p.set.keys().copied().collect::<Vec<_>>(), vec![50, 51]);
        assert_eq!(p.reset, vec![10, 11, 12]);
    }

    #[test]
    fn no_readers_means_clear_everything() {
        let have = vec![0u8; 20];
        let active: BTreeSet<u32> = [1, 2, 3].into_iter().collect();
        let p = plan(&policy(4), &[], &have, &active);
        assert!(p.set.is_empty());
        assert_eq!(p.reset, vec![1, 2, 3]);
    }

    #[test]
    fn two_readers_merge_and_the_nearer_deadline_wins() {
        let have = vec![0u8; 100];
        let heads = [
            ReadHead {
                span: span(0, 99),
                piece: 10,
            },
            ReadHead {
                span: span(0, 99),
                piece: 12,
            },
        ];
        let p = plan(&policy(3), &heads, &have, &BTreeSet::new());
        // Piece 12 is one reader's head, so it must be due now rather than carrying
        // the other reader's distance-based value.
        assert_eq!(p.set[&12], 0);
        assert_eq!(p.set[&10], 0);
    }

    /// The window is the same amount of film on every torrent, whatever its piece size.
    /// Measured against the real releases on this disk, because the fault this replaces was
    /// invisible until a 4K file with small pieces stalled.
    #[test]
    fn the_window_is_a_quantity_of_video_not_a_piece_count() {
        const MB: u64 = 1024 * 1024;
        let readahead = 64 * MB;

        // The 4K remux that stalled: 2 MiB pieces, so 64 MB is 32 of them. The old formula
        // gave 4 pieces, which was 8 MB, under a second of 4K video.
        assert_eq!(prefetch_for_piece_size(2 * MB, readahead), 32);
        // The other 4K release here has 16 MiB pieces: the same 64 MB, four pieces.
        assert_eq!(prefetch_for_piece_size(16 * MB, readahead), 4);
        // The 1080p episode has half-megabyte pieces: still 64 MB.
        assert_eq!(prefetch_for_piece_size(MB / 2, readahead), 128);

        // Every one of those is the same quantity of video, which is the whole point.
        for piece in [MB / 2, 2 * MB, 4 * MB, 16 * MB] {
            let bytes = prefetch_for_piece_size(piece, readahead) as u64 * piece;
            assert!(
                bytes >= readahead,
                "a {piece} byte piece size gave only {bytes} bytes of readahead"
            );
        }
    }

    #[test]
    fn the_window_never_collapses_to_nothing() {
        // Enormous pieces still get a run at the file rather than one piece at a time.
        assert_eq!(prefetch_for_piece_size(u64::MAX, 1024), 4);
        // A nonsense piece size must not panic or divide by zero.
        assert!(prefetch_for_piece_size(0, 64 * 1024 * 1024) >= 4);
        assert!(prefetch_for_piece_size(1024, 0) >= 4);
    }

    #[test]
    fn file_span_from_offsets() {
        assert_eq!(
            FileSpan::from_offsets(0, 4 * 2_097_152, 2_097_152),
            span(0, 3)
        );
        // A file starting mid-piece shares its first piece with the previous file.
        assert_eq!(
            FileSpan::from_offsets(3_000_000, 2_097_152, 2_097_152),
            span(1, 2)
        );
        assert_eq!(FileSpan::from_offsets(2_097_152, 0, 2_097_152), span(1, 1));
    }

    #[test]
    fn piece_of_uses_the_global_offset() {
        assert_eq!(piece_of(0, 0, 1000), 0);
        assert_eq!(piece_of(0, 999, 1000), 0);
        assert_eq!(piece_of(0, 1000, 1000), 1);
        assert_eq!(piece_of(25_000, 600, 1000), 25);
    }

    #[test]
    fn deadline_arithmetic_cannot_overflow() {
        let have = vec![0u8; 8];
        let p = Policy {
            prefetch_pieces: u32::MAX,
            head_deadline_ms: 0,
            base_deadline_ms: u32::MAX - 1,
            deadline_step_ms: u32::MAX,
            pin_file_edges: true,
        };
        let heads = [ReadHead {
            span: span(0, 7),
            piece: 0,
        }];
        let out = plan(&p, &heads, &have, &BTreeSet::new());
        assert_eq!(out.set.len(), 8);
    }

    #[test]
    fn connection_limit_follows_the_streaming_state() {
        assert_eq!(max_connections(true, 50, 20), 50);
        assert_eq!(max_connections(false, 50, 20), 20);
    }
}
