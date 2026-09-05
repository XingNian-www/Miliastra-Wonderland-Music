mod cache;
mod catalog;
mod core;
mod credentials;
mod domain;
mod engine;
mod login;
mod lyrics;
mod model;
mod runtime;

pub use cache::{
    AudioCache, AudioCacheConfig, AudioCacheStats, AudioCacheTrackStatus, CacheTrackSortKey,
    CachedTrackInfo, CachedTrackPage, DEFAULT_MAX_REGISTRY_ENTRIES,
};
pub use catalog::{
    BilibiliAdapter, KugouAccountStatus, KugouAdapter, KugouListenReport, PlaybackEligibility,
    ProviderAccountStatus, ProviderId, SourceAdapter, bilibili_is_bvid, bilibili_normalize_bvid,
    kugou_calculate_mid, kugou_normalize_guid, kugou_register_device,
};
pub use credentials::{CredentialError, CredentialStatus, CredentialStore, ProviderCredential};
pub use domain::{
    EndBehavior, EndCause, EngineState, Failure, ResolverLocator, ResolverLocatorError, SongKey,
    StreamSource,
};
pub use lyrics::{
    LyricsParseError, MAX_LYRICS_LEAD_SECONDS, TimedLyricLine, TimedLyrics, parse_lrc_pair,
};
pub use model::{
    PlayableTrack, SearchCandidate, SearchQuery, TrackKey, TrackKeyError, TrackMetadata, TrackRef,
};
pub use runtime::{
    LoginOperation, LoginOperationWaitError, LoginSession, LoginStatus, PlaybackError,
    PlaybackHandle, PlaybackOperation, PlaybackRuntime, PlaybackSnapshot,
};
