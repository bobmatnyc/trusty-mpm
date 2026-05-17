use super::*;
use axum::http::StatusCode;
use trusty_mpm_core::session::{ControlModel, Session, SessionStatus};

fn state_with_session() -> (Arc<DaemonState>, SessionId) {
    let state = DaemonState::shared();
    let id = SessionId::new();
    let mut session = Session::new(id, "/tmp/p", ControlModel::Tmux);
    session.status = SessionStatus::Active;
    state.register_session(session);
    (state, id)
}

#[tokio::test]
async fn health_endpoint_responds() {
    assert_eq!(health().await, "ok");
}

#[tokio::test]
async fn current_project_found_and_missing() {
    // `GET /projects/current` returns the project for a registered path
    // and `404` for an unregistered one.
    let state = DaemonState::shared();
    let _ = register_project(
        State(Arc::clone(&state)),
        Json(RegisterProject {
            path: "/work/demo".into(),
        }),
    )
    .await;

    let ok = current_project(
        State(Arc::clone(&state)),
        Query(CurrentProjectQuery {
            path: "/work/demo".into(),
        }),
    )
    .await;
    assert!(ok.is_ok());

    let err = current_project(
        State(state),
        Query(CurrentProjectQuery {
            path: "/work/missing".into(),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn register_session_associates_project() {
    // A `POST /sessions` body carrying `project_path` must associate the
    // new session with that project.
    let state = DaemonState::shared();
    let Json(body) = register_session(
        State(Arc::clone(&state)),
        Json(RegisterSession {
            workdir: "/work/demo".into(),
            project_path: Some("/work/demo".into()),
        }),
    )
    .await;
    let id = body.id.0.to_string();
    let listed = state.list_sessions();
    let session = listed
        .iter()
        .find(|s| s.id.0.to_string() == id)
        .expect("session registered");
    assert_eq!(session.project_path, Some(PathBuf::from("/work/demo")));
}

#[tokio::test]
async fn list_sessions_filters_by_project() {
    // `GET /sessions?project=<path>` returns only sessions of that project.
    let state = DaemonState::shared();
    let _ = register_session(
        State(Arc::clone(&state)),
        Json(RegisterSession {
            workdir: "/work/demo".into(),
            project_path: Some("/work/demo".into()),
        }),
    )
    .await;
    let _ = register_session(
        State(Arc::clone(&state)),
        Json(RegisterSession {
            workdir: "/work/other".into(),
            project_path: Some("/work/other".into()),
        }),
    )
    .await;

    let Json(all) = list_sessions(State(Arc::clone(&state)), Query(SessionQuery::default())).await;
    assert_eq!(all.sessions.len(), 2);

    let Json(scoped) = list_sessions(
        State(state),
        Query(SessionQuery {
            project: Some("/work/demo".into()),
        }),
    )
    .await;
    assert_eq!(scoped.sessions.len(), 1);
}

#[tokio::test]
async fn hook_relay_ingests_known_event() {
    let (state, id) = state_with_session();
    let post = HookPost {
        session_id: id.0.to_string(),
        event: HookEvent::PostToolUse,
        payload: serde_json::json!({"tool": "Edit"}),
    };
    let result = ingest_hook(State(state.clone()), Json(post)).await;
    assert!(result.is_ok());
    assert_eq!(state.recent_hook_events().len(), 1);
}

#[tokio::test]
async fn register_and_remove_session() {
    let state = DaemonState::shared();
    let Json(body) = register_session(
        State(state.clone()),
        Json(RegisterSession {
            workdir: "/tmp/new".into(),
            project_path: None,
        }),
    )
    .await;
    let id = body.id.0.to_string();
    assert_eq!(state.list_sessions().len(), 1);
    // Removing it succeeds; removing again is a 404.
    assert!(
        remove_session(State(state.clone()), Path(id.clone()))
            .await
            .is_ok()
    );
    let err = remove_session(State(state), Path(id)).await.unwrap_err();
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn registered_session_has_friendly_tmux_name() {
    // A registered session must carry a `tmpm-<adj>-<noun>` tmux name
    // derived from its UUID, not the legacy `trusty-mpm-<uuid>` form.
    let state = DaemonState::shared();
    let Json(body) = register_session(
        State(Arc::clone(&state)),
        Json(RegisterSession {
            workdir: "/tmp/friendly".into(),
            project_path: None,
        }),
    )
    .await;
    let id = body.id.0.to_string();
    let listed = state.list_sessions();
    let session = listed
        .iter()
        .find(|s| s.id.0.to_string() == id)
        .expect("session registered");
    assert!(
        session.tmux_name.starts_with("tmpm-"),
        "friendly name: {}",
        session.tmux_name
    );
    assert!(session.tmux_name.len() <= 25);
}

#[tokio::test]
async fn reap_sessions_returns_removed_count() {
    // `DELETE /sessions/dead` always returns a well-formed `{ "removed": N }`
    // body. The exact count depends on whether tmux is installed: with tmux
    // the lone test session (no live tmux session named `tmpm-*`) is reaped
    // (1); without tmux nothing is reaped (0). Either way the registry must
    // not contain a session that is missing from tmux afterwards.
    let (state, _) = state_with_session();
    let Json(body) = reap_sessions(State(Arc::clone(&state))).await;
    let removed = body.removed;
    assert!(removed <= 1, "at most the one test session is reaped");
    assert_eq!(state.list_sessions().len(), 1 - removed);
}

#[tokio::test]
async fn register_session_returns_id_even_without_tmux() {
    // Graceful-degradation invariant: tmux is unavailable in CI, yet
    // `POST /sessions` must still return a JSON body carrying an `id`, and
    // that id must be visible in the subsequent `GET /sessions` snapshot.
    let state = DaemonState::shared();
    let Json(body) = register_session(
        State(Arc::clone(&state)),
        Json(RegisterSession {
            workdir: "/tmp/no-tmux".into(),
            project_path: None,
        }),
    )
    .await;
    let id_str = body.id.0.to_string();
    let listed = state.list_sessions();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id.0.to_string(), id_str);
}

#[tokio::test]
async fn hook_relay_rejects_bad_session_id() {
    let (state, _) = state_with_session();
    let post = HookPost {
        session_id: "not-a-uuid".into(),
        event: HookEvent::Stop,
        payload: serde_json::Value::Null,
    };
    let err = ingest_hook(State(state), Json(post)).await.unwrap_err();
    assert_eq!(err.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn hook_relay_runs_with_disabled_overseer() {
    // With the overseer disabled (the default), a PreToolUse event must
    // still be ingested normally — the overseer fast-path allows it.
    let (state, id) = state_with_session();
    let post = HookPost {
        session_id: id.0.to_string(),
        event: HookEvent::PreToolUse,
        payload: serde_json::json!({"tool": "Bash", "input": {"command": "ls"}}),
    };
    let result = ingest_hook(State(state.clone()), Json(post)).await;
    assert!(result.is_ok());
    assert_eq!(state.recent_hook_events().len(), 1);
}

#[tokio::test]
async fn openapi_spec_is_valid() {
    // `GET /api-docs/openapi.json` must return 200 with a document that
    // carries the `openapi` version key and the daemon's title.
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = router(DaemonState::shared());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api-docs/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let spec: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        spec.get("openapi").is_some(),
        "spec must have an openapi key"
    );
    assert!(
        spec["info"]["title"]
            .as_str()
            .unwrap_or_default()
            .contains("trusty-mpm"),
        "spec title must mention trusty-mpm"
    );
}

#[tokio::test]
async fn pause_then_resume_round_trips() {
    // Pausing flips a session to `Paused`; resuming flips it back to
    // `Active` and clears the pause metadata.
    let (state, id) = state_with_session();
    let Json(body) = pause_session(
        State(Arc::clone(&state)),
        Path(id.0.to_string()),
        Json(PauseRequest {
            summary: Some("mid-task".into()),
        }),
    )
    .await
    .expect("pause succeeds");
    assert!(body.paused);
    assert_eq!(body.summary, "mid-task");

    let paused = state.session(id).expect("session exists");
    assert_eq!(paused.status, SessionStatus::Paused);
    assert_eq!(paused.pause_summary.as_deref(), Some("mid-task"));
    assert!(paused.paused_at.is_some());

    let Json(resumed) = resume_session(State(Arc::clone(&state)), Path(id.0.to_string()))
        .await
        .expect("resume succeeds");
    assert!(resumed.resumed);

    let active = state.session(id).expect("session exists");
    assert_eq!(active.status, SessionStatus::Active);
    assert_eq!(active.paused_at, None);
    assert_eq!(active.pause_summary, None);
}

#[tokio::test]
async fn pause_unknown_session_is_404() {
    let state = DaemonState::shared();
    let err = pause_session(
        State(state),
        Path(SessionId::new().0.to_string()),
        Json(PauseRequest::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resume_unpaused_session_is_409() {
    // A session that was never paused cannot be resumed.
    let (state, id) = state_with_session();
    let err = resume_session(State(state), Path(id.0.to_string()))
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn command_to_stopped_session_is_409() {
    let state = DaemonState::shared();
    let id = SessionId::new();
    let mut session = Session::new(id, "/tmp/p", ControlModel::Tmux);
    session.status = SessionStatus::Stopped;
    state.register_session(session);

    let err = send_command(
        State(state),
        Path(id.0.to_string()),
        Query(CommandQuery::default()),
        Json(CommandRequest {
            command: "help".into(),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn output_unknown_session_is_404() {
    let state = DaemonState::shared();
    let err = get_output(
        State(state),
        Path(SessionId::new().0.to_string()),
        Query(OutputQuery::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pause_resolves_session_by_friendly_name() {
    // The pause endpoint accepts a friendly tmux name, not just a UUID.
    let (state, id) = state_with_session();
    let name = state.session(id).expect("session").tmux_name;
    let Json(body) = pause_session(
        State(Arc::clone(&state)),
        Path(name),
        Json(PauseRequest::default()),
    )
    .await
    .expect("pause by name succeeds");
    assert!(body.paused);
}

#[test]
fn send_command_compress_query_defaults_off() {
    // A `CommandQuery` with no `compress` field deserializes to `None`, so
    // omitting `?compress=` defaults to no compression.
    let query: CommandQuery = serde_json::from_str("{}").expect("empty query deserializes");
    assert_eq!(query.compress, None);
}

#[test]
fn output_query_defaults() {
    // An `OutputQuery` with no fields set has neither a line count nor a
    // compression level.
    let query: OutputQuery = serde_json::from_str("{}").expect("empty query deserializes");
    assert_eq!(query.lines, None);
    assert_eq!(query.compress, None);
}

#[test]
fn compress_level_roundtrips_serde() {
    // `CompressionLevel::Summarise` serializes to the lowercase wire name
    // `"summarise"` and deserializes back to the same variant.
    let json = serde_json::to_string(&CompressionLevel::Summarise).expect("serialize");
    assert_eq!(json, "\"summarise\"");
    let parsed: CompressionLevel = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, CompressionLevel::Summarise);
}

#[test]
fn compress_level_label_matches_serde() {
    // The lowercase label helper agrees with serde's wire representation.
    assert_eq!(compression_level_label(CompressionLevel::Off), "off");
    assert_eq!(compression_level_label(CompressionLevel::Trim), "trim");
    assert_eq!(
        compression_level_label(CompressionLevel::Summarise),
        "summarise"
    );
    assert_eq!(
        compression_level_label(CompressionLevel::Caveman),
        "caveman"
    );
}

#[test]
fn apply_compression_off_is_passthrough() {
    // With no level, the text is returned unchanged and there is no label.
    let result = apply_compression(None, "raw pane text");
    assert_eq!(result.text, "raw pane text");
    assert_eq!(result.level_label, None);
}

#[test]
fn apply_compression_summarise() {
    // With a level set, the label is recorded and stats reflect the input.
    let raw = "x".repeat(100);
    let result = apply_compression(Some(CompressionLevel::Summarise), &raw);
    assert_eq!(result.level_label.as_deref(), Some("summarise"));
    assert_eq!(result.stats.original_bytes, 100);
}

#[tokio::test]
async fn adopt_tmux_session_handles_missing() {
    // Adopting a session that does not exist (or with tmux absent) is 404.
    let state = DaemonState::shared();
    let result = adopt_tmux_session(
        State(state),
        Json(AdoptRequest {
            session: "trusty-mpm-no-such-session-xyz".into(),
        }),
    )
    .await;
    assert_eq!(result.unwrap_err().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tmux_snapshot_unknown_session_is_404() {
    let state = DaemonState::shared();
    let result = tmux_snapshot(State(state), Path("no-such-session-xyz".into())).await;
    assert_eq!(result.unwrap_err().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_checkpoint_returns_id() {
    // `POST /claude-config/checkpoints` returns an `id` and the checkpoint
    // is then visible via the list endpoint.
    let dir = tempfile::tempdir().unwrap();
    let state = DaemonState::shared();
    let Json(body) = create_checkpoint(
        State(Arc::clone(&state)),
        Json(CreateCheckpointRequest {
            project: dir.path().to_path_buf(),
            label: Some("manual".into()),
        }),
    )
    .await
    .expect("create succeeds");
    assert!(!body.id.is_empty());

    let Json(listed) = list_checkpoints(
        State(state),
        Query(CheckpointQuery {
            project: dir.path().to_path_buf(),
        }),
    )
    .await;
    assert_eq!(listed.checkpoints.len(), 1);
}

#[tokio::test]
async fn restore_unknown_checkpoint_is_500() {
    let dir = tempfile::tempdir().unwrap();
    let state = DaemonState::shared();
    let err = restore_checkpoint(
        State(state),
        Json(RestoreRequest {
            project: dir.path().to_path_buf(),
            checkpoint_id: "no-such-checkpoint".into(),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn delete_unknown_checkpoint_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let state = DaemonState::shared();
    let err = delete_checkpoint(
        State(state),
        Path("no-such-checkpoint".into()),
        Query(CheckpointQuery {
            project: dir.path().to_path_buf(),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deploy_profile_returns_checkpoint_id() {
    // `POST /claude-config/deploy` deploys a built-in profile and returns a
    // checkpoint id for undo.
    let dir = tempfile::tempdir().unwrap();
    let state = DaemonState::shared();
    let Json(body) = deploy_profile(
        State(state),
        Json(DeployProfileRequest {
            project: dir.path().to_path_buf(),
            profile_name: "minimal".into(),
            target: None,
        }),
    )
    .await
    .expect("deploy succeeds");
    assert_eq!(body.deployed, "minimal");
    assert!(!body.checkpoint_id.is_empty());
}

#[tokio::test]
async fn deploy_unknown_profile_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let state = DaemonState::shared();
    let err = deploy_profile(
        State(state),
        Json(DeployProfileRequest {
            project: dir.path().to_path_buf(),
            profile_name: "no-such-profile".into(),
            target: None,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pair_confirm_rejects_bad_code() {
    // A code that was never issued must not pair the daemon.
    let state = DaemonState::shared();
    let _ = pair_request(State(Arc::clone(&state))).await;
    let Json(confirm) = pair_confirm(
        State(Arc::clone(&state)),
        Json(PairConfirmRequest {
            code: "ZZZZZZ".into(),
            chat_id: 777,
        }),
    )
    .await;
    assert!(!confirm.success);
    assert!(confirm.error.as_deref().unwrap().contains("invalid"));

    let Json(status) = pair_status(State(state)).await;
    assert!(!status.paired);
    assert!(status.chat_id.is_none());
}

#[tokio::test]
async fn apply_claude_config_unknown_rec_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let state = DaemonState::shared();
    let result = apply_claude_config(
        State(state),
        Json(ApplyConfigRequest {
            project: dir.path().to_path_buf(),
            recommendation_id: "no-such-recommendation".into(),
        }),
    )
    .await;
    assert_eq!(result.unwrap_err(), StatusCode::NOT_FOUND);
}
