use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use miliastra_playback::{
    EndCause, EngineState, PlayableTrack, PlaybackHandle, PlaybackRuntime, ProviderCredential,
    ProviderId, SearchQuery, TrackKey, TrackMetadata, TrackRef,
};

const LEGACY_CREDENTIAL_DIRECTORY: &str = "MILIASTRA_LEGACY_CREDENTIAL_DIRECTORY";
const SEARCH_KEYWORD: &str = "\u{828a}\u{828a}";
const BILIBILI_BVID: &str = "BV1us411d75p";

struct LiveRuntime {
    runtime: Option<PlaybackRuntime>,
    credential_directory: PathBuf,
}

impl LiveRuntime {
    fn start(providers: &[ProviderId]) -> Self {
        let source = std::env::var_os(LEGACY_CREDENTIAL_DIRECTORY)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                panic!(
                    "set {LEGACY_CREDENTIAL_DIRECTORY} to a directory containing the three legacy credential JSON files"
                )
            });
        let credential_directory = temporary_credential_directory();
        let runtime =
            PlaybackRuntime::start(&credential_directory).expect("start playback runtime");
        let handle = runtime.handle();
        for provider in providers.iter().copied() {
            let path = source.join(format!("{}.json", provider.as_str()));
            let credential: ProviderCredential =
                serde_json::from_slice(&fs::read(&path).unwrap_or_else(|error| {
                    panic!("read legacy credential {}: {error}", path.display())
                }))
                .unwrap_or_else(|error| {
                    panic!("parse legacy credential {}: {error}", path.display())
                });
            assert_eq!(credential.provider(), provider.as_str());
            let status = handle
                .save_credential(credential)
                .unwrap_or_else(|error| panic!("save {} credential: {error}", provider.as_str()));
            assert!(status.configured);
        }
        Self {
            runtime: Some(runtime),
            credential_directory,
        }
    }

    fn handle(&self) -> PlaybackHandle {
        self.runtime.as_ref().expect("runtime exists").handle()
    }
}

impl Drop for LiveRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown().expect("shutdown playback runtime");
        }
        let _ = fs::remove_dir_all(&self.credential_directory);
    }
}

#[test]
#[ignore = "requires live provider credentials and public network access"]
fn live_search_reports_three_provider_candidates() {
    let runtime = LiveRuntime::start(&ProviderId::ALL);
    let handle = runtime.handle();

    for provider in ProviderId::ALL {
        let candidates = handle
            .search(SearchQuery {
                keyword: SEARCH_KEYWORD.to_owned(),
                providers: vec![provider],
                limit: 5,
            })
            .unwrap_or_else(|error| panic!("search {}: {error}", provider.as_str()));
        assert!(
            !candidates.is_empty(),
            "{} returned no live candidates",
            provider.as_str()
        );
        println!(
            "LIVE_SEARCH provider={} candidates={}",
            provider.as_str(),
            serde_json::to_string(&candidates).expect("serialize live search candidates")
        );
    }
}

#[test]
#[ignore = "plays the complete public Bilibili track through the production runtime"]
fn live_bilibili_runtime_controls_and_natural_end() {
    let runtime = LiveRuntime::start(&[ProviderId::Bilibili]);
    let handle = runtime.handle();
    let track = PlayableTrack {
        track_ref: TrackRef {
            key: TrackKey::new(ProviderId::Bilibili, BILIBILI_BVID).unwrap(),
            resolver_locator: None,
        },
        metadata: TrackMetadata {
            title: BILIBILI_BVID.to_owned(),
            artists: Vec::new(),
            album: None,
            duration_ms: None,
        },
    };

    handle
        .play(track.clone())
        .expect("submit Bilibili playback")
        .wait()
        .expect("start Bilibili playback");
    let first = wait_for_snapshot(&handle, Duration::from_secs(45), |snapshot| {
        snapshot.state == EngineState::Playing
    });

    handle.pause().expect("pause playback");
    let paused = wait_for_snapshot(&handle, Duration::from_secs(5), |snapshot| {
        snapshot.state == EngineState::Paused
    });
    thread::sleep(Duration::from_millis(750));
    let paused_later = handle.snapshot().expect("read paused snapshot");
    if let (Some(before), Some(after)) = (paused.position_seconds, paused_later.position_seconds) {
        assert!((after - before).abs() < 0.5, "paused position advanced");
    }

    handle.set_volume(37).expect("set playback volume");
    assert_eq!(handle.snapshot().expect("read volume").volume, 37);
    handle.resume().expect("resume playback");
    wait_for_snapshot(&handle, Duration::from_secs(5), |snapshot| {
        snapshot.state == EngineState::Playing
    });

    handle
        .play(track)
        .expect("submit replacement playback")
        .wait()
        .expect("replace active playback");
    let replacement = wait_for_snapshot(&handle, Duration::from_secs(45), |snapshot| {
        snapshot.generation > first.generation && snapshot.state == EngineState::Playing
    });
    let started = Instant::now();
    let ended = wait_for_snapshot(&handle, Duration::from_secs(300), |snapshot| {
        snapshot.state == EngineState::Stopped
            && snapshot.last_end_cause == Some(EndCause::NaturalEnd)
            && snapshot.generation == replacement.generation
    });

    let duration = ended.duration_seconds.expect("Bilibili duration evidence");
    assert!(
        (230.0..=260.0).contains(&duration),
        "unexpected Bilibili duration: {duration}"
    );
    let elapsed = started.elapsed().as_secs_f64();
    assert!(
        (225.0..=300.0).contains(&elapsed),
        "complete playback elapsed time was {elapsed:.3}s"
    );
    println!(
        "LIVE_PLAYBACK bvid={BILIBILI_BVID} generation={} duration_seconds={duration:.3} elapsed_seconds={elapsed:.3} end_cause=natural_end",
        ended.generation
    );
}

fn wait_for_snapshot(
    handle: &PlaybackHandle,
    timeout: Duration,
    predicate: impl Fn(&miliastra_playback::PlaybackSnapshot) -> bool,
) -> miliastra_playback::PlaybackSnapshot {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = handle.snapshot().expect("read playback snapshot");
        if predicate(&snapshot) {
            return snapshot;
        }
        assert_ne!(
            snapshot.state,
            EngineState::Failed,
            "playback failed: {:?}",
            snapshot.failure
        );
        assert!(Instant::now() < deadline, "timed out with {snapshot:?}");
        thread::sleep(Duration::from_millis(200));
    }
}

fn temporary_credential_directory() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "miliastra-live-runtime-{}-{nonce}",
        std::process::id()
    ))
}
