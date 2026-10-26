//! Command line interface.

use crate::logger::{self, Level, Logger};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use simpletickets_core::audit::AuditLog;
use simpletickets_core::config::Config;
use simpletickets_core::doctor;
use simpletickets_core::migration::{self, Journal, JournalState};
use simpletickets_core::ops::{self, NewTicket, Role, Update};
use simpletickets_core::query::{self, Filter};
use simpletickets_core::rules::{self, RunOptions, RunReport};
use simpletickets_core::search::Query;
use simpletickets_core::state::{audit_log_path, ensure_state_dir};
use simpletickets_core::store::Store;
use simpletickets_core::templates;
use simpletickets_core::view::{doctor_json, hit_json, ticket_json};
use simpletickets_core::write::WriteOptions;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "simpletickets",
    version,
    about = "Markdown tickets in a folder"
)]
pub struct Cli {
    /// Tickets folder holding simpletickets.yml (default: $SIMPLETICKETS_DIR or the current folder)
    #[arg(long, global = true)]
    pub dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a ticket
    Create(CreateArgs),
    /// List tickets, optionally filtered
    List(ListArgs),
    /// Search ticket titles and bodies (words must all match; "quoted phrases" match in order)
    Search(SearchArgs),
    /// Show one ticket with its derived data
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Change the status of a ticket
    SetStatus { id: String, status: String },
    /// Assign a ticket to someone
    Assign { id: String, assignee: String },
    /// Remove the assignee of a ticket
    Unassign { id: String },
    /// Report inconsistencies in the ticket files
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Give a conflicted copy of a duplicated ticket ID a fresh ID (only its frontmatter id changes)
    ReassignId {
        /// The duplicated ID
        id: String,
        /// The copy to change; optional when there is exactly one
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Acknowledge a deleted ticket so links to it stop counting (releases its blocked tickets)
    Forget { id: String },
    /// Scan all folders and run the rules
    Rescan {
        /// Show the changes the rules would make without writing them
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// List available ticket templates
    Templates,
    /// Run the background daemon with the local API
    Daemon(DaemonArgs),
    /// Move tickets and rewrite statuses for a new configuration
    Migrate {
        #[command(subcommand)]
        command: MigrateCommand,
    },
}

#[derive(Subcommand)]
pub enum MigrateCommand {
    /// Show what changing to the given config file would do, without changing anything
    Plan {
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Apply the migration to the given config file, then install it as the config
    Apply {
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Finish an interrupted migration
    Resume {
        #[arg(long)]
        json: bool,
    },
    /// Show the last or current migration
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub struct DaemonArgs {
    /// Port on 127.0.0.1
    #[arg(long, default_value_t = crate::daemon::DEFAULT_PORT)]
    pub port: u16,
    /// Listen on all interfaces; only for use inside a container (see the README)
    #[arg(long)]
    pub bind_all: bool,
    /// Seconds between periodic scans
    #[arg(long, default_value_t = 60)]
    pub scan_interval: u64,
    /// Token file (default: .simpletickets/token in the tickets folder)
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// Do not watch files; rely on periodic scans
    #[arg(long)]
    pub no_watch: bool,
    /// Detect file changes by polling instead of file events (for Docker bind mounts)
    #[arg(long, conflicts_with = "no_watch")]
    pub poll: bool,
    /// Seconds between polls with --poll
    #[arg(long, default_value_t = 2)]
    pub poll_interval: u64,
    /// Do not check GitHub for newer releases
    #[arg(long)]
    pub no_update_check: bool,
    /// Hours between update checks
    #[arg(long, default_value_t = 24)]
    pub update_interval_hours: u64,
    /// Log level: error, warn, info or debug (default: $SIMPLETICKETS_LOG_LEVEL or info)
    #[arg(long)]
    pub log_level: Option<String>,
    /// Append logs to this file instead of stderr (default: $SIMPLETICKETS_LOG_FILE)
    #[arg(long)]
    pub log_file: Option<PathBuf>,
}

#[derive(Args)]
pub struct CreateArgs {
    pub title: String,
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long)]
    pub priority: Option<String>,
    #[arg(long)]
    pub assignee: Option<String>,
    /// Due date, YYYY-MM-DD
    #[arg(long)]
    pub due: Option<String>,
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    #[arg(long)]
    pub parent: Option<String>,
    #[arg(long = "blocked-by")]
    pub blocked_by: Vec<String>,
    #[arg(long)]
    pub related: Vec<String>,
    #[arg(long)]
    pub template: Option<String>,
    /// Body text, replacing the template
    #[arg(long)]
    pub body: Option<String>,
    /// Create in the parent or sub-ticket folder
    #[arg(long, value_parser = ["parent", "sub"])]
    pub role: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct SearchArgs {
    /// Words and "quoted phrases"; case-insensitive, all must match
    pub query: String,
    /// Maximum number of results
    #[arg(long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub status: Option<String>,
    /// todo, in-progress, done or cancelled
    #[arg(long)]
    pub category: Option<String>,
    #[arg(long)]
    pub assignee: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long)]
    pub status: Option<String>,
    /// todo, in-progress, done or cancelled
    #[arg(long)]
    pub category: Option<String>,
    #[arg(long)]
    pub priority: Option<String>,
    #[arg(long)]
    pub assignee: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub parent: Option<String>,
    /// Substring of the title
    #[arg(long)]
    pub title: Option<String>,
    /// true or false
    #[arg(long)]
    pub blocked: Option<String>,
    /// Due on or before this date (YYYY-MM-DD or today)
    #[arg(long)]
    pub due_by: Option<String>,
    /// Exact match on a free-form key, as key=value
    #[arg(long = "field")]
    pub fields: Vec<String>,
    #[arg(long)]
    pub json: bool,
}

impl ListArgs {
    fn filter(&self) -> Result<Filter, String> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let named = [
            ("status", &self.status),
            ("category", &self.category),
            ("priority", &self.priority),
            ("assignee", &self.assignee),
            ("tag", &self.tag),
            ("parent", &self.parent),
            ("title", &self.title),
            ("blocked", &self.blocked),
            ("due_by", &self.due_by),
        ];
        for (key, value) in named {
            if let Some(v) = value {
                pairs.push((key.into(), v.clone()));
            }
        }
        for field in &self.fields {
            let (k, v) = field
                .split_once('=')
                .ok_or_else(|| format!("--field expects key=value, got `{field}`"))?;
            pairs.push((format!("field.{k}"), v.into()));
        }
        Filter::from_pairs(&pairs)
    }
}

pub fn resolve_dir(arg: Option<PathBuf>) -> PathBuf {
    arg.or_else(|| std::env::var_os("SIMPLETICKETS_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Context {
    dir: PathBuf,
    config: Config,
}

impl Context {
    fn open(dir: PathBuf) -> Result<Context, String> {
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let config = Config::load_from_dir(&dir).map_err(|e| e.to_string())?;
        Ok(Context { dir, config })
    }

    fn scan(&self) -> Store {
        Store::scan(&self.config)
    }

    fn audit(&self) -> AuditLog {
        let _ = ensure_state_dir(&self.dir);
        AuditLog::new(audit_log_path(&self.dir))
    }

    /// Runs the rules after a change and prints what they did.
    fn reconcile(&self, store: Store) -> Store {
        let (store, report) = rules::reconcile(store, Some(&self.audit()), explicit());
        print_rules(&report);
        store
    }
}

/// Rules run on explicit user commands ignore the recent-modification guard;
/// the content-hash check still protects concurrent edits.
fn explicit() -> RunOptions {
    RunOptions {
        dry_run: false,
        write: WriteOptions {
            min_age: Duration::ZERO,
        },
    }
}

fn print_rules(report: &RunReport) {
    let verb = if report.dry_run { "would set" } else { "set" };
    for c in &report.changes {
        println!("rule {}: {verb} {} {} -> {}", c.rule, c.id, c.from, c.to);
    }
    for s in &report.skipped {
        println!("skipped {}: {}", s.id, s.reason);
    }
    for w in &report.warnings {
        println!("warning {}: {}", w.id, w.message);
    }
    if let Some(e) = &report.state_error {
        println!("state: {e}");
    }
}

pub fn run(cli: Cli) -> Result<ExitCode, String> {
    let ctx = Context::open(resolve_dir(cli.dir))?;
    match cli.command {
        Command::Create(args) => create(&ctx, args),
        Command::List(args) => list(&ctx, &args),
        Command::Search(args) => search(&ctx, &args),
        Command::Show { id, json } => show(&ctx, &id, json),
        Command::SetStatus { id, status } => {
            let store = ctx.scan();
            let up = Update {
                status: Some(status.clone()),
                ..Update::default()
            };
            let options = WriteOptions {
                min_age: Duration::ZERO,
            };
            let outcome = ops::update(&store, &id, &up, options).map_err(|e| e.to_string())?;
            if outcome.changed {
                println!("{id} -> {status}");
            } else {
                println!("{id} is already {status}");
            }
            ctx.reconcile(ctx.scan());
            Ok(ExitCode::SUCCESS)
        }
        Command::Assign { id, assignee } => assign(&ctx, &id, Some(assignee)),
        Command::Unassign { id } => assign(&ctx, &id, None),
        Command::ReassignId { id, path } => {
            let store = ctx.scan();
            let done = ops::reassign_id(&store, &id, path.as_deref(), explicit().write)
                .map_err(|e| e.to_string())?;
            println!(
                "{} -> {} ({})",
                done.old_id,
                done.new_id,
                done.path.display()
            );
            ctx.reconcile(ctx.scan());
            Ok(ExitCode::SUCCESS)
        }
        Command::Forget { id } => {
            ops::forget_tombstone(&ctx.scan(), &id).map_err(|e| e.to_string())?;
            println!("forgot {id}");
            ctx.reconcile(ctx.scan());
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor { json } => {
            let store = ctx.scan();
            let report = doctor::check(&store);
            if json {
                println!("{}", doctor_json(&store));
            } else {
                print!("{report}");
                for w in simpletickets_core::warnings::load(&ctx.dir) {
                    println!("{w}");
                }
            }
            Ok(if report.is_clean() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Rescan { dry_run, json } => {
            let store = ctx.scan();
            let count = store.tickets().count();
            let options = RunOptions {
                dry_run,
                ..explicit()
            };
            let (_, report) = rules::reconcile(store, Some(&ctx.audit()), options);
            if json {
                let v = json!({"tickets": count, "rules": report});
                println!("{v}");
            } else {
                println!("{count} tickets");
                print_rules(&report);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Daemon(args) => {
            let mut options = crate::daemon::Options::new(ctx.dir.clone());
            options.port = args.port;
            options.bind_all = args.bind_all;
            options.scan_interval = Duration::from_secs(args.scan_interval.max(1));
            options.token_file = args.token_file;
            options.watch = !args.no_watch;
            options.poll_interval = args
                .poll
                .then(|| Duration::from_secs(args.poll_interval.max(1)));
            if !args.no_update_check {
                let mut check = crate::update::UpdateCheck::github();
                check.interval = Duration::from_secs(args.update_interval_hours.max(1) * 3600);
                options.update_check = Some(check);
            }
            options.logger = Arc::new(make_logger(args.log_level, args.log_file)?);
            let running = crate::daemon::start(options)?;
            println!("listening on http://{}/v1", running.addr);
            println!("token file: {}", running.token_file().display());
            let (signal_tx, signal_rx) = std::sync::mpsc::channel();
            ctrlc::set_handler(move || {
                let _ = signal_tx.send(());
            })
            .map_err(|e| format!("cannot install the signal handler: {e}"))?;
            // Blocks until SIGINT, SIGTERM or SIGHUP (Ctrl-C, Ctrl-Break or console close on
            // Windows), then runs the normal stop path.
            let _ = signal_rx.recv();
            running.shared.logger.info("shutdown requested");
            running.stop();
            Ok(ExitCode::SUCCESS)
        }
        Command::Migrate { command } => migrate(&ctx, command),
        Command::Templates => {
            for name in templates::list(&ctx.dir) {
                println!("{name}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn assign(ctx: &Context, id: &str, assignee: Option<String>) -> Result<ExitCode, String> {
    let store = ctx.scan();
    let up = Update {
        assignee: Some(assignee.clone()),
        ..Update::default()
    };
    let outcome = ops::update(&store, id, &up, explicit().write).map_err(|e| e.to_string())?;
    let who = assignee.as_deref().map_or("nobody", str::trim);
    if outcome.changed {
        println!("{id} -> {who}");
    } else {
        println!("{id} is already assigned to {who}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Builds the daemon logger from the flags, falling back to the environment.
fn make_logger(level: Option<String>, file: Option<PathBuf>) -> Result<Logger, String> {
    let level = match level.or_else(|| std::env::var(logger::LEVEL_ENV).ok()) {
        Some(text) => Level::parse(&text)
            .ok_or_else(|| format!("unknown log level {text:?} (error, warn, info, debug)"))?,
        None => Level::Info,
    };
    let file = file.or_else(|| {
        std::env::var_os(logger::FILE_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });
    match file {
        Some(path) => {
            Logger::file(level, &path).map_err(|e| format!("log file {}: {e}", path.display()))
        }
        None => Ok(Logger::stderr(level)),
    }
}

fn create(ctx: &Context, args: CreateArgs) -> Result<ExitCode, String> {
    let store = ctx.scan();
    let new = NewTicket {
        title: args.title,
        status: args.status,
        priority: args.priority,
        assignee: args.assignee,
        due: args.due,
        tags: args.tags,
        parent: args.parent,
        blocked_by: args.blocked_by,
        related: args.related,
        template: args.template,
        body: args.body,
        role: args.role.map(|r| {
            if r == "parent" {
                Role::Parent
            } else {
                Role::Sub
            }
        }),
    };
    let created = ops::create(&store, &ctx.dir, new).map_err(|e| e.to_string())?;
    if args.json {
        println!(
            "{}",
            json!({"id": created.id, "path": created.path.display().to_string()})
        );
    } else {
        println!("{} {}", created.id, created.path.display());
    }
    ctx.reconcile(ctx.scan());
    Ok(ExitCode::SUCCESS)
}

fn list(ctx: &Context, args: &ListArgs) -> Result<ExitCode, String> {
    let filter = args.filter()?;
    let store = ctx.scan();
    let found = query::list(&store, &filter);
    if args.json {
        let items: Vec<Value> = found
            .iter()
            .map(|e| ticket_json(&store, e, false))
            .collect();
        println!("{}", Value::Array(items));
        return Ok(ExitCode::SUCCESS);
    }
    let width = found.iter().map(|e| e.ticket.id.len()).max().unwrap_or(0);
    let status_width = found
        .iter()
        .map(|e| e.ticket.status.len())
        .max()
        .unwrap_or(0);
    for e in found {
        let t = &e.ticket;
        let mut flags = String::new();
        if store.is_blocked(&t.id) {
            flags.push_str(" [blocked]");
        }
        if let Some(due) = &t.due {
            flags.push_str(&format!(" (due {due})"));
        }
        if let Some(who) = &t.assignee {
            flags.push_str(&format!(" @{who}"));
        }
        println!(
            "{:width$}  {:status_width$}  {:6}  {}{flags}",
            t.id,
            t.status,
            t.priority.as_deref().unwrap_or("-"),
            t.title
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn search(ctx: &Context, args: &SearchArgs) -> Result<ExitCode, String> {
    let query = Query::parse(&args.query)?;
    let named = [
        ("status", &args.status),
        ("category", &args.category),
        ("assignee", &args.assignee),
        ("tag", &args.tag),
    ];
    let pairs: Vec<(String, String)> = named
        .into_iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| (k.to_string(), v.clone())))
        .collect();
    let filter = Filter::from_pairs(&pairs)?;
    let store = ctx.scan();
    let found: Vec<_> = query::search(&store, &query, &filter)
        .into_iter()
        .take(args.limit.unwrap_or(usize::MAX))
        .collect();
    if args.json {
        let items: Vec<Value> = found
            .iter()
            .map(|(e, hit)| hit_json(&store, e, hit))
            .collect();
        println!("{}", Value::Array(items));
        return Ok(ExitCode::SUCCESS);
    }
    for (e, hit) in &found {
        let t = &e.ticket;
        println!("{}  {}  {}", t.id, t.status, t.title);
        if hit.field == simpletickets_core::search::Field::Body {
            println!("    {}", hit.snippet);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn show(ctx: &Context, id: &str, json: bool) -> Result<ExitCode, String> {
    let store = ctx.scan();
    let entry = store.get(id).ok_or_else(|| format!("no ticket {id}"))?;
    let v = ticket_json(&store, entry, true);
    if json {
        println!("{v}");
        return Ok(ExitCode::SUCCESS);
    }
    let list = |key: &str| {
        v[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let text = |key: &str| v[key].as_str().unwrap_or("-").to_string();
    println!("{}  {}", text("id"), text("title"));
    println!("status:    {} ({})", text("status"), text("category"));
    if text("effective_category") != text("category") {
        println!("derived:   {}", text("effective_category"));
    }
    println!("priority:  {}", text("priority"));
    println!("assignee:  {}", text("assignee"));
    println!("due:       {}", text("due"));
    println!("tags:      {}", list("tags"));
    println!("parent:    {}", text("parent"));
    println!("children:  {}", list("children"));
    println!("blocked by: {}", list("blocked_by"));
    println!("open blockers: {}", list("open_blockers"));
    println!("blocks:    {}", list("blocks"));
    println!("related:   {}", list("related"));
    println!("path:      {}", text("path"));
    print!("\n{}", v["body"].as_str().unwrap_or(""));
    Ok(ExitCode::SUCCESS)
}

fn print_journal(journal: &Journal, json: bool) {
    if json {
        println!("{}", json!(journal));
        return;
    }
    for step in &journal.steps {
        let note = step
            .note
            .as_deref()
            .map_or(String::new(), |n| format!(" ({n})"));
        println!(
            "{:?}: {}{note}",
            step.state,
            migration::describe(&step.action)
        );
    }
    for c in &journal.conflicts {
        println!("conflict {}: {}", c.id, c.message);
    }
    println!("migration {:?}", journal.state);
    if let Some(i) = &journal.integrity {
        println!(
            "integrity: {} (missing ids: {}, broken links {} -> {})",
            if i.ok { "ok" } else { "FAILED" },
            i.missing_ids.len(),
            i.missing_targets_before,
            i.missing_targets_after
        );
    }
}

fn migrate(ctx: &Context, command: MigrateCommand) -> Result<ExitCode, String> {
    let read = |path: &PathBuf| {
        std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
    };
    let finish = |journal: Journal, json: bool| {
        print_journal(&journal, json);
        let ok = journal.state == JournalState::Complete
            && journal.integrity.as_ref().map_or(true, |i| i.ok);
        Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    };
    match command {
        MigrateCommand::Plan { config, json } => {
            let text = read(&config)?;
            let new = Config::parse(&text, &ctx.dir).map_err(|e| e.to_string())?;
            let plan = migration::plan(&ctx.scan(), &new);
            if json {
                println!("{}", json!(plan));
            } else if plan.is_empty() {
                println!("no migration needed");
            } else {
                for a in &plan.actions {
                    println!("{}", migration::describe(a));
                }
                for c in &plan.conflicts {
                    println!("conflict {}: {}", c.id, c.message);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        MigrateCommand::Apply { config, json } => {
            let text = read(&config)?;
            let journal =
                migration::begin(&ctx.dir, &ctx.scan(), &text).map_err(|e| e.to_string())?;
            let journal = migration::run(&ctx.dir, journal, migration::RunOptions::default())
                .map_err(|e| e.to_string())?;
            finish(journal, json)
        }
        MigrateCommand::Resume { json } => {
            match migration::resume(&ctx.dir, migration::RunOptions::default())
                .map_err(|e| e.to_string())?
            {
                Some(journal) => finish(journal, json),
                None => {
                    println!("no unfinished migration");
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
        MigrateCommand::Status { json } => {
            match migration::load(&ctx.dir).map_err(|e| e.to_string())? {
                Some(journal) => {
                    print_journal(&journal, json);
                    Ok(ExitCode::SUCCESS)
                }
                None => {
                    println!("no migration recorded");
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
    }
}
