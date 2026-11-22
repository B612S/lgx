//! Decoded picture buffer slot planning.
//!
//! The reference stream is low-delay (no reordering), so two slots and the
//! rule "reuse a slot that no current reference needs" cover it.

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dpb {
    pub poc: [i32; 2],
    pub occupied: [bool; 2],
}

impl Dpb {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every picture (new IDR).
    pub fn reset(&mut self) {
        self.poc = [0; 2];
        self.occupied = [false; 2];
    }

    /// Picks the slot for the next picture. `ref_pocs` are the POCs the next
    /// picture references and must stay intact.
    pub fn choose(&self, ref_pocs: &[i32]) -> Option<usize> {
        if let Some(free) = (0..2).find(|s| !self.occupied[*s]) {
            return Some(free);
        }
        (0..2).find(|s| !ref_pocs.contains(&self.poc[*s]))
    }

    pub fn commit(&mut self, slot: usize, poc: i32) {
        self.poc[slot] = poc;
        self.occupied[slot] = true;
    }

    pub fn slot_of(&self, poc: i32) -> Option<usize> {
        (0..2).find(|s| self.occupied[*s] && self.poc[*s] == poc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_delay_ping_pong() {
        let mut dpb = Dpb::new();
        let mut pocs = Vec::new();
        // IDR POC 0 then POCs 1..=5, each referencing the previous picture.
        let s = dpb.choose(&[]).unwrap();
        dpb.commit(s, 0);
        pocs.push((0, s));
        for poc in 1..=5 {
            let slot = dpb.choose(&[poc - 1]).unwrap();
            assert_ne!(dpb.slot_of(poc - 1), Some(slot), "reference was overwritten");
            dpb.commit(slot, poc);
            pocs.push((poc, slot));
        }
        assert_eq!(pocs, [(0, 0), (1, 1), (2, 0), (3, 1), (4, 0), (5, 1)]);
        assert_eq!(dpb.slot_of(5), Some(1));
    }

    #[test]
    fn idr_reset_clears_references() {
        let mut dpb = Dpb::new();
        dpb.commit(0, 0);
        dpb.commit(1, 1);
        dpb.reset();
        assert!(dpb.slot_of(0).is_none());
        assert_eq!(dpb.choose(&[]), Some(0));
    }
}
