pub use miliastra_playback::{PlaybackEligibility as CandidateEligibility, SearchCandidate};

/// 一次平台搜索的最终选中结果。
///
/// 三份数据是同一次搜索的三个视图，必须一起传递：`candidate` 是被选中的那一首，
/// `candidate_snapshot` 是本次搜索的完整候选列表（`@N` 重选与换源都从这里取），
/// `formatted_candidates` 是给玩家看的编号文案。任何一份单独传都会在某个
/// 调用点被迫重新拼一次，所以它们绑在同一个结构里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickedCandidate {
    pub candidate: SearchCandidate,
    pub candidate_snapshot: Vec<SearchCandidate>,
    /// 搜索词要用的规范化歌名。
    ///
    /// 播放链路把它当成「最终关键词」：解析出的音源与用户输入往往不一致
    /// （别名、伴奏、罗马音），后续换源与去重都必须以这份文本为准。
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

    /// 可直接交给播放器的曲目。
    ///
    /// 播放链路只关心「播哪首」，不关心它是从哪个候选列表里选出来的；
    /// 需要时从选中项派生，避免再造一份 `(text, track, snapshot)` 的平行结构。
    pub fn playable_track(&self) -> miliastra_playback::PlayableTrack {
        self.candidate.playable_track()
    }

    /// 播放链路使用的规范化关键词，即 [`Self::formatted_candidates`]。
    pub fn keyword(&self) -> &str {
        &self.formatted_candidates
    }

    /// 本次搜索的完整候选列表。
    pub fn snapshot(&self) -> &[SearchCandidate] {
        &self.candidate_snapshot
    }
}
