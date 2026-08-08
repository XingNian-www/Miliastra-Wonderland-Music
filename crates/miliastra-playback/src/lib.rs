mod catalog;
mod credentials;
mod daemon;
mod domain;
mod engine;
mod login;
mod model;
mod runtime;

pub use catalog::{PlaybackEligibility, ProviderId};
pub use credentials::{CredentialError, CredentialStatus, ProviderCredential};
pub use domain::{EndCause, EngineState, Failure, ResolverLocator, ResolverLocatorError};
pub use model::{
    PlayableTrack, SearchCandidate, SearchQuery, TrackKey, TrackKeyError, TrackMetadata, TrackRef,
};
pub use runtime::{
    LoginSession, LoginStatus, PlaybackError, PlaybackHandle, PlaybackRuntime, PlaybackSnapshot,
};
