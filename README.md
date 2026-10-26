# SimpleTickets

A lightweight, local-first ticketing app. Tickets are plain Markdown files in a folder tree, much like an Obsidian vault. A small background daemon keeps them consistent and exposes a local API. Everything beyond ticket management is a plugin.

Status: early development. The command line, daemon, local API, update check and MCP plugin work.

## Goals

- Very small core, open source.
- Tickets are Markdown with frontmatter: id, title, status, priority, due, tags and links.
- Relations between tickets: parent, blocked by, related.
- Automatic rules: finishing a ticket unblocks the tickets it blocks; starting a sub-ticket marks its parent in progress.
- Local HTTP API (localhost only, token protected) for listing, creating and updating tickets.
- Runs natively on Mac, Windows and Linux, or in Docker.
- UI, MCP server and sync to other trackers are plugins.

## Repository layout

| Path | Contents |
|---|---|
| `docs/` | Specification and design review |
| `crates/` | Rust crates for the core |
| `plugins/` | Out-of-process plugins |
| `examples/tickets/` | Sample ticket folder |

## Usage

Build with `cargo build --release`; the binary is `target/release/simpletickets`. Point it at a tickets folder with `--dir` or `SIMPLETICKETS_DIR` (default: the current folder). Settings live in `simpletickets.yml` inside that folder; see [examples/tickets](examples/tickets) for a sample.

```
simpletickets create "Write report" --priority high --due 2026-11-05 --tag work
simpletickets create "Collect data" --parent ST-1 --blocked-by ST-2
simpletickets list --status todo --tag work --blocked false --due-by today
simpletickets list --field team=core --json
simpletickets show ST-1
simpletickets set-status ST-1 done
simpletickets doctor
simpletickets rescan --dry-run
simpletickets templates
```

- `create` picks the next ID (`ST-1`, `ST-2`, ...), renders the body from a template (`--template`, default `default`) and writes `ID-title.md`. With `parent_folder` and `subticket_folder` configured, `--role parent` and `--parent` choose the folder.
- `list` filters: `--status`, `--category`, `--priority`, `--tag`, `--parent`, `--title`, `--blocked`, `--due-by`, `--field key=value`. Add `--json` for machine-readable output.
- `set-status`, `create` and `rescan` run the rules afterwards. A sub-ticket in progress sets its parent to in progress; a done (or, by default, cancelled) blocker stops blocking. Parents inside `parent_folder` are specification files: their state is derived and never written. Automatic changes are appended to `.simpletickets/audit.jsonl`; `rescan --dry-run` shows them without writing.
- Custom templates are Markdown files in `.simpletickets/templates/NAME.md` using `{{title}}`, `{{id}}` and `{{date}}`.
- `doctor` exits non-zero when it finds duplicate IDs, malformed frontmatter, missing link targets, cycles or orphans.

## Daemon and API

```
simpletickets daemon --port 7878 --scan-interval 60
```

The daemon scans all folders on startup, watches them for changes, rescans periodically and runs the rules after each scan. The API listens on `127.0.0.1` only and requires `Authorization: Bearer <token>`. The token is generated on first run and stored in `.simpletickets/token` (owner-only permissions; `--token-file` overrides the location). Requests whose `Host` is not a localhost name are refused.

| Route | Purpose |
|---|---|
| `GET /v1/health` | version, ticket count and `update` (`null`, or the newer release found by the update check) |
| `GET /v1/parents` | parent tickets |
| `GET /v1/tickets?status=&category=&priority=&tag=&parent=&title=&blocked=&due_by=&field.<key>=&limit=` | list and filter |
| `GET /v1/tickets/{id}` | one ticket with body and derived data |
| `POST /v1/tickets` | create (`title`, `status`, `priority`, `due`, `tags`, `parent`, `blocked_by`, `related`, `template`, `body`, `role`) |
| `PATCH /v1/tickets/{id}` | update; `null` clears `priority`, `due`, `parent` |
| `POST /v1/rescan` | scan and run the rules now |
| `GET /v1/doctor` | consistency report |
| `GET /v1/config`, `PUT /v1/config` | read or replace the configuration (`{"yaml": "..."}`); a change that needs a migration is refused with its plan unless `"migrate": true` is sent |
| `POST /v1/migration` | `{"yaml": "...", "dry_run": true}` returns the migration plan for a new config; `"dry_run": false` applies it |
| `GET /v1/migration`, `POST /v1/migration/resume` | journal of the last or current migration; finish an interrupted one |
| `GET /v1/events?since=N&wait=S` | event stream by long polling: events after sequence `N`, waiting up to `S` seconds |
| `GET /v1/audit?limit=N` | automatic changes |
| `GET /v1/templates` | template names |

## MCP plugin

`plugins/mcp` builds `simpletickets-mcp`, a separate process that speaks MCP over stdio (newline-delimited JSON-RPC) and forwards tool calls to the running daemon. It has no dependency on the daemon's code at runtime, only on the local API and its token.

```
cargo build --release -p simpletickets-mcp
simpletickets-mcp --dir ~/tickets            # reads ~/tickets/.simpletickets/token
simpletickets-mcp --url http://127.0.0.1:7878 --token-file /path/to/token
```

Settings come from flags or `SIMPLETICKETS_URL`, `SIMPLETICKETS_TOKEN`, `SIMPLETICKETS_DIR`. Register it in an MCP client as a stdio server whose command is `simpletickets-mcp` with those arguments. Start the daemon first.

| Tool | Purpose |
|---|---|
| `list_tickets` | filters: `status`, `category`, `priority`, `tag`, `parent`, `title`, `blocked`, `due_by`, `fields`, `limit` |
| `show_ticket` | one ticket with body and derived data |
| `create_ticket` | `title` plus any of the other ticket fields, `body`, `template`, `role` (`parent` or `sub`) |
| `update_ticket` | `id` plus the fields to change; `null` clears `priority`, `due`, `parent` |
| `next_ticket` | the first unblocked todo ticket matching the optional filters, ordered by `order` (default `due`, `priority`; `id` is also accepted). Tickets without a due date sort last; priorities rank critical, high, medium, low, then `P0`-style numbers, then the rest |

API errors come back as tool errors, so a client sees the daemon's message (for example an unknown status).

## Update check

The daemon asks the GitHub releases of `MichalVacek/SimpleTickets` for the latest release at startup and then every 24 hours (`--update-interval-hours`). A newer version is announced once through an `update.available` event and in the `update` field of `/v1/health`. Nothing is downloaded or installed. Being offline or hitting a GitHub error is silent; the check simply runs again later. Disable it with `--no-update-check`.

## Migration

Changing `parent_folder`, `subticket_folder` or the status mapping can require moving files or rewriting statuses. A migration is planned first (dry run), then applied through a journal in `.simpletickets/migration.json` that is saved after every step. Steps are idempotent, so an interrupted migration is finished with `migrate resume` (the daemon does this on startup). The new config file is written last. Ticket IDs never change and links are IDs, so they stay valid; a check after completion confirms that no ticket disappeared and no link broke.

```
simpletickets migrate plan new-config.yml     # show the steps, change nothing
simpletickets migrate apply new-config.yml    # journaled apply, then install the config
simpletickets migrate resume
simpletickets migrate status
```

- Tickets inside the old parent or sub-ticket folder move to the new folder of the same kind, keeping their relative path. Moves that would overwrite a file or leave every scanned folder are reported as conflicts and skipped.
- A status removed from the mapping is replaced by the first status of its old category.
- A step whose file changed since the plan is skipped rather than overwritten; a step that fails (locked or read-only file) is retried by `migrate resume`.
- The daemon only suggests a migration (`migration.suggested` event) when the config file is edited by hand.

## Documentation

- [Specification](docs/spec.md)
- [Design review](docs/design-review.md)

## License

Not yet chosen.
