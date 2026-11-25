//! Who gets a turn in a room, and when the room is done. No clocks and no I/O: rooms/mod.rs
//! tells it what happened and does what it answers.
//!
//! A guy gets a turn for one of three reasons ([`Why`]):
//! - it was addressed (by you or by another guy);
//! - it leads the room, and a message of yours named nobody;
//! - it's listening, and the room went quiet: one look per message of yours, one guy at a time.
//!
//! Nothing else gives a turn, so a room stops by itself: unprompted speech is at most one
//! look per listener, and a chain of guys addressing each other goes on only while each
//! reply names someone. [`Floor::replied`] counts those and pauses the room past `max_hops`.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Addressed,
    Lead,
    Look,
}

impl Why {
    pub fn as_str(self) -> &'static str {
        match self {
            Why::Addressed => "addressed",
            Why::Lead => "lead",
            Why::Look => "look",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wake {
    pub uid: String,
    pub why: Why,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Give this guy a turn.
    Run(Wake),
    /// Nobody is working and nobody is owed an answer: the round is over.
    Quiet,
    /// The guys have addressed each other more than allowed since your last message. Turns
    /// they're owed wait for [`Floor::resume`] (or your next message).
    Pause,
}

/// A member as the floor needs it, in the room's order.
#[derive(Debug, Clone, Copy)]
pub struct Seat<'a> {
    pub uid: &'a str,
    pub listening: bool,
}

#[derive(Debug, Default)]
pub struct Floor {
    /// Turns owed, oldest first: their guy is busy, or the room is paused.
    queue: Vec<Wake>,
    running: HashMap<String, Why>,
    /// Had their look since your last message.
    looked: HashSet<String>,
    /// Spoke since your last message.
    spoke: HashSet<String>,
    /// Times a guy addressed another since your last message.
    hops: u32,
    paused: bool,
    /// A round is under way: `Quiet` or `Pause` hasn't closed it.
    open: bool,
}

impl Floor {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Who is working, and why.
    pub fn running(&self) -> Vec<Wake> {
        let mut all: Vec<Wake> = self.running.iter().map(|(uid, why)| Wake { uid: uid.clone(), why: *why }).collect();
        all.sort_by(|a, b| a.uid.cmp(&b.uid));
        all
    }

    /// Guys owed a turn while one of theirs is under way: they can take what's new now
    /// ([`Floor::steered`]), or get the turn when this one ends.
    pub fn owed_while_running(&self) -> Vec<String> {
        self.queue.iter().filter(|w| self.running.contains_key(&w.uid)).map(|w| w.uid.clone()).collect()
    }

    /// The guy took what it was owed into the turn it's on.
    pub fn steered(&mut self, uid: &str) {
        self.queue.retain(|w| w.uid != uid);
    }

    /// You wrote, to `to` (members; when you named nobody, whoever the room sends it to, with
    /// `why` saying which it was). A new round: everyone may speak and look again.
    pub fn human(&mut self, to: &[String], why: Why, seats: &[Seat]) -> Vec<Action> {
        self.looked.clear();
        self.spoke.clear();
        self.hops = 0;
        self.paused = false;
        self.open = true;
        for uid in to {
            self.owe(uid, why, seats);
        }
        self.pump(seats)
    }

    /// A guy's turn ended. `mentions`: None when it stayed silent (or failed), else the
    /// members its message addresses.
    pub fn replied(&mut self, uid: &str, mentions: Option<&[String]>, seats: &[Seat], max_hops: u32) -> Vec<Action> {
        if self.running.remove(uid).is_none() {
            return Vec::new();
        }
        if let Some(mentions) = mentions {
            self.spoke.insert(uid.to_string());
            for to in mentions.iter().filter(|to| to.as_str() != uid) {
                if !seats.iter().any(|s| s.uid == to) {
                    continue;
                }
                self.hops += 1;
                if self.hops > max_hops {
                    self.paused = true;
                }
                self.owe(to, Why::Addressed, seats);
            }
        }
        self.pump(seats)
    }

    /// A turn has run long. Listeners don't wait for it: the next one looks now.
    pub fn waited(&mut self, seats: &[Seat]) -> Vec<Action> {
        if !self.open || self.paused || self.running.values().any(|why| *why == Why::Look) {
            return Vec::new();
        }
        self.look(seats).into_iter().collect()
    }

    /// "Let them continue": the turns that were held go ahead.
    pub fn resume(&mut self, seats: &[Seat]) -> Vec<Action> {
        if !self.paused {
            return Vec::new();
        }
        self.paused = false;
        self.hops = 0;
        self.open = true;
        self.pump(seats)
    }

    /// You stopped the room. Turns under way are no longer the floor's: their replies are dropped.
    pub fn abort(&mut self) {
        self.queue.clear();
        self.running.clear();
        self.paused = false;
        self.open = false;
    }

    /// A member left: it's owed nothing, and a turn of its under way no longer counts.
    pub fn removed(&mut self, uid: &str, seats: &[Seat]) -> Vec<Action> {
        self.queue.retain(|w| w.uid != uid);
        if self.running.remove(uid).is_some() {
            return self.pump(seats);
        }
        Vec::new()
    }

    fn owe(&mut self, uid: &str, why: Why, seats: &[Seat]) {
        if seats.iter().any(|s| s.uid == uid) && !self.queue.iter().any(|w| w.uid == uid) {
            self.queue.push(Wake { uid: uid.to_string(), why });
        }
    }

    fn pump(&mut self, seats: &[Seat]) -> Vec<Action> {
        let mut actions = Vec::new();
        if !self.open {
            return actions;
        }
        if !self.paused {
            let mut waiting = Vec::new();
            for wake in std::mem::take(&mut self.queue) {
                if self.running.contains_key(&wake.uid) {
                    waiting.push(wake);
                } else {
                    self.running.insert(wake.uid.clone(), wake.why);
                    actions.push(Action::Run(wake));
                }
            }
            self.queue = waiting;
        }
        if !self.running.is_empty() {
            return actions;
        }
        if self.paused {
            self.open = false;
            actions.push(Action::Pause);
        } else if let Some(look) = self.look(seats) {
            actions.push(look);
        } else {
            self.open = false;
            actions.push(Action::Quiet);
        }
        actions
    }

    /// The next listener that hasn't spoken or looked since your last message, if any.
    fn look(&mut self, seats: &[Seat]) -> Option<Action> {
        let next = seats
            .iter()
            .find(|s| s.listening && !self.looked.contains(s.uid) && !self.spoke.contains(s.uid) && !self.running.contains_key(s.uid))?;
        self.looked.insert(next.uid.to_string());
        self.running.insert(next.uid.to_string(), Why::Look);
        Some(Action::Run(Wake { uid: next.uid.to_string(), why: Why::Look }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOPS: u32 = 8;

    fn seats() -> Vec<Seat<'static>> {
        vec![
            Seat { uid: "scout", listening: false },
            Seat { uid: "patch", listening: false },
            Seat { uid: "vera", listening: true },
        ]
    }

    fn run(uid: &str, why: Why) -> Action {
        Action::Run(Wake { uid: uid.into(), why })
    }

    fn to(uids: &[&str]) -> Vec<String> {
        uids.iter().map(|u| u.to_string()).collect()
    }

    #[test]
    fn the_walkthrough_ends_by_itself() {
        let s = seats();
        let mut f = Floor::default();
        // You, to nobody: the lead takes it.
        assert_eq!(f.human(&to(&["scout"]), Why::Lead, &s), vec![run("scout", Why::Lead)]);
        // Scout hands it to Patch.
        assert_eq!(f.replied("scout", Some(&to(&["patch"])), &s, HOPS), vec![run("patch", Why::Addressed)]);
        // Patch answers nobody in particular: the room goes quiet, so the listener looks.
        assert_eq!(f.replied("patch", Some(&[]), &s, HOPS), vec![run("vera", Why::Look)]);
        // Vera has something for Patch.
        assert_eq!(f.replied("vera", Some(&to(&["patch"])), &s, HOPS), vec![run("patch", Why::Addressed)]);
        // Patch answers; Vera already looked, nobody else listens: over.
        assert_eq!(f.replied("patch", Some(&[]), &s, HOPS), vec![Action::Quiet]);
        assert!(!f.is_open());
        // Your next message gives Vera a look again, and she can pass on it.
        assert_eq!(f.human(&to(&["patch"]), Why::Addressed, &s), vec![run("patch", Why::Addressed)]);
        assert_eq!(f.replied("patch", Some(&[]), &s, HOPS), vec![run("vera", Why::Look)]);
        assert_eq!(f.replied("vera", None, &s, HOPS), vec![Action::Quiet]);
    }

    #[test]
    fn a_listener_that_spoke_does_not_also_look() {
        let s = seats();
        let mut f = Floor::default();
        f.human(&to(&["vera"]), Why::Addressed, &s);
        assert_eq!(f.replied("vera", Some(&[]), &s, HOPS), vec![Action::Quiet]);
    }

    #[test]
    fn listeners_look_one_at_a_time_and_once_each() {
        let s = vec![Seat { uid: "a", listening: true }, Seat { uid: "b", listening: true }, Seat { uid: "c", listening: true }];
        let mut f = Floor::default();
        assert_eq!(f.human(&to(&["a"]), Why::Lead, &s), vec![run("a", Why::Lead)]);
        assert_eq!(f.replied("a", Some(&[]), &s, HOPS), vec![run("b", Why::Look)]);
        // b speaks to nobody: c still gets its look, b and a don't get another.
        assert_eq!(f.replied("b", Some(&[]), &s, HOPS), vec![run("c", Why::Look)]);
        assert_eq!(f.replied("c", Some(&[]), &s, HOPS), vec![Action::Quiet]);
    }

    #[test]
    fn several_addressed_at_once_run_together() {
        let s = seats();
        let mut f = Floor::default();
        assert_eq!(f.human(&to(&["scout", "patch"]), Why::Addressed, &s), vec![run("scout", Why::Addressed), run("patch", Why::Addressed)]);
        // The room isn't quiet until both are done.
        assert_eq!(f.replied("scout", Some(&[]), &s, HOPS), vec![]);
        assert_eq!(f.replied("patch", Some(&[]), &s, HOPS), vec![run("vera", Why::Look)]);
    }

    #[test]
    fn a_busy_guy_is_owed_its_turn_or_takes_it_now() {
        let s = seats();
        let mut f = Floor::default();
        f.human(&to(&["patch"]), Why::Addressed, &s);
        // You write to Patch again while it works.
        assert_eq!(f.human(&to(&["patch"]), Why::Addressed, &s), vec![]);
        assert_eq!(f.owed_while_running(), vec!["patch".to_string()]);
        // It can't take it now: it gets the turn when this one ends.
        assert_eq!(f.replied("patch", Some(&[]), &s, HOPS), vec![run("patch", Why::Addressed)]);

        let mut f = Floor::default();
        f.human(&to(&["patch"]), Why::Addressed, &s);
        f.human(&to(&["patch"]), Why::Addressed, &s);
        f.steered("patch");
        assert_eq!(f.replied("patch", Some(&[]), &s, HOPS), vec![run("vera", Why::Look)]);
    }

    #[test]
    fn guys_going_back_and_forth_are_paused_then_continue_when_you_say() {
        let s = seats();
        let mut f = Floor::default();
        f.human(&to(&["scout"]), Why::Lead, &s);
        let mut from = "scout";
        for _ in 0..2 {
            let other = if from == "scout" { "patch" } else { "scout" };
            assert_eq!(f.replied(from, Some(&to(&[other])), &s, 2), vec![run(other, Why::Addressed)]);
            from = other;
        }
        // The third time in a row is one too many.
        let other = if from == "scout" { "patch" } else { "scout" };
        assert_eq!(f.replied(from, Some(&to(&[other])), &s, 2), vec![Action::Pause]);
        assert!(f.is_paused() && !f.is_open());
        assert_eq!(f.resume(&s), vec![run(other, Why::Addressed)]);
        assert!(f.is_open());
    }

    #[test]
    fn your_message_lifts_a_pause_and_keeps_what_was_owed() {
        let s = seats();
        let mut f = Floor::default();
        f.human(&to(&["scout"]), Why::Lead, &s);
        assert_eq!(f.replied("scout", Some(&to(&["patch"])), &s, 0), vec![Action::Pause]);
        assert_eq!(f.human(&to(&["scout"]), Why::Addressed, &s), vec![run("patch", Why::Addressed), run("scout", Why::Addressed)]);
    }

    #[test]
    fn stopped_rooms_drop_late_replies() {
        let s = seats();
        let mut f = Floor::default();
        f.human(&to(&["scout"]), Why::Lead, &s);
        f.abort();
        assert_eq!(f.replied("scout", Some(&to(&["patch"])), &s, HOPS), vec![]);
        assert!(!f.is_open());
    }

    #[test]
    fn a_long_turn_does_not_hold_up_a_look() {
        let s = seats();
        let mut f = Floor::default();
        f.human(&to(&["patch"]), Why::Addressed, &s);
        assert_eq!(f.waited(&s), vec![run("vera", Why::Look)]);
        assert_eq!(f.waited(&s), vec![]);
        assert_eq!(f.replied("vera", None, &s, HOPS), vec![]);
        // Patch finishes: Vera has had her look.
        assert_eq!(f.replied("patch", Some(&[]), &s, HOPS), vec![Action::Quiet]);
    }

    #[test]
    fn mentions_of_strangers_and_of_oneself_give_no_turn() {
        let s = seats();
        let mut f = Floor::default();
        f.human(&to(&["scout"]), Why::Lead, &s);
        assert_eq!(f.replied("scout", Some(&to(&["scout", "nobody"])), &s, HOPS), vec![run("vera", Why::Look)]);
    }

    #[test]
    fn a_member_leaving_mid_turn_does_not_stall_the_room() {
        let s = seats();
        let mut f = Floor::default();
        f.human(&to(&["patch"]), Why::Addressed, &s);
        let left: Vec<Seat> = s.iter().copied().filter(|m| m.uid != "patch").collect();
        assert_eq!(f.removed("patch", &left), vec![run("vera", Why::Look)]);
    }
}
