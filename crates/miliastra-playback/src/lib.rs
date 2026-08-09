mod catalog;
mod core;
mod credentials;
mod domain;
mod engine;
mod login;
mod lyrics;
mod model;
mod runtime;

pub use catalog::{KugouAccountStatus, KugouListenReport, PlaybackEligibility, ProviderId};
pub use credentials::{CredentialError, CredentialStatus, ProviderCredential};
pub use domain::{
    EndBehavior, EndCause, EngineState, Failure, ResolverLocator, ResolverLocatorError,
};
pub use lyrics::{LyricsParseError, TimedLyricLine, TimedLyrics, parse_lrc_pair};
pub use model::{
    PlayableTrack, SearchCandidate, SearchQuery, TrackKey, TrackKeyError, TrackMetadata, TrackRef,
};
pub use runtime::{
    LoginOperation, LoginOperationWaitError, LoginSession, LoginStatus, PlaybackError,
    PlaybackHandle, PlaybackOperation, PlaybackRuntime, PlaybackSnapshot,
};
