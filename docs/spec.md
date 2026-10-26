# SimpleTickets specification

Lightweight, open-source ticketing app that runs in the background (Mac, Windows, Linux) or in a Docker container. The base is very small; everything on top is a plugin.

## Vision

- Open source, very lightweight base.
- Tickets are Markdown files in a folder structure, similar to how Obsidian manages files.
- Everything beyond ticket management is a plugin.

## Why this exists

- Note tools have no automation, and a global vault for files spread across many repos and projects gets messy.
- Local-first tickets that can be synced to a hosted tracker when needed, so someone else can pick up the work.
- A Kanban board in a UI to see what is in progress.
- An MCP server lets automation pick the next thing to work on. Tickets are not only code: "Create reporting, deadline Thursday" gets picked up because its deadline is today.
- Long-term: automated workflows with visible tracking.

## Core (build now)

### Ticket model

- Tickets are written and updated as Markdown in a folder structure.
- Each ticket has: id, status, priority, links to other tickets.
- Relations: related and blocking tickets.
- Default folder where tickets are managed; the configuration lives there too.

### Parent and sub-tickets

- Config can point parent tickets at one folder and sub-tickets at another.
- Parent tickets are recommended to be specification files. They may sit alongside the code.
- Sub-tickets track implementation. They may sit in the default folder.

### Automatic rules

- When a ticket's status becomes done, all tickets it blocks are unblocked.
- When work starts on any sub-ticket, the parent is automatically marked in progress.

### Background behaviour

- Periodically checks the managed folders for new files/tickets.
- Periodically checks the open GitHub repository for app updates.

### API layer (runs in the background)

1. Get all parent tickets.
2. Get all tickets, with filtering by name and other named fields.
3. Create and update tickets.
4. Trigger ticket migration based on a config change.
5. Change config.

### Distribution

- Installer in the Git repository that installs the app on the current system and asks which addons to include.
- Must be easily extendable to support the addons.

## Addons (not in the first build)

1. UI for the API layer.
2. MCP server layer on top of the API.
3. Synchronization to other software (built against Jira).

## Decisions

- Audience: personal automated workflow first; open source stays the goal.
- Core stays minimal; everything else is a plugin. "Next ticket" is a plugin (custom filter + order + first item), not core. Core must be prepared for a future workflow-automation plugin.
- Plugins are out-of-process (local API + event stream) in v0.1. In-process hooks come later, only when a real plugin needs them.
- Statuses: custom names mapped to four categories (todo, in-progress, done, cancelled). Rules act on categories. "Blocked" is derived from open blockers, never stored.
- Parent status (in progress/done) is derived from its sub-tickets, never written into the spec file.
- IDs: sequential with random fallback on collision.
- The daemon writes frontmatter only, atomically, with compare-and-swap. It never touches the body in rules.
- Rules are event-driven. A full scan runs on startup and there is an API route to trigger a rescan. The periodic scan stays.
- Migration on config change: dry-run plan, then a journaled, resumable apply. IDs never change.
- API binds localhost only and needs a token. Auto-update only notifies until releases are signed.
- Core fields: id, title, status, priority, due, tags, links, free-form extra keys. Tags have custom logging driven by config.
- Core reserves no Jira fields. A sync plugin owns sync state and talks to separate connector plugins (Jira, archive, ...), so the sync plugin does not care how many targets exist.
- Ticket marker: required in repo roots; every `.md` counts in dedicated ticket folders.
- Roots: explicit list in config, plus automatic scanning for new files in folders listed in config.
- Links: IDs in frontmatter (`parent`, `blocked_by`, `related`). Reverse links derived. Wikilinks in the body are optional display only.
- Bad state (deleted ticket, duplicate ID, malformed frontmatter, cycles): never guess, report through a doctor endpoint. Unparseable files are skipped and never rewritten.
- v0.1 platform: native binary on Mac first, Docker second.
- v0.1 done: core plus a minimal MCP plugin so a client can list and update tickets end to end.
- Language: Rust.
- Rules run after every scan (startup, periodic, watcher, API), so a transition missed while the daemon was down or a watcher dropped an event is picked up by the next scan.
- Cancelled tickets unblock dependents by default (`cancelled_unblocks`, configurable). Blocked state is derived, so reopening a done ticket re-blocks its dependents without a write; in-progress and done dependents get `blocked-in-progress` and `done-while-blocked` warnings.
- Known ticket IDs are persisted in `.simpletickets/known-ids.json`. A known ID that goes missing is a tombstone: links keep their meaning and a missing blocker stays open until the tombstone is acknowledged (`forget`, `DELETE /v1/tombstones/{id}`). IDs of tombstones are not reused.
- An unreadable root is offline: its tickets keep their last known index entries, are frozen (no rule touches them, no writes, no unblocking), and it is surfaced in doctor, health and a `root.offline` event; it recovers by itself on the next scan.
- One rule-running daemon per folder through a heartbeat lease in `.simpletickets/daemon.lock` (stale after 30 seconds). CLI commands do not need the lease; compare-and-swap protects their writes.
- Parent "ready for review": with `ready_for_review` on, a parent whose sub-tickets are all finished (one done at least) is set to `review_status` (an in-progress status) instead of counting as done; only a human marks it done.
- A sub-ticket starting under a done or cancelled parent either reopens the parent with a warning (`reopen`, default) or is refused by the CLI and API (`refuse`), configured by `subticket_under_finished_parent`.
- Rule warnings are persisted in `.simpletickets/warnings.json`, exposed by doctor and announced with `warning.raised`. Conditions clear with their cause; events are kept as capped history.
- Duplicate IDs: the older file (modification time, then path) keeps the ID in the index, the copies are conflicted and reported by doctor; `reassign-id` gives a copy a new ID by rewriting only its frontmatter `id` with compare-and-swap.
- Event stream contract (see [api.md](api.md)): envelope `{seq, time, type, data}`, with `seq` consecutive within one daemon run; per-type payloads; in-order, at-least-once delivery over long polling. The backlog is in memory and its size is set with `--event-backlog` (default 1000). A reader that fell behind it, or holds a cursor from before a restart, is told through `gap`, `gap_reason`, `oldest` and `stream` and must resync: take `next`, reload state through the list routes, continue from the cursor.
- Ticket-level events (`ticket.created`, `ticket.updated`, `ticket.status_changed`, `ticket.moved`, `ticket.deleted`) come from diffing the previous and the new ticket index on every scan, so one code path covers API writes, rule writes, command line writes and hand edits. The `source` field (`api`, `rule`, `scan`) is best effort. Edits between two scans can collapse into one event.
- API versioning: the version is the integer in the path (`/v1`) and is reported as `api_version` by health and in the `X-SimpleTickets-Api-Version` header. Within a version changes are additive only; a breaking change goes to `/v2` and `/v1` stays for at least 12 months. Clients ignore unknown fields and event types.
- Webhooks are core, config-driven and outgoing only (`webhooks` in `simpletickets.yml`: `url`, event globs, `secret_env`). Bodies are the event envelope, signed with HMAC-SHA256 over `timestamp.body`; the secret is read from a named environment variable and never stored. Only `http` and `https` URLs are accepted, local and private targets are allowed. Delivery has its own queue and threads per webhook, retries with capped backoff, and never blocks the API or the rules; a failed event is dropped after the last attempt and logged. It stays at-least-once and not durable; durable consumers use the event stream.

## Open questions

- Offline detection is per root: an unreadable subfolder inside a readable root is reported by doctor, but its tickets are covered by tombstones only, not frozen.
- The lease compares heartbeats with the local clock, so a large clock difference between machines sharing a folder can make a live lease look stale. Should it carry a host name, or use file locks where the file system supports them?
- Durable webhook delivery (an on-disk queue that survives a daemon restart) and a way to replay missed events, if a receiver ever needs more than at-least-once while the daemon is up.
- Plugin discovery, install and lifecycle (who starts and supervises plugin processes).
- Config schema: tags with custom logging, status-to-category mapping, roots, auto-scan folders.
- Daemon packaging per OS (launchd first) and the Docker polling fallback.
- See [design review](design-review.md) for the full list of edge cases and risks.
