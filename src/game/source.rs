//! Where a run takes the account's matches and what happened in them.

pub(crate) mod api;

use crate::error::Result;
use crate::game::events::MatchEvents;
use crate::output::MatchStatus;
use crate::timeline::align::HistoryMatch;
use crate::timeline::place::Pause;

/// A match this short is a remake: nothing happened worth a request.
const MIN_MATCH_S: u32 = 180;
/// The standard mode; others, such as Street Brawl, restart the clock every round.
const STANDARD_MODE: u32 = 1;

pub(crate) trait MatchSource {
    /// The account's matches, as align needs them.
    fn history(&mut self) -> Result<Vec<HistoryMatch>>;
    /// The match's timing and events, or why there are none. Call only for a match
    /// [`skip_status`] returns `None` for; an adapter may assume it.
    fn fetch(&mut self, h: &HistoryMatch) -> Fetched;
}

#[derive(Debug)]
pub(crate) enum Fetched {
    Ready {
        timing: MatchTiming,
        events: MatchEvents,
    },
    Status(MatchStatus),
}

#[derive(Debug)]
pub(crate) struct MatchTiming {
    pub duration_s: f64,
    pub pauses: Vec<Pause>,
}

/// Why `h` is not worth a request, or `None` for a full match of the standard mode.
pub(crate) fn skip_status(h: &HistoryMatch) -> Option<MatchStatus> {
    if h.duration_s < MIN_MATCH_S {
        Some(MatchStatus::Remake)
    } else if h.game_mode != Some(STANDARD_MODE) {
        Some(MatchStatus::UnsupportedMode {
            game_mode: h.game_mode,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(id: u64, duration_s: u32, game_mode: Option<u32>) -> HistoryMatch {
        HistoryMatch {
            match_id: id,
            start_time: 1_793_000_000,
            duration_s,
            game_mode,
        }
    }

    #[test]
    fn skip_status_passes_only_a_full_standard_match() {
        assert_eq!(skip_status(&hist(1, 180, Some(1))), None);
        assert_eq!(
            skip_status(&hist(1, 179, Some(1))),
            Some(MatchStatus::Remake)
        );
        assert_eq!(
            skip_status(&hist(1, 1800, Some(4))),
            Some(MatchStatus::UnsupportedMode { game_mode: Some(4) })
        );
        assert_eq!(
            skip_status(&hist(1, 1800, None)),
            Some(MatchStatus::UnsupportedMode { game_mode: None })
        );
    }
}
