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
- An MCP server lets an agent pick the next thing to work on. Tickets are not only code: "Create reporting, deadline Thursday" gets picked up because its deadline is today.
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

- Audience: personal agent-driven workflow first; open source stays the goal.
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
- v0.1 done: core plus a minimal MCP plugin so an agent can list and update tickets end to end.
- Language: Rust.

## Open questions

- Event-driven rules can miss transitions while the daemon is down or a watcher drops events. Is startup scan plus periodic scan enough, or do rules also need to run after each periodic scan?
- Ticket lifecycle details: do cancelled tickets unblock dependents (default yes, configurable)? What happens when a done ticket is reopened, or a sub-ticket starts under a done parent?
- Rule-engine concurrency: one daemon per root (lock/lease) when folders sync across machines.
- Event stream contract and API versioning for out-of-process plugins.
- Plugin discovery, install and lifecycle (who starts and supervises plugin processes).
- Config schema: tags with custom logging, status-to-category mapping, roots, auto-scan folders.
- Daemon packaging per OS (launchd first) and the Docker polling fallback.
- See [design review](design-review.md) for the full list of edge cases and risks.
