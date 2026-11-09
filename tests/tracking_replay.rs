use closet::{
    replay::{Scenario, Step},
    tracking::models::{DataSource, UnitStatus},
};
use serde::Deserialize;

fn scenario() -> Scenario {
    serde_json::from_str(include_str!("../config/tracking-replay.example.json")).unwrap()
}

#[tokio::test]
async fn real_sqlite_replay_enrolls_and_infers_with_repeatable_semantic_output() {
    let first = closet::replay::run(scenario()).await.unwrap();
    let second = closet::replay::run(scenario()).await.unwrap();
    assert_eq!(first, second);
    let tee = first
        .units
        .iter()
        .find(|unit| unit.catalog_id == "example-tee")
        .unwrap();
    assert_eq!(tee.status, UnitStatus::BeingWorn);
    assert!(!tee.ready_to_wear);
    assert!(
        first
            .transitions
            .iter()
            .any(|transition| transition.catalog_id == "example-tee"
                && transition.next == UnitStatus::CleanReady)
    );
    assert!(
        first
            .units
            .iter()
            .filter(|unit| unit.catalog_id != "example-tee")
            .all(|unit| unit.status == UnitStatus::Unknown)
    );
    assert_eq!(first.catalog_revision, 1);
}

#[tokio::test]
async fn replay_refuses_live_input_and_backwards_clock_events() {
    let mut live = scenario();
    if let Step::Observe { batch, .. } = &mut live.steps[1] {
        batch.data_source = DataSource::Live;
    }
    assert!(closet::replay::run(live).await.is_err());
    let mut backwards = scenario();
    backwards.steps.push(Step::Tick {
        at: backwards.started_at,
    });
    assert!(closet::replay::run(backwards).await.is_err());
}

#[tokio::test]
async fn configuration_and_restart_events_replay_without_reusing_old_coverage() {
    fn changed() -> Scenario {
        let mut scenario = scenario();
        let mut config = scenario.config.clone();
        for area in &mut config.areas {
            area.enabled = false;
        }
        scenario.steps.push(Step::Restart {
            at: scenario.started_at + chrono::Duration::seconds(5),
        });
        scenario.steps.push(Step::Configure {
            at: scenario.started_at + chrono::Duration::seconds(6),
            config,
        });
        scenario.steps.push(Step::Tick {
            at: scenario.started_at + chrono::Duration::hours(1),
        });
        scenario
    }
    let first = closet::replay::run(changed()).await.unwrap();
    assert_eq!(first, closet::replay::run(changed()).await.unwrap());
    assert_eq!(
        first
            .units
            .iter()
            .find(|unit| unit.catalog_id == "example-tee")
            .unwrap()
            .status,
        UnitStatus::BeingWorn
    );
}

#[test]
fn offline_cli_validate_and_replay_do_not_resolve_credentials_or_connect() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    for args in [
        vec![
            "tracking",
            "config",
            "validate",
            "--input",
            "config/tracking.example.json",
        ],
        vec![
            "tracking",
            "replay",
            "--input",
            "config/tracking-replay.example.json",
        ],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ha-closet"))
            .args(["--token-file", "/does-not-exist"])
            .args(args)
            .env("CLOSET_API_URL", &origin)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        #[derive(Deserialize)]
        struct Envelope {
            status: Option<u16>,
            etag: Option<String>,
        }
        let envelope: Envelope = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(envelope.status, None);
        assert_eq!(envelope.etag, None);
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
