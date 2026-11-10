//! Differential fixture capture; the driver compares these production paths to TS v0.9.8.
use super::*;

#[test]
#[ignore = "run scripts/verify_agents_view_native_parity.py with the TS reference checkout"]
fn native_agents_view_parity_snapshot() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../scripts/fixtures/agents-view-native-parity.json"
    ))
    .unwrap();
    let rows: Vec<AgentsViewRow> = fixture["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| AgentsViewRow {
            summary: row["summary"].clone(),
            kind: if row["kind"] == "subagent" {
                RowKind::Subagent
            } else {
                RowKind::Agent
            },
            section: match row["section"].as_str().unwrap() {
                "running" => Section::Running,
                "idle" => Section::Idle,
                "inactive" => Section::Inactive,
                section => panic!("unexpected section {section}"),
            },
            identity: row["identity"].as_str().unwrap().to_string(),
            parent_identity: row["parentIdentity"].as_str().map(str::to_string),
            title: row["title"].as_str().unwrap().to_string(),
            model: row["model"].as_str().unwrap().to_string(),
            cost: row["cost"].as_f64().unwrap(),
            age: row["age"].as_str().unwrap().to_string(),
            depth: row["depth"].as_u64().unwrap() as usize,
            descendant_count: 0,
            running_subagent_count: 0,
            expanded: false,
            has_spawn_code: false,
        })
        .collect();
    let mut mode = mode_with_parent_and_child();
    mode.roster = rows
        .iter()
        .map(|row| roster_entry(&row.identity, "idle", &row.summary))
        .collect();
    mode.rows = rows;
    let rendered: Vec<Value> = fixture["widths"].as_array().unwrap().iter().map(|width| {
        let width = width.as_u64().unwrap() as usize;
        let layout = build_layout(&mode.rows, width);
        serde_json::json!({"width":width, "legend":layout.legend,
            "rows":mode.rows.iter().map(|row| flat(&mode.render_row(row,&layout,width,false))).collect::<Vec<_>>()})
    }).collect();
    let mut actions = Vec::new();
    for index in 0..mode.rows.len() {
        mode.selected = index;
        mode.opened = None;
        mode.open_selected();
        let opened = mode.opened.take().unwrap();
        let (selection, target) = match opened.selection {
            SessionSelection::Attach(target) => ("attach", target),
            SessionSelection::Resume(target) => ("resume", target.to_string_lossy().into_owned()),
            SessionSelection::New | SessionSelection::NewChild { .. } => {
                panic!("native row opens an existing session")
            }
        };
        mode.running = true;
        mode.composer = Composer::Search;
        mode.pending_reply = None;
        mode.handle_key("space");
        let reply_key = mode.reply_target().map(|target| target.key);
        let armed = matches!(mode.composer, Composer::Reply(_));
        if armed {
            mode.handle_paste("Follow up");
            mode.handle_key("enter");
        }
        let reply = mode.pending_reply.take().map(|request|serde_json::json!({
            "key":request.key,"text":request.text,"activeSessionId":request.summary.get("activeSessionId"),
            "sessionFile":request.summary["sessionFile"]
        }));
        actions.push(serde_json::json!({"identity":mode.rows[index].identity,"open":{"selection":selection,"target":target,
            "ancestors":opened.expanded_ancestors},"replyArmed":armed,"replyKey":reply_key,"reply":reply}));
    }
    let roster: Vec<Value> = mode
        .rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "identity":crate::agents_view_forest::summary_identity(&row.summary),
                "agentId":pa_types::daemon::agent_roster::roster_agent_id_for_summary(&row.summary),
                "status":pa_types::daemon::agent_roster::classify_summary_value(&row.summary,false)
            })
        })
        .collect();
    let snapshot = serde_json::json!({"rendered":rendered,"actions":actions,"roster":roster});
    let path = std::env::var("PA_NATIVE_PARITY_SNAPSHOT").expect("snapshot output path");
    std::fs::write(path, serde_json::to_vec_pretty(&snapshot).unwrap()).unwrap();
}
