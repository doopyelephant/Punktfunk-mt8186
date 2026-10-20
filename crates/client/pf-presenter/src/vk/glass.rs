//! The glass instruments' lifecycle per swapchain, decided in one place. The presenter
//! sends an [`Event`] as a swapchain is made, retires, drains or overflows the engine's
//! result queue; [`lifecycle_step`] answers the [`Act`] the clock performs: arm the
//! `VK_EXT_present_timing` engine on the new swapchain, drain the present-wait waiter,
//! disarm the engine.
//!
//! The engine's count of stamps asked and not yet reported moves on two threads (the
//! presenter asks, the waiter's poll reads the reports), so it lives in the engine;
//! [`asks_after`] is its rule. The waiter polls only while that count is above zero.
//!
//! Pure: built and tested on every platform. Pinned by the table tests below.

/// Where the engine stands with the live swapchain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lifecycle {
    /// No swapchain armed. `stamps`: the next one is made for engine stamps and armed.
    Unarmed { stamps: bool },
    /// The engine stamps the presents on `swapchain` (its raw handle).
    Armed { swapchain: u64 },
    /// The swapchain retires; nothing is armed until the waiter has drained it.
    Draining { stamps: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// A swapchain was made (its raw handle).
    Created(u64),
    /// The live swapchain goes: a rebuild retires it, or the native lane takes the window.
    Retiring,
    /// No wait names the retired swapchain any more.
    Drained,
    /// A present asked into a full result queue: no stamps from here on.
    QueueFull,
    /// A present asked the engine for a stamp.
    Asked,
    /// The engine reported on a present that asked.
    Reported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Act {
    /// Size the swapchain's result queue and pick its clock.
    Arm(u64),
    /// Forget the stamps asked for.
    Disarm,
    /// Wait out the waiter and drop the unclaimed present.
    Drain,
    Keep,
}

impl Lifecycle {
    /// A swapchain made now asks for engine stamps.
    pub(crate) fn stamps(self) -> bool {
        match self {
            Lifecycle::Unarmed { stamps } | Lifecycle::Draining { stamps } => stamps,
            Lifecycle::Armed { .. } => true,
        }
    }

    pub(crate) fn armed(self) -> bool {
        matches!(self, Lifecycle::Armed { .. })
    }
}

pub(crate) fn lifecycle_step(s: Lifecycle, e: Event) -> (Lifecycle, Act) {
    use Lifecycle::*;
    match e {
        Event::Created(_) if matches!(s, Draining { .. }) => (s, Act::Keep),
        Event::Created(swapchain) if s.stamps() => (Armed { swapchain }, Act::Arm(swapchain)),
        Event::Created(_) => (s, Act::Keep),
        Event::Retiring => (Draining { stamps: s.stamps() }, Act::Drain),
        Event::Drained => (Unarmed { stamps: s.stamps() }, Act::Disarm),
        Event::QueueFull => match s {
            Draining { .. } => (Draining { stamps: false }, Act::Disarm),
            _ => (Unarmed { stamps: false }, Act::Disarm),
        },
        Event::Asked | Event::Reported => (s, Act::Keep),
    }
}

/// The stamps asked and not yet reported, after `e`: an ask counts one, a report takes one
/// back and never goes below zero; a new, retired or overflowed swapchain forgets them.
pub(crate) fn asks_after(asked: u32, e: Event) -> u32 {
    match e {
        Event::Asked => asked.saturating_add(1),
        Event::Reported => asked.saturating_sub(1),
        Event::Created(_) | Event::Retiring | Event::Drained | Event::QueueFull => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Act::*;
    use Event::*;
    use Lifecycle::*;

    const OLD: u64 = 0x10;
    const NEW: u64 = 0x20;

    #[test]
    fn the_engine_follows_each_swapchain() {
        let on = Unarmed { stamps: true };
        let off = Unarmed { stamps: false };
        let armed = Armed { swapchain: OLD };
        #[rustfmt::skip]
        let rows: &[(&str, Lifecycle, Event, Lifecycle, Act)] = &[
            ("a created swapchain arms", on, Created(NEW), Armed { swapchain: NEW }, Arm(NEW)),
            ("without the engine a swapchain arms nothing", off, Created(NEW), off, Keep),
            ("retiring drains the waiter", armed, Retiring, Draining { stamps: true }, Drain),
            ("an unarmed swapchain drains too", off, Retiring, Draining { stamps: false }, Drain),
            ("drained disarms", Draining { stamps: true }, Drained, on, Disarm),
            ("a created swapchain waits for the drain", Draining { stamps: true }, Created(NEW), Draining { stamps: true }, Keep),
            ("R2-3: a full queue disarms", armed, QueueFull, off, Disarm),
            ("R2-3: an unarmed swapchain takes no stamps", off, Created(NEW), off, Keep),
            ("R2-3: a full queue mid-drain keeps draining", Draining { stamps: true }, QueueFull, Draining { stamps: false }, Disarm),
            ("an ask keeps the swapchain armed", armed, Asked, armed, Keep),
            ("a report keeps the swapchain armed", armed, Reported, armed, Keep),
            ("a report for an unarmed swapchain is no word", on, Reported, on, Keep),
        ];
        for &(name, from, event, to, act) in rows {
            assert_eq!(lifecycle_step(from, event), (to, act), "{name}");
        }
    }

    /// (events, acts performed, end state) from an armed swapchain.
    type Run = (&'static str, &'static [Event], &'static [Act], Lifecycle);

    #[test]
    fn a_rebuild_drains_then_arms() {
        #[rustfmt::skip]
        let runs: &[Run] = &[
            ("a rebuild drains, then arms the new swapchain",
                &[Retiring, Drained, Created(NEW)], &[Drain, Disarm, Arm(NEW)], Armed { swapchain: NEW }),
            ("the lane takes the window: drained, nothing armed",
                &[Retiring, Drained], &[Drain, Disarm], Unarmed { stamps: true }),
            ("R2-3: after a full queue the rebuilt swapchain asks for no stamps",
                &[QueueFull, Retiring, Drained, Created(NEW)], &[Disarm, Drain, Disarm, Keep], Unarmed { stamps: false }),
        ];
        for &(name, events, acts, end) in runs {
            let mut s = Armed { swapchain: OLD };
            let mut done = Vec::new();
            for &e in events {
                let (next, act) = lifecycle_step(s, e);
                s = next;
                done.push(act);
            }
            assert_eq!((done.as_slice(), s), (acts, end), "{name}");
        }
    }

    #[test]
    fn asks_count_until_reported() {
        #[rustfmt::skip]
        let rows: &[(&str, u32, Event, u32)] = &[
            ("an ask counts", 2, Asked, 3),
            ("a report takes one back", 3, Reported, 2),
            ("R2-3: never below zero", 0, Reported, 0),
            ("a new swapchain starts at zero", 5, Created(NEW), 0),
            ("a full queue forgets the asks", 5, QueueFull, 0),
            ("a drained swapchain forgets the asks", 5, Drained, 0),
        ];
        for &(name, asked, event, want) in rows {
            assert_eq!(asks_after(asked, event), want, "{name}");
        }
    }

    /// The waiter polls the engine only while asks are outstanding: after a disarm, a
    /// report the swapchain still owes leaves nothing to poll for.
    #[test]
    fn a_disarmed_engine_is_not_polled() {
        let asked = asks_after(asks_after(4, QueueFull), Reported);
        assert_eq!(asked, 0);
    }
}
