# Implementation plan

Source: [spec.md](spec.md). Language: Rust. Work lands directly on `main`, one slice per commit, strictly sequential. Each slice must build and pass its tests before the next starts.

## Layout

```
Cargo.toml                workspace
crates/
  simpletickets-core/     library: model, config, store, rules, migration
  simpletickets/          binary: CLI + daemon + HTTP API
plugins/
  mcp/                    out-of-process MCP plugin (separate crate)
```

## Slices

### 1. Workspace scaffold
- Cargo workspace, two crates, `rustfmt` and `clippy` clean, CI workflow running build, test, clippy.
- Size: ~100 lines.

### 2. Ticket model and frontmatter
- Parse and write YAML frontmatter, preserving the body and unknown keys byte-for-byte.
- Fields: `id`, `title`, `status`, `priority`, `due`, `tags`, `parent`, `blocked_by`, `related`, extra keys.
- ID generation: sequential with random fallback on collision.
- Ticket marker detection.
- Size: ~400 lines.

### 3. Config
- Config file in the default tickets folder: roots, auto-scan folders, parent/sub-ticket folders, status-to-category mapping (todo, in-progress, done, cancelled), tag logging.
- Validation with clear errors.
- Depends on: 2. Size: ~300 lines.

### 4. Store and doctor
- Scan roots into an in-memory index keyed by `id`; path is only a location.
- Derived data: reverse links, blocked state, parent status.
- Doctor report: duplicate IDs, malformed frontmatter (file and line), missing link targets, cycles, orphans. Unparseable files are skipped and never rewritten.
- Depends on: 2, 3. Size: ~500 lines.

### 5. Safe writes
- Frontmatter-only patching, temp file + rename, compare-and-swap on content hash, skip recently changed files.
- Depends on: 2. Size: ~250 lines.

### 6. Rules
- Idempotent reconcilers over full state: unblock on done, parent in progress when a sub-ticket starts, parent status derived from sub-tickets, cancelled unblocks dependents (configurable).
- Dry-run mode and an audit log of automatic changes.
- Depends on: 4, 5. Size: ~450 lines.

### 7. CLI
- `create`, `list` (filters), `show`, `set-status`, `doctor`, `rescan`, templates for new tickets.
- Depends on: 4, 5, 6. Size: ~400 lines.

### 8. Daemon and HTTP API
- Background process: startup full scan, file watcher, periodic scan, event stream.
- Localhost-only HTTP API with token: list parents, list/filter tickets, create/update, rescan, doctor, get/change config, trigger migration.
- Depends on: 6, 7. Size: ~500 lines.

### 9. Migration
- On config change: dry-run plan, then journaled, resumable apply. IDs never change; links stay valid.
- Depends on: 5, 8. Size: ~450 lines.

### 10. Update check
- Periodically query the GitHub releases of the repository and notify only; no auto-install.
- Depends on: 8. Size: ~150 lines.

### 11. MCP plugin
- Separate process talking to the local API: list, show, create, update, next-ticket (filter + order + first item).
- v0.1 is done when this works end to end.
- Depends on: 8. Size: ~400 lines.

### 12. Distribution
- Installer script for macOS (launchd) that asks which addons to include, Dockerfile with polling fallback for bind mounts.
- Depends on: 8, 11. Size: ~300 lines.

## Open questions

- Event-driven rules versus rules after each periodic scan (slice 6 runs reconcilers after every scan; revisit if too costly).
- Event stream contract and API versioning (slice 8 starts at `/v1`).
- Plugin discovery and supervision (the MCP plugin is started by its client; supervision comes later).
- Linux and Windows packaging after the macOS installer.

## Testing checklist

- Unit tests for parsing, round-trip fidelity, ID generation, rules and cycle detection.
- Filesystem tests in temporary directories for scanning, safe writes, crash during write, migration resume.
- API tests: token required, localhost bind, create/update/filter.
- Every slice: `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
