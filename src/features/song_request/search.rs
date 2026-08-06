#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CandidateEligibility {
    Eligible,
    #[default]
    Unknown,
    Ineligible,
}

impl CandidateEligibility {
    pub fn from_playerd(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "eligible" => Self::Eligible,
            "ineligible" => Self::Ineligible,
            _ => Self::Unknown,
        }
    }

    pub const fn preference_rank(self) -> u8 {
        match self {
            Self::Eligible => 2,
            Self::Unknown => 1,
            Self::Ineligible => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchCandidate {
    pub text: String,
    pub uri: String,
    #[serde(default)]
    pub eligibility: CandidateEligibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver_locator: Option<String>,
}

impl SearchCandidate {
    pub fn new(text: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            uri: uri.into(),
            eligibility: CandidateEligibility::Unknown,
            resolver_locator: None,
        }
    }

    pub fn with_metadata(
        text: impl Into<String>,
        uri: impl Into<String>,
        eligibility: CandidateEligibility,
        resolver_locator: Option<String>,
    ) -> Self {
        Self {
            text: text.into(),
            uri: uri.into(),
            eligibility,
            resolver_locator,
        }
    }

    pub fn select_preferred_equivalent(candidates: &[Self]) -> Option<Self> {
        let first = candidates.first()?;
        let identity = comparable_identity(&first.text);
        candidates
            .iter()
            .filter(|candidate| comparable_identity(&candidate.text) == identity)
            .max_by_key(|candidate| candidate.eligibility.preference_rank())
            .cloned()
    }
}

fn comparable_identity(text: &str) -> String {
    text.trim()
        .rsplit_once('[')
        .map_or(
            text,
            |(prefix, suffix)| {
                if suffix.ends_with(']') { prefix } else { text }
            },
        )
        .trim()
        .to_ascii_lowercase()
}

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

#[cfg(test)]
mod tests {
    use super::{CandidateEligibility, SearchCandidate};

    #[test]
    fn equivalent_eligible_candidate_wins_without_reordering_distinct_matches() {
        let unknown = SearchCandidate::with_metadata(
            "Canon - Pachelbel [netease]",
            "miliastra://track/netease/1",
            CandidateEligibility::Unknown,
            None,
        );
        let eligible = SearchCandidate::with_metadata(
            "Canon - Pachelbel [netease]",
            "miliastra://track/netease/2",
            CandidateEligibility::Eligible,
            None,
        );
        let distinct = SearchCandidate::with_metadata(
            "Canon in D - Chamber Orchestra [netease]",
            "miliastra://track/netease/3",
            CandidateEligibility::Eligible,
            None,
        );

        assert_eq!(
            SearchCandidate::select_preferred_equivalent(&[
                unknown.clone(),
                eligible.clone(),
                distinct.clone(),
            ]),
            Some(eligible)
        );
        assert_eq!(
            SearchCandidate::select_preferred_equivalent(&[
                unknown.clone(),
                distinct,
                unknown.clone()
            ]),
            Some(unknown)
        );
    }
}
