# SimpleTickets

A lightweight, local-first ticketing app. Tickets are plain Markdown files in a folder tree, much like an Obsidian vault. A small background daemon keeps them consistent and exposes a local API. Everything beyond ticket management is a plugin.

Status: early development, not usable yet.

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

## Documentation

- [Specification](docs/spec.md)
- [Design review](docs/design-review.md)

## License

Not yet chosen.
