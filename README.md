# SimpleTickets

A lightweight, local-first ticketing app. Tickets are plain Markdown files in a folder tree, much like an Obsidian vault. A small background daemon keeps them consistent and exposes a local API. Everything beyond ticket management is a plugin.

Status: v0.1 scope is complete: the core (command line, daemon, local API, rules, migration, update check) and a minimal MCP plugin work end to end, with installers for macOS, Linux and Windows, a release workflow and a Docker image. Only macOS and Docker installs have been exercised on a real machine; see [Install](#install). Also built: synchronization to other trackers (a sync plugin with an archive and a Jira connector; the Jira connector has only been tested against a mock server, not a live site). Still to come: signed releases.

## Goals

- Very small core, open source.
- Tickets are Markdown with frontmatter: id, title, status, priority, due, assignee, tags and links.
- Relations between tickets: parent, blocked by, related.
- Automatic rules: finishing a ticket unblocks the tickets it blocks; starting a sub-ticket marks its parent in progress.
- Local HTTP API (localhost only, token protected) for listing, creating and updating tickets.
- Runs natively on macOS, Linux and Windows (installers provided) or in Docker.
- UI, MCP server and sync to other trackers are plugins.

## Repository layout

| Path | Contents |
|---|---|
| `docs/` | Specification, design review, API and plugin documentation |
| `scripts/` | Installers: `install.sh` (macOS, Linux), `install.ps1` (Windows) |
| `Dockerfile` | Container image |
| `crates/` | Rust crates for the core |
| `plugins/` | Out-of-process plugins |
| `plugins/sync`, `plugins/connector-*` | Synchronization: the sync plugin, the connector protocol, the archive and Jira connectors |
| `examples/plugins/` | Example plugins, such as `follow-up` (a workflow plugin) |
| `examples/tickets/` | Sample ticket folder |

## Usage

Build with `cargo build --release`; the binary is `target/release/simpletickets`. Point it at a tickets folder with `--dir` or `SIMPLETICKETS_DIR` (default: the current folder). Settings live in `simpletickets.yml` inside that folder; see [examples/tickets](examples/tickets) for a sample.

```
simpletickets create "Write report" --priority high --due 2026-11-05 --tag work
simpletickets create "Collect data" --parent ST-1 --blocked-by ST-2
simpletickets list --status todo --tag work --blocked false --due-by today
simpletickets create "Review" --assignee ann
simpletickets assign ST-3 ann
simpletickets unassign ST-3
simpletickets list --assignee ann
simpletickets list --field team=core --json
simpletickets show ST-1
simpletickets set-status ST-1 done
simpletickets search "quarterly report" --status todo
simpletickets search quarterly --json
simpletickets doctor
simpletickets forget ST-2
simpletickets reassign-id ST-4 --path ~/tickets/copy-of-st-4.md
simpletickets rescan --dry-run
simpletickets templates
simpletickets plugins list
simpletickets plugins enable mcp
```

- `create` picks the next ID (`ST-1`, `ST-2`, ...), renders the body from a template (`--template`, default `default`) and writes `ID-title.md`. With `parent_folder` and `subticket_folder` configured, `--role parent` and `--parent` choose the folder.
- `list` filters: `--status`, `--category`, `--priority`, `--assignee`, `--tag`, `--parent`, `--title`, `--blocked`, `--due-by`, `--field key=value`. Add `--json` for machine-readable output.
- The assignee is a core frontmatter key (`assignee: Ann`), a single line of text. `create --assignee`, `assign ID NAME` and `unassign ID` set and clear it; `list --assignee` matches case-insensitively and `list` shows it as `@name`. A top-level `assignee` text or number that already exists in a file is read as the core field, so `--field assignee=Ann` and files written before it became a core key keep working; an `assignee` of any other shape (a list, say) stays a free-form key.
- `search QUERY` finds tickets by title and body text, best match first, with a snippet of the first body match (`--json` adds `score`, `match_field` and `snippet`; `--limit`, `--status`, `--category`, `--assignee`, `--tag` narrow it). See Search below.
- `set-status`, `create` and `rescan` run the rules afterwards. A sub-ticket in progress sets its parent to in progress; a done (or, by default, cancelled) blocker stops blocking. Parents inside `parent_folder` are specification files: their state is derived and never written. Automatic changes are appended to `.simpletickets/audit.jsonl`; `rescan --dry-run` shows them without writing.
- `plugins list|enable|disable|status` manages plugins: manifests are found in `.simpletickets/plugins/<name>/plugin.yml` and in the per-user plugins folder, and the `plugins.enabled` list in `simpletickets.yml` says which are on. See [docs/plugins.md](docs/plugins.md).
- Custom templates are Markdown files in `.simpletickets/templates/NAME.md` using `{{title}}`, `{{id}}` and `{{date}}`.
- `doctor` exits non-zero when it finds duplicate IDs, malformed frontmatter, missing link targets, cycles, orphans or an offline root. It also lists the rule warnings recorded by the last rule run (see below); warnings never change the exit code.
- `forget ID` acknowledges a deleted ticket (see Data safety). `reassign-id ID [--path FILE]` resolves a duplicate ID (see Duplicate IDs).

## Install

Release archives are named `simpletickets-<target>.tar.gz` (`.zip` on Windows) and contain `simpletickets`, `simpletickets-mcp`, `simpletickets-ui`, `simpletickets-sync` and the connectors `simpletickets-connector-archive` and `simpletickets-connector-jira`, plus a `plugins/<name>/plugin.yml` manifest for each; `SHA256SUMS` lists their checksums. The installers verify the archive against it.

| Target | Archive |
|---|---|
| macOS Apple silicon | `simpletickets-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `simpletickets-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (glibc) | `simpletickets-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 (glibc) | `simpletickets-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `simpletickets-x86_64-pc-windows-msvc.zip` |

Pushing a tag `v*` runs `.github/workflows/release.yml`, which builds these, publishes the GitHub release and pushes a Docker image to `ghcr.io`. No release has been published yet, so until one exists the installers must build from a checkout (`--from-source`, the default when `cargo` is available).

### macOS

```
scripts/install.sh                 # asks which addons to include
scripts/install.sh --yes --addons mcp,ui
scripts/install.sh --dry-run       # print the plan and the launchd plist, change nothing
```

The installer builds from this checkout when `cargo` is available (otherwise it downloads the latest release archive), copies the binaries to `~/.local/bin`, creates the tickets folder (`~/SimpleTickets`), writes `~/Library/LaunchAgents/com.simpletickets.daemon.plist` and offers to load it so the daemon runs at login. Logs go to `~/Library/Logs/SimpleTickets/daemon.log`. To remove the service: `launchctl bootout gui/$(id -u)/com.simpletickets.daemon` and delete the plist.

### Linux

The same script, `scripts/install.sh`, detects Linux and writes a systemd user unit, `~/.config/systemd/user/simpletickets.service`, then offers to run `systemctl --user enable --now simpletickets.service`. Logs go to the journal (`journalctl --user -u simpletickets`). To keep the daemon running while you are logged out: `loginctl enable-linger $USER`. Remove it with `systemctl --user disable --now simpletickets.service` and delete the unit file.

`scripts/install.sh --dry-run --os linux` previews the Linux plan, including the unit text, from any machine. Status: the script passes `bash -n` and `shellcheck`, and the dry-run output was checked, but no install has been run on a real Linux machine, and the release archives for Linux have not been built yet.

### Windows

```
powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -DryRun
powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -Yes -Addons mcp
```

The script copies the binaries to `%LOCALAPPDATA%\SimpleTickets\bin`, creates the tickets folder (`~\SimpleTickets`) and registers a per-user scheduled task named `SimpleTickets` that starts the daemon at logon (hidden, restarted up to three times if it exits). A true Windows service is out of scope: the daemon does not implement the service control protocol, and a scheduled task needs no administrator rights. Remove it with `Unregister-ScheduledTask -TaskName SimpleTickets -Confirm:$false`. Parameters: `-Prefix`, `-TicketsDir`, `-Port`, `-Addons`, `-PluginsDir`, `-FromSource`, `-FromRelease`, `-NoService`, `-NoStart`, `-Yes`, `-DryRun`.

Status: untested. The script has not been run or parsed by PowerShell, and the Windows build is only exercised by the CI Windows job once it runs.

### Options of `install.sh`

`--prefix`, `--tickets-dir`, `--port`, `--addons LIST|none`, `--plugins-dir`, `--from-source`, `--from-release`, `--launchagents-dir` (macOS), `--systemd-dir` (Linux), `--no-service`, `--no-load`, `--yes`, `--dry-run`, `--os macos|linux` (previews only, needs `--dry-run`).

`install.sh` builds its addon list from the plugin manifests in `plugins/*/plugin.yml`: a manifest with an `install:` block (package and binary) is offered as an addon, and its manifest is copied into the per-user plugins folder so the daemon finds it (see [docs/plugins.md](docs/plugins.md)). `install.ps1` does the same with a minimal line parser and copies the manifests into `%APPDATA%\simpletickets\plugins` (`-PluginsDir` changes it); this manifest handling is unrun, like the rest of the script.

## Docker

```
docker build -t simpletickets .
docker run -d --name simpletickets -v ~/tickets:/data -p 127.0.0.1:7878:7878 simpletickets
cat ~/tickets/.simpletickets/token       # the API token
docker exec -i simpletickets simpletickets-mcp --dir /data    # MCP server inside the container
```

The image contains `simpletickets`, `simpletickets-mcp`, `simpletickets-sync` and both connectors, with their manifests in `/usr/local/share/simpletickets/plugins` (`SIMPLETICKETS_PLUGINS_DIR`), so `docker exec simpletickets simpletickets plugins list` shows them and `plugins enable sync` works. The UI is not in the image: `simpletickets-ui` only listens on `127.0.0.1` and has no bind-address option, so it would not be reachable from outside the container. Use the UI from a native install, pointed at the published daemon port.

The container runs `simpletickets --dir /data daemon --bind-all --poll`. Bind mounts often deliver no file events, so `--poll` (every 2 seconds, `--poll-interval`) detects changes by polling instead; use it for any folder on a network share or mount that misses events. `--bind-all` makes the daemon listen on all interfaces inside the container, which is what lets Docker publish the port; keep the `127.0.0.1:` prefix on `-p` so the API stays reachable from this machine only. The token still applies. The image has a health check that opens a TCP connection to port 7878; it cannot call `/v1/health` because every API route, health included, requires the token. The process runs as UID 1000; the mounted folder must be writable by it, or pass `--user $(id -u):$(id -g)`.

## Daemon and API

```
simpletickets daemon --port 7878 --scan-interval 60
```

Other flags: `--no-watch` (periodic scans only), `--poll` (see Docker), `--token-file`, `--no-update-check`, `--event-backlog` (events kept for late pollers, default 1000), `--log-level`, `--log-file`.

Logging: one line per entry, `<UTC time> <LEVEL> <message> key=value ...`, on stderr or appended to `--log-file`. `--log-level` is `error`, `warn`, `info` (default) or `debug`; `SIMPLETICKETS_LOG_LEVEL` and `SIMPLETICKETS_LOG_FILE` supply defaults that the flags override. The daemon logs startup and shutdown, scans (`debug`, except the startup scan), applied rules, rule warnings, errors, roots going offline or online, lease changes, migrations and update checks. A rule application is logged at `warn` when the ticket carries a tag configured as `warn` in `tag_logging`, otherwise at `info`. The API token is never logged.

Shutdown: on SIGINT or SIGTERM (and SIGHUP on unix; Ctrl-C, Ctrl-Break or closing the console on Windows) the daemon stops accepting requests, lets the workers and the scheduler finish, releases the folder lease, flushes the log and exits with status 0. A second daemon can take over immediately instead of waiting for the lease to go stale. `launchctl bootout`, `systemctl --user stop` and `docker stop` all send SIGTERM, so they shut the daemon down the same way.

Events: `ticket.created`, `ticket.updated`, `ticket.status_changed`, `ticket.moved`, `ticket.deleted` (for every change to a ticket, whether made through the API, a rule, the command line or by hand), plus `scan`, `rule.applied`, `root.offline`, `root.online`, `warning.raised`, `tombstone.forgotten`, `ticket.id_reassigned`, `lease.lost`, `lease.acquired`, `state.error` and others. The full contract (envelope, payloads, gap detection, delivery guarantees) is in [docs/api.md](docs/api.md).

The daemon scans all folders on startup, watches them for changes, rescans periodically and runs the rules after each scan. Every response carries `X-SimpleTickets-Api-Version`; within `v1` changes are additive only (see [docs/api.md](docs/api.md#versioning-and-compatibility)). The API listens on `127.0.0.1` only and requires `Authorization: Bearer <token>`. The token is generated on first run and stored in `.simpletickets/token` (owner-only permissions; `--token-file` overrides the location). Requests whose `Host` is not a localhost name are refused.

| Route | Purpose |
|---|---|
| `GET /v1/health` | `api_version` (integer, 1), `version`, `status` (`ok` or `degraded` while a root is offline), ticket and frozen counts, `offline_roots`, `rules_enabled` and `update` (`null`, or the newer release found by the update check) |
| `GET /v1/parents` | parent tickets |
| `GET /v1/tickets?status=&category=&priority=&assignee=&tag=&parent=&title=&blocked=&due_by=&field.<key>=&limit=` | list and filter |
| `GET /v1/search?q=&limit=&<filters>` | full-text search; the same filters as the list, results carry `score`, `match_field` and `snippet`; `q` on `GET /v1/tickets` does the same |
| `GET /v1/tickets/{id}` | one ticket with body and derived data |
| `POST /v1/tickets` | create (`title`, `status`, `priority`, `assignee`, `due`, `tags`, `parent`, `blocked_by`, `related`, `template`, `body`, `role`) |
| `PATCH /v1/tickets/{id}` | update; `null` clears `priority`, `assignee`, `due`, `parent` |
| `POST /v1/rescan` | scan and run the rules now |
| `GET /v1/doctor` | consistency report: `clean`, `issues`, `tombstones` and the recorded rule `warnings` |
| `GET /v1/tombstones`, `DELETE /v1/tombstones/{id}` | deleted tickets that are still referenced; acknowledge one |
| `POST /v1/tickets/{id}/reassign-id` | give a conflicted copy of a duplicated ID a new one (`{"path": "..."}` optional) |
| `GET /v1/config`, `PUT /v1/config` | read or replace the configuration (`{"yaml": "..."}`); a change that needs a migration is refused with its plan unless `"migrate": true` is sent |
| `POST /v1/migration` | `{"yaml": "...", "dry_run": true}` returns the migration plan for a new config; `"dry_run": false` applies it |
| `GET /v1/migration`, `POST /v1/migration/resume` | journal of the last or current migration; finish an interrupted one |
| `GET /v1/events?since=N&wait=S&stream=ID` | event stream by long polling: events after sequence `N`, waiting up to `S` seconds; the response says when `N` is older than the retained backlog (`gap`) |
| `GET /v1/audit?limit=N` | automatic changes |
| `GET /v1/templates` | template names |

## Webhooks

`simpletickets.yml` can list outgoing webhooks. Matching events are POSTed as JSON with an HMAC-SHA256 signature; the secret is read from the environment variable named by `secret_env`, never stored in the config.

```yaml
webhooks:
  - url: http://127.0.0.1:9000/simpletickets
    events: ["ticket.*", "rule.applied"]    # globs; default ["*"]
    secret_env: SIMPLETICKETS_WEBHOOK_SECRET
```

Only `http` and `https` URLs are accepted; local and private addresses are fine. Failed deliveries are retried with a capped backoff (1 s doubling to 30 s, six attempts), logged, and never delay the API or the rules. Headers, signature verification and delivery guarantees are in [docs/api.md](docs/api.md#webhooks).

## Search

Search covers ticket titles and Markdown bodies (other frontmatter keys are not searched; use the list filters for those). The files stay the source of truth: the daemon and the CLI build an in-memory inverted index on every scan and discard it with the scan, so results are as fresh as the last scan and nothing extra is stored on disk.

- Case-insensitive and token-based: a token is a run of letters and digits, so `ST-12` is the tokens `st` and `12`. A word with punctuation matches only when its tokens sit next to each other.
- Every word and every `"quoted phrase"` must match (AND). A phrase matches its tokens in order, next to each other. An unterminated quote runs to the end of the query.
- Words match whole tokens: no stemming, prefixes, fuzziness, accent folding, OR or NOT. `report` does not find `reports`.
- Ranking: each title occurrence scores 5, each body occurrence 1; equal scores come in ID order, so the order is deterministic.
- Bodies are held in memory for snippets and matching, so memory grows with the total size of the ticket files. The index is rebuilt in full on each scan, which is fine for thousands of tickets but is not designed for very large folders.
- A query with no letters or digits is refused (`400` over the API).

## UI plugin

`plugins/ui` builds `simpletickets-ui`, a local kanban board: a small web server on `127.0.0.1` that serves a single-page app and relays its requests to the daemon, adding the API token on the server side so the browser never sees it. Other websites cannot drive it: the `Host` must be a localhost name, state-changing requests must come from the board's own origin, and every request needs a per-session header token issued with the page.

```
cargo build --release -p simpletickets-ui
simpletickets-ui --dir ~/tickets --port 7979     # then open http://127.0.0.1:7979/
```

Start the daemon first, or enable the plugin (`simpletickets plugins enable ui`) so the daemon starts and supervises the board on port 7979 (see [docs/plugins.md](docs/plugins.md)). Columns follow the status categories, cards can be dragged between them, and the board follows the event stream live. See [docs/ui.md](docs/ui.md).

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
| `list_tickets` | filters: `status`, `category`, `priority`, `assignee`, `tag`, `parent`, `title`, `blocked`, `due_by`, `fields`, `limit` |
| `search_tickets` | `query` (words and `"quoted phrases"`), optional `status`, `category`, `priority`, `assignee`, `tag`, `parent`, `limit`; best matches first with `score` and `snippet` |
| `show_ticket` | one ticket with body and derived data |
| `create_ticket` | `title` plus any of the other ticket fields, `body`, `template`, `role` (`parent` or `sub`) |
| `update_ticket` | `id` plus the fields to change; `null` clears `priority`, `assignee`, `due`, `parent` |
| `next_ticket` | the first unblocked todo ticket matching the optional filters, ordered by `order` (default `due`, `priority`; `id` is also accepted). Tickets without a due date sort last; priorities rank critical, high, medium, low, then `P0`-style numbers, then the rest |

API errors come back as tool errors, so a client sees the daemon's message (for example an unknown status).

## Sync plugin

`plugins/sync` builds `simpletickets-sync`, a service plugin that mirrors tickets to other trackers through separate connector programs. The core reserves no fields for it: the plugin keeps its own state (ticket to remote item, last synced values, conflicts) under `.simpletickets/sync/` and never writes into ticket files. It follows the event stream (resyncing on a gap), reconciles periodically, pushes local changes through the connector protocol, and with `direction: both` writes remote status and field changes back through the API as the actor `plugin:sync`. Local wins conflicts by default; conflicts are always recorded and listed, never guessed, and a ticket body is never overwritten. Deleted tickets close the remote item and never delete it.

```
scripts/install.sh --addons sync,connector-archive,connector-jira
simpletickets plugins enable sync
simpletickets-sync status [--check]
simpletickets-sync dry-run
simpletickets-sync push-now [ID...] [--prefer local|remote]
simpletickets-sync list-conflicts
```

- `connector-archive` writes one JSON or Markdown file per ticket to a folder; it is the reference connector and the test double.
- `connector-jira` talks to Jira Cloud (REST API v3): issues, status transitions, priority, labels, assignee, sub-tasks, blocked-by links, descriptions rendered to ADF. Credentials come only from `JIRA_SITE_URL`, `JIRA_EMAIL` and `JIRA_API_TOKEN`. **It is tested against an in-process mock server only; behaviour on a live Jira site is unverified.**

Configuration, the state model, conflict and deletion rules, the connector protocol and the Jira mapping and limits are in [docs/sync.md](docs/sync.md).

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
- Configuring `parent_folder` or `subticket_folder` for the first time, when tickets already sit scattered in the roots, moves them into place by role: a ticket that has sub-tickets pointing at it goes to `parent_folder`, a ticket with a `parent` goes to `subticket_folder`, keeping its file name. Tickets that are neither, and tickets already inside a configured folder, stay where they are. A ticket that is both a parent and a sub-ticket, a target file that already exists, two tickets that would land on the same file name, and a ticket in an offline root are reported as conflicts and skipped, never overwritten. Run `migrate plan` first; the moves are journaled and resumable like any other step.
- `id_prefix` changes need no steps: existing IDs keep the prefix they were created with (IDs never change), and only new tickets use the new prefix. The plan prints a warning saying so, and `PUT /v1/config` returns it in `warnings`. Sequence numbers are counted per prefix: with `id_prefix: TK`, tickets `ST-1`..`ST-5` and `TK-2` give the next ID `TK-3`, and switching back to `ST` continues at `ST-6`. IDs of deleted tickets (tombstones) are never reused, whatever their prefix.
- A status removed from the mapping is replaced by the first status of its old category.
- A step whose file changed since the plan is skipped rather than overwritten; a step that fails (locked or read-only file) is retried by `migrate resume`.
- The daemon only suggests a migration (`migration.suggested` event) when the config file is edited by hand.

## Documentation

- [Specification](docs/spec.md)
- [HTTP API and event contract](docs/api.md)
- [Plugins](docs/plugins.md)
- [Synchronization and the connector protocol](docs/sync.md)
- [Design review](docs/design-review.md)

## License

Not yet chosen.
