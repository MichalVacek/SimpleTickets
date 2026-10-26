//! Command line interface.

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use simpletickets_core::audit::AuditLog;
use simpletickets_core::config::Config;
use simpletickets_core::doctor;
use simpletickets_core::ops::{self, NewTicket, Role, Update};
use simpletickets_core::query::{self, Filter};
use simpletickets_core::rules::{self, RunOptions, RunReport};
use simpletickets_core::state::{audit_log_path, ensure_state_dir};
use simpletickets_core::store::Store;
use simpletickets_core::templates;
use simpletickets_core::view::ticket_json;
use simpletickets_core::write::WriteOptions;
use std::path::PathBuf;
use std::process::ExitCode;
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
    /// Show one ticket with its derived data
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Change the status of a ticket
    SetStatus { id: String, status: String },
    /// Report inconsistencies in the ticket files
    Doctor {
        #[arg(long)]
        json: bool,
    },
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
}

#[derive(Args)]
pub struct CreateArgs {
    pub title: String,
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long)]
    pub priority: Option<String>,
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
pub struct ListArgs {
    #[arg(long)]
    pub status: Option<String>,
    /// todo, in-progress, done or cancelled
    #[arg(long)]
    pub category: Option<String>,
    #[arg(long)]
    pub priority: Option<String>,
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
}

pub fn run(cli: Cli) -> Result<ExitCode, String> {
    let ctx = Context::open(resolve_dir(cli.dir))?;
    match cli.command {
        Command::Create(args) => create(&ctx, args),
        Command::List(args) => list(&ctx, &args),
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
        Command::Doctor { json } => {
            let report = doctor::check(&ctx.scan());
            if json {
                let issues: Vec<String> = report.issues.iter().map(ToString::to_string).collect();
                println!("{}", json!({"clean": report.is_clean(), "issues": issues}));
            } else {
                print!("{report}");
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
        Command::Templates => {
            for name in templates::list(&ctx.dir) {
                println!("{name}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn create(ctx: &Context, args: CreateArgs) -> Result<ExitCode, String> {
    let store = ctx.scan();
    let new = NewTicket {
        title: args.title,
        status: args.status,
        priority: args.priority,
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
