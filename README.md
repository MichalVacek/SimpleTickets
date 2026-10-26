# SimpleTickets

A lightweight, local-first ticketing app. Tickets are plain Markdown files in a folder tree, much like an Obsidian vault. A small background daemon keeps them consistent and exposes a local API. Everything beyond ticket management is a plugin.

Status: early development. The command line works; the daemon and API are in progress.

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

## Documentation

- [Specification](docs/spec.md)
- [Design review](docs/design-review.md)

## License

Not yet chosen.
