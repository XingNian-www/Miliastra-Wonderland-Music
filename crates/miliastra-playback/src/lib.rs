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
    AudioCache, AudioCacheConfig, AudioCacheStats, AudioCacheTrackStatus, CachedTrackInfo,
    CachedTrackPage, DEFAULT_MAX_REGISTRY_ENTRIES,
};
pub use catalog::{
    KugouAccountStatus, KugouListenReport, PlaybackEligibility, ProviderAccountStatus, ProviderId,
    kugou_calculate_mid, kugou_normalize_guid, kugou_register_device,
};
pub use credentials::{CredentialError, CredentialStatus, ProviderCredential};
pub use domain::{
    EndBehavior, EndCause, EngineState, Failure, ResolverLocator, ResolverLocatorError, SongKey,
    StreamSource,
};
pub use lyrics::{LyricsParseError, TimedLyricLine, TimedLyrics, parse_lrc_pair};
pub use model::{
    PlayableTrack, SearchCandidate, SearchQuery, TrackKey, TrackKeyError, TrackMetadata, TrackRef,
};
pub use runtime::{
    LoginOperation, LoginOperationWaitError, LoginSession, LoginStatus, PlaybackError,
    PlaybackHandle, PlaybackOperation, PlaybackRuntime, PlaybackSnapshot,
};
