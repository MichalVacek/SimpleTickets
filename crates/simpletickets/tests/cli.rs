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
