//! Port of `Services/LastFm/RadioTuneInSelector.cs`.
//!
//! Picks where a tune-in starts inside a station snapshot. Every listen used to open with the
//! same three cached tracks in snapshot order, which is most of why a station felt like the same
//! handful of songs on repeat.

pub trait IRadioTuneInSelector: Send + Sync {
    /// Index into the station's candidates to start scanning for cached tracks.
    fn start(&self, candidate_count: usize) -> usize;
}

/// `Random.Shared.Next(candidateCount)`.
#[derive(Debug, Default)]
pub struct RandomRadioTuneInSelector;

impl IRadioTuneInSelector for RandomRadioTuneInSelector {
    fn start(&self, candidate_count: usize) -> usize {
        if candidate_count == 0 {
            0
        } else {
            (rand::random::<u64>() % candidate_count as u64) as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_is_inside_the_candidates() {
        let selector = RandomRadioTuneInSelector;
        assert_eq!(selector.start(0), 0);
        assert!((0..100).all(|_| selector.start(3) < 3));
    }
}
