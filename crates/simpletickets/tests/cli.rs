use std::path::Path;
use std::process::{Command, Output};

fn st(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_simpletickets"))
        .arg("--dir")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = st(dir, args);
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn create_id(dir: &Path, args: &[&str]) -> String {
    let mut all = vec!["create", "--json"];
    all.extend_from_slice(args);
    let v: serde_json::Value = serde_json::from_str(&ok(dir, &all)).unwrap();
    v["id"].as_str().unwrap().to_string()
}

#[test]
fn create_list_show_set_status() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let a = create_id(d, &["Write report", "--priority", "high", "--tag", "work"]);
    let b = create_id(
        d,
        &["Send report", "--blocked-by", &a, "--due", "2026-11-05"],
    );
    assert_eq!((a.as_str(), b.as_str()), ("ST-1", "ST-2"));

    let all = ok(d, &["list"]);
    assert!(
        all.contains("Write report") && all.contains("Send report"),
        "{all}"
    );
    assert!(all.contains("[blocked]"));

    assert!(ok(d, &["list", "--blocked", "true"]).contains("Send report"));
    assert!(!ok(d, &["list", "--blocked", "true"]).contains("Write report"));
    assert!(ok(d, &["list", "--tag", "work"]).contains("Write report"));
    assert!(ok(d, &["list", "--due-by", "2026-11-05"]).contains("Send report"));
    assert!(!ok(d, &["list", "--priority", "low"]).contains("report"));

    let shown = ok(d, &["show", &b]);
    assert!(shown.contains("open blockers: ST-1"), "{shown}");
    let json: serde_json::Value = serde_json::from_str(&ok(d, &["show", &a, "--json"])).unwrap();
    assert_eq!(json["blocks"][0], "ST-2");
    assert!(json["body"].as_str().unwrap().contains("# Write report"));

    ok(d, &["set-status", &a, "done"]);
    assert!(ok(d, &["show", &a]).contains("status:    done"));
    assert!(!ok(d, &["list", "--blocked", "true"]).contains("Send report"));
    assert!(ok(d, &["list", "--category", "done"]).contains("Write report"));
}

#[test]
fn assignee_create_filter_assign_and_unassign() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let a = create_id(d, &["Owned", "--assignee", "Ann"]);
    let b = create_id(d, &["Free"]);
    assert!(ok(d, &["list", "--assignee", "ann"]).contains("Owned"));
    assert!(!ok(d, &["list", "--assignee", "ann"]).contains("Free"));
    assert!(ok(d, &["list"]).contains("@Ann"));
    assert!(ok(d, &["show", &a]).contains("assignee:  Ann"));

    ok(d, &["assign", &b, "Bob Stone"]);
    let json: serde_json::Value = serde_json::from_str(&ok(d, &["show", &b, "--json"])).unwrap();
    assert_eq!(json["assignee"], "Bob Stone");
    assert!(ok(d, &["assign", &b, "Bob Stone"]).contains("already"));
    ok(d, &["unassign", &a]);
    assert!(ok(d, &["show", &a]).contains("assignee:  -"));
    assert!(!st(d, &["assign", &b, " "]).status.success());
}

#[test]
fn search_prints_ranked_hits_with_snippets_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    create_id(d, &["Quarterly report", "--body", "Collect the numbers\n"]);
    let b = create_id(d, &["Send mail", "--body", "Mail the quarterly numbers\n"]);
    ok(d, &["set-status", &b, "done"]);

    let text = ok(d, &["search", "quarterly"]);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with("ST-1") && lines[0].contains("Quarterly report"),
        "{text}"
    );
    assert!(lines[1].starts_with("ST-2"), "{text}");
    assert!(lines[2].trim() == "Mail the quarterly numbers", "{text}");

    let json: serde_json::Value =
        serde_json::from_str(&ok(d, &["search", "\"quarterly numbers\"", "--json"])).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
    assert_eq!(json[0]["id"], "ST-2");
    assert_eq!(json[0]["match_field"], "body");

    assert!(ok(d, &["search", "quarterly", "--status", "todo"]).contains("ST-1"));
    assert!(!ok(d, &["search", "quarterly", "--status", "todo"]).contains("ST-2"));
    assert!(ok(d, &["search", "quarterly", "--limit", "1"]).contains("ST-1"));
    assert!(ok(d, &["search", "zzz"]).is_empty());
    assert!(!st(d, &["search", "--", "!!"]).status.success());
}

#[test]
fn rules_run_after_set_status_and_are_audited() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let p = create_id(d, &["Parent"]);
    let c = create_id(d, &["Child", "--parent", &p]);
    let out = ok(d, &["set-status", &c, "in-progress"]);
    assert!(out.contains("rule parent-in-progress"), "{out}");
    assert!(ok(d, &["show", &p]).contains("status:    in-progress"));
    let audit = std::fs::read_to_string(d.join(".simpletickets/audit.jsonl")).unwrap();
    assert!(audit.contains(&format!("\"id\":\"{p}\"")));
}

#[test]
fn rescan_dry_run_reports_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("p.md"), "---\nid: P\ntitle: P\nstatus: todo\n---\n").unwrap();
    std::fs::write(
        d.join("c.md"),
        "---\nid: C\ntitle: C\nstatus: in-progress\nparent: P\n---\n",
    )
    .unwrap();
    let out = ok(d, &["rescan", "--dry-run"]);
    assert!(out.contains("would set P todo -> in-progress"), "{out}");
    assert!(std::fs::read_to_string(d.join("p.md"))
        .unwrap()
        .contains("status: todo"));
    let out = ok(d, &["rescan"]);
    assert!(out.contains("set P todo -> in-progress"), "{out}");
    assert!(std::fs::read_to_string(d.join("p.md"))
        .unwrap()
        .contains("status: in-progress"));
    assert!(ok(d, &["rescan"]).starts_with("2 tickets\n"));
}

#[test]
fn errors_are_reported_with_failure_status() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    create_id(d, &["One"]);
    for args in [
        vec!["set-status", "ST-1", "bogus"],
        vec!["set-status", "ST-9", "done"],
        vec!["show", "ST-9"],
        vec!["create", "x", "--parent", "ST-9"],
        vec!["create", "x", "--due", "soon"],
        vec!["list", "--category", "nope"],
        vec!["list", "--field", "novalue"],
    ] {
        let out = st(d, &args);
        assert!(!out.status.success(), "{args:?} should fail");
        assert!(String::from_utf8_lossy(&out.stderr).starts_with("error: "));
    }
}

#[test]
fn doctor_reports_problems_and_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    create_id(d, &["One"]);
    assert!(ok(d, &["doctor"]).contains("no problems"));
    std::fs::write(
        d.join("bad.md"),
        "---\nid: X\ntitle: [oops\nstatus: todo\n---\n",
    )
    .unwrap();
    let out = st(d, &["doctor"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("bad.md"));
}

#[test]
fn templates_are_listed_and_used() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    assert_eq!(ok(d, &["templates"]), "default\nspec\n");
    std::fs::create_dir_all(d.join(".simpletickets/templates")).unwrap();
    std::fs::write(
        d.join(".simpletickets/templates/bug.md"),
        "Bug {{id}}: {{title}}\n",
    )
    .unwrap();
    assert!(ok(d, &["templates"]).contains("bug"));
    let id = create_id(d, &["Crash", "--template", "bug"]);
    assert!(ok(d, &["show", &id]).contains("Bug ST-1: Crash"));
    create_id(d, &["Spec thing", "--template", "spec"]);
}

#[test]
fn example_folder_is_consistent() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/tickets");
    let copy = tempfile::tempdir().unwrap();
    copy_dir(&example, copy.path());
    assert!(ok(copy.path(), &["doctor"]).contains("no problems"));
    let list = ok(copy.path(), &["list", "--parent", "ST-1"]);
    assert_eq!(list.lines().count(), 3, "{list}");
    assert!(ok(copy.path(), &["list", "--blocked", "true"]).contains("Send report"));
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn migrate_plan_apply_status_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("specs")).unwrap();
    std::fs::write(d.join("simpletickets.yml"), "parent_folder: specs\n").unwrap();
    std::fs::write(
        d.join("specs/P-1.md"),
        "---\nid: P-1\ntitle: P\nstatus: todo\n---\n",
    )
    .unwrap();
    let new = d.join("new.yml");
    std::fs::write(&new, "parent_folder: plans\n").unwrap();
    let new = new.to_str().unwrap();

    assert!(ok(d, &["migrate", "plan", new]).contains("move P-1"));
    assert!(d.join("specs/P-1.md").exists());
    assert!(ok(d, &["migrate", "status"]).contains("no migration recorded"));
    assert!(ok(d, &["migrate", "resume"]).contains("no unfinished"));

    let out = ok(d, &["migrate", "apply", new]);
    assert!(
        out.contains("migration Complete") && out.contains("integrity: ok"),
        "{out}"
    );
    assert!(d.join("plans/P-1.md").exists());
    assert!(ok(d, &["show", "P-1"]).contains("plans"));
    assert!(ok(d, &["migrate", "plan", new]).contains("no migration needed"));
    assert!(ok(d, &["migrate", "status", "--json"]).contains("\"complete\""));
}

#[test]
fn forget_releases_tickets_blocked_by_a_deleted_ticket() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let a = create_id(d, &["Blocker"]);
    let b = create_id(d, &["Waiting", "--blocked-by", &a]);
    ok(d, &["set-status", &a, "done"]);
    std::fs::remove_file(d.join(format!("{a}-blocker.md"))).unwrap();

    assert!(ok(d, &["list", "--blocked", "true"]).contains("Waiting"));
    let out = st(d, &["doctor"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("tombstone"));

    assert!(!st(d, &["forget", &b]).status.success());
    assert!(!st(d, &["forget", "ST-99"]).status.success());
    assert!(ok(d, &["forget", &a]).contains("forgot"));
    assert!(!ok(d, &["list", "--blocked", "true"]).contains("Waiting"));
    let out = String::from_utf8(st(d, &["doctor"]).stdout).unwrap();
    assert!(out.contains("points to missing ticket") && !out.contains("tombstone"));
}

#[test]
fn cli_commands_work_while_a_daemon_runs_but_a_second_daemon_is_refused() {
    use simpletickets::daemon::{self, Options};
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut options = Options::new(d.to_path_buf());
    options.port = 0;
    options.watch = false;
    let running = daemon::start(options).unwrap();

    let id = create_id(d, &["While the daemon runs"]);
    ok(d, &["set-status", &id, "in-progress"]);
    assert!(ok(d, &["show", &id]).contains("in-progress"));
    ok(d, &["rescan"]);

    let out = st(d, &["daemon", "--port", "0", "--no-update-check"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("another daemon") && err.contains("already runs"),
        "{err}"
    );
    running.stop();
}

#[test]
fn doctor_lists_persisted_warnings_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let a = create_id(d, &["Blocker"]);
    let b = create_id(d, &["Waiting", "--blocked-by", &a]);
    ok(d, &["set-status", &b, "in-progress"]);
    let out = ok(d, &["doctor"]);
    assert!(
        out.contains("no problems found") && out.contains("warning [blocked-in-progress]"),
        "{out}"
    );
    let json: serde_json::Value = serde_json::from_str(&ok(d, &["doctor", "--json"])).unwrap();
    assert_eq!(json["clean"], true);
    assert_eq!(json["warnings"][0]["id"], b.as_str());

    ok(d, &["set-status", &a, "done"]);
    assert!(!ok(d, &["doctor"]).contains("warning"));
}

#[test]
fn reassign_id_resolves_a_duplicate_found_by_doctor() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let a = create_id(d, &["Original"]);
    let original = d.join(format!("{a}-original.md"));
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(300);
    std::fs::File::options()
        .write(true)
        .open(&original)
        .unwrap()
        .set_modified(when)
        .unwrap();
    let copy = d.join("copied.md");
    std::fs::copy(&original, &copy).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&copy)
        .unwrap()
        .set_modified(std::time::SystemTime::now())
        .unwrap();

    let out = st(d, &["doctor"]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("duplicate id ST-1") && text.contains("copied.md"),
        "{text}"
    );

    assert!(!st(
        d,
        &["reassign-id", "ST-1", "--path", original.to_str().unwrap()]
    )
    .status
    .success());
    let out = ok(
        d,
        &["reassign-id", "ST-1", "--path", copy.to_str().unwrap()],
    );
    assert!(out.starts_with("ST-1 -> ST-2"), "{out}");
    assert!(ok(d, &["doctor"]).contains("no problems"));
    assert!(ok(d, &["show", "ST-2"]).contains("copied.md"));
    assert!(ok(d, &["show", "ST-1"]).contains(&format!("{a}-original.md")));
    assert!(!st(d, &["reassign-id", "ST-1"]).status.success());
}
