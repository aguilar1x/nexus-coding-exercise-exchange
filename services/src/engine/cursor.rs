//! Feed cursor: where we are in the stream, and how to notice when the ground
//! moves under us.
//!
//! `GET /orders?since=N` returns ids strictly greater than `N`. Because the
//! feed assigns ids under the same lock as the push and never truncates
//! history, this cursor cannot gap or duplicate.
//!
//! The one failure it must survive is a feed restart: ids reset to 1, so a
//! cursor from the previous run sits permanently above them, `since` matches
//! nothing, and the consumer goes *silently* deaf. The detection lives here,
//! away from the I/O, so it can be tested without a server.

use crate::feed::OrderId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    since: OrderId,
    empty_polls: u32,
    /// Consecutive empty polls tolerated before suspecting a restart. A live
    /// market refills instantly, so this only fires on a genuinely quiet feed.
    probe_after: u32,
}

impl Cursor {
    pub fn new(from: OrderId, probe_after: u32) -> Self {
        Cursor {
            since: from,
            empty_polls: 0,
            probe_after,
        }
    }

    pub fn since(&self) -> OrderId {
        self.since
    }

    /// Records that a message was consumed. Called before the message is
    /// processed: even one we skip has been read, and re-reading it would loop
    /// forever.
    pub fn advance_to(&mut self, id: OrderId) {
        self.since = id;
        self.empty_polls = 0;
    }

    /// Records an empty poll. Returns true when it is worth asking the feed
    /// for its newest id to check for a restart.
    pub fn note_empty_poll(&mut self) -> bool {
        self.empty_polls += 1;
        if self.empty_polls >= self.probe_after {
            self.empty_polls = 0;
            return true;
        }
        false
    }

    /// Given the feed's newest id, has it restarted beneath us?
    ///
    /// An empty feed (`None`) is not a restart: a freshly started feed that has
    /// published nothing yet looks the same as one that never will, and our
    /// cursor is 0 there anyway.
    pub fn detects_restart(&self, feed_max_id: Option<OrderId>) -> bool {
        matches!(feed_max_id, Some(max) if max < self.since)
    }

    /// Rewinds to the beginning. Safe because the fold is deterministic and the
    /// feed retains everything.
    pub fn rewind(&mut self) {
        self.since = 0;
        self.empty_polls = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advancing_tracks_the_last_id_seen() {
        let mut c = Cursor::new(0, 5);
        assert_eq!(c.since(), 0);
        c.advance_to(1);
        c.advance_to(2);
        c.advance_to(42);
        assert_eq!(c.since(), 42);
    }

    #[test]
    fn it_starts_where_it_was_told_to() {
        assert_eq!(Cursor::new(900, 5).since(), 900);
    }

    #[test]
    fn a_probe_is_only_worth_it_after_repeated_silence() {
        let mut c = Cursor::new(0, 3);
        assert!(!c.note_empty_poll());
        assert!(!c.note_empty_poll());
        assert!(c.note_empty_poll());
    }

    #[test]
    fn the_silence_counter_resets_when_a_message_arrives() {
        let mut c = Cursor::new(0, 3);
        c.note_empty_poll();
        c.note_empty_poll();
        c.advance_to(7); // traffic resumed
        assert!(!c.note_empty_poll());
        assert!(!c.note_empty_poll());
        assert!(c.note_empty_poll());
    }

    #[test]
    fn the_counter_also_resets_after_probing_so_it_does_not_spam() {
        let mut c = Cursor::new(0, 2);
        c.note_empty_poll();
        assert!(c.note_empty_poll());
        assert!(!c.note_empty_poll(), "should wait again before re-probing");
    }

    #[test]
    fn a_feed_whose_newest_id_fell_behind_us_has_restarted() {
        let mut c = Cursor::new(0, 5);
        c.advance_to(3000);
        // The feed came back up and is publishing from 1 again.
        assert!(c.detects_restart(Some(12)));
    }

    #[test]
    fn a_feed_that_is_merely_quiet_has_not_restarted() {
        let mut c = Cursor::new(0, 5);
        c.advance_to(3000);
        assert!(!c.detects_restart(Some(3000)), "caught up is not restarted");
        assert!(!c.detects_restart(Some(3001)));
    }

    #[test]
    fn an_empty_feed_is_not_treated_as_a_restart() {
        let mut c = Cursor::new(0, 5);
        c.advance_to(100);
        // No messages at all: a feed that just booted looks like this, and so
        // does an unreachable one. Rewinding on it would be a guess.
        assert!(!c.detects_restart(None));
    }

    #[test]
    fn a_fresh_cursor_never_reports_a_restart() {
        let c = Cursor::new(0, 5);
        assert!(!c.detects_restart(Some(0)));
        assert!(!c.detects_restart(Some(500)));
    }

    #[test]
    fn rewinding_returns_to_the_start_of_the_stream() {
        let mut c = Cursor::new(0, 5);
        c.advance_to(3000);
        c.note_empty_poll();
        c.rewind();
        assert_eq!(c.since(), 0);
        // And the silence counter starts over, so the next probe is not immediate.
        assert!(!c.note_empty_poll());
    }
}
