//! How far a stalled encoder's recovery escalates while no access unit arrives. AMF, QSV and
//! Media Foundation keep their context across a reset and rebuild in place first; a second
//! reset with no access unit between drops the context instead, because a re-Init on the same
//! context cannot clear a dead hardware session. Pure, so its table runs wherever this crate
//! builds.

/// Where a backend's stall recovery stands since its last access unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stall {
    /// An access unit came after the last reset, or no reset came yet.
    #[default]
    Clear,
    /// A reset came, and no access unit since.
    Reset,
}

/// What a reset does to the encoder's context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rebuild {
    /// Re-Init on the same context.
    InPlace,
    /// Drop the context; the next submit opens a fresh one.
    Reopen,
}

impl Stall {
    /// A reset: the first since an access unit rebuilds in place, any later one reopens.
    pub fn reset(self) -> (Stall, Rebuild) {
        let rebuild = match self {
            Stall::Clear => Rebuild::InPlace,
            Stall::Reset => Rebuild::Reopen,
        };
        (Stall::Reset, rebuild)
    }
}

#[cfg(test)]
mod tests {
    use super::{Rebuild, Stall};

    /// Events: `r` a reset, `o` an access unit. Rebuilds: `I` in place, `R` reopen.
    #[test]
    fn a_second_reset_without_output_reopens() {
        for (events, rebuilds) in [
            ("r", "I"),
            ("rr", "IR"),
            ("rrr", "IRR"),
            ("ror", "II"),
            ("rror", "IRI"),
            ("orr", "IR"),
        ] {
            let mut stall = Stall::default();
            let mut seen = String::new();
            for e in events.chars() {
                if e == 'o' {
                    stall = Stall::Clear;
                    continue;
                }
                let rebuild;
                (stall, rebuild) = stall.reset();
                seen.push(match rebuild {
                    Rebuild::InPlace => 'I',
                    Rebuild::Reopen => 'R',
                });
            }
            assert_eq!(seen, rebuilds, "{events}");
        }
    }
}
