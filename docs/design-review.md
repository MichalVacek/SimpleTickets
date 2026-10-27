# Design review

Defaults below are recommendations, not decisions. Decisions live in [spec.md](spec.md).

## Questions and recommended defaults

1. Files are the only source of truth; any index is a rebuildable cache. (Default: yes, SQLite cache rebuilt on startup.)
2. May the daemon write into files that humans, editors and git also own? (Default: only frontmatter keys it owns, atomic writes, never the body.)
3. Parent specs in git repos: what happens on branch switch, rebase, revert? (Default: do not store parent in-progress/done in the spec; derive from sub-tickets.)
4. Main writer: human, script, or API? (Default: assume scripts and humans both write files directly.)
5. Rules fire on events or by reconciliation? (Default: idempotent reconcilers over full state each scan.)
6. Is "blocked" stored or derived? (Default: derived, never written.)
7. What counts as "work starts" and "done"; do cancelled tickets unblock? (Default: status categories todo / in-progress / done / cancelled.)
8. Relations stored on one side or both? (Default: child stores `parent`, blocked stores `blocked_by`, reverse derived.)
9. Hierarchy exactly two levels, one parent? (Default: yes; role comes from the link, not the folder.)
10. Ticket identity and link target: ID, filename or path? (Default: immutable frontmatter `id`.)
11. ID generation across roots, machines, offline writers? (Default: prefix + short random suffix.)
12. How does a file become a ticket? (Default: explicit frontmatter marker.)
13. Migration: move files or re-index; dry run and rollback? (Default: plan, dry-run diff, journaled apply, IDs never change.)
14. What can plugins do in v0.1? (Default: out-of-process, API plus event stream, no rule-engine hooks.)
15. Fixed fields or free-form frontmatter, filterable? (Default: fixed core fields plus free-form extras, exact-match filter.)
16. Docker: bind-mount file watching on Mac/Windows? (Default: native is primary in v0.1.)
17. Auto-update installs or only notifies? (Default: notify only until releases are signed.)
18. Reserve sync fields in the file format now? (Superseded: the spec decides that core reserves no sync fields; sync state lives in the sync plugin.)
19. What does this do that Backlog.md, git-bug, Obsidian Tasks/Kanban or GitHub Issues do not? Open.

## Edge cases to handle

1. Ticket file deleted by hand: tombstone, treat blocker as unresolved, broken-links report, never auto-unblock.
2. Ticket renamed or moved by hand: identity is `id`, path is only location.
3. File copied, duplicate ID: keep older, mark copy conflicted, offer reassign.
4. Two blockers, one finishes: blocked while any blocker is open.
5. Blocking cycle: reject via API, flag in doctor report for hand edits.
6. Done ticket reopened: not-started dependents re-block; in-progress ones get a warning.
7. All sub-tickets done: parent goes to "ready for review", not done (configurable).
8. Sub-ticket starts under a done/cancelled parent: reopen parent to in progress with warning, or refuse via config.
9. Parent changes or disappears: recompute old and new parents each scan; list orphans.
10. Root unavailable: mark offline, freeze tickets, run no rules. Detection is per root: an unreadable subfolder inside a readable root is not offline, so its tickets are protected by tombstones only, not frozen.
11. Malformed frontmatter: skip, report file and line, never rewrite.
12. Human and daemon edit the same file: compare-and-swap on content hash, patch frontmatter only, skip recently changed files.
13. Crash mid-write or mid-migration: temp file + rename, migration journal, resume or roll back on startup.
14. Two machines create tickets offline, folder syncs later: non-colliding IDs, one rule-running daemon per root via lease/lock.
15. Config change during migration, or locked/read-only files: one migration at a time, retry list instead of abort.

## Capabilities

| Capability | Where | Reason |
|---|---|---|
| CLI (create, list, show, set status, doctor) | core | Scripts, debugging. |
| Lint/doctor | core | Hand-edited files drift. |
| Full-text search | core | Index exists already. |
| Templates | core | Create needs default body and frontmatter. |
| Labels/tags | core | Trivial field, heavily filtered. |
| Assignee | core | Tickets need an owner field. |
| Audit log of daemon changes | core | Explain automatic status changes. |
| Event stream / webhooks | core | Plugins need notifications. |
| Dry-run for rules and migration | core | Trust before rewriting files. |
| Board/kanban view | plugin | Part of UI addon. |
| Git/PR linking | plugin | Fits specs-next-to-code. |
| Import from GitHub Issues / Jira / Backlog.md | plugin | Adoption; shares code with sync. |
| Saved queries/views | plugin | Built on filter API. |
| Notifications | plugin | Depends on event stream. |
| Due dates | plugin | Needs reminders/views. |
| Comments | skip | Body plus git history. |
| Multi-user permissions | skip | File system is the permission model. |
| Time tracking / estimates | skip | Out of scope. |

## Top risks to v0.1

1. Daemon rewrites files it does not own: lost edits, noisy diffs, branch-dependent state.
2. Identity and link format undecided before files exist; later change migrates every user's files.
3. Scope too big: daemon, API, plugin system, three-OS installer, Docker, auto-update, migration before the core value is proven. Suggested v0.1: file format, library, CLI, reconciling rules, optional daemon with API on one OS.
4. Event-triggered rules over periodic scans give missed or doubled transitions.
5. Security: unauthenticated local API writing files, plus a background updater pulling from a public repo. Bind to localhost with a token; notify-only updates.
