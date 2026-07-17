#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SearchCandidate {
    pub text: String,
    pub uri: String,
}

impl SearchCandidate {
    pub fn new(text: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            uri: uri.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickedCandidate {
    pub candidate: SearchCandidate,
    pub formatted_candidates: String,
}

impl PickedCandidate {
    pub fn new(candidate: SearchCandidate, formatted_candidates: impl Into<String>) -> Self {
        Self {
            candidate,
            formatted_candidates: formatted_candidates.into(),
        }
    }
}
