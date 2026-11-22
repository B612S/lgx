//! Decoded picture buffer slot planning.
//!
//! The reference stream is low-delay (no reordering), so a handful of slots and
//! the rule "reuse a slot that no current reference needs" cover it. Extra
//! slots matter for the zero-copy client: a picture the renderer still holds
//! must not be rewritten, so the decoder keeps several pictures alive at once.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dpb {
    pub poc: [i32; SLOTS],
    pub occupied: [bool; SLOTS],
}

/// DPB slots. A low-delay stream needs two references plus the picture being
/// decoded; the rest let the client hold a few exported pictures while the
/// decoder keeps running.
pub const SLOTS: usize = 6;

impl Default for Dpb {
    fn default() -> Self {
        Self {
            poc: [0; SLOTS],
            occupied: [false; SLOTS],
        }
    }
}

impl Dpb {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every picture (new IDR).
    pub fn reset(&mut self) {
        self.poc = [0; SLOTS];
        self.occupied = [false; SLOTS];
    }

    /// Picks the slot for the next picture. `ref_pocs` are the POCs the next
    /// picture references and must stay intact; `held` slots are still used by
    /// the client's renderer and must not be written either.
    pub fn choose(&self, ref_pocs: &[i32], held: &[bool; SLOTS]) -> Option<usize> {
        if let Some(free) = (0..SLOTS).find(|s| !self.occupied[*s] && !held[*s]) {
            return Some(free);
        }
        (0..SLOTS)
            .filter(|s| !held[*s] && !ref_pocs.contains(&self.poc[*s]))
            .min_by_key(|s| self.poc[*s])
    }

    pub fn commit(&mut self, slot: usize, poc: i32) {
        self.poc[slot] = poc;
        self.occupied[slot] = true;
    }

    pub fn slot_of(&self, poc: i32) -> Option<usize> {
        (0..SLOTS).find(|s| self.occupied[*s] && self.poc[*s] == poc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held_none() -> [bool; SLOTS] {
        [false; SLOTS]
    }

    #[test]
    fn low_delay_ping_pong() {
        let mut dpb = Dpb::new();
        let none = held_none();
        let s = dpb.choose(&[], &none).unwrap();
        dpb.commit(s, 0);
        for poc in 1..=12 {
            let slot = dpb.choose(&[poc - 1], &none).unwrap();
            assert_ne!(dpb.slot_of(poc - 1), Some(slot), "reference was overwritten");
            dpb.commit(slot, poc);
        }
        // Six slots cycle through every slot.
        assert_eq!(dpb.slot_of(12), Some(0));
    }

    #[test]
    fn idr_reset_clears_references() {
        let mut dpb = Dpb::new();
        let none = held_none();
        dpb.commit(0, 0);
        dpb.commit(1, 1);
        dpb.reset();
        assert!(dpb.slot_of(0).is_none());
        assert_eq!(dpb.choose(&[], &none), Some(0));
    }

    #[test]
    fn held_slots_are_never_chosen() {
        let mut dpb = Dpb::new();
        for (slot, poc) in (0..SLOTS as i32).map(|i| (i as usize, i)) {
            dpb.commit(slot, poc);
        }
        let mut held = held_none();
        for slot in 0..SLOTS - 1 {
            held[slot] = true;
        }
        assert_eq!(dpb.choose(&[], &held), Some(SLOTS - 1));
        // Every slot held -> no choice, so the caller has to wait.
        held[SLOTS - 1] = true;
        assert_eq!(dpb.choose(&[], &held), None);
    }
}
