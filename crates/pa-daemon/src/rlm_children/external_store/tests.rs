//! Result-source and manifest regression tests for RFC-0001.

use super::*;

fn record(dir: &Path) -> ExternalRecord {
    ExternalRecord {
        child_id: "sub-12345678".into(),
        name: "review".into(),
        dir: dir.into(),
        socket_path: "/tmp/herdr.sock".into(),
        info: ExternalSubagentInfo {
            agent: "codex".into(),
            workspace_id: "w1".into(),
            pane_id: "w1:p2".into(),
            terminal_id: "terminal-2".into(),
            agent_name: "pa-sub-12345678".into(),
            turn: 1,
            state: "working".into(),
            answer_path: Some(
                dir.join("turns/001/answer.md")
                    .to_string_lossy()
                    .into_owned(),
            ),
            answer_truncated: false,
            output: None,
            notification_error: None,
            worktree: None,
        },
        expected_agent: Some("codex".into()),
        status: "running".into(),
        settled: false,
        text: String::new(),
        error: None,
        baseline_state_seq: 3,
        baseline_completion_seq: Some(2),
        started_at_ms: 1000,
        notice_sent: false,
        closed: false,
    }
}

#[tokio::test]
async fn manifests_round_trip_and_replace_atomically() {
    let root = tempfile::tempdir().unwrap();
    let mut expected = record(&root.path().join("child"));
    expected.persist().await.unwrap();
    let path = expected.dir.join(MANIFEST_NAME);
    assert_eq!(load(&path).await.unwrap(), expected);
    expected.text = "done".into();
    expected.status = "completed".into();
    expected.settled = true;
    expected.info.worktree = Some(
        pa_types::daemon::external_subagent::ExternalSubagentWorktree {
            path: root.path().join("checkout").display().to_string(),
            branch: "task/review".into(),
            repo_root: root.path().display().to_string(),
        },
    );
    expected.persist().await.unwrap();
    assert_eq!(load(&path).await.unwrap(), expected);
    let entries = std::fs::read_dir(&expected.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![std::ffi::OsString::from(MANIFEST_NAME)]);
}

#[tokio::test]
async fn malformed_and_misplaced_manifests_are_explicit_errors() {
    let root = tempfile::tempdir().unwrap();
    let expected = record(root.path());
    let path = root.path().join(MANIFEST_NAME);
    for bytes in [
        b"{broken".as_slice(),
        b"{\"version\":1,\"record\":{}}".as_slice(),
    ] {
        tokio::fs::write(&path, bytes).await.unwrap();
        assert!(load(&path).await.is_err());
    }
    let unsupported = serde_json::to_vec(&Manifest {
        version: 999,
        record: expected.clone(),
    })
    .unwrap();
    tokio::fs::write(&path, unsupported).await.unwrap();
    assert!(load(&path)
        .await
        .unwrap_err()
        .to_string()
        .contains("unsupported version"));
    let misplaced = Manifest {
        version: STORE_VERSION,
        record: record(&root.path().join("other")),
    };
    tokio::fs::write(&path, serde_json::to_vec(&misplaced).unwrap())
        .await
        .unwrap();
    assert!(load(&path)
        .await
        .unwrap_err()
        .to_string()
        .contains("does not belong"));
}

#[tokio::test]
async fn malformed_turn_paths_runtime_identities_and_statuses_are_rejected_on_write_and_load() {
    let root = tempfile::tempdir().unwrap();
    let valid = record(root.path());
    let mut invalid = Vec::new();
    let mut case = valid.clone();
    case.info.answer_path = Some(root.path().join("another-answer.md").display().to_string());
    invalid.push(case);
    let mut case = valid.clone();
    case.info.turn = 2;
    invalid.push(case);
    let mut case = valid.clone();
    case.info.answer_path = None;
    invalid.push(case);
    let mut case = valid.clone();
    case.dir = PathBuf::from("relative-child");
    invalid.push(case);
    let mut case = valid.clone();
    case.info.turn = 0;
    invalid.push(case);
    let mut case = valid.clone();
    case.info.terminal_id = " ".into();
    invalid.push(case);
    let mut case = valid.clone();
    case.info.workspace_id.clear();
    invalid.push(case);
    let mut case = valid.clone();
    case.info.pane_id.clear();
    invalid.push(case);
    let mut case = valid.clone();
    case.info.agent_name.clear();
    invalid.push(case);
    let mut case = valid.clone();
    case.expected_agent = Some(String::new());
    invalid.push(case);
    let mut case = valid.clone();
    case.status = "idle".into();
    invalid.push(case);
    let mut case = valid.clone();
    case.info.worktree = Some(
        pa_types::daemon::external_subagent::ExternalSubagentWorktree {
            path: "relative-checkout".into(),
            branch: "task/review".into(),
            repo_root: root.path().display().to_string(),
        },
    );
    invalid.push(case);
    let mut case = valid;
    case.settled = true;
    invalid.push(case);
    let path = root.path().join(MANIFEST_NAME);
    for record in invalid {
        assert!(record.persist().await.is_err(), "{record:?}");
        let malformed = Manifest {
            version: STORE_VERSION,
            record,
        };
        tokio::fs::write(&path, serde_json::to_vec(&malformed).unwrap())
            .await
            .unwrap();
        assert!(load(&path).await.is_err(), "{malformed:?}");
    }
}

#[tokio::test]
async fn rejected_updates_preserve_the_previous_manifest() {
    let root = tempfile::tempdir().unwrap();
    let expected = record(root.path());
    expected.persist().await.unwrap();
    let mut invalid = expected.clone();
    invalid.name.clear();
    assert!(invalid.persist().await.is_err());
    assert_eq!(
        load(&root.path().join(MANIFEST_NAME)).await.unwrap(),
        expected
    );
}

#[tokio::test]
async fn rejects_missing_nonregular_empty_and_invalid_answers() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("answer.md");
    assert!(read_answer(&path, 100).await.is_err());
    assert!(read_answer(root.path(), 100).await.is_err());
    for bytes in [
        b"".as_slice(),
        b" \n\t".as_slice(),
        b"valid prefix\xff".as_slice(),
        b"valid prefix\xf0\x9f".as_slice(),
    ] {
        tokio::fs::write(&path, bytes).await.unwrap();
        assert!(read_answer(&path, 3).await.is_err());
    }
    tokio::fs::write(&path, "done").await.unwrap();
    assert!(read_answer(&path, 0).await.is_err());
}

#[tokio::test]
async fn truncation_counts_characters_and_keeps_the_answer_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("answer.md");
    let answer = "检查🙂结果";
    tokio::fs::write(&path, answer).await.unwrap();
    assert_eq!(
        read_answer(&path, 3).await.unwrap(),
        ("检查🙂".into(), true)
    );
    assert_eq!(
        read_answer(&path, answer.chars().count()).await.unwrap(),
        (answer.into(), false)
    );
    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), answer);
}

#[tokio::test]
async fn utf8_split_across_read_chunks_is_validated_after_the_prefix_limit() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("answer.md");
    let prefix = "a".repeat(8191);
    let answer = format!("{prefix}🙂tail");
    tokio::fs::write(&path, &answer).await.unwrap();
    assert_eq!(
        read_answer(&path, 8192).await.unwrap(),
        (format!("{prefix}🙂"), true)
    );
    let mut invalid = answer.into_bytes();
    invalid.push(0xff);
    tokio::fs::write(&path, invalid).await.unwrap();
    assert!(read_answer(&path, 1).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn answer_paths_cannot_redirect_through_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("elsewhere.md");
    let path = root.path().join("answer.md");
    tokio::fs::write(&source, "done").await.unwrap();
    std::os::unix::fs::symlink(&source, &path).unwrap();
    assert!(read_answer(&path, 100).await.is_err());
    assert_eq!(tokio::fs::read_to_string(&source).await.unwrap(), "done");
}
