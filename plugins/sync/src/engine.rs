//! The sync passes: pull remote changes back, push local changes out, close what was
//! deleted, per target, each ticket independent of the others.

use crate::api::Api;
use crate::config::{Config, ConflictPolicy, DeletePolicy, Direction, Target};
use crate::mapping::{self, Fields};
use crate::state::{now_iso, now_unix, Conflict, Lock, Phase, TargetState};
use serde_json::{json, Map, Value};
use simpletickets_connector_protocol::{
    ErrorCode, ErrorInfo, Process, Spawn, Transport, PROTOCOL_VERSION,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Upper bound of the wait before a transient failure is tried again.
const MAX_BACKOFF_SECONDS: u64 = 900;
/// Fetch pages followed in one pass.
const MAX_FETCH_PAGES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    Local,
    Remote,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Plan only: no remote call that changes anything, no ticket write, no state write.
    pub dry_run: bool,
    /// Push even when nothing changed, and ignore retry waits and permanent refusals.
    pub force: bool,
    /// Restrict the pass to these tickets; `None` means all.
    pub only_ids: Option<BTreeSet<String>>,
    pub only_target: Option<String>,
    /// Ask the remotes for changes first (targets whose direction is `both`).
    pub pull: bool,
    /// With `pull`: look at every known remote item, and push only the tickets the pull
    /// touched.
    pub pull_only: bool,
    /// Settle the open conflicts of `only_ids` in favour of one side.
    pub prefer: Option<Prefer>,
}

impl Options {
    /// Whether the pass covers this ticket.
    pub fn includes(&self, id: &str) -> bool {
        match &self.only_ids {
            None => true,
            Some(ids) => ids.contains(id),
        }
    }
}

/// One thing a pass did, or with `dry_run` would do.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub target: String,
    pub id: String,
    /// `create`, `update`, `close`, `pull`, `conflict`, `skip`, `warn` or `error`.
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub actions: Vec<Action>,
    /// Problems with a whole target (connector unavailable, refused credentials).
    pub target_errors: Vec<(String, String)>,
}

impl Report {
    fn add(&mut self, target: &str, id: &str, kind: &str, detail: impl Into<String>) {
        self.actions.push(Action {
            target: target.into(),
            id: id.into(),
            kind: kind.into(),
            detail: detail.into(),
        });
    }

    pub fn count(&self, kind: &str) -> usize {
        self.actions.iter().filter(|a| a.kind == kind).count()
    }

    pub fn has_errors(&self) -> bool {
        !self.target_errors.is_empty() || self.count("error") > 0
    }
}

/// Seconds to wait after the `attempts`-th consecutive transient failure.
pub fn backoff_seconds(attempts: u32, retry_after_ms: Option<u64>) -> u64 {
    let exponential = 5u64
        .saturating_mul(1 << attempts.saturating_sub(1).min(10))
        .min(MAX_BACKOFF_SECONDS);
    exponential.max(retry_after_ms.map_or(0, |ms| ms.div_ceil(1000)))
}

fn short_hash(text: &str) -> String {
    mapping::payload_hash(&Value::String(text.into()), &Fields::new(), &Value::Null)[..16]
        .to_string()
}

/// Ids ordered so that parents and blockers come before the tickets that refer to them.
pub fn dependency_order(ids: &BTreeSet<String>, tickets: &BTreeMap<String, &Value>) -> Vec<String> {
    fn visit(
        id: &str,
        ids: &BTreeSet<String>,
        tickets: &BTreeMap<String, &Value>,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(id.to_string()) {
            return;
        }
        if let Some(ticket) = tickets.get(id) {
            let parent = ticket["parent"].as_str().into_iter();
            let blockers = ticket["blocked_by"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            for dep in parent.chain(blockers) {
                if ids.contains(dep) {
                    visit(dep, ids, tickets, seen, out);
                }
            }
        }
        out.push(id.to_string());
    }
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for id in ids {
        visit(id, ids, tickets, &mut seen, &mut out);
    }
    out
}

pub struct Engine<'a> {
    api: &'a Api,
    config: &'a Config,
    tickets_dir: PathBuf,
    lock: Option<&'a Lock>,
    connectors: BTreeMap<String, Box<dyn Transport>>,
}

impl<'a> Engine<'a> {
    pub fn new(
        api: &'a Api,
        config: &'a Config,
        tickets_dir: &Path,
        lock: Option<&'a Lock>,
    ) -> Engine<'a> {
        Engine {
            api,
            config,
            tickets_dir: tickets_dir.to_path_buf(),
            lock,
            connectors: BTreeMap::new(),
        }
    }

    /// Uses `connector` for `target` instead of starting the configured command.
    pub fn use_connector(&mut self, target: &str, connector: Box<dyn Transport>) {
        self.connectors.insert(target.to_string(), connector);
    }

    /// Runs one pass over every target (or the one named).
    pub fn run(&mut self, options: &Options) -> Result<Report, String> {
        let list = self.api.list_tickets()?;
        let mut report = Report::default();
        for (name, target) in &self.config.targets {
            if options.only_target.as_ref().is_some_and(|t| t != name) {
                continue;
            }
            let mut state = TargetState::load(&self.tickets_dir, name)?;
            let connector = self.connectors.remove(name);
            let mut pass = Pass {
                api: self.api,
                name,
                target,
                tickets_dir: &self.tickets_dir,
                lock: self.lock,
                state: &mut state,
                connector,
                caps: Vec::new(),
                options,
                list: &list,
                report: &mut report,
            };
            pass.run();
            let connector = pass.connector.take();
            if !options.dry_run {
                state.last_pass = Some(now_iso());
                state.save(&self.tickets_dir, name)?;
            }
            if let Some(connector) = connector {
                self.connectors.insert(name.clone(), connector);
            }
        }
        Ok(report)
    }
}

struct Pass<'a> {
    api: &'a Api,
    name: &'a str,
    target: &'a Target,
    tickets_dir: &'a Path,
    lock: Option<&'a Lock>,
    state: &'a mut TargetState,
    connector: Option<Box<dyn Transport>>,
    caps: Vec<String>,
    options: &'a Options,
    list: &'a [Value],
    report: &'a mut Report,
}

fn id_of(ticket: &Value) -> &str {
    ticket["id"].as_str().unwrap_or_default()
}

impl Pass<'_> {
    fn run(&mut self) {
        let pulling = self.options.pull && self.target.direction == Direction::Both;
        if !self.options.dry_run || pulling {
            if let Err(message) = self.connect() {
                self.fail_target(message);
                return;
            }
            if pulling && !self.caps.iter().any(|c| c == "fetch") {
                self.fail_target(
                    "the connector cannot fetch changes, so `direction: both` is not possible"
                        .into(),
                );
                return;
            }
        }
        if !self.options.dry_run {
            self.state.last_error = None;
        }

        let candidates: BTreeMap<String, &Value> = self
            .list
            .iter()
            .filter(|t| t["frozen"] != true && mapping::passes_filter(self.target, t))
            .map(|t| (id_of(t).to_string(), t))
            .collect();
        let in_scope = |id: &str| self.options.includes(id);

        let mut work: BTreeSet<String> = if self.options.pull_only {
            BTreeSet::new()
        } else {
            candidates
                .keys()
                .filter(|id| in_scope(id))
                .cloned()
                .collect()
        };
        if self.options.prefer.is_some() {
            self.settle_conflicts(&mut work);
        }
        if pulling {
            self.pull(&candidates, &mut work);
        }
        self.push(&candidates, &work);
        if !self.options.pull_only {
            self.close_deleted(&candidates);
        }
    }

    fn fail_target(&mut self, message: String) {
        self.report
            .target_errors
            .push((self.name.into(), message.clone()));
        if !self.options.dry_run {
            self.state.last_error = Some(message);
        }
    }

    fn heartbeat(&self) {
        if let Some(lock) = self.lock {
            lock.touch();
        }
    }

    fn save(&mut self) {
        if !self.options.dry_run {
            if let Err(e) = self.state.save(self.tickets_dir, self.name) {
                eprintln!("sync: {e}");
            }
        }
    }

    /// Starts the connector if it is not running, and checks the protocol and its options.
    fn connect(&mut self) -> Result<(), String> {
        if let Some(c) = self.connector.as_mut() {
            if !c.is_alive() {
                self.connector = None;
            }
        }
        if self.connector.is_none() {
            let spec = &self.target.connector;
            let process = Process::spawn(&Spawn {
                command: spec.command.clone(),
                args: spec.args.clone(),
                pass_env: spec.env.clone(),
                timeout: Duration::from_secs(spec.timeout_seconds),
            })?;
            self.connector = Some(Box::new(process));
        }
        let connector = self.connector.as_mut().expect("connector was just started");
        let describe = connector
            .call("describe", json!({}))
            .map_err(|e| format!("describe failed: {e}"))?;
        let speaks = describe["protocol"]
            .as_array()
            .is_some_and(|v| v.iter().any(|p| p.as_u64() == Some(PROTOCOL_VERSION)));
        if !speaks {
            self.connector = None;
            return Err(format!(
                "the connector does not speak protocol version {PROTOCOL_VERSION} (it offers {})",
                describe["protocol"]
            ));
        }
        let configured = connector
            .call(
                "configure",
                json!({
                    "target": self.name,
                    "options": self.target.options,
                    "tickets_dir": self.tickets_dir.display().to_string(),
                }),
            )
            .map_err(|e| format!("configure failed: {e}"))?;
        self.caps = configured["capabilities"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        Ok(())
    }

    fn call(&mut self, op: &str, params: Value) -> Result<Value, ErrorInfo> {
        match self.connector.as_mut() {
            Some(c) => c.call(op, params),
            None => Err(ErrorInfo::new(ErrorCode::Unavailable, "no connector")),
        }
    }

    fn policy(&self) -> ConflictPolicy {
        match self.options.prefer {
            Some(Prefer::Local) => ConflictPolicy::LocalWins,
            Some(Prefer::Remote) => ConflictPolicy::RemoteWins,
            None => self.target.on_conflict,
        }
    }

    /// Records a transient or permanent failure on a ticket's entry.
    fn record_failure(&mut self, id: &str, error: &ErrorInfo, hash: Option<&str>) {
        let entry = self.state.tickets.entry(id.to_string()).or_default();
        entry.error = Some(error.to_string());
        if entry.phase == Phase::Synced || entry.remote.is_none() {
            entry.phase = Phase::Error;
        }
        if error.retryable {
            entry.attempts += 1;
            entry.next_attempt =
                Some(now_unix() + backoff_seconds(entry.attempts, error.retry_after_ms));
        } else if let Some(hash) = hash {
            entry.failed_hash = Some(hash.to_string());
        }
        self.report.add(self.name, id, "error", error.to_string());
    }

    fn open_conflict(
        &mut self,
        id: &str,
        kind: &str,
        field: Option<&str>,
        local: &Value,
        remote: &Value,
        resolution: Option<&str>,
    ) {
        let existing = self.state.conflicts.iter_mut().find(|c| {
            c.id == id && c.kind == kind && c.field.as_deref() == field && c.resolution.is_none()
        });
        match existing {
            Some(c) => {
                c.local = local.clone();
                c.remote = remote.clone();
                c.resolution = resolution.map(String::from);
            }
            None => {
                let repeat = resolution.is_some()
                    && self.state.conflicts.last().is_some_and(|c| {
                        c.id == id
                            && c.kind == kind
                            && c.field.as_deref() == field
                            && c.remote == *remote
                            && c.resolution.as_deref() == resolution
                    });
                if !repeat {
                    self.state.conflicts.push(Conflict {
                        id: id.into(),
                        kind: kind.into(),
                        field: field.map(String::from),
                        local: local.clone(),
                        remote: remote.clone(),
                        detected_at: now_iso(),
                        resolution: resolution.map(String::from),
                    });
                }
            }
        }
        let detail = match (field, resolution) {
            (Some(f), Some(r)) => format!("{f}: local {local} vs remote {remote}, resolved by {r}"),
            (Some(f), None) => format!("{f}: local {local} vs remote {remote}, needs a decision"),
            (None, _) => "the remote item no longer exists".to_string(),
        };
        self.report.add(self.name, id, "conflict", detail);
    }

    fn resolve_conflicts(&mut self, id: &str, field: Option<&str>, resolution: &str) {
        for c in &mut self.state.conflicts {
            if c.id == id
                && c.resolution.is_none()
                && (field.is_none() || c.field.as_deref() == field)
            {
                c.resolution = Some(resolution.into());
            }
        }
    }

    /// Applies `--prefer` to the open conflicts of the selected tickets.
    fn settle_conflicts(&mut self, work: &mut BTreeSet<String>) {
        let Some(prefer) = self.options.prefer else {
            return;
        };
        let ids: Vec<String> = self
            .options
            .only_ids
            .iter()
            .flatten()
            .filter(|id| self.state.open_conflicts().any(|c| &c.id == *id))
            .cloned()
            .collect();
        for id in ids {
            let open: Vec<Conflict> = self
                .state
                .open_conflicts()
                .filter(|c| c.id == id)
                .cloned()
                .collect();
            let missing = open.iter().any(|c| c.kind == "remote-missing");
            match prefer {
                Prefer::Local => {
                    if let Some(entry) = self.state.tickets.get_mut(&id) {
                        entry.hash.clear();
                        entry.failed_hash = None;
                        entry.phase = Phase::Synced;
                        if missing {
                            entry.remote = None;
                        }
                    }
                    self.resolve_conflicts(&id, None, "local-wins");
                    work.insert(id.clone());
                }
                Prefer::Remote => {
                    let mut patch = Map::new();
                    let mut remotes = Vec::new();
                    for c in open.iter().filter(|c| c.kind == "field") {
                        let field = c.field.clone().unwrap_or_default();
                        match mapping::from_remote(self.target, &field, &c.remote) {
                            Ok(v) => {
                                patch.insert(field.clone(), v);
                                remotes.push((field, c.remote.clone()));
                            }
                            Err(e) => {
                                self.report
                                    .add(self.name, &id, "warn", format!("{field}: {e}"));
                            }
                        }
                    }
                    if self.apply_patch(&id, patch, &remotes, "resolve") {
                        if let Some(entry) = self.state.tickets.get_mut(&id) {
                            entry.phase = Phase::Synced;
                        }
                        self.resolve_conflicts(&id, None, "remote-wins");
                    }
                }
            }
        }
    }

    /// Writes remote values into a local ticket through the API; the body is never part of
    /// it. On success the remote values become the base. Returns whether it was written.
    fn apply_patch(
        &mut self,
        id: &str,
        patch: Map<String, Value>,
        remotes: &[(String, Value)],
        version: &str,
    ) -> bool {
        if patch.is_empty() {
            return true;
        }
        let summary = describe_fields(&patch);
        if self.options.dry_run {
            self.report
                .add(self.name, id, "pull", format!("would set {summary}"));
            return false;
        }
        let key = format!(
            "sync:{}:{id}:{}",
            self.name,
            short_hash(&format!(
                "{version}{}",
                mapping::canonical(&Value::Object(patch.clone()))
            ))
        );
        match self.api.patch_ticket(id, &Value::Object(patch), &key) {
            Ok(_) => {
                let entry = self.state.tickets.entry(id.to_string()).or_default();
                for (field, remote) in remotes {
                    entry.base.insert(field.clone(), remote.clone());
                }
                self.report
                    .add(self.name, id, "pull", format!("set {summary}"));
                true
            }
            Err(e) => {
                self.report.add(self.name, id, "error", e);
                false
            }
        }
    }

    fn pull(&mut self, candidates: &BTreeMap<String, &Value>, work: &mut BTreeSet<String>) {
        let by_ref: BTreeMap<String, String> = self
            .state
            .tickets
            .iter()
            .filter(|(id, e)| {
                matches!(e.phase, Phase::Synced | Phase::Error | Phase::Conflict)
                    && (self.options.pull_only || self.options.includes(id))
            })
            .filter_map(|(id, e)| e.remote.clone().map(|r| (r, id.clone())))
            .collect();
        if by_ref.is_empty() {
            return;
        }
        let refs: Vec<&String> = by_ref.keys().collect();
        let mut cursor = self.state.cursor.clone();
        for _ in 0..MAX_FETCH_PAGES {
            let page = match self.call("fetch", json!({"cursor": cursor, "refs": refs})) {
                Ok(p) => p,
                Err(e) => {
                    self.report
                        .target_errors
                        .push((self.name.into(), format!("fetch failed: {e}")));
                    return;
                }
            };
            for item in page["items"].as_array().into_iter().flatten() {
                let Some(id) = item["ref"].as_str().and_then(|r| by_ref.get(r)) else {
                    continue;
                };
                if candidates.contains_key(id) {
                    self.pull_item(id, item, work);
                }
            }
            cursor = page["cursor"].as_str().map(String::from).or(cursor);
            if page["more"] != true {
                break;
            }
        }
        if !self.options.dry_run {
            self.state.cursor = cursor;
            self.save();
        }
    }

    fn pull_item(&mut self, id: &str, item: &Value, work: &mut BTreeSet<String>) {
        let remote_fields = item["fields"].as_object().cloned().unwrap_or_default();
        let version = item["version"].as_str().unwrap_or_default().to_string();
        let mut changed: Vec<(String, Value)> = Vec::new();
        for field in &self.target.pull.fields {
            let Some(remote) = remote_fields.get(field) else {
                continue;
            };
            let entry = self.state.tickets.entry(id.to_string()).or_default();
            match entry.base.get(field) {
                None => {
                    if !self.options.dry_run {
                        entry.base.insert(field.clone(), remote.clone());
                    }
                }
                Some(base) if base == remote => {}
                Some(_) => changed.push((field.clone(), remote.clone())),
            }
        }
        if changed.is_empty() {
            return;
        }
        let local = match self.api.get_ticket(id) {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(e) => {
                self.report.add(self.name, id, "error", e);
                return;
            }
        };
        let local_fields = mapping::map_fields(self.target, &local);
        let policy = self.policy();
        let mut patch = Map::new();
        let mut remotes: Vec<(String, Value)> = Vec::new();
        for (field, remote) in changed {
            let base = self.state.tickets[id].base.get(&field).cloned();
            let local_value = local_fields.get(&field).cloned();
            let local_changed = local_value
                .as_ref()
                .is_some_and(|l| Some(l) != base.as_ref());
            let Some(local_value) = local_value.filter(|_| local_changed) else {
                self.collect_patch(id, &field, &remote, &mut patch, &mut remotes);
                continue;
            };
            if local_value == remote {
                if !self.options.dry_run {
                    if let Some(entry) = self.state.tickets.get_mut(id) {
                        entry.base.insert(field.clone(), remote);
                    }
                    self.resolve_conflicts(id, Some(&field), "converged");
                }
                continue;
            }
            let resolution = match policy {
                ConflictPolicy::LocalWins => Some("local-wins"),
                ConflictPolicy::RemoteWins => Some("remote-wins"),
                ConflictPolicy::Manual => None,
            };
            if self.options.dry_run {
                self.report.add(
                    self.name,
                    id,
                    "conflict",
                    format!(
                        "{field}: local {local_value} vs remote {remote}, would be resolved by {}",
                        policy.as_str()
                    ),
                );
            } else {
                self.open_conflict(id, "field", Some(&field), &local_value, &remote, resolution);
            }
            match policy {
                ConflictPolicy::LocalWins => {
                    work.insert(id.to_string());
                }
                ConflictPolicy::RemoteWins => {
                    self.collect_patch(id, &field, &remote, &mut patch, &mut remotes);
                }
                ConflictPolicy::Manual => {
                    if let Some(entry) = self.state.tickets.get_mut(id) {
                        if !self.options.dry_run {
                            entry.phase = Phase::Conflict;
                        }
                    }
                }
            }
        }
        if self.apply_patch(id, patch, &remotes, &version) {
            work.insert(id.to_string());
        }
        self.save();
    }

    fn collect_patch(
        &mut self,
        id: &str,
        field: &str,
        remote: &Value,
        patch: &mut Map<String, Value>,
        remotes: &mut Vec<(String, Value)>,
    ) {
        match mapping::from_remote(self.target, field, remote) {
            Ok(value) => {
                patch.insert(field.to_string(), value);
                remotes.push((field.to_string(), remote.clone()));
            }
            Err(e) => {
                if !self.options.dry_run {
                    if let Some(entry) = self.state.tickets.get_mut(id) {
                        let warning = format!("{field}: {e}");
                        if !entry.warnings.contains(&warning) {
                            entry.warnings.push(warning);
                        }
                    }
                }
                self.report
                    .add(self.name, id, "warn", format!("{field}: {e}"));
            }
        }
    }

    fn push(&mut self, candidates: &BTreeMap<String, &Value>, work: &BTreeSet<String>) {
        let order = dependency_order(work, candidates);
        for id in order {
            self.heartbeat();
            if !candidates.contains_key(&id) {
                continue;
            }
            if self.push_one(&id, candidates) == Flow::Stop {
                break;
            }
        }
    }

    fn push_one(&mut self, id: &str, candidates: &BTreeMap<String, &Value>) -> Flow {
        let force = self.options.force;
        let entry = self.state.tickets.get(id).cloned();
        if let Some(e) = &entry {
            match e.phase {
                Phase::Conflict => {
                    self.report.add(
                        self.name,
                        id,
                        "skip",
                        "waiting for a decision on a conflict",
                    );
                    return Flow::Go;
                }
                Phase::RemoteMissing => {
                    self.report.add(
                        self.name,
                        id,
                        "skip",
                        "the remote item is gone; settle it with `push-now --prefer`",
                    );
                    return Flow::Go;
                }
                _ => {}
            }
            if !force && e.next_attempt.is_some_and(|t| t > now_unix()) {
                return Flow::Go;
            }
        }
        let ticket = match self.api.get_ticket(id) {
            Ok(Some(t)) => t,
            Ok(None) => return Flow::Go,
            Err(e) => {
                self.report.add(self.name, id, "error", e);
                return Flow::Go;
            }
        };
        let fields = mapping::map_fields(self.target, &ticket);
        let snapshot = mapping::snapshot(&ticket);
        let ref_of = |other: &str| {
            self.state
                .tickets
                .get(other)
                .filter(|e| !matches!(e.phase, Phase::Closed | Phase::Deleted))
                .and_then(|e| e.remote.clone())
        };
        let parent_ref = match ticket["parent"].as_str() {
            Some(parent) => match ref_of(parent) {
                Some(r) => Some(r),
                None if candidates.contains_key(parent) => {
                    self.report.add(
                        self.name,
                        id,
                        "skip",
                        format!("waiting for its parent {parent} to be synced"),
                    );
                    return Flow::Go;
                }
                None => None,
            },
            None => None,
        };
        let mut blocked_refs: Vec<String> = ticket["blocked_by"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(ref_of)
            .collect();
        blocked_refs.sort();
        let links = json!({"parent_ref": parent_ref, "blocked_by_refs": blocked_refs});
        let hash = mapping::payload_hash(&snapshot, &fields, &links);

        let unchanged = entry
            .as_ref()
            .is_some_and(|e| e.phase == Phase::Synced && e.remote.is_some() && e.hash == hash);
        if unchanged && !force {
            return Flow::Go;
        }
        if !force
            && entry
                .as_ref()
                .is_some_and(|e| e.failed_hash.as_deref() == Some(hash.as_str()))
        {
            return Flow::Go;
        }
        let remote_ref = entry.as_ref().and_then(|e| e.remote.clone());
        if self.options.dry_run {
            let kind = if remote_ref.is_some() {
                "update"
            } else {
                "create"
            };
            self.report.add(
                self.name,
                id,
                kind,
                format!("push {}", describe_fields(&fields)),
            );
            return Flow::Go;
        }
        let previous = entry.as_ref().map(|e| e.links.clone()).unwrap_or_default();
        let mut remote_ref = remote_ref;
        for attempt in 0..2 {
            let params = json!({
                "ticket": snapshot,
                "fields": fields,
                "links": {
                    "parent_ref": parent_ref,
                    "blocked_by_refs": blocked_refs,
                    "previous_blocked_by_refs": previous,
                },
                "ref": remote_ref,
            });
            match self.call("push", params) {
                Ok(result) => {
                    self.record_push(
                        id,
                        &result,
                        &fields,
                        &hash,
                        &blocked_refs,
                        remote_ref.is_none(),
                    );
                    return Flow::Go;
                }
                Err(e) if e.code == ErrorCode::NotFound && remote_ref.is_some() && attempt == 0 => {
                    let local = json!(remote_ref);
                    if self.policy() == ConflictPolicy::LocalWins {
                        self.open_conflict(
                            id,
                            "remote-missing",
                            None,
                            &local,
                            &Value::Null,
                            Some("local-wins"),
                        );
                        remote_ref = None;
                        continue;
                    }
                    self.open_conflict(id, "remote-missing", None, &local, &Value::Null, None);
                    if let Some(entry) = self.state.tickets.get_mut(id) {
                        entry.phase = Phase::RemoteMissing;
                        entry.error = Some(e.to_string());
                    }
                    self.save();
                    return Flow::Go;
                }
                Err(e) => {
                    self.record_failure(id, &e, Some(&hash));
                    self.save();
                    return if e.code == ErrorCode::Auth {
                        self.fail_target(format!("credentials refused: {}", e.message));
                        Flow::Stop
                    } else {
                        Flow::Go
                    };
                }
            }
        }
        Flow::Go
    }

    fn record_push(
        &mut self,
        id: &str,
        result: &Value,
        fields: &Fields,
        hash: &str,
        blocked_refs: &[String],
        created: bool,
    ) {
        let entry = self.state.tickets.entry(id.to_string()).or_default();
        entry.remote = result["ref"]
            .as_str()
            .map(String::from)
            .or(entry.remote.take());
        entry.url = result["url"].as_str().map(String::from);
        entry.hash = hash.to_string();
        entry.links = blocked_refs.to_vec();
        let confirmed = result["fields"].as_object().unwrap_or(fields);
        for (k, v) in confirmed {
            entry.base.insert(k.clone(), v.clone());
        }
        entry.phase = Phase::Synced;
        entry.error = None;
        entry.failed_hash = None;
        entry.attempts = 0;
        entry.next_attempt = None;
        entry.warnings = result["warnings"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        entry.synced_at = Some(now_iso());
        let outcome =
            result["outcome"]
                .as_str()
                .unwrap_or(if created { "created" } else { "updated" });
        let warnings = entry.warnings.clone();
        let reference = entry.remote.clone().unwrap_or_default();
        let kind = if created { "create" } else { "update" };
        if outcome != "unchanged" || created {
            self.report
                .add(self.name, id, kind, format!("{outcome} {reference}"));
        }
        for w in warnings {
            self.report.add(self.name, id, "warn", w);
        }
        self.save();
    }

    /// Closes the remote items of tickets that no longer exist. Nothing is deleted.
    fn close_deleted(&mut self, candidates: &BTreeMap<String, &Value>) {
        let listed: BTreeSet<&str> = self.list.iter().map(id_of).collect();
        let gone: Vec<String> = self
            .state
            .tickets
            .iter()
            .filter(|(id, e)| {
                !listed.contains(id.as_str())
                    && !candidates.contains_key(*id)
                    && !matches!(
                        e.phase,
                        Phase::Closed | Phase::Deleted | Phase::RemoteMissing
                    )
                    && self.options.includes(id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            // The list is a snapshot; the ticket must really be unknown before anything
            // is closed.
            match self.api.get_ticket(&id) {
                Ok(None) => {}
                Ok(Some(_)) => continue,
                Err(e) => {
                    self.report.add(self.name, &id, "error", e);
                    continue;
                }
            }
            self.heartbeat();
            let Some(entry) = self.state.tickets.get(&id).cloned() else {
                continue;
            };
            let Some(reference) = entry.remote.clone() else {
                if !self.options.dry_run {
                    self.state.tickets.remove(&id);
                }
                continue;
            };
            if self.target.on_delete == DeletePolicy::Ignore {
                if !self.options.dry_run {
                    if let Some(e) = self.state.tickets.get_mut(&id) {
                        e.phase = Phase::Deleted;
                    }
                }
                self.report.add(
                    self.name,
                    &id,
                    "skip",
                    format!("deleted locally; {reference} is left as it is"),
                );
                continue;
            }
            if self.options.dry_run {
                self.report
                    .add(self.name, &id, "close", format!("would close {reference}"));
                continue;
            }
            if !self.caps.iter().any(|c| c == "close") {
                self.report.add(
                    self.name,
                    &id,
                    "warn",
                    "the connector cannot close items; nothing was done",
                );
                if let Some(e) = self.state.tickets.get_mut(&id) {
                    e.phase = Phase::Deleted;
                }
                continue;
            }
            if entry.next_attempt.is_some_and(|t| t > now_unix()) && !self.options.force {
                continue;
            }
            match self.call("close", json!({"ref": reference, "ticket_id": id})) {
                Ok(_) => {
                    if let Some(e) = self.state.tickets.get_mut(&id) {
                        e.phase = Phase::Closed;
                        e.error = None;
                        e.attempts = 0;
                        e.next_attempt = None;
                    }
                    self.report
                        .add(self.name, &id, "close", format!("closed {reference}"));
                }
                Err(e) if e.code == ErrorCode::NotFound => {
                    if let Some(s) = self.state.tickets.get_mut(&id) {
                        s.phase = Phase::Closed;
                    }
                    self.report.add(
                        self.name,
                        &id,
                        "skip",
                        format!("{reference} is already gone"),
                    );
                }
                Err(e) => self.record_failure(&id, &e, None),
            }
            self.save();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Go,
    Stop,
}

fn describe_fields(fields: &Map<String, Value>) -> String {
    fields
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "title" | "tags" | "due") || fields.len() == 1)
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Starts a target's connector, configures it and, when it can, asks it to verify the
/// remote and the credentials. Returns the connector's own description of what it found.
pub fn check_target(name: &str, target: &Target, tickets_dir: &Path) -> Result<String, String> {
    let spec = &target.connector;
    let mut process = Process::spawn(&Spawn {
        command: spec.command.clone(),
        args: spec.args.clone(),
        pass_env: spec.env.clone(),
        timeout: Duration::from_secs(spec.timeout_seconds),
    })?;
    let result = (|| {
        let describe = process
            .call("describe", json!({}))
            .map_err(|e| format!("describe failed: {e}"))?;
        let configured = process
            .call(
                "configure",
                json!({
                    "target": name,
                    "options": target.options,
                    "tickets_dir": tickets_dir.display().to_string(),
                }),
            )
            .map_err(|e| format!("configure failed: {e}"))?;
        let can_check = configured["capabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|v| v == "check"));
        let connector = describe["name"].as_str().unwrap_or("connector");
        if !can_check {
            return Ok(format!(
                "{connector}: configured (no connectivity check offered)"
            ));
        }
        let checked = process
            .call("check", json!({}))
            .map_err(|e| format!("check failed: {e}"))?;
        Ok(format!(
            "{connector}: {}",
            checked["message"].as_str().unwrap_or("ok")
        ))
    })();
    process.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_is_capped_and_honours_retry_after() {
        assert_eq!(backoff_seconds(1, None), 5);
        assert_eq!(backoff_seconds(2, None), 10);
        assert_eq!(backoff_seconds(4, None), 40);
        assert_eq!(backoff_seconds(30, None), MAX_BACKOFF_SECONDS);
        assert_eq!(backoff_seconds(1, Some(60_000)), 60);
        assert_eq!(backoff_seconds(1, Some(1_500)), 5);
    }

    #[test]
    fn parents_and_blockers_come_first() {
        let a = json!({"id": "A", "parent": "P", "blocked_by": ["B"]});
        let b = json!({"id": "B"});
        let p = json!({"id": "P", "blocked_by": ["B"]});
        let tickets: BTreeMap<String, &Value> = [
            ("A".to_string(), &a),
            ("B".to_string(), &b),
            ("P".to_string(), &p),
        ]
        .into();
        let ids: BTreeSet<String> = tickets.keys().cloned().collect();
        let order = dependency_order(&ids, &tickets);
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("B") < pos("P") && pos("P") < pos("A"));
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn a_dependency_cycle_does_not_loop() {
        let a = json!({"id": "A", "blocked_by": ["B"]});
        let b = json!({"id": "B", "blocked_by": ["A"]});
        let tickets: BTreeMap<String, &Value> =
            [("A".to_string(), &a), ("B".to_string(), &b)].into();
        let ids: BTreeSet<String> = tickets.keys().cloned().collect();
        assert_eq!(dependency_order(&ids, &tickets).len(), 2);
    }
}
