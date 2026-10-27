//! A live in-process daemon, the sync engine, the real connector binary over the protocol
//! and the mock Jira: mapping, push, write-back. Not a statement about a live Jira.

mod mock;

use serde_json::{json, Value};
use simpletickets::daemon::{self, Options};
use simpletickets_sync::api::Api;
use simpletickets_sync::config::Config;
use simpletickets_sync::engine::{Engine, Options as Pass};
use std::collections::BTreeSet;

#[test]
fn tickets_reach_the_mock_jira_with_mapped_values_and_status_changes_come_back() {
    let jira = mock::start();
    // The connector binary reads its credentials from its environment, as in real use.
    std::env::set_var("JIRA_SITE_URL", &jira.url);
    std::env::set_var("JIRA_EMAIL", mock::EMAIL);
    std::env::set_var("JIRA_API_TOKEN", mock::TOKEN);

    let folder = tempfile::tempdir().unwrap();
    let dir = folder.path().to_path_buf();
    let mut options = Options::new(dir.clone());
    options.port = 0;
    options.watch = false;
    options.user_plugins_dir = None;
    let running = daemon::start(options).expect("daemon starts");
    let token = std::fs::read_to_string(running.token_file()).unwrap();
    let url = format!("http://{}", running.addr);
    let sync = Api::new(&url, &token, "plugin:sync").unwrap();
    let user = Api::new(&url, &token, "test-user").unwrap();

    let config = Config::parse(&format!(
        "targets:
  jira:
    connector:
      command: '{}'
      env: [JIRA_SITE_URL, JIRA_EMAIL, JIRA_API_TOKEN]
      timeout_seconds: 30
    options: {{project: ST, retry: {{attempts: 2, base_ms: 1, max_ms: 5}}}}
    direction: both
    map:
      status: {{todo: To Do, in-progress: In Progress, done: Done}}
      priority: {{high: High}}
      assignee: {{ann: acc-ann}}
",
        env!("CARGO_BIN_EXE_simpletickets-connector-jira")
    ))
    .unwrap();
    let run = |only: Option<&str>| {
        Engine::new(&sync, &config, &dir, None)
            .run(&Pass {
                pull: true,
                only_ids: only.map(|id| BTreeSet::from([id.to_string()])),
                ..Pass::default()
            })
            .unwrap()
    };

    let (status, created) = user
        .request(
            "POST",
            "/v1/tickets",
            &[],
            Some(
                &json!({"title": "Write report", "priority": "high", "assignee": "ann",
                          "tags": ["work"], "body": "Hello **Jira**\n"}),
            ),
        )
        .unwrap();
    assert_eq!(status, 201);
    let id = created["id"].as_str().unwrap().to_string();

    let report = run(None);
    assert!(
        !report.has_errors(),
        "{:?} {:?}",
        report.actions,
        report.target_errors
    );
    {
        let state = jira.state.lock().unwrap();
        let issue = state.issue("ST-1");
        assert_eq!(issue.status, "To Do");
        assert_eq!(issue.fields["priority"]["name"], "High");
        assert_eq!(issue.fields["assignee"]["accountId"], "acc-ann");
        assert_eq!(issue.fields["summary"], "Write report");
        assert!(issue.fields["labels"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("simpletickets-{id}"))));
    }

    // A local status change is mapped to a Jira transition.
    let (status, _) = user
        .request(
            "PATCH",
            &format!("/v1/tickets/{id}"),
            &[],
            Some(&json!({"status": "in-progress"})),
        )
        .unwrap();
    assert_eq!(status, 200);
    run(Some(&id));
    assert_eq!(
        jira.state.lock().unwrap().issue("ST-1").status,
        "In Progress"
    );

    // A transition made in Jira is written back through the API and not pushed again.
    jira.state.lock().unwrap().set_status("ST-1", "Done");
    let report = run(None);
    assert!(
        report.actions.iter().any(|a| a.kind == "pull"),
        "{:?}",
        report.actions
    );
    let ticket = sync.get_ticket(&id).unwrap().unwrap();
    assert_eq!(ticket["status"], "done");
    assert_eq!(ticket["body"], "Hello **Jira**\n");
    let (_, audit) = sync
        .request("GET", "/v1/audit?actor=plugin%3Async", &[], None)
        .unwrap();
    assert!(audit
        .as_array()
        .unwrap()
        .iter()
        .any(|e: &Value| e["field"] == "status" && e["to"] == "done"));

    // Settled: another pass has nothing to do, and Jira still has the one issue.
    run(None);
    let report = run(None);
    assert!(report.actions.is_empty(), "{:?}", report.actions);
    let state = jira.state.lock().unwrap();
    assert_eq!(state.issues.len(), 1);
    let creates = state
        .requests
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/rest/api/3/issue")
        .count();
    assert_eq!(creates, 1);
    drop(state);
    running.stop();
}
