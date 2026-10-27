//! The Jira connector against the in-process mock of the endpoints it uses.
//! None of this has run against a live Jira.

mod mock;

use mock::{start, Mock, EMAIL, TOKEN};
use serde_json::{json, Value};
use simpletickets_connector_jira::JiraConnector;
use simpletickets_connector_protocol::{Connector, ErrorCode, ErrorInfo};

fn connector_with(mock: &Mock, options: Value) -> JiraConnector {
    let url = mock.url.clone();
    let mut connector = JiraConnector::with_env(move |name| match name {
        "JIRA_SITE_URL" => Some(url.clone()),
        "JIRA_EMAIL" => Some(EMAIL.to_string()),
        "JIRA_API_TOKEN" => Some(TOKEN.to_string()),
        _ => None,
    });
    let mut merged = json!({"project": "ST", "retry": {"attempts": 3, "base_ms": 1, "max_ms": 5}});
    for (k, v) in options.as_object().into_iter().flatten() {
        merged[k] = v.clone();
    }
    connector
        .handle("configure", &json!({"options": merged}))
        .unwrap();
    connector
}

fn connector(mock: &Mock) -> JiraConnector {
    connector_with(mock, json!({}))
}

fn ticket(id: &str) -> Value {
    json!({"id": id, "title": "Write report", "body": "Some **bold** text\n\n- one\n- two\n"})
}

fn push(
    c: &mut JiraConnector,
    ticket: Value,
    fields: Value,
    links: Value,
    reference: Value,
) -> Result<Value, ErrorInfo> {
    c.handle(
        "push",
        &json!({"ticket": ticket, "fields": fields, "links": links, "ref": reference}),
    )
}

fn simple_push(c: &mut JiraConnector, id: &str, reference: Value, status: &str) -> Value {
    push(
        c,
        ticket(id),
        json!({"title": "Write report", "tags": ["work"], "due": null, "status": status}),
        json!({"parent_ref": null, "blocked_by_refs": [], "previous_blocked_by_refs": []}),
        reference,
    )
    .unwrap()
}

#[test]
fn configure_reports_missing_credentials_by_name_and_never_by_value() {
    let mut c = JiraConnector::with_env(|name| {
        (name == "JIRA_SITE_URL").then(|| "https://x.atlassian.net".into())
    });
    let error = c
        .handle("configure", &json!({"options": {"project": "ST"}}))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Auth);
    assert!(error.message.contains("JIRA_EMAIL"));
    assert!(!error.retryable);

    let mut plain = JiraConnector::with_env(|name| match name {
        "JIRA_SITE_URL" => Some("http://jira.example.test".into()),
        _ => Some(TOKEN.into()),
    });
    let error = plain
        .handle("configure", &json!({"options": {"project": "ST"}}))
        .unwrap_err();
    assert!(error.message.contains("https"));
    assert!(!error.message.contains(TOKEN));

    let mut none = JiraConnector::with_env(|_| None);
    assert!(none.handle("configure", &json!({"options": {}})).is_err());
    assert!(none
        .handle(
            "configure",
            &json!({"options": {"project": "ST", "description": "html"}})
        )
        .is_err());
    assert_eq!(
        none.handle("push", &json!({})).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn check_authenticates_and_finds_the_project() {
    let mock = start();
    let mut c = connector(&mock);
    let message = c.handle("check", &json!({})).unwrap()["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("Mock User"), "{message}");
    assert!(message.contains("Mock Project"), "{message}");
    assert!(!message.contains(TOKEN));
}

#[test]
fn push_creates_an_issue_with_mapped_fields_and_moves_it_to_the_wanted_status() {
    let mock = start();
    let mut c = connector(&mock);
    let result = push(
        &mut c,
        ticket("ST-1"),
        json!({"title": "Write report", "tags": ["needs review", "work"], "due": "2026-11-05",
               "status": "In Progress", "priority": "High", "assignee": "acc-123"}),
        json!({"parent_ref": null, "blocked_by_refs": [], "previous_blocked_by_refs": []}),
        Value::Null,
    )
    .unwrap();
    assert_eq!(result["outcome"], "created");
    assert_eq!(result["ref"], "ST-1");
    assert_eq!(result["fields"]["status"], "In Progress");
    assert!(result["url"].as_str().unwrap().ends_with("/browse/ST-1"));

    let state = mock.state.lock().unwrap();
    let issue = state.issue("ST-1");
    assert_eq!(issue.status, "In Progress");
    assert_eq!(issue.fields["summary"], "Write report");
    assert_eq!(issue.fields["issuetype"]["name"], "Task");
    assert_eq!(issue.fields["priority"]["name"], "High");
    assert_eq!(issue.fields["assignee"]["accountId"], "acc-123");
    assert_eq!(issue.fields["duedate"], "2026-11-05");
    assert_eq!(
        issue.fields["labels"],
        json!(["needs_review", "simpletickets-ST-1", "work"])
    );
    let description = &issue.fields["description"];
    assert_eq!(description["type"], "doc");
    assert_eq!(
        description["content"][0]["content"][1]["marks"][0]["type"],
        "strong"
    );
    assert_eq!(description["content"][1]["type"], "bulletList");
}

#[test]
fn every_request_carries_basic_authentication_and_the_token_is_never_echoed() {
    let mock = start();
    let mut c = connector(&mock);
    let result = simple_push(&mut c, "ST-1", Value::Null, "To Do");
    assert!(!result.to_string().contains(TOKEN));
    // The mock answers 401 to any request without the exact header, so reaching an
    // issue at all proves it was sent; a bad token is refused as `auth`.
    let mut wrong = JiraConnector::with_env({
        let url = mock.url.clone();
        move |name| match name {
            "JIRA_SITE_URL" => Some(url.clone()),
            "JIRA_EMAIL" => Some(EMAIL.into()),
            _ => Some("wrong-token".into()),
        }
    });
    wrong
        .handle("configure", &json!({"options": {"project": "ST"}}))
        .unwrap();
    let error = wrong.handle("check", &json!({})).unwrap_err();
    assert_eq!(error.code, ErrorCode::Auth);
    assert!(!error.retryable);
    assert!(!error.message.contains("wrong-token"));
}

#[test]
fn push_updates_an_existing_issue_and_only_transitions_when_the_status_differs() {
    let mock = start();
    let mut c = connector(&mock);
    simple_push(&mut c, "ST-1", Value::Null, "To Do");
    let again = simple_push(&mut c, "ST-1", json!("ST-1"), "To Do");
    assert_eq!(again["outcome"], "updated");
    assert_eq!(mock.state.lock().unwrap().count("POST", "/transitions"), 0);

    let moved = simple_push(&mut c, "ST-1", json!("ST-1"), "Done");
    assert_eq!(moved["fields"]["status"], "Done");
    let state = mock.state.lock().unwrap();
    assert_eq!(state.issue("ST-1").status, "Done");
    assert_eq!(state.count("POST", "/transitions"), 1);
    assert_eq!(state.issues.len(), 1);
}

#[test]
fn clearing_the_assignee_and_due_date_sends_null() {
    let mock = start();
    let mut c = connector(&mock);
    let links = json!({"parent_ref": null, "blocked_by_refs": [], "previous_blocked_by_refs": []});
    push(
        &mut c,
        ticket("ST-1"),
        json!({"title": "T", "tags": [], "due": "2026-11-05", "assignee": "acc-1"}),
        links.clone(),
        Value::Null,
    )
    .unwrap();
    push(
        &mut c,
        ticket("ST-1"),
        json!({"title": "T", "tags": [], "due": null, "assignee": null}),
        links,
        json!("ST-1"),
    )
    .unwrap();
    let state = mock.state.lock().unwrap();
    assert!(state.issue("ST-1").fields.get("assignee").is_none());
    assert!(state.issue("ST-1").fields.get("duedate").is_none());
}

#[test]
fn a_status_with_no_transition_is_a_warning_and_the_real_status_is_reported() {
    let mock = start();
    let mut c = connector(&mock);
    let result = simple_push(&mut c, "ST-1", Value::Null, "Cancelled");
    assert_eq!(result["fields"]["status"], "To Do");
    let warning = result["warnings"][0].as_str().unwrap();
    assert!(warning.contains("Cancelled"), "{warning}");
}

#[test]
fn sub_tasks_get_the_sub_task_type_and_their_parent() {
    let mock = start();
    let mut c = connector(&mock);
    simple_push(&mut c, "ST-1", Value::Null, "To Do");
    push(
        &mut c,
        ticket("ST-2"),
        json!({"title": "Child", "tags": [], "due": null}),
        json!({"parent_ref": "ST-1", "blocked_by_refs": [], "previous_blocked_by_refs": []}),
        Value::Null,
    )
    .unwrap();
    let state = mock.state.lock().unwrap();
    let child = state.issue("ST-2");
    assert_eq!(child.fields["issuetype"]["name"], "Sub-task");
    assert_eq!(child.fields["parent"]["key"], "ST-1");
}

#[test]
fn a_ticket_whose_issue_exists_is_found_by_its_label_and_not_created_twice() {
    let mock = start();
    let mut c = connector(&mock);
    simple_push(&mut c, "ST-1", Value::Null, "To Do");
    // The sync plugin lost its state: it asks for a create again.
    let result = simple_push(&mut c, "ST-1", Value::Null, "To Do");
    assert_eq!(result["outcome"], "updated");
    assert_eq!(result["ref"], "ST-1");
    let state = mock.state.lock().unwrap();
    assert_eq!(state.issues.len(), 1);
    assert_eq!(state.count("POST", "/rest/api/3/issue"), 1);
}

#[test]
fn blocked_by_becomes_a_blocks_link_in_the_right_direction_and_is_removed_when_dropped() {
    let mock = start();
    let mut c = connector(&mock);
    simple_push(&mut c, "ST-1", Value::Null, "To Do");
    simple_push(&mut c, "ST-2", Value::Null, "To Do");
    let blocked = |refs: Value, previous: Value| json!({"parent_ref": null, "blocked_by_refs": refs, "previous_blocked_by_refs": previous});
    let fields = json!({"title": "T", "tags": []});
    // ST-2 (the second issue) is blocked by ST-1.
    push(
        &mut c,
        ticket("ST-2"),
        fields.clone(),
        blocked(json!(["ST-1"]), json!([])),
        json!("ST-2"),
    )
    .unwrap();
    {
        let state = mock.state.lock().unwrap();
        assert_eq!(state.links.len(), 1);
        assert_eq!(state.links[0].kind, "Blocks");
        assert_eq!(state.links[0].inward, "ST-2");
        assert_eq!(state.links[0].outward, "ST-1");
    }
    // Pushing again does not duplicate the link.
    push(
        &mut c,
        ticket("ST-2"),
        fields.clone(),
        blocked(json!(["ST-1"]), json!(["ST-1"])),
        json!("ST-2"),
    )
    .unwrap();
    assert_eq!(mock.state.lock().unwrap().links.len(), 1);

    // A link somebody made by hand is kept when the ticket drops a different blocker.
    mock.state.lock().unwrap().links.push(mock::Link {
        id: "H1".into(),
        kind: "Blocks".into(),
        inward: "ST-2".into(),
        outward: "ST-9".into(),
    });
    push(
        &mut c,
        ticket("ST-2"),
        fields,
        blocked(json!([]), json!(["ST-1"])),
        json!("ST-2"),
    )
    .unwrap();
    let state = mock.state.lock().unwrap();
    assert_eq!(state.links.len(), 1);
    assert_eq!(state.links[0].outward, "ST-9");
}

#[test]
fn a_link_to_a_missing_issue_is_a_warning_not_a_failure() {
    let mock = start();
    let mut c = connector(&mock);
    let result = push(
        &mut c,
        ticket("ST-1"),
        json!({"title": "T", "tags": []}),
        json!({"parent_ref": null, "blocked_by_refs": ["ST-404"], "previous_blocked_by_refs": []}),
        Value::Null,
    )
    .unwrap();
    assert_eq!(result["ref"], "ST-1");
    assert!(result["warnings"][0].as_str().unwrap().contains("ST-404"));
}

#[test]
fn rate_limits_are_retried_honouring_retry_after_then_reported_as_retryable() {
    let mock = start();
    let mut c = connector(&mock);
    simple_push(&mut c, "ST-1", Value::Null, "To Do");
    {
        let mut state = mock.state.lock().unwrap();
        state.fail_on = "/issue/ST-1".into();
        state.failures.extend([(429, Some(0)), (429, None)]);
        state.requests.clear();
    }
    let result = simple_push(&mut c, "ST-1", json!("ST-1"), "To Do");
    assert_eq!(result["ref"], "ST-1");
    assert_eq!(mock.state.lock().unwrap().count("PUT", "/issue/ST-1"), 3);

    {
        let mut state = mock.state.lock().unwrap();
        state
            .failures
            .extend([(429, Some(7)), (429, Some(7)), (429, Some(7))]);
    }
    let error = push(
        &mut c,
        ticket("ST-1"),
        json!({"title": "T", "tags": []}),
        json!({}),
        json!("ST-1"),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::RateLimited);
    assert!(error.retryable);
    assert_eq!(error.retry_after_ms, Some(7000));
}

#[test]
fn server_errors_are_retried_for_reads_and_updates_but_never_for_creates() {
    let mock = start();
    let mut c = connector(&mock);
    simple_push(&mut c, "ST-1", Value::Null, "To Do");
    {
        let mut state = mock.state.lock().unwrap();
        state.fail_on = "/issue/ST-1".into();
        state.failures.extend([(503, None), (502, None)]);
        state.requests.clear();
    }
    simple_push(&mut c, "ST-1", json!("ST-1"), "To Do");
    assert_eq!(mock.state.lock().unwrap().count("PUT", "/issue/ST-1"), 3);

    // A create that fails with a server error may have happened; it is not repeated
    // blindly. The next push finds the issue through its label instead.
    {
        let mut state = mock.state.lock().unwrap();
        state.fail_on = "/rest/api/3/issue".into();
        state.failures.push_back((503, None));
        state.requests.clear();
    }
    let error = push(
        &mut c,
        ticket("ST-2"),
        json!({"title": "T", "tags": []}),
        json!({}),
        Value::Null,
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.retryable);
    assert_eq!(
        mock.state
            .lock()
            .unwrap()
            .count("POST", "/rest/api/3/issue"),
        1
    );
}

#[test]
fn refusals_map_to_not_found_and_rejected_with_jiras_message() {
    let mock = start();
    let mut c = connector(&mock);
    let gone = push(
        &mut c,
        ticket("ST-1"),
        json!({"title": "T"}),
        json!({}),
        json!("ST-77"),
    )
    .unwrap_err();
    assert_eq!(gone.code, ErrorCode::NotFound);
    assert!(!gone.retryable);

    simple_push(&mut c, "ST-1", Value::Null, "To Do");
    let refused = push(
        &mut c,
        ticket("ST-1"),
        json!({"title": ""}),
        json!({}),
        json!("ST-1"),
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Rejected);
    assert!(!refused.retryable);
    assert!(
        refused.message.contains("summary: bad"),
        "{}",
        refused.message
    );
}

#[test]
fn close_moves_the_issue_to_the_close_status_and_never_deletes_it() {
    let mock = start();
    let mut c = connector(&mock);
    simple_push(&mut c, "ST-1", Value::Null, "In Progress");
    c.handle("close", &json!({"ref": "ST-1"})).unwrap();
    {
        let state = mock.state.lock().unwrap();
        assert_eq!(state.issue("ST-1").status, "Done");
        assert_eq!(state.count("DELETE", "/issue/"), 0);
    }
    // Closing twice is fine.
    c.handle("close", &json!({"ref": "ST-1"})).unwrap();
    assert_eq!(
        c.handle("close", &json!({"ref": "ST-404"}))
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let mut custom = connector_with(&mock, json!({"close_status": "Closed"}));
    let error = custom.handle("close", &json!({"ref": "ST-1"})).unwrap_err();
    assert_eq!(error.code, ErrorCode::Rejected);
}

#[test]
fn fetch_returns_current_values_then_only_what_changed_and_pages() {
    let mock = start();
    let mut c = connector(&mock);
    let links = json!({"parent_ref": null, "blocked_by_refs": [], "previous_blocked_by_refs": []});
    for id in ["ST-1", "ST-2", "ST-3"] {
        push(
            &mut c,
            ticket(id),
            json!({"title": format!("Title {id}"), "tags": ["work"], "status": "To Do", "priority": "High", "assignee": "acc-1"}),
            links.clone(),
            Value::Null,
        )
        .unwrap();
    }
    let refs = json!(["ST-1", "ST-2"]);
    let first = c
        .handle("fetch", &json!({"cursor": null, "refs": refs}))
        .unwrap();
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "only known items are returned");
    assert_eq!(items[0]["fields"]["status"], "To Do");
    assert_eq!(items[0]["fields"]["priority"], "High");
    assert_eq!(items[0]["fields"]["assignee"], "acc-1");
    assert_eq!(items[0]["fields"]["tags"], json!(["work"]));
    assert_eq!(items[0]["fields"]["title"], "Title ST-1");
    let cursor = first["cursor"].as_str().unwrap().to_string();
    assert_eq!(first["more"], false);

    mock.state.lock().unwrap().set_status("ST-2", "Done");
    let second = c
        .handle(
            "fetch",
            &json!({"cursor": cursor, "refs": ["ST-1", "ST-2"]}),
        )
        .unwrap();
    let changed: Vec<&Value> = second["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["ref"] == "ST-2")
        .collect();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["fields"]["status"], "Done");
    assert!(changed[0]["version"].is_string());

    // With a small page size the connector hands back a cursor to continue.
    mock.state.lock().unwrap().page_size = 1;
    let mut seen = Vec::new();
    let mut next = cursor;
    for _ in 0..10 {
        let page = c
            .handle(
                "fetch",
                &json!({"cursor": next, "refs": ["ST-1", "ST-2", "ST-3"]}),
            )
            .unwrap();
        seen.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["ref"].as_str().unwrap().to_string()),
        );
        next = page["cursor"].as_str().unwrap().to_string();
        if page["more"] != true {
            break;
        }
    }
    assert!(seen.contains(&"ST-2".to_string()), "{seen:?}");
}
