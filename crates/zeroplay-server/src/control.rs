//! Who holds which console controller slot, and when it changes hands.
//!
//! - **Hand-off**: when a player leaves the stream ("Back to home") or is AFK
//!   for 5 minutes, their slot goes to the player who has waited longest. With
//!   nobody waiting and nobody else playing, the stream stops.
//! - **Held slots**: when a player's connection drops, their controller is
//!   unplugged on the console (most games pause on that) and the slot is kept
//!   for their profile for a minute.
//! - **Timed play** (admin setting): players who opt in take turns. With
//!   someone waiting, the player in control gets warnings, a 10 second
//!   countdown, and then the next one takes over. More time can be asked for;
//!   an admin approves it, and the clock stops while they decide.
//! - **Empty**: with nobody connected for 30 seconds, the stream stops.
//!
//! Everything here is plain state; the app carries out the [`Effect`]s.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use zeroplay_xbox::input::GamepadState;

pub const SLOTS: usize = 4;
/// The controller index a hand-off assigns (all of a browser's controllers by default).
pub const MERGED_LOCAL: u8 = 0;

/// What the app has to do after a change.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Plug this controller in on the console (or update it).
    Plug(GamepadState),
    /// Unplug this slot on the console.
    Unplug(u8),
    /// Tell one player something. `countdown` shows a big countdown in seconds.
    Notice {
        viewer: u64,
        text: String,
        countdown: Option<u32>,
    },
    /// End the stream and free the console.
    Stop(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct Rules {
    pub afk: Duration,
    pub afk_warning: Duration,
    pub hold: Duration,
    pub empty: Duration,
    /// Turn length when timed play is on.
    pub turn: Option<Duration>,
}

impl Default for Rules {
    fn default() -> Rules {
        Rules {
            afk: Duration::from_secs(300),
            afk_warning: Duration::from_secs(30),
            hold: Duration::from_secs(60),
            empty: Duration::from_secs(30),
            turn: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimeRequest {
    pub id: u64,
    pub viewer: u64,
    pub slot: usize,
    pub minutes: u32,
}

#[derive(Default)]
pub struct Seats {
    /// Which player (and which of their controllers) holds each slot.
    pub owner: [Option<(u64, u8)>; SLOTS],
    /// The last state of each slot's controller.
    pub pad: [Option<GamepadState>; SLOTS],
    /// Slots kept for a profile whose connection dropped, until the time given.
    reserved: [Option<(String, Instant)>; SLOTS],
    last_input: HashMap<u64, Instant>,
    afk_warned: HashSet<u64>,
    /// Players watching without a slot, and since when (the queue order).
    waiting: HashMap<u64, Instant>,
    /// Players who take part in timed play.
    pub opted_in: HashSet<u64>,
    turn_end: [Option<Instant>; SLOTS],
    /// Which turn warnings were sent (bits: 2 min, 30 s, 10 s).
    turn_marks: [u8; SLOTS],
    pub time_requests: Vec<TimeRequest>,
    next_request: u64,
    empty_since: Option<Instant>,
}

fn neutral(slot: usize) -> GamepadState {
    GamepadState::new(slot as u8)
}

fn same_input(a: &GamepadState, b: &GamepadState) -> bool {
    a.buttons == b.buttons
        && a.left_x == b.left_x
        && a.left_y == b.left_y
        && a.right_x == b.right_x
        && a.right_y == b.right_y
        && a.left_trigger == b.left_trigger
        && a.right_trigger == b.right_trigger
}

impl Seats {
    pub fn slots_of(&self, viewer: u64) -> Vec<usize> {
        (0..SLOTS)
            .filter(|&s| matches!(self.owner[s], Some((v, _)) if v == viewer))
            .collect()
    }

    /// Waiting players in queue order.
    pub fn queue(&self) -> Vec<u64> {
        let mut queue: Vec<(u64, Instant)> = self.waiting.iter().map(|(v, t)| (*v, *t)).collect();
        queue.sort_by_key(|(_, t)| *t);
        queue.into_iter().map(|(v, _)| v).collect()
    }

    /// Seconds left of the turn for this slot, if timed play is counting.
    pub fn turn_left(&self, slot: usize, now: Instant) -> Option<u64> {
        self.turn_end[slot].map(|end| end.saturating_duration_since(now).as_secs())
    }

    /// The slot for one of a player's controllers: the one it already has,
    /// a slot held for the player's profile, or a free one.
    pub fn claim(
        &mut self,
        viewer: u64,
        local: u8,
        profile: Option<&str>,
        now: Instant,
    ) -> Option<(usize, bool)> {
        if let Some(slot) = self.owner.iter().position(|o| *o == Some((viewer, local))) {
            return Some((slot, false));
        }
        let held = profile.and_then(|p| {
            self.reserved
                .iter()
                .position(|r| matches!(r, Some((owner, _)) if owner == p))
        });
        let slot = held.or_else(|| {
            (0..SLOTS).find(|&s| self.owner[s].is_none() && self.reserved[s].is_none())
        })?;
        self.reserved[slot] = None;
        self.owner[slot] = Some((viewer, local));
        self.waiting.remove(&viewer);
        self.last_input.insert(viewer, now);
        Some((slot, true))
    }

    /// A controller's new state. Returns the state to send to the console.
    pub fn input(
        &mut self,
        viewer: u64,
        slot: usize,
        mut state: GamepadState,
        now: Instant,
    ) -> GamepadState {
        state.index = slot as u8;
        let changed = self.pad[slot]
            .as_ref()
            .is_none_or(|old| !same_input(old, &state));
        if changed {
            self.last_input.insert(viewer, now);
            self.afk_warned.remove(&viewer);
        }
        self.pad[slot] = Some(state);
        state
    }

    /// A player unplugs one controller (separate-players mode).
    pub fn unplug(&mut self, viewer: u64, local: u8) -> Option<usize> {
        let slot = self
            .owner
            .iter()
            .position(|o| *o == Some((viewer, local)))?;
        self.owner[slot] = None;
        self.pad[slot] = None;
        self.turn_end[slot] = None;
        Some(slot)
    }

    pub fn start_watching(&mut self, viewer: u64, now: Instant) {
        self.empty_since = None;
        if self.slots_of(viewer).is_empty() {
            self.waiting.entry(viewer).or_insert(now);
        }
    }

    /// The player went back to home: their slots go to the next in line.
    pub fn stop_watching(&mut self, viewer: u64, now: Instant) -> Vec<Effect> {
        self.waiting.remove(&viewer);
        let mut effects = Vec::new();
        for slot in self.slots_of(viewer) {
            self.hand_off(slot, now, false, &mut effects);
        }
        effects
    }

    /// The player's connection is gone. With a stream running, their slots are
    /// held for their profile and the controllers unplugged (so games pause).
    pub fn disconnect(
        &mut self,
        viewer: u64,
        profile: Option<&str>,
        live: bool,
        now: Instant,
        rules: &Rules,
    ) -> Vec<Effect> {
        self.waiting.remove(&viewer);
        self.opted_in.remove(&viewer);
        self.last_input.remove(&viewer);
        self.afk_warned.remove(&viewer);
        self.time_requests.retain(|r| r.viewer != viewer);
        let mut effects = Vec::new();
        for slot in self.slots_of(viewer) {
            match profile.filter(|_| live) {
                Some(profile) => {
                    self.owner[slot] = None;
                    self.pad[slot] = None;
                    self.turn_end[slot] = None;
                    self.reserved[slot] = Some((profile.to_owned(), now + rules.hold));
                    effects.push(Effect::Unplug(slot as u8));
                }
                None => self.hand_off(slot, now, false, &mut effects),
            }
        }
        effects
    }

    /// Gives a slot to the next waiting player (in timed play, the next one who
    /// opted in), or frees it.
    /// `requeue` puts the previous player back in the queue (a timed-play turn ending).
    fn hand_off(&mut self, slot: usize, now: Instant, requeue: bool, effects: &mut Vec<Effect>) {
        let previous = self.owner[slot].map(|(v, _)| v);
        let queue = self.queue();
        let next = queue
            .iter()
            .copied()
            .find(|v| self.opted_in.contains(v))
            .or_else(|| queue.first().copied());
        self.turn_end[slot] = None;
        self.turn_marks[slot] = 0;
        self.time_requests.retain(|r| r.slot != slot);
        match next {
            Some(viewer) => {
                self.waiting.remove(&viewer);
                self.owner[slot] = Some((viewer, MERGED_LOCAL));
                self.pad[slot] = Some(neutral(slot));
                self.last_input.insert(viewer, now);
                self.afk_warned.remove(&viewer);
                effects.push(Effect::Plug(neutral(slot)));
                effects.push(Effect::Notice {
                    viewer,
                    text: format!("You have control now (player {}).", slot + 1),
                    countdown: None,
                });
            }
            None => {
                self.owner[slot] = None;
                self.pad[slot] = None;
                effects.push(Effect::Unplug(slot as u8));
            }
        }
        // After a timed turn the previous player waits for their next one.
        if let Some(previous) = previous.filter(|p| requeue && Some(*p) != next) {
            if self.slots_of(previous).is_empty() {
                self.waiting.insert(previous, now);
            }
        }
    }

    /// Asks for more time in timed play. Returns the request id.
    pub fn request_time(&mut self, viewer: u64, minutes: u32) -> Option<u64> {
        let slot = *self.slots_of(viewer).first()?;
        if self.turn_end[slot].is_none() || self.time_requests.iter().any(|r| r.slot == slot) {
            return None;
        }
        self.next_request += 1;
        self.time_requests.push(TimeRequest {
            id: self.next_request,
            viewer,
            slot,
            minutes,
        });
        Some(self.next_request)
    }

    pub fn answer_time(&mut self, id: u64, approve: bool) -> Vec<Effect> {
        let Some(index) = self.time_requests.iter().position(|r| r.id == id) else {
            return Vec::new();
        };
        let request = self.time_requests.remove(index);
        if approve {
            if let Some(end) = &mut self.turn_end[request.slot] {
                *end += Duration::from_secs(request.minutes as u64 * 60);
            }
            self.turn_marks[request.slot] = 0;
        }
        let text = if approve {
            format!("You got {} more minutes.", request.minutes)
        } else {
            "No extra time this turn.".to_owned()
        };
        vec![Effect::Notice {
            viewer: request.viewer,
            text,
            countdown: None,
        }]
    }

    /// Runs every second.
    pub fn tick(
        &mut self,
        now: Instant,
        rules: &Rules,
        live: bool,
        connected: usize,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if !live {
            self.empty_since = None;
            return effects;
        }

        // Nobody connected: stop after a while.
        if connected == 0 {
            let since = *self.empty_since.get_or_insert(now);
            if now.duration_since(since) >= rules.empty {
                self.empty_since = None;
                effects.push(Effect::Stop("Nobody is connected any more."));
                return effects;
            }
        } else {
            self.empty_since = None;
        }

        // Held slots that nobody came back for.
        for slot in 0..SLOTS {
            if matches!(&self.reserved[slot], Some((_, until)) if *until <= now) {
                self.reserved[slot] = None;
                if self.owner[slot].is_none() && !self.waiting.is_empty() {
                    self.hand_off(slot, now, false, &mut effects);
                }
            }
        }

        // AFK.
        for slot in 0..SLOTS {
            let Some((viewer, _)) = self.owner[slot] else {
                continue;
            };
            let idle = self
                .last_input
                .get(&viewer)
                .map(|t| now.duration_since(*t))
                .unwrap_or_default();
            if idle >= rules.afk {
                effects.push(Effect::Notice {
                    viewer,
                    text: "No input for 5 minutes, so control was passed on.".to_owned(),
                    countdown: None,
                });
                self.hand_off(slot, now, false, &mut effects);
            } else if idle + rules.afk_warning >= rules.afk && self.afk_warned.insert(viewer) {
                effects.push(Effect::Notice {
                    viewer,
                    text: "No input for a while. Press any button to keep control.".to_owned(),
                    countdown: Some(rules.afk_warning.as_secs() as u32),
                });
            }
        }

        // Timed play.
        if let Some(turn) = rules.turn {
            let someone_waiting = self.queue().iter().any(|v| self.opted_in.contains(v));
            for slot in 0..SLOTS {
                let Some((viewer, _)) = self.owner[slot] else {
                    continue;
                };
                if !self.opted_in.contains(&viewer) || !someone_waiting {
                    self.turn_end[slot] = None;
                    self.turn_marks[slot] = 0;
                    continue;
                }
                let end = *self.turn_end[slot].get_or_insert(now + turn);
                if self.time_requests.iter().any(|r| r.slot == slot) {
                    // The clock stops while an admin decides.
                    self.turn_end[slot] = Some(end + Duration::from_secs(1));
                    continue;
                }
                let left = end.saturating_duration_since(now).as_secs();
                let marks = &mut self.turn_marks[slot];
                if left == 0 {
                    effects.push(Effect::Notice {
                        viewer,
                        text: "Your turn is over. You are back in the queue.".to_owned(),
                        countdown: None,
                    });
                    self.hand_off(slot, now, true, &mut effects);
                } else if left <= 10 && *marks & 4 == 0 {
                    *marks |= 7;
                    effects.push(Effect::Notice {
                        viewer,
                        text: "Switching players".to_owned(),
                        countdown: Some(left as u32),
                    });
                } else if left <= 30 && *marks & 2 == 0 {
                    *marks |= 3;
                    effects.push(Effect::Notice {
                        viewer,
                        text: "30 seconds left of your turn.".to_owned(),
                        countdown: None,
                    });
                } else if left <= 120 && *marks & 1 == 0 {
                    *marks |= 1;
                    effects.push(Effect::Notice {
                        viewer,
                        text: "2 minutes left of your turn.".to_owned(),
                        countdown: None,
                    });
                }
            }
        } else {
            self.turn_end = [None; SLOTS];
        }

        // AFK with nobody to hand over to and nobody else playing: stop.
        let anyone_playing =
            self.owner.iter().any(Option::is_some) || self.reserved.iter().any(Option::is_some);
        if !anyone_playing
            && effects.iter().any(|e| matches!(e, Effect::Unplug(_)))
            && self.waiting.is_empty()
        {
            effects.push(Effect::Stop("Nobody was playing any more."));
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn pressed(slot: usize) -> GamepadState {
        GamepadState {
            index: slot as u8,
            buttons: 16,
            ..GamepadState::default()
        }
    }

    fn notices_for(effects: &[Effect], viewer: u64) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Notice {
                    viewer: v, text, ..
                } if *v == viewer => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn back_to_home_hands_control_to_the_longest_waiting() {
        let t0 = Instant::now();
        let mut seats = Seats::default();
        let (slot, _) = seats.claim(1, 0, Some("a"), t0).unwrap();
        assert_eq!(slot, 0);
        seats.start_watching(2, t0 + secs(5));
        seats.start_watching(3, t0 + secs(1));
        let effects = seats.stop_watching(1, t0 + secs(10));
        assert_eq!(seats.owner[0], Some((3, MERGED_LOCAL)), "3 waited longest");
        assert!(effects.contains(&Effect::Plug(GamepadState::new(0))));
        assert_eq!(
            notices_for(&effects, 3),
            vec!["You have control now (player 1).".to_owned()]
        );
        assert_eq!(seats.queue(), vec![2]);
    }

    #[test]
    fn a_dropped_connection_unplugs_and_holds_the_slot() {
        let t0 = Instant::now();
        let rules = Rules::default();
        let mut seats = Seats::default();
        seats.claim(1, 0, Some("anna"), t0).unwrap();
        seats.start_watching(2, t0);
        let effects = seats.disconnect(1, Some("anna"), true, t0, &rules);
        assert_eq!(effects, vec![Effect::Unplug(0)]);
        // Someone else pressing a button does not get the held slot.
        let (other, _) = seats.claim(2, 0, Some("ben"), t0 + secs(1)).unwrap();
        assert_eq!(other, 1);
        // Anna comes back within the minute and gets player 1 again.
        let (back, _) = seats.claim(9, 0, Some("anna"), t0 + secs(30)).unwrap();
        assert_eq!(back, 0);
    }

    #[test]
    fn a_held_slot_goes_to_the_queue_after_a_minute() {
        let t0 = Instant::now();
        let rules = Rules::default();
        let mut seats = Seats::default();
        seats.claim(1, 0, Some("anna"), t0).unwrap();
        seats.claim(5, 0, Some("eve"), t0).unwrap();
        seats.disconnect(1, Some("anna"), true, t0, &rules);
        seats.start_watching(2, t0);
        assert!(seats.tick(t0 + secs(30), &rules, true, 2).is_empty());
        let effects = seats.tick(t0 + secs(61), &rules, true, 2);
        assert_eq!(seats.owner[0], Some((2, MERGED_LOCAL)));
        assert_eq!(notices_for(&effects, 2).len(), 1);
    }

    #[test]
    fn afk_warns_then_hands_off() {
        let t0 = Instant::now();
        let rules = Rules::default();
        let mut seats = Seats::default();
        seats.claim(1, 0, None, t0).unwrap();
        seats.input(1, 0, pressed(0), t0);
        seats.start_watching(2, t0);
        assert!(seats.tick(t0 + secs(200), &rules, true, 2).is_empty());
        let warning = seats.tick(t0 + secs(271), &rules, true, 2);
        assert!(matches!(
            warning.as_slice(),
            [Effect::Notice {
                viewer: 1,
                countdown: Some(30),
                ..
            }]
        ));
        assert!(
            seats.tick(t0 + secs(272), &rules, true, 2).is_empty(),
            "warned once"
        );
        let effects = seats.tick(t0 + secs(300), &rules, true, 2);
        assert_eq!(seats.owner[0], Some((2, MERGED_LOCAL)));
        assert!(!notices_for(&effects, 1).is_empty());
    }

    #[test]
    fn pressing_a_button_resets_afk() {
        let t0 = Instant::now();
        let rules = Rules::default();
        let mut seats = Seats::default();
        seats.claim(1, 0, None, t0).unwrap();
        seats.input(1, 0, pressed(0), t0 + secs(280));
        seats.start_watching(2, t0);
        assert!(seats.tick(t0 + secs(300), &rules, true, 2).is_empty());
        assert_eq!(seats.owner[0].map(|o| o.0), Some(1));
        // Holding the same state does not count as input.
        seats.input(1, 0, pressed(0), t0 + secs(400));
        let effects = seats.tick(t0 + secs(581), &rules, true, 2);
        assert_eq!(seats.owner[0].map(|o| o.0), Some(2));
        assert!(!effects.is_empty());
    }

    #[test]
    fn afk_with_nobody_waiting_stops_the_stream() {
        let t0 = Instant::now();
        let rules = Rules::default();
        let mut seats = Seats::default();
        seats.claim(1, 0, None, t0).unwrap();
        let effects = seats.tick(t0 + secs(300), &rules, true, 1);
        assert!(effects.iter().any(|e| matches!(e, Effect::Stop(_))));
    }

    #[test]
    fn nobody_connected_stops_after_30_seconds() {
        let t0 = Instant::now();
        let rules = Rules::default();
        let mut seats = Seats::default();
        assert!(seats.tick(t0, &rules, true, 0).is_empty());
        assert!(seats.tick(t0 + secs(29), &rules, true, 0).is_empty());
        assert!(matches!(
            seats.tick(t0 + secs(30), &rules, true, 0).as_slice(),
            [Effect::Stop(_)]
        ));
        // Somebody back in time resets the clock.
        let mut seats = Seats::default();
        seats.tick(t0, &rules, true, 0);
        seats.tick(t0 + secs(20), &rules, true, 1);
        assert!(seats.tick(t0 + secs(40), &rules, true, 0).is_empty());
    }

    #[test]
    fn timed_play_counts_down_and_switches() {
        let t0 = Instant::now();
        let rules = Rules {
            turn: Some(secs(600)),
            ..Rules::default()
        };
        let mut seats = Seats::default();
        seats.claim(1, 0, None, t0).unwrap();
        seats.opted_in.insert(1);
        seats.start_watching(2, t0);
        // 2 is waiting but has not opted in: no turns.
        let mut t = t0;
        for _ in 0..3 {
            t += secs(200);
            seats.input(
                1,
                0,
                GamepadState {
                    buttons: (t - t0).as_secs() as u16,
                    ..GamepadState::default()
                },
                t,
            );
            assert!(seats.tick(t, &rules, true, 2).is_empty());
        }
        assert_eq!(seats.turn_left(0, t), None);
        seats.opted_in.insert(2);
        seats.tick(t, &rules, true, 2);
        assert_eq!(seats.turn_left(0, t), Some(600));
        let keep_active = |seats: &mut Seats, at: Instant| {
            seats.input(
                1,
                0,
                GamepadState {
                    buttons: (at - t0).as_secs() as u16,
                    ..GamepadState::default()
                },
                at,
            );
        };
        keep_active(&mut seats, t + secs(480));
        assert_eq!(
            notices_for(&seats.tick(t + secs(480), &rules, true, 2), 1),
            vec!["2 minutes left of your turn.".to_owned()]
        );
        keep_active(&mut seats, t + secs(570));
        assert_eq!(
            notices_for(&seats.tick(t + secs(570), &rules, true, 2), 1),
            vec!["30 seconds left of your turn.".to_owned()]
        );
        keep_active(&mut seats, t + secs(590));
        let countdown = seats.tick(t + secs(590), &rules, true, 2);
        assert!(matches!(
            countdown.as_slice(),
            [Effect::Notice {
                viewer: 1,
                countdown: Some(10),
                ..
            }]
        ));
        keep_active(&mut seats, t + secs(600));
        seats.tick(t + secs(600), &rules, true, 2);
        assert_eq!(seats.owner[0].map(|o| o.0), Some(2));
        assert_eq!(
            seats.queue(),
            vec![1],
            "the previous player waits for the next turn"
        );
    }

    #[test]
    fn asking_for_more_time_stops_the_clock_until_answered() {
        let t0 = Instant::now();
        let rules = Rules {
            turn: Some(secs(600)),
            ..Rules::default()
        };
        let mut seats = Seats::default();
        seats.claim(1, 0, None, t0).unwrap();
        seats.opted_in.extend([1, 2]);
        seats.start_watching(2, t0);
        seats.tick(t0, &rules, true, 2);
        let id = seats.request_time(1, 15).unwrap();
        assert!(seats.request_time(1, 15).is_none(), "one request at a time");
        for s in 1..=100 {
            seats.input(
                1,
                0,
                GamepadState {
                    buttons: s,
                    ..GamepadState::default()
                },
                t0 + secs(s as u64),
            );
            seats.tick(t0 + secs(s as u64), &rules, true, 2);
        }
        assert_eq!(
            seats.turn_left(0, t0 + secs(100)),
            Some(600),
            "paused while waiting"
        );
        let answer = seats.answer_time(id, true);
        assert_eq!(
            notices_for(&answer, 1),
            vec!["You got 15 more minutes.".to_owned()]
        );
        assert_eq!(seats.turn_left(0, t0 + secs(100)), Some(600 + 900));
    }
}
