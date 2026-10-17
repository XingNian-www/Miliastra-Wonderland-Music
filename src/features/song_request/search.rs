pub use miliastra_playback::{PlaybackEligibility as CandidateEligibility, SearchCandidate};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickedCandidate {
    pub candidate: SearchCandidate,
    pub candidate_snapshot: Vec<SearchCandidate>,
    pub formatted_candidates: String,
}

impl PickedCandidate {
    pub fn new(candidate: SearchCandidate, formatted_candidates: impl Into<String>) -> Self {
        Self {
            candidate_snapshot: vec![candidate.clone()],
            candidate,
            formatted_candidates: formatted_candidates.into(),
        }
    }

    pub fn with_snapshot(
        candidate: SearchCandidate,
        candidate_snapshot: Vec<SearchCandidate>,
        formatted_candidates: impl Into<String>,
    ) -> Self {
        Self {
            candidate,
            candidate_snapshot,
            formatted_candidates: formatted_candidates.into(),
        }
    }
}
