use std::time::{Duration, Instant};

use super::clock::Clock;

pub const DEFAULT_PLAYER_STABILITY_SAMPLES: usize = 2;
pub const DEFAULT_PLAYER_STALE_TIMEOUT: Duration = Duration::from_secs(5);

const DEFAULT_RESTART_PREVIOUS_PROGRESS: Duration = Duration::from_secs(10);
const DEFAULT_RESTART_NEAR_START: Duration = Duration::from_secs(3);
const DEFAULT_RESTART_MINIMUM_DROP: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportState {
    Playing,
    Paused,
    Stopped,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawPlayerSample {
    pub uri: Option<String>,
    pub transport: Option<TransportState>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_name: Option<String>,
    pub lyric_line_text: Option<String>,
    pub progress: Option<Duration>,
    pub duration: Option<Duration>,
    pub playback_rate: Option<f64>,
    pub volume: Option<i64>,
}

impl RawPlayerSample {
    pub fn new(uri: impl Into<String>, transport: TransportState) -> Self {
        Self {
            uri: Some(uri.into()),
            transport: Some(transport),
            ..Self::default()
        }
    }

    pub fn is_complete(&self) -> bool {
        normalized_uri(self.uri.as_deref()).is_some() && self.transport.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlaybackInstance(u64);

impl PlaybackInstance {
    pub const INITIAL: Self = Self(1);

    pub const fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationFreshness {
    Fresh,
    Stale { age: Duration },
    Unknown,
}

impl ObservationFreshness {
    pub const fn is_fresh(self) -> bool {
        matches!(self, Self::Fresh)
    }

    pub const fn is_confirmable(self) -> bool {
        self.is_fresh()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StabilityEvidence<T> {
    pub value: T,
    pub consecutive_samples: usize,
    pub required_samples: usize,
    pub confirmed_at: Option<Instant>,
    pub last_sampled_at: Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlayerObservation {
    pub evaluated_at: Instant,
    pub last_attempted_at: Option<Instant>,
    pub last_successful_observed_at: Option<Instant>,
    pub uri: Option<String>,
    pub uri_freshness: ObservationFreshness,
    pub uri_evidence: Option<StabilityEvidence<String>>,
    pub uri_candidate: Option<StabilityEvidence<String>>,
    pub transport: Option<TransportState>,
    pub transport_freshness: ObservationFreshness,
    pub transport_evidence: Option<StabilityEvidence<TransportState>>,
    pub transport_candidate: Option<StabilityEvidence<TransportState>>,
    pub playback_instance: Option<PlaybackInstance>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_name: Option<String>,
    pub lyric_line_text: Option<String>,
    pub progress: Option<Duration>,
    pub duration: Option<Duration>,
    pub playback_rate: Option<f64>,
    pub volume: Option<i64>,
    pub sampled_at: Option<Instant>,
}

impl PlayerObservation {
    pub fn confirms_uri(&self, expected: &str) -> bool {
        self.uri_freshness.is_confirmable()
            && normalized_uri(Some(expected))
                .zip(self.uri.as_deref())
                .is_some_and(|(expected, actual)| expected == actual)
    }

    /// Re-evaluates a cached snapshot without claiming that another player RPC occurred.
    ///
    /// Stable evidence older than `stale_timeout` is removed. A still-fresh playing snapshot
    /// advances its displayed progress from the prior evaluation time, while stale or incomplete
    /// observations keep their last sampled progress.
    pub fn reevaluated_at(&self, now: Instant, stale_timeout: Duration) -> Self {
        let mut reevaluated = self.clone();
        reevaluated.evaluated_at = now;

        let uri_expired = reevaluate_stable_field(
            &mut reevaluated.uri,
            &mut reevaluated.uri_freshness,
            &mut reevaluated.uri_evidence,
            now,
            stale_timeout,
        );
        if uri_expired {
            reevaluated.uri_candidate = None;
        } else {
            expire_candidate(&mut reevaluated.uri_candidate, now, stale_timeout);
        }

        let transport_expired = reevaluate_stable_field(
            &mut reevaluated.transport,
            &mut reevaluated.transport_freshness,
            &mut reevaluated.transport_evidence,
            now,
            stale_timeout,
        );
        if transport_expired {
            reevaluated.transport_candidate = None;
        } else {
            expire_candidate(&mut reevaluated.transport_candidate, now, stale_timeout);
        }

        if reevaluated.uri.is_none() {
            reevaluated.playback_instance = None;
            reevaluated.title = None;
            reevaluated.artist = None;
            reevaluated.album_name = None;
            reevaluated.lyric_line_text = None;
            reevaluated.progress = None;
            reevaluated.duration = None;
            reevaluated.playback_rate = None;
            reevaluated.volume = None;
            reevaluated.sampled_at = None;
        } else if reevaluated.uri_freshness.is_fresh()
            && reevaluated.transport_freshness.is_fresh()
            && reevaluated.transport == Some(TransportState::Playing)
            && let Some(progress) = reevaluated.progress
        {
            let elapsed = now.saturating_duration_since(self.evaluated_at);
            let estimated = progress.saturating_add(saturating_duration_mul(
                elapsed,
                reevaluated.playback_rate.unwrap_or(1.0),
            ));
            reevaluated.progress = Some(
                reevaluated
                    .duration
                    .map_or(estimated, |duration| estimated.min(duration)),
            );
        }

        reevaluated
    }
}

fn reevaluate_stable_field<T>(
    value: &mut Option<T>,
    freshness: &mut ObservationFreshness,
    evidence: &mut Option<StabilityEvidence<T>>,
    now: Instant,
    stale_timeout: Duration,
) -> bool {
    let Some(last_sampled_at) = evidence.as_ref().map(|evidence| evidence.last_sampled_at) else {
        let expired = value.is_some();
        *value = None;
        *freshness = ObservationFreshness::Unknown;
        return expired;
    };
    let age = now.saturating_duration_since(last_sampled_at);
    if age > stale_timeout {
        *value = None;
        *freshness = ObservationFreshness::Unknown;
        *evidence = None;
        return true;
    }
    if matches!(freshness, ObservationFreshness::Stale { .. }) {
        *freshness = ObservationFreshness::Stale { age };
    }
    false
}

fn expire_candidate<T>(
    candidate: &mut Option<StabilityEvidence<T>>,
    now: Instant,
    stale_timeout: Duration,
) {
    if candidate.as_ref().is_some_and(|candidate| {
        now.saturating_duration_since(candidate.last_sampled_at) > stale_timeout
    }) {
        *candidate = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerObservationConfig {
    pub uri_stable_samples: usize,
    pub transport_stable_samples: usize,
    pub stale_timeout: Duration,
    pub restart_previous_progress: Duration,
    pub restart_near_start: Duration,
    pub restart_minimum_drop: Duration,
}

impl Default for PlayerObservationConfig {
    fn default() -> Self {
        Self {
            uri_stable_samples: DEFAULT_PLAYER_STABILITY_SAMPLES,
            transport_stable_samples: DEFAULT_PLAYER_STABILITY_SAMPLES,
            stale_timeout: DEFAULT_PLAYER_STALE_TIMEOUT,
            restart_previous_progress: DEFAULT_RESTART_PREVIOUS_PROGRESS,
            restart_near_start: DEFAULT_RESTART_NEAR_START,
            restart_minimum_drop: DEFAULT_RESTART_MINIMUM_DROP,
        }
    }
}

impl PlayerObservationConfig {
    fn normalized(mut self) -> Self {
        self.uri_stable_samples = normalized_stability_count(self.uri_stable_samples);
        self.transport_stable_samples = normalized_stability_count(self.transport_stable_samples);
        self
    }
}

#[derive(Clone, Debug)]
struct Candidate<T> {
    value: T,
    consecutive_samples: usize,
    last_sampled_at: Instant,
}

#[derive(Clone, Debug)]
struct StableField<T> {
    value: T,
    consecutive_samples: usize,
    confirmed_at: Instant,
    last_sampled_at: Instant,
    fresh: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct UriUpdate {
    accepted_new: bool,
    identity_changed: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct TransportUpdate {
    accepted_new: bool,
    restarted: bool,
}

#[derive(Clone, Debug, Default)]
struct TrackEvidence {
    title: Option<String>,
    artist: Option<String>,
    album_name: Option<String>,
    lyric_line_text: Option<String>,
    progress: Option<Duration>,
    duration: Option<Duration>,
    playback_rate: Option<f64>,
    volume: Option<i64>,
    sampled_at: Option<Instant>,
    progress_sampled_at: Option<Instant>,
}

impl TrackEvidence {
    fn update(&mut self, sample: &RawPlayerSample, sampled_at: Instant) {
        if let Some(title) = normalized_owned(sample.title.clone()) {
            self.title = Some(title);
        }
        if let Some(artist) = normalized_owned(sample.artist.clone()) {
            self.artist = Some(artist);
        }
        if let Some(album_name) = normalized_owned(sample.album_name.clone()) {
            self.album_name = Some(album_name);
        }
        if let Some(lyric_line_text) = sample.lyric_line_text.as_ref() {
            self.lyric_line_text = normalized_owned(Some(lyric_line_text.clone()));
        }
        if let Some(progress) = sample.progress {
            self.progress = Some(progress);
            self.progress_sampled_at = Some(sampled_at);
        }
        if let Some(duration) = sample.duration {
            self.duration = Some(duration);
        }
        if let Some(playback_rate) = sample
            .playback_rate
            .filter(|rate| rate.is_finite() && *rate > 0.0)
        {
            self.playback_rate = Some(playback_rate);
        }
        if let Some(volume) = sample.volume {
            self.volume = Some(volume);
        }
        self.sampled_at = Some(sampled_at);
    }
}

/// Converts raw player samples into independently stabilized URI and transport observations.
///
/// Request failures must be reported with [`Self::observe_failure`]. A successful but partial
/// response is still useful: each present field advances only its own stability counter.
pub struct PlayerObserver<C> {
    clock: C,
    config: PlayerObservationConfig,
    stable_uri: Option<StableField<String>>,
    uri_candidate: Option<Candidate<String>>,
    stable_transport: Option<StableField<TransportState>>,
    transport_candidate: Option<Candidate<TransportState>>,
    evidence: TrackEvidence,
    playback_instance: Option<PlaybackInstance>,
    last_identity_uri: Option<String>,
    last_transport: Option<TransportState>,
    last_attempted_at: Option<Instant>,
    last_successful_observed_at: Option<Instant>,
    pending_transport_restart: bool,
    identity_waiting_for_transport: bool,
}

impl<C: Clock> PlayerObserver<C> {
    pub fn new(clock: C, config: PlayerObservationConfig) -> Self {
        Self {
            clock,
            config: config.normalized(),
            stable_uri: None,
            uri_candidate: None,
            stable_transport: None,
            transport_candidate: None,
            evidence: TrackEvidence::default(),
            playback_instance: None,
            last_identity_uri: None,
            last_transport: None,
            last_attempted_at: None,
            last_successful_observed_at: None,
            pending_transport_restart: false,
            identity_waiting_for_transport: false,
        }
    }

    pub fn observe_sample(&mut self, sample: RawPlayerSample) -> PlayerObservation {
        let now = self.clock.now();
        self.expire_stale_fields(now);
        self.last_attempted_at = Some(now);
        self.last_successful_observed_at = Some(now);

        let raw_uri = normalized_uri(sample.uri.as_deref()).map(str::to_owned);
        let prior_progress = raw_uri
            .as_deref()
            .filter(|uri| {
                self.stable_uri
                    .as_ref()
                    .is_some_and(|stable| stable.value == *uri)
            })
            .and(self.evidence.progress);

        let uri_update = self.observe_uri(raw_uri.as_deref(), now);
        if uri_update.accepted_new {
            self.evidence = TrackEvidence::default();
        }
        let transport_update = self.observe_transport(sample.transport, now);
        let progress_restarted = !uri_update.identity_changed
            && raw_uri.as_deref().is_some_and(|uri| {
                self.stable_uri
                    .as_ref()
                    .is_some_and(|stable| stable.fresh && stable.value == uri)
            })
            && prior_progress
                .zip(sample.progress)
                .is_some_and(|(before, after)| self.progress_restarted(before, after));

        let accepted_current_uri = raw_uri.as_deref().is_some_and(|uri| {
            self.stable_uri
                .as_ref()
                .is_some_and(|stable| stable.fresh && stable.value == uri)
        });
        if accepted_current_uri {
            self.evidence.update(&sample, now);
        }

        let has_fresh_track = self.stable_uri.as_ref().is_some_and(|stable| stable.fresh);
        let transport_is_fresh = self
            .stable_transport
            .as_ref()
            .is_some_and(|stable| stable.fresh);
        if uri_update.identity_changed && !transport_is_fresh {
            self.identity_waiting_for_transport = true;
        }

        let mut advance_instance = uri_update.identity_changed || progress_restarted;
        if transport_update.accepted_new && self.identity_waiting_for_transport {
            self.identity_waiting_for_transport = false;
        } else if transport_update.restarted {
            if self.identity_waiting_for_transport {
                self.identity_waiting_for_transport = false;
            } else if has_fresh_track {
                advance_instance = true;
            } else {
                self.pending_transport_restart = true;
            }
        }
        if has_fresh_track && self.pending_transport_restart {
            self.pending_transport_restart = false;
            advance_instance = true;
        }

        if has_fresh_track {
            if self.playback_instance.is_none() {
                self.playback_instance = Some(PlaybackInstance::INITIAL);
                self.pending_transport_restart = false;
            } else if advance_instance {
                self.playback_instance = self.playback_instance.map(PlaybackInstance::next);
            }
        }

        self.snapshot_at(now)
    }

    pub fn observe_failure(&mut self) -> PlayerObservation {
        let now = self.clock.now();
        self.expire_stale_fields(now);
        self.last_attempted_at = Some(now);
        self.mark_uri_stale();
        self.mark_transport_stale();
        self.snapshot_at(now)
    }

    pub fn current(&mut self) -> PlayerObservation {
        let now = self.clock.now();
        self.expire_stale_fields(now);
        self.snapshot_at(now)
    }

    fn observe_uri(&mut self, uri: Option<&str>, now: Instant) -> UriUpdate {
        let Some(uri) = uri else {
            self.mark_uri_stale();
            return UriUpdate::default();
        };

        if self
            .stable_uri
            .as_ref()
            .is_some_and(|stable| stable.value == uri)
        {
            if let Some(stable) = self.stable_uri.as_mut() {
                stable.last_sampled_at = now;
                stable.fresh = true;
            }
            self.uri_candidate = None;
            return UriUpdate::default();
        }

        if let Some(stable) = self.stable_uri.as_mut() {
            stable.fresh = false;
        }
        let accepted = advance_candidate(
            &mut self.uri_candidate,
            uri.to_owned(),
            self.config.uri_stable_samples,
            now,
        );
        if !accepted {
            return UriUpdate::default();
        }

        let candidate = self
            .uri_candidate
            .take()
            .expect("accepted URI candidate must exist");
        let identity_changed = self
            .last_identity_uri
            .as_deref()
            .is_some_and(|previous| previous != candidate.value);
        self.last_identity_uri = Some(candidate.value.clone());
        self.stable_uri = Some(StableField {
            value: candidate.value,
            consecutive_samples: candidate.consecutive_samples,
            confirmed_at: now,
            last_sampled_at: candidate.last_sampled_at,
            fresh: true,
        });
        UriUpdate {
            accepted_new: true,
            identity_changed,
        }
    }

    fn observe_transport(
        &mut self,
        transport: Option<TransportState>,
        now: Instant,
    ) -> TransportUpdate {
        let Some(transport) = transport else {
            self.mark_transport_stale();
            return TransportUpdate::default();
        };

        if self
            .stable_transport
            .as_ref()
            .is_some_and(|stable| stable.fresh && stable.value == transport)
        {
            if let Some(stable) = self.stable_transport.as_mut() {
                stable.last_sampled_at = now;
            }
            self.transport_candidate = None;
            return TransportUpdate::default();
        }

        if let Some(stable) = self.stable_transport.as_mut() {
            stable.fresh = false;
        }
        let accepted = advance_candidate(
            &mut self.transport_candidate,
            transport,
            self.config.transport_stable_samples,
            now,
        );
        if !accepted {
            return TransportUpdate::default();
        }

        let candidate = self
            .transport_candidate
            .take()
            .expect("accepted transport candidate must exist");
        let restarted = self.last_transport == Some(TransportState::Stopped)
            && candidate.value == TransportState::Playing;
        self.last_transport = Some(candidate.value);
        self.stable_transport = Some(StableField {
            value: candidate.value,
            consecutive_samples: candidate.consecutive_samples,
            confirmed_at: now,
            last_sampled_at: candidate.last_sampled_at,
            fresh: true,
        });
        TransportUpdate {
            accepted_new: true,
            restarted,
        }
    }

    fn progress_restarted(&self, before: Duration, after: Duration) -> bool {
        before >= self.config.restart_previous_progress
            && after <= self.config.restart_near_start
            && before.saturating_sub(after) >= self.config.restart_minimum_drop
    }

    fn mark_uri_stale(&mut self) {
        self.uri_candidate = None;
        if let Some(stable) = self.stable_uri.as_mut() {
            stable.fresh = false;
        }
    }

    fn mark_transport_stale(&mut self) {
        self.transport_candidate = None;
        if let Some(stable) = self.stable_transport.as_mut() {
            stable.fresh = false;
        }
    }

    fn expire_stale_fields(&mut self, now: Instant) {
        if self.uri_candidate.as_ref().is_some_and(|candidate| {
            now.saturating_duration_since(candidate.last_sampled_at) > self.config.stale_timeout
        }) {
            self.uri_candidate = None;
        }
        if self.transport_candidate.as_ref().is_some_and(|candidate| {
            now.saturating_duration_since(candidate.last_sampled_at) > self.config.stale_timeout
        }) {
            self.transport_candidate = None;
        }

        let uri_expired = self.stable_uri.as_ref().is_some_and(|stable| {
            now.saturating_duration_since(stable.last_sampled_at) > self.config.stale_timeout
        });
        if uri_expired {
            self.stable_uri = None;
            self.uri_candidate = None;
            self.evidence = TrackEvidence::default();
        }

        let transport_expired = self.stable_transport.as_ref().is_some_and(|stable| {
            now.saturating_duration_since(stable.last_sampled_at) > self.config.stale_timeout
        });
        if transport_expired {
            self.stable_transport = None;
            self.transport_candidate = None;
            self.last_transport = None;
            self.identity_waiting_for_transport = false;
        }
        if uri_expired || transport_expired {
            self.pending_transport_restart = false;
            self.identity_waiting_for_transport = false;
        }
    }

    fn snapshot_at(&self, now: Instant) -> PlayerObservation {
        let uri_freshness = field_freshness(self.stable_uri.as_ref(), now);
        let transport_freshness = field_freshness(self.stable_transport.as_ref(), now);
        let progress = self.estimated_progress(now, uri_freshness, transport_freshness);

        PlayerObservation {
            evaluated_at: now,
            last_attempted_at: self.last_attempted_at,
            last_successful_observed_at: self.last_successful_observed_at,
            uri: self.stable_uri.as_ref().map(|stable| stable.value.clone()),
            uri_freshness,
            uri_evidence: stable_evidence(self.stable_uri.as_ref(), self.config.uri_stable_samples),
            uri_candidate: candidate_evidence(
                self.uri_candidate.as_ref(),
                self.config.uri_stable_samples,
            ),
            transport: self.stable_transport.as_ref().map(|stable| stable.value),
            transport_freshness,
            transport_evidence: stable_evidence(
                self.stable_transport.as_ref(),
                self.config.transport_stable_samples,
            ),
            transport_candidate: candidate_evidence(
                self.transport_candidate.as_ref(),
                self.config.transport_stable_samples,
            ),
            playback_instance: self.stable_uri.as_ref().and(self.playback_instance),
            title: self.evidence.title.clone(),
            artist: self.evidence.artist.clone(),
            album_name: self.evidence.album_name.clone(),
            lyric_line_text: self.evidence.lyric_line_text.clone(),
            progress,
            duration: self.evidence.duration,
            playback_rate: self.evidence.playback_rate,
            volume: self.evidence.volume,
            sampled_at: self.evidence.sampled_at,
        }
    }

    fn estimated_progress(
        &self,
        now: Instant,
        uri_freshness: ObservationFreshness,
        transport_freshness: ObservationFreshness,
    ) -> Option<Duration> {
        let sampled = self.evidence.progress?;
        if !uri_freshness.is_fresh()
            || !transport_freshness.is_fresh()
            || self.stable_transport.as_ref().map(|stable| stable.value)
                != Some(TransportState::Playing)
        {
            return Some(sampled);
        }

        let sampled_at = self.evidence.progress_sampled_at?;
        let elapsed = now.saturating_duration_since(sampled_at);
        let estimated = sampled.saturating_add(saturating_duration_mul(
            elapsed,
            self.evidence.playback_rate.unwrap_or(1.0),
        ));
        Some(
            self.evidence
                .duration
                .map_or(estimated, |duration| estimated.min(duration)),
        )
    }
}

fn advance_candidate<T: Eq>(
    candidate: &mut Option<Candidate<T>>,
    value: T,
    required_samples: usize,
    sampled_at: Instant,
) -> bool {
    match candidate {
        Some(candidate) if candidate.value == value => {
            candidate.consecutive_samples = candidate.consecutive_samples.saturating_add(1);
            candidate.last_sampled_at = sampled_at;
        }
        _ => {
            *candidate = Some(Candidate {
                value,
                consecutive_samples: 1,
                last_sampled_at: sampled_at,
            });
        }
    }
    candidate
        .as_ref()
        .is_some_and(|candidate| candidate.consecutive_samples >= required_samples)
}

fn candidate_evidence<T: Clone>(
    candidate: Option<&Candidate<T>>,
    required_samples: usize,
) -> Option<StabilityEvidence<T>> {
    candidate.map(|candidate| StabilityEvidence {
        value: candidate.value.clone(),
        consecutive_samples: candidate.consecutive_samples,
        required_samples,
        confirmed_at: None,
        last_sampled_at: candidate.last_sampled_at,
    })
}

fn stable_evidence<T: Clone>(
    field: Option<&StableField<T>>,
    required_samples: usize,
) -> Option<StabilityEvidence<T>> {
    field.map(|field| StabilityEvidence {
        value: field.value.clone(),
        consecutive_samples: field.consecutive_samples,
        required_samples,
        confirmed_at: Some(field.confirmed_at),
        last_sampled_at: field.last_sampled_at,
    })
}

fn field_freshness<T>(field: Option<&StableField<T>>, now: Instant) -> ObservationFreshness {
    match field {
        Some(field) if field.fresh => ObservationFreshness::Fresh,
        Some(field) => ObservationFreshness::Stale {
            age: now.saturating_duration_since(field.last_sampled_at),
        },
        None => ObservationFreshness::Unknown,
    }
}

fn normalized_uri(uri: Option<&str>) -> Option<&str> {
    uri.map(str::trim).filter(|uri| !uri.is_empty())
}

fn normalized_owned(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn normalized_stability_count(configured: usize) -> usize {
    if configured > 1 {
        configured
    } else {
        DEFAULT_PLAYER_STABILITY_SAMPLES
    }
}

fn saturating_duration_mul(duration: Duration, factor: f64) -> Duration {
    if !factor.is_finite() || factor <= 0.0 {
        return Duration::ZERO;
    }
    Duration::try_from_secs_f64(duration.as_secs_f64() * factor).unwrap_or(Duration::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        ObservationFreshness, PlaybackInstance, PlayerObservationConfig, PlayerObserver,
        RawPlayerSample, TransportState,
    };
    use crate::runtime::clock::{Clock, ManualClock};

    fn observer(
        clock: &ManualClock,
        config: PlayerObservationConfig,
    ) -> PlayerObserver<ManualClock> {
        PlayerObserver::new(clock.clone(), config)
    }

    fn sample(uri: &str, transport: TransportState, progress_secs: u64) -> RawPlayerSample {
        RawPlayerSample {
            uri: Some(uri.to_owned()),
            transport: Some(transport),
            progress: Some(Duration::from_secs(progress_secs)),
            duration: Some(Duration::from_secs(180)),
            ..RawPlayerSample::default()
        }
    }

    fn stabilize(
        observer: &mut PlayerObserver<ManualClock>,
        uri: &str,
        transport: TransportState,
        progress_secs: u64,
    ) {
        observer.observe_sample(sample(uri, transport, progress_secs));
        observer.observe_sample(sample(uri, transport, progress_secs));
    }

    #[test]
    fn uri_and_transport_reach_stability_independently() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(
            &clock,
            PlayerObservationConfig {
                transport_stable_samples: 3,
                ..PlayerObservationConfig::default()
            },
        );

        let first = observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 1));
        assert_eq!(first.uri_freshness, ObservationFreshness::Unknown);
        assert_eq!(first.transport_freshness, ObservationFreshness::Unknown);

        let second = observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 2));
        assert_eq!(second.uri.as_deref(), Some("fuo://song/1"));
        assert_eq!(second.uri_freshness, ObservationFreshness::Fresh);
        assert_eq!(second.transport, None);
        let transport_candidate = second.transport_candidate.unwrap();
        assert_eq!(transport_candidate.value, TransportState::Playing);
        assert_eq!(transport_candidate.consecutive_samples, 2);
        assert_eq!(transport_candidate.required_samples, 3);
        assert_eq!(transport_candidate.confirmed_at, None);

        let third = observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 3));
        assert_eq!(third.transport, Some(TransportState::Playing));
        assert_eq!(third.transport_freshness, ObservationFreshness::Fresh);
        assert_eq!(third.playback_instance, Some(PlaybackInstance::INITIAL));
    }

    #[test]
    fn changing_progress_and_metadata_does_not_reset_stability() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        let mut first = sample("fuo://song/1", TransportState::Playing, 20);
        first.title = Some("old title".to_owned());
        observer.observe_sample(first);

        let mut second = sample("fuo://song/1", TransportState::Playing, 21);
        second.title = Some("new title".to_owned());
        second.volume = Some(80);
        let observation = observer.observe_sample(second);

        assert_eq!(observation.uri_freshness, ObservationFreshness::Fresh);
        assert_eq!(observation.transport_freshness, ObservationFreshness::Fresh);
        assert_eq!(observation.title.as_deref(), Some("new title"));
        assert_eq!(observation.volume, Some(80));
    }

    #[test]
    fn empty_uri_never_becomes_a_track_identity_but_transport_can_stabilize() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        let incomplete = RawPlayerSample {
            uri: Some("  ".to_owned()),
            transport: Some(TransportState::Playing),
            ..RawPlayerSample::default()
        };

        observer.observe_sample(incomplete.clone());
        let observation = observer.observe_sample(incomplete);

        assert_eq!(observation.uri, None);
        assert_eq!(observation.uri_freshness, ObservationFreshness::Unknown);
        assert_eq!(observation.transport, Some(TransportState::Playing));
        assert_eq!(observation.transport_freshness, ObservationFreshness::Fresh);
        assert_eq!(observation.playback_instance, None);
    }

    #[test]
    fn incomplete_samples_only_interrupt_the_missing_field() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);

        let observation = observer.observe_sample(RawPlayerSample {
            uri: Some("fuo://song/1".to_owned()),
            transport: None,
            progress: Some(Duration::from_secs(21)),
            ..RawPlayerSample::default()
        });

        assert_eq!(observation.uri_freshness, ObservationFreshness::Fresh);
        assert!(matches!(
            observation.transport_freshness,
            ObservationFreshness::Stale { .. }
        ));
        assert_eq!(observation.progress, Some(Duration::from_secs(21)));
    }

    #[test]
    fn failures_retain_stale_identity_for_at_most_five_seconds() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);

        clock.advance(Duration::from_secs(2)).unwrap();
        let stale = observer.observe_failure();
        assert_eq!(stale.uri.as_deref(), Some("fuo://song/1"));
        assert_eq!(
            stale.uri_freshness,
            ObservationFreshness::Stale {
                age: Duration::from_secs(2)
            }
        );
        assert!(!stale.confirms_uri("fuo://song/1"));

        clock.advance(Duration::from_secs(3)).unwrap();
        assert_eq!(
            observer.current().uri_freshness,
            ObservationFreshness::Stale {
                age: Duration::from_secs(5)
            }
        );

        clock.advance(Duration::from_millis(1)).unwrap();
        let unknown = observer.current();
        assert_eq!(unknown.uri, None);
        assert_eq!(unknown.uri_freshness, ObservationFreshness::Unknown);
        assert_eq!(unknown.playback_instance, None);
    }

    #[test]
    fn same_uri_recovers_from_stale_immediately() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);
        observer.observe_failure();

        let recovered =
            observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 21));

        assert_eq!(recovered.uri_freshness, ObservationFreshness::Fresh);
        assert!(recovered.confirms_uri("fuo://song/1"));
        assert_eq!(recovered.playback_instance, Some(PlaybackInstance::INITIAL));
    }

    #[test]
    fn a_different_uri_requires_full_stability_while_old_uri_is_stale() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);

        let pending = observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 1));
        assert_eq!(pending.uri.as_deref(), Some("fuo://song/1"));
        assert!(matches!(
            pending.uri_freshness,
            ObservationFreshness::Stale { .. }
        ));
        let stable_evidence = pending.uri_evidence.as_ref().unwrap();
        assert!(stable_evidence.consecutive_samples >= stable_evidence.required_samples);
        assert!(stable_evidence.confirmed_at.is_some());
        let uri_candidate = pending.uri_candidate.unwrap();
        assert_eq!(uri_candidate.value, "fuo://song/2");
        assert_eq!(uri_candidate.consecutive_samples, 1);
        assert_eq!(uri_candidate.required_samples, 2);
        assert_eq!(uri_candidate.confirmed_at, None);

        let accepted = observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 2));
        assert_eq!(accepted.uri.as_deref(), Some("fuo://song/2"));
        assert_eq!(accepted.uri_freshness, ObservationFreshness::Fresh);
        assert_eq!(
            accepted.playback_instance.map(PlaybackInstance::get),
            Some(2)
        );
    }

    #[test]
    fn invalid_uri_breaks_candidate_consecutiveness() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);
        observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 1));
        observer.observe_sample(RawPlayerSample {
            uri: None,
            transport: Some(TransportState::Playing),
            ..RawPlayerSample::default()
        });

        let restarted = observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 1));
        assert_eq!(
            restarted
                .uri_candidate
                .as_ref()
                .map(|candidate| candidate.consecutive_samples),
            Some(1)
        );
        assert_eq!(restarted.uri.as_deref(), Some("fuo://song/1"));
    }

    #[test]
    fn expired_pending_identity_and_transport_require_full_stability_again() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        observer.observe_sample(sample("fuo://song/pending", TransportState::Playing, 1));

        clock.advance(Duration::from_secs(6)).unwrap();
        let restarted =
            observer.observe_sample(sample("fuo://song/pending", TransportState::Playing, 2));

        assert_eq!(restarted.uri, None);
        assert_eq!(restarted.transport, None);
        assert_eq!(
            restarted
                .uri_candidate
                .map(|evidence| evidence.consecutive_samples),
            Some(1)
        );
        assert_eq!(
            restarted
                .transport_candidate
                .map(|evidence| evidence.consecutive_samples),
            Some(1)
        );
    }

    #[test]
    fn stopped_to_playing_advances_the_instance_after_transport_is_stable() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Stopped, 20);

        let pending = observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 20));
        assert_eq!(
            pending.playback_instance.map(PlaybackInstance::get),
            Some(1)
        );
        let playing = observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 20));

        assert_eq!(
            playing.playback_instance.map(PlaybackInstance::get),
            Some(2)
        );
    }

    #[test]
    fn clear_return_to_start_advances_instance_but_small_jitter_does_not() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 30);

        let jittered = observer.observe_sample(RawPlayerSample {
            progress: Some(Duration::from_millis(29_200)),
            ..sample("fuo://song/1", TransportState::Playing, 29)
        });
        assert_eq!(
            jittered.playback_instance.map(PlaybackInstance::get),
            Some(1)
        );

        let restarted = observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 1));
        assert_eq!(
            restarted.playback_instance.map(PlaybackInstance::get),
            Some(2)
        );
    }

    #[test]
    fn local_progress_estimation_uses_the_manual_clock_and_stops_when_stale() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);

        clock.advance(Duration::from_secs(2)).unwrap();
        assert_eq!(observer.current().progress, Some(Duration::from_secs(22)));

        let stale = observer.observe_failure();
        clock.advance(Duration::from_secs(1)).unwrap();
        assert_eq!(stale.progress, Some(Duration::from_secs(20)));
        assert_eq!(observer.current().progress, Some(Duration::from_secs(20)));
    }

    #[test]
    fn expired_same_uri_must_stabilize_again_without_inventing_a_new_instance() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);
        observer.observe_failure();
        clock.advance(Duration::from_secs(6)).unwrap();
        assert_eq!(
            observer.current().uri_freshness,
            ObservationFreshness::Unknown
        );

        let pending = observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 21));
        assert_eq!(pending.uri_freshness, ObservationFreshness::Unknown);
        let recovered =
            observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 22));

        assert_eq!(recovered.uri_freshness, ObservationFreshness::Fresh);
        assert_eq!(
            recovered.playback_instance.map(PlaybackInstance::get),
            Some(1)
        );
    }

    #[test]
    fn transport_restart_waits_for_the_same_uri_to_recover() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Stopped, 20);

        let missing_uri = RawPlayerSample {
            uri: None,
            transport: Some(TransportState::Playing),
            progress: Some(Duration::from_secs(20)),
            ..RawPlayerSample::default()
        };
        observer.observe_sample(missing_uri.clone());
        let restarted_without_identity = observer.observe_sample(missing_uri);
        assert_eq!(
            restarted_without_identity
                .playback_instance
                .map(PlaybackInstance::get),
            Some(1)
        );

        let recovered =
            observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 21));
        assert_eq!(
            recovered.playback_instance.map(PlaybackInstance::get),
            Some(2)
        );
    }

    #[test]
    fn uri_change_and_later_transport_restart_advance_one_instance() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(
            &clock,
            PlayerObservationConfig {
                uri_stable_samples: 2,
                transport_stable_samples: 3,
                ..PlayerObservationConfig::default()
            },
        );
        for _ in 0..3 {
            observer.observe_sample(sample("fuo://song/1", TransportState::Stopped, 20));
        }

        observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 1));
        let uri_accepted =
            observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 2));
        assert_eq!(
            uri_accepted.playback_instance.map(PlaybackInstance::get),
            Some(2)
        );

        let transport_accepted =
            observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 3));
        assert_eq!(
            transport_accepted
                .playback_instance
                .map(PlaybackInstance::get),
            Some(2)
        );
    }

    #[test]
    fn uri_accepted_without_transport_merges_the_later_playing_transition() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Stopped, 20);

        let new_uri_without_transport = RawPlayerSample {
            uri: Some("fuo://song/2".to_owned()),
            transport: None,
            progress: Some(Duration::from_secs(1)),
            ..RawPlayerSample::default()
        };
        observer.observe_sample(new_uri_without_transport.clone());
        let uri_accepted = observer.observe_sample(new_uri_without_transport);
        assert_eq!(
            uri_accepted.playback_instance.map(PlaybackInstance::get),
            Some(2)
        );

        observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 2));
        let transport_accepted =
            observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 3));
        assert_eq!(
            transport_accepted
                .playback_instance
                .map(PlaybackInstance::get),
            Some(2)
        );
    }

    #[test]
    fn partial_same_uri_sample_preserves_existing_track_evidence() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        let mut complete = sample("fuo://song/1", TransportState::Playing, 20);
        complete.title = Some("title".to_owned());
        complete.artist = Some("artist".to_owned());
        complete.album_name = Some("album".to_owned());
        complete.lyric_line_text = Some("lyric".to_owned());
        complete.volume = Some(70);
        complete.playback_rate = Some(1.25);
        observer.observe_sample(complete.clone());
        observer.observe_sample(complete);

        let partial = observer.observe_sample(RawPlayerSample {
            uri: Some("fuo://song/1".to_owned()),
            transport: Some(TransportState::Playing),
            ..RawPlayerSample::default()
        });

        assert_eq!(partial.title.as_deref(), Some("title"));
        assert_eq!(partial.artist.as_deref(), Some("artist"));
        assert_eq!(partial.album_name.as_deref(), Some("album"));
        assert_eq!(partial.lyric_line_text.as_deref(), Some("lyric"));
        assert_eq!(partial.progress, Some(Duration::from_secs(20)));
        assert_eq!(partial.duration, Some(Duration::from_secs(180)));
        assert_eq!(partial.playback_rate, Some(1.25));
        assert_eq!(partial.volume, Some(70));
    }

    #[test]
    fn huge_finite_playback_rate_saturates_progress_without_panicking() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        let mut fast = sample("fuo://song/1", TransportState::Playing, 20);
        fast.duration = None;
        fast.playback_rate = Some(1e308);
        observer.observe_sample(fast.clone());
        observer.observe_sample(fast);

        clock.advance(Duration::from_secs(1)).unwrap();
        assert_eq!(observer.current().progress, Some(Duration::MAX));
    }

    #[test]
    fn zero_and_one_stability_counts_use_the_default_two_samples() {
        for configured in [0, 1] {
            let clock = ManualClock::new(Instant::now());
            let mut observer = observer(
                &clock,
                PlayerObservationConfig {
                    uri_stable_samples: configured,
                    transport_stable_samples: configured,
                    ..PlayerObservationConfig::default()
                },
            );

            let first = observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 1));
            assert_eq!(first.uri_freshness, ObservationFreshness::Unknown);
            assert_eq!(first.transport_freshness, ObservationFreshness::Unknown);

            let second =
                observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 2));
            assert_eq!(second.uri_freshness, ObservationFreshness::Fresh);
            assert_eq!(second.transport_freshness, ObservationFreshness::Fresh);
        }
    }

    #[test]
    fn current_expires_old_fields_without_forging_a_new_observation_time() {
        let started_at = Instant::now();
        let clock = ManualClock::new(started_at);
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);

        clock.advance(Duration::from_secs(6)).unwrap();
        let current = observer.current();

        assert_eq!(current.evaluated_at, started_at + Duration::from_secs(6));
        assert_eq!(current.last_attempted_at, Some(started_at));
        assert_eq!(current.last_successful_observed_at, Some(started_at));
        assert_eq!(current.uri_freshness, ObservationFreshness::Unknown);
        assert_eq!(current.transport_freshness, ObservationFreshness::Unknown);
        assert_eq!(current.uri, None);
        assert_eq!(current.transport, None);
    }

    #[test]
    fn first_failure_or_empty_sample_after_timeout_cannot_return_overage_stale_data() {
        let started_at = Instant::now();
        let failure_clock = ManualClock::new(started_at);
        let mut failure_observer = observer(&failure_clock, PlayerObservationConfig::default());
        stabilize(
            &mut failure_observer,
            "fuo://song/1",
            TransportState::Playing,
            20,
        );
        failure_clock.advance(Duration::from_secs(6)).unwrap();

        let failed = failure_observer.observe_failure();
        assert_eq!(failed.uri_freshness, ObservationFreshness::Unknown);
        assert_eq!(failed.transport_freshness, ObservationFreshness::Unknown);
        assert_eq!(
            failed.last_attempted_at,
            Some(started_at + Duration::from_secs(6))
        );
        assert_eq!(failed.last_successful_observed_at, Some(started_at));

        let empty_clock = ManualClock::new(started_at);
        let mut empty_observer = observer(&empty_clock, PlayerObservationConfig::default());
        stabilize(
            &mut empty_observer,
            "fuo://song/1",
            TransportState::Playing,
            20,
        );
        empty_clock.advance(Duration::from_secs(6)).unwrap();

        let empty = empty_observer.observe_sample(RawPlayerSample::default());
        assert_eq!(empty.uri_freshness, ObservationFreshness::Unknown);
        assert_eq!(empty.transport_freshness, ObservationFreshness::Unknown);
        assert_eq!(
            empty.last_attempted_at,
            Some(started_at + Duration::from_secs(6))
        );
        assert_eq!(
            empty.last_successful_observed_at,
            Some(started_at + Duration::from_secs(6))
        );
    }

    #[test]
    fn stable_and_pending_fields_publish_timed_stability_evidence() {
        let started_at = Instant::now();
        let clock = ManualClock::new(started_at);
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 20));
        clock.advance(Duration::from_secs(1)).unwrap();
        let confirmed =
            observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 21));

        let uri_evidence = confirmed.uri_evidence.unwrap();
        assert_eq!(uri_evidence.value, "fuo://song/1");
        assert_eq!(uri_evidence.consecutive_samples, 2);
        assert_eq!(uri_evidence.required_samples, 2);
        assert_eq!(
            uri_evidence.confirmed_at,
            Some(started_at + Duration::from_secs(1))
        );
        assert_eq!(
            uri_evidence.last_sampled_at,
            started_at + Duration::from_secs(1)
        );

        let transport_evidence = confirmed.transport_evidence.unwrap();
        assert_eq!(transport_evidence.value, TransportState::Playing);
        assert_eq!(transport_evidence.consecutive_samples, 2);
        assert_eq!(transport_evidence.required_samples, 2);
        assert_eq!(
            transport_evidence.confirmed_at,
            Some(started_at + Duration::from_secs(1))
        );

        clock.advance(Duration::from_secs(1)).unwrap();
        observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 22));
        clock.advance(Duration::from_secs(1)).unwrap();
        let pending = observer.observe_sample(sample("fuo://song/2", TransportState::Playing, 1));
        let candidate = pending.uri_candidate.unwrap();
        assert_eq!(candidate.value, "fuo://song/2");
        assert_eq!(candidate.consecutive_samples, 1);
        assert_eq!(candidate.required_samples, 2);
        assert_eq!(candidate.confirmed_at, None);
        assert_eq!(
            candidate.last_sampled_at,
            started_at + Duration::from_secs(3)
        );
    }

    #[test]
    fn cached_observation_reevaluation_advances_progress_without_forging_an_attempt() {
        let started_at = Instant::now();
        let clock = ManualClock::new(started_at);
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);
        let snapshot = observer.current();

        let reevaluated =
            snapshot.reevaluated_at(started_at + Duration::from_secs(2), Duration::from_secs(5));

        assert_eq!(
            reevaluated.evaluated_at,
            started_at + Duration::from_secs(2)
        );
        assert_eq!(reevaluated.last_attempted_at, Some(started_at));
        assert_eq!(reevaluated.last_successful_observed_at, Some(started_at));
        assert_eq!(reevaluated.uri_freshness, ObservationFreshness::Fresh);
        assert_eq!(reevaluated.transport_freshness, ObservationFreshness::Fresh);
        assert_eq!(reevaluated.progress, Some(Duration::from_secs(22)));
    }

    #[test]
    fn cached_observation_reevaluation_expires_identity_transport_and_track_evidence() {
        let started_at = Instant::now();
        let clock = ManualClock::new(started_at);
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        let mut complete = sample("fuo://song/1", TransportState::Playing, 20);
        complete.title = Some("title".to_owned());
        complete.artist = Some("artist".to_owned());
        complete.album_name = Some("album".to_owned());
        complete.lyric_line_text = Some("lyric".to_owned());
        complete.playback_rate = Some(1.25);
        complete.volume = Some(70);
        observer.observe_sample(complete.clone());
        let snapshot = observer.observe_sample(complete);

        let reevaluated =
            snapshot.reevaluated_at(started_at + Duration::from_secs(6), Duration::from_secs(5));

        assert_eq!(reevaluated.uri, None);
        assert_eq!(reevaluated.uri_freshness, ObservationFreshness::Unknown);
        assert_eq!(reevaluated.uri_evidence, None);
        assert_eq!(reevaluated.uri_candidate, None);
        assert_eq!(reevaluated.playback_instance, None);
        assert_eq!(reevaluated.transport, None);
        assert_eq!(
            reevaluated.transport_freshness,
            ObservationFreshness::Unknown
        );
        assert_eq!(reevaluated.transport_evidence, None);
        assert_eq!(reevaluated.transport_candidate, None);
        assert_eq!(reevaluated.title, None);
        assert_eq!(reevaluated.artist, None);
        assert_eq!(reevaluated.album_name, None);
        assert_eq!(reevaluated.lyric_line_text, None);
        assert_eq!(reevaluated.progress, None);
        assert_eq!(reevaluated.duration, None);
        assert_eq!(reevaluated.playback_rate, None);
        assert_eq!(reevaluated.volume, None);
        assert_eq!(reevaluated.sampled_at, None);
    }

    #[test]
    fn cached_stale_observation_reevaluation_updates_age_without_advancing_progress() {
        let started_at = Instant::now();
        let clock = ManualClock::new(started_at);
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);
        clock.advance(Duration::from_secs(1)).unwrap();
        let stale = observer.observe_failure();

        let reevaluated =
            stale.reevaluated_at(started_at + Duration::from_secs(3), Duration::from_secs(5));

        assert_eq!(
            reevaluated.uri_freshness,
            ObservationFreshness::Stale {
                age: Duration::from_secs(3)
            }
        );
        assert_eq!(
            reevaluated.transport_freshness,
            ObservationFreshness::Stale {
                age: Duration::from_secs(3)
            }
        );
        assert_eq!(reevaluated.progress, Some(Duration::from_secs(20)));
    }

    #[test]
    fn accepting_a_new_uri_clears_the_previous_tracks_optional_evidence() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        let mut old = sample("fuo://song/1", TransportState::Playing, 20);
        old.title = Some("old title".to_owned());
        old.artist = Some("old artist".to_owned());
        old.album_name = Some("old album".to_owned());
        old.lyric_line_text = Some("old lyric".to_owned());
        old.playback_rate = Some(1.25);
        old.volume = Some(70);
        observer.observe_sample(old.clone());
        observer.observe_sample(old);

        let new_track = RawPlayerSample::new("fuo://song/2", TransportState::Playing);
        observer.observe_sample(new_track.clone());
        let accepted = observer.observe_sample(new_track);

        assert_eq!(accepted.uri.as_deref(), Some("fuo://song/2"));
        assert_eq!(accepted.title, None);
        assert_eq!(accepted.artist, None);
        assert_eq!(accepted.album_name, None);
        assert_eq!(accepted.lyric_line_text, None);
        assert_eq!(accepted.progress, None);
        assert_eq!(accepted.duration, None);
        assert_eq!(accepted.playback_rate, None);
        assert_eq!(accepted.volume, None);
        assert_eq!(accepted.sampled_at, Some(clock.now()));
    }

    #[test]
    fn expired_identity_does_not_consume_an_old_pending_transport_restart() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Stopped, 20);

        let missing_uri_playing = RawPlayerSample {
            uri: None,
            transport: Some(TransportState::Playing),
            progress: Some(Duration::from_secs(20)),
            ..RawPlayerSample::default()
        };
        observer.observe_sample(missing_uri_playing.clone());
        observer.observe_sample(missing_uri_playing);

        clock.advance(Duration::from_secs(6)).unwrap();
        assert_eq!(
            observer.current().uri_freshness,
            ObservationFreshness::Unknown
        );
        observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 21));
        let restabilized =
            observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 22));

        assert_eq!(
            restabilized.playback_instance.map(PlaybackInstance::get),
            Some(1)
        );
    }

    #[test]
    fn stable_evidence_keeps_the_original_confirmation_sample_count() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        stabilize(&mut observer, "fuo://song/1", TransportState::Playing, 20);
        observer.observe_sample(RawPlayerSample {
            uri: None,
            transport: Some(TransportState::Playing),
            ..RawPlayerSample::default()
        });

        let recovered =
            observer.observe_sample(sample("fuo://song/1", TransportState::Playing, 21));
        assert_eq!(
            recovered
                .uri_evidence
                .as_ref()
                .map(|evidence| evidence.consecutive_samples),
            Some(2)
        );
        assert_eq!(
            recovered
                .transport_evidence
                .as_ref()
                .map(|evidence| evidence.consecutive_samples),
            Some(2)
        );
    }

    #[test]
    fn blank_lyric_explicitly_clears_it_while_missing_lyric_preserves_it() {
        let clock = ManualClock::new(Instant::now());
        let mut observer = observer(&clock, PlayerObservationConfig::default());
        let mut initial = sample("fuo://song/1", TransportState::Playing, 20);
        initial.lyric_line_text = Some("first line".to_owned());
        observer.observe_sample(initial.clone());
        observer.observe_sample(initial);

        let missing = observer.observe_sample(RawPlayerSample {
            uri: Some("fuo://song/1".to_owned()),
            transport: Some(TransportState::Playing),
            lyric_line_text: None,
            ..RawPlayerSample::default()
        });
        assert_eq!(missing.lyric_line_text.as_deref(), Some("first line"));

        let blank = observer.observe_sample(RawPlayerSample {
            uri: Some("fuo://song/1".to_owned()),
            transport: Some(TransportState::Playing),
            lyric_line_text: Some("   ".to_owned()),
            ..RawPlayerSample::default()
        });
        assert_eq!(blank.lyric_line_text, None);

        let updated = observer.observe_sample(RawPlayerSample {
            uri: Some("fuo://song/1".to_owned()),
            transport: Some(TransportState::Playing),
            lyric_line_text: Some("  next line  ".to_owned()),
            ..RawPlayerSample::default()
        });
        assert_eq!(updated.lyric_line_text.as_deref(), Some("next line"));
    }
}
