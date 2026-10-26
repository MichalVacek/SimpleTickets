use serde_json::{json, Value};
use simpletickets::daemon::{self, Options};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

struct Plugin {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Plugin {
    fn spawn(url: &str, token_file: &std::path::Path) -> Plugin {
        let mut child = Command::new(env!("CARGO_BIN_EXE_simpletickets-mcp"))
            .args(["--url", url, "--token-file"])
            .arg(token_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Plugin {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id);
        reply
    }

    /// Calls a tool; returns the error flag and the parsed text content.
    fn tool(&mut self, name: &str, arguments: Value) -> (bool, Value) {
        let reply = self.rpc("tools/call", json!({"name": name, "arguments": arguments}));
        let result = &reply["result"];
        let text = result["content"][0]["text"].as_str().unwrap();
        let value = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
        (result["isError"].as_bool().unwrap(), value)
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.wait();
    }
}

fn write(dir: &std::path::Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

fn ticket(id: &str, extra: &str) -> String {
    format!("---\nid: {id}\ntitle: Ticket {id}\n{extra}---\nbody of {id}\n")
}

#[test]
fn drives_the_daemon_end_to_end_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a.md",
        &ticket(
            "ST-1",
            "status: todo\npriority: low\ndue: 2026-12-01\ntags: [work]\n",
        ),
    );
    write(
        dir.path(),
        "b.md",
        &ticket("ST-2", "status: todo\npriority: high\ndue: 2026-12-01\n"),
    );
    write(
        dir.path(),
        "c.md",
        &ticket(
            "ST-3",
            "status: todo\npriority: critical\ndue: 2026-11-01\nblocked_by: [ST-1]\n",
        ),
    );
    write(
        dir.path(),
        "d.md",
        &ticket("ST-4", "status: done\ndue: 2026-10-01\n"),
    );

    let mut options = Options::new(dir.path().to_path_buf());
    options.port = 0;
    options.watch = false;
    options.scan_interval = Duration::from_secs(3600);
    let running = daemon::start(options).unwrap();
    let url = format!("http://{}", running.addr);

    {
        let mut plugin = Plugin::spawn(&url, running.token_file());

        let init = plugin.rpc(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "simpletickets-mcp");
        plugin.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

        let tools = plugin.rpc("tools/list", json!({}));
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "list_tickets",
                "search_tickets",
                "show_ticket",
                "create_ticket",
                "update_ticket",
                "next_ticket"
            ]
        );

        let (err, all) = plugin.tool("list_tickets", json!({}));
        assert!(!err);
        assert_eq!(all.as_array().unwrap().len(), 4);
        let (_, work) = plugin.tool("list_tickets", json!({"tag": "work"}));
        assert_eq!(work.as_array().unwrap().len(), 1);
        let (_, todo) = plugin.tool(
            "list_tickets",
            json!({"category": "todo", "blocked": false}),
        );
        assert_eq!(todo.as_array().unwrap().len(), 2);

        let (err, shown) = plugin.tool("show_ticket", json!({"id": "ST-3"}));
        assert!(!err);
        assert_eq!(shown["blocked"], true);
        assert_eq!(shown["body"], "body of ST-3\n");
        let (err, missing) = plugin.tool("show_ticket", json!({"id": "ST-99"}));
        assert!(err);
        assert!(missing.as_str().unwrap().contains("404"));

        // ST-3 is due first but blocked; ST-4 is done; ST-2 beats ST-1 on priority.
        let (_, next) = plugin.tool("next_ticket", json!({}));
        assert_eq!(next["id"], "ST-2");
        let (_, next) = plugin.tool("next_ticket", json!({"tag": "work"}));
        assert_eq!(next["id"], "ST-1");

        let (err, created) = plugin.tool(
            "create_ticket",
            json!({"title": "Create reporting", "due": "2026-10-30", "priority": "medium"}),
        );
        assert!(!err);
        let new_id = created["id"].as_str().unwrap().to_string();
        let (_, next) = plugin.tool("next_ticket", json!({}));
        assert_eq!(next["id"], new_id.as_str());

        let (err, updated) = plugin.tool("update_ticket", json!({"id": new_id, "status": "done"}));
        assert!(!err);
        assert_eq!(updated["category"], "done");
        let (_, next) = plugin.tool("next_ticket", json!({}));
        assert_eq!(next["id"], "ST-2");

        // Finishing ST-1 unblocks ST-3 through the rules, and its earlier due date wins.
        plugin.tool("update_ticket", json!({"id": "ST-1", "status": "done"}));
        let (_, next) = plugin.tool("next_ticket", json!({}));
        assert_eq!(next["id"], "ST-3");

        let (err, owned) = plugin.tool(
            "create_ticket",
            json!({"title": "Owned work", "assignee": "Ann"}),
        );
        assert!(!err);
        assert_eq!(owned["assignee"], "Ann");
        let owned_id = owned["id"].as_str().unwrap().to_string();
        let (_, mine) = plugin.tool("list_tickets", json!({"assignee": "ann"}));
        assert_eq!(mine.as_array().unwrap().len(), 1);
        let (_, next) = plugin.tool("next_ticket", json!({"assignee": "Ann"}));
        assert_eq!(next["id"], owned_id.as_str());
        let (_, cleared) = plugin.tool("update_ticket", json!({"id": owned_id, "assignee": null}));
        assert!(cleared["assignee"].is_null());

        let (err, found) = plugin.tool("search_tickets", json!({"query": "\"body of ST-2\""}));
        assert!(!err);
        assert_eq!(found.as_array().unwrap().len(), 1);
        assert_eq!(found[0]["id"], "ST-2");
        assert!(found[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("body of ST-2"));
        let (err, _) = plugin.tool("search_tickets", json!({"query": "!!"}));
        assert!(err);

        let (err, bad) = plugin.tool("update_ticket", json!({"id": "ST-2", "status": "nonsense"}));
        assert!(err);
        assert!(bad.as_str().unwrap().contains("400"));
    }
    running.stop();
}

#[test]
fn wrong_token_surfaces_as_a_tool_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = Options::new(dir.path().to_path_buf());
    options.port = 0;
    options.watch = false;
    let running = daemon::start(options).unwrap();
    let url = format!("http://{}", running.addr);
    let bad_token = dir.path().join("bad-token");
    std::fs::write(&bad_token, "wrong\n").unwrap();
    {
        let mut plugin = Plugin::spawn(&url, &bad_token);
        let (err, text) = plugin.tool("list_tickets", json!({}));
        assert!(err);
        assert!(text.as_str().unwrap().contains("401"));
    }
    running.stop();
}
