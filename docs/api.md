# HTTP API

The daemon serves a JSON API under `/v1` on `127.0.0.1` (default port 7878). This document is the contract for clients and plugins: routes, request and response shapes, errors and the event stream. See the [README](../README.md) for running the daemon.

## Conventions

- Requests and responses are JSON (`Content-Type: application/json`). Request bodies are limited to 1 MiB.
- Request bodies are strict: an unknown field is a `400`.
- Timestamps in events are UTC, `YYYY-MM-DDTHH:MM:SSZ`.
- Ticket IDs are opaque strings such as `ST-12`. Paths in responses are absolute file system paths in the host's own notation.

## Versioning and compatibility

The API version is an integer, currently `1`, and it is the number in the path: every route lives under `/v1`. The same version covers the event stream and the webhook payloads.

- `GET /v1/health` returns `api_version` (integer) and `version` (the daemon's release, for example `0.1.0`). `api_version` changes only when a new path prefix is introduced; `version` changes with every release.
- Every response, errors included, carries the header `X-SimpleTickets-Api-Version: 1`, so a client can check it without calling `/v1/health`.
- Within `v1` changes are additive only: new routes, new optional request fields, new response fields, new event types, new event payload fields, new query parameters. Clients must ignore response and payload fields and event types they do not know. Existing fields, routes, status codes for existing situations and the event envelope keep their meaning and shape.
- A breaking change (removing or renaming a field, route or event type, changing a type or meaning, tightening validation of accepted input) is made only in a new version served under `/v2`. When `/v2` ships, `/v1` stays available and unchanged for at least 12 months, and both versions run side by side in the same daemon. The deprecation is announced in the release notes before `/v1` is removed.
- A bug fix that makes the daemon match this document is not a breaking change.
- Routes that do not exist, including any `/v2` route on a daemon that does not serve it, return a JSON `404` (`{"error": "no such route: GET /v2/health"}`) once the token is valid. Without a valid token every route answers `401`, so a client can detect an unsupported version only with a token.

## Authentication

Every route requires `Authorization: Bearer <token>`. The token is generated on first start and stored in `.simpletickets/token` in the tickets folder (owner-only permissions; `--token-file` overrides the location). Requests whose `Host` header is not `127.0.0.1`, `localhost` or `::1` are refused with `403`, which blocks DNS-rebinding from a browser.

A missing or wrong token is `401` with `WWW-Authenticate: Bearer`, also for routes that do not exist.

## Errors

Every error status has the body

```json
{"error": "human readable message"}
```

The message is for people and can change; clients branch on the status code.

| Status | Meaning |
|---|---|
| 400 | Invalid JSON, unknown field, invalid value (for example an unknown status), invalid query parameter |
| 401 | Missing or invalid token |
| 403 | `Host` is not a localhost name |
| 404 | Unknown route, unknown ticket, nothing to resume |
| 405 | Route exists, method does not |
| 409 | Conflict: a write lost a race with a file change, or a config change needs a migration (the body then also has `plan`), or a migration is already running |
| 413 | Body over 1 MiB |
| 500 | The daemon could not write a file or state |

## Ticket object

Returned by the ticket routes (`body` only by `GET /v1/tickets/{id}` and after writes).

| Field | Type | Meaning |
|---|---|---|
| `id`, `title`, `status` | string | Core fields |
| `category`, `effective_category` | string or null | `todo`, `in-progress`, `done`, `cancelled`; the first is the ticket's own status category, the second accounts for derived parent status |
| `priority`, `assignee`, `due`, `parent` | string or null | `due` is a date, `YYYY-MM-DD` |
| `tags`, `blocked_by`, `related` | array of string | |
| `blocked` | bool | Derived: an open blocker exists |
| `open_blockers`, `blocks`, `children`, `related_by` | array of string | Derived relations |
| `extra` | object | Frontmatter keys the core does not own |
| `path` | string | File location |
| `frozen` | bool | The ticket sits in an offline root and is not touched |
| `ready_for_review` | bool | Derived parent state |
| `body` | string or null | Markdown after the frontmatter (detail only) |

Search results add `score` (number), `match_field` (`title` or `body`) and `snippet`.

## Routes

`{id}` is a ticket ID. "List" responses are bare JSON arrays.

| Route | Request | Response |
|---|---|---|
| `GET /v1/health` | | `{api_version, version, status, tickets, frozen, rules_enabled, offline_roots: [{root, message}], update}`; `status` is `ok` or `degraded` while a root is offline; `update` is `null` or `{latest, url}` |
| `GET /v1/parents` | | list of ticket objects |
| `GET /v1/tickets` | query: `status`, `category`, `priority`, `assignee`, `tag`, `parent`, `title`, `blocked`, `due_by`, `field.<key>`, `limit`; with `q` it behaves as search | list of ticket objects |
| `GET /v1/search` | query: `q` (required) plus the list filters and `limit` | list of search results, best first |
| `GET /v1/tickets/{id}` | | ticket object with `body`; `404` for an unknown ID |
| `POST /v1/tickets` | `{title, status?, priority?, assignee?, due?, tags?, parent?, blocked_by?, related?, template?, body?, role?}`; `role` is `parent` or `sub` | `201`, ticket object with `body` |
| `PATCH /v1/tickets/{id}` (also `PUT`) | any of `title, status, priority, assignee, due, parent, tags, blocked_by, related`; `null` clears `priority`, `assignee`, `due`, `parent` | `200`, ticket object with `body` |
| `POST /v1/tickets/{id}/reassign-id` | optional `{path}` naming the copy | ticket object plus `old_id` |
| `POST /v1/rescan` | | `{reason, tickets, problems, rules, rules_enabled}`; scans and runs the rules |
| `GET /v1/doctor` | | `{clean, issues: [string], tombstones, warnings}` |
| `GET /v1/tombstones` | | list of `{id, path, first_seen, referenced_by}` |
| `DELETE /v1/tombstones/{id}` | | `{forgotten: id}` |
| `GET /v1/config` | | `{yaml, config}`: the file text (or null) and the parsed configuration |
| `PUT /v1/config` | `{yaml, migrate?}` | `{config, warnings}` or, after a migration, `{config, journal}`; `409` with `plan` when a migration is needed and `migrate` is not true |
| `POST /v1/migration` | `{yaml, dry_run?}`; `dry_run` defaults to true | `{dry_run, plan}` or `{dry_run: false, journal}` |
| `GET /v1/migration` | | `{journal}`, null when there never was one |
| `POST /v1/migration/resume` | | `{journal}`; `404` when nothing is unfinished |
| `GET /v1/events` | query: `since`, `wait`, `stream` | an [event page](#polling) |
| `GET /v1/audit` | query: `limit` (default 100) | list of `{ts, rule, id, field, from, to, reason, level}`, oldest first |
| `GET /v1/templates` | | list of template names |

## Events

The daemon publishes what happens as events. A client follows them by long polling `GET /v1/events`; this is how plugins stay in sync without rescanning.

### Envelope

Every event has exactly these four keys; they never change within `v1`.

```json
{"seq": 41, "time": "2026-10-26T15:58:42Z", "type": "ticket.status_changed", "data": {}}
```

| Key | Type | Meaning |
|---|---|---|
| `seq` | integer | Position in the stream. Starts at 1 for each daemon run and grows by exactly 1 per event, with no holes |
| `time` | string | When the event was published |
| `type` | string | Dotted name, see below |
| `data` | object | Payload of that type; fields are only ever added |

### Polling

`GET /v1/events?since=N&wait=S&stream=ID` returns the events with `seq` greater than `N`, in order. With `wait` (seconds, at most 30) and no event available the request waits for the first one and returns an empty list on timeout. `since` defaults to 0.

```json
{
  "events": [],
  "next": 41,
  "oldest": 12,
  "latest": 41,
  "gap": false,
  "gap_reason": null,
  "stream": "18a3c0f2b7e-1f4d"
}
```

| Field | Meaning |
|---|---|
| `events` | The events after `since`, oldest first |
| `next` | The `seq` to pass as `since` on the next call; equal to `latest` |
| `oldest` | `seq` of the oldest event still retained; `latest + 1` when none is retained |
| `latest` | `seq` of the newest event published, 0 before the first |
| `gap` | True when the client cannot continue from `since` without having missed events |
| `gap_reason` | `null`, or why: `backlog_exceeded`, `ahead_of_stream`, `stream_changed` |
| `stream` | Identifies this daemon run. Pass it back as `stream` to detect restarts |

The daemon keeps the most recent events only: 1000 by default, set with `simpletickets daemon --event-backlog N`. A client that polls often enough never loses anything.

### Gap detection and resync

A page is a gap when:

- `backlog_exceeded`: `since + 1 < oldest`, so events between `since` and `oldest` were dropped. The page still carries the retained events (from `oldest`).
- `ahead_of_stream`: `since > latest`. The client holds a number from before a daemon restart, which resets `seq`. The page has no events.
- `stream_changed`: the `stream` the client passed is not this run's. The page has no events.

A gap page returns immediately, without waiting. When `gap` is true the client must resync:

1. Remember `next` (`latest`) from the gap page; this is the new cursor.
2. Reload its state through the list routes (`GET /v1/tickets`, `GET /v1/doctor`, ...).
3. Continue polling with `since` set to the remembered cursor and `stream` set to the page's `stream`.

Taking the cursor before reloading state guarantees nothing is missed: events published during the reload are delivered again, and clients treat events as idempotent notifications (see below).

A client that has no cursor yet calls `GET /v1/events` with no parameters and uses `next`; it also gets the retained backlog.

### Ordering and delivery guarantees

- In order: events are numbered under one lock, so `seq` order is publication order, and a page is always in `seq` order.
- No holes: within one `stream`, if the client has seen `seq` N it can ask for N+1 and either gets it, or `gap` tells it the event is gone. Nothing is skipped silently.
- At least once: a client that passes the `next` of its last processed page gets every later event. If it crashes between processing and saving its cursor it sees some events again, so handlers should be idempotent (use `seq` to drop repeats).
- Not durable: the backlog lives in memory. A daemon restart starts a new `stream` at `seq` 1; clients resync with the rule above.
- Ticket events describe the change the daemon observed between two index states. Several edits between two scans can collapse into one `ticket.updated`; a ticket created and deleted between two scans produces no event.

### Event types

Payload fields are listed per type. `source` is `api` (a write made through this API), `rule` (a status written by an automatic rule) or `scan` (anything found by a scan: a command line write, a hand edit, a sync tool). It is best effort: a hand edit that lands in the same instant as an API write is reported as `api`.

Ticket events are produced by comparing the ticket index before and after every scan, so they fire the same way for API writes, rule writes, command line writes and hand edits. A ticket is identified by its ID: the same ID at another path is a move.

| Type | Payload |
|---|---|
| `ticket.created` | `id`, `title`, `status`, `path`, `source` |
| `ticket.updated` | `id`, `title`, `path`, `changes`, `body_changed`, `source`; `changes` maps each changed field (`title`, `status`, `priority`, `assignee`, `due`, `tags`, `parent`, `blocked_by`, `related`, `extra`) to `{"from": ..., "to": ...}`, with `null` for an absent value; `body_changed` is true when the Markdown body differs |
| `ticket.status_changed` | `id`, `title`, `path`, `from`, `to`, `from_category`, `to_category`, `source`; sent after the `ticket.updated` of the same change |
| `ticket.moved` | `id`, `title`, `path` (the new one), `from`, `to`, `source` |
| `ticket.deleted` | `id`, `title`, `path` (the last known), `source`. A deleted ID stays known as a tombstone while it is referenced |
| `ticket.id_reassigned` | `old_id`, `new_id`, `path` |
| `rule.applied` | `rule`, `id`, `path`, `from`, `to`, `reason`, `level` (`info` or `warn`); a rule rewrote a ticket's status. It is followed by the matching `ticket.updated` and `ticket.status_changed` with `source` `rule` |
| `warning.raised` | `rule`, `id`, `message`, `kind` (`condition` or `event`), `first_seen` |
| `tombstone.forgotten` | `id` |
| `scan` | `reason`, `tickets`, `problems`; sent at the end of every scan. `reason` is `startup`, `periodic`, `watch`, `api` (the rescan route) or the write that triggered it (`create`, `update`, `reassign-id`, `forget`, `config`, `migration`) |
| `config.changed` | `reason` |
| `config.invalid` | `message` |
| `migration.suggested` | `actions`, `conflicts`; the config file was edited by hand and needs a migration |
| `migration.started` | `steps`, `conflicts` |
| `migration.finished` | `state`, `started`, `steps`, `done`, `skipped`, `failed`, `pending` |
| `migration.error` | `message` |
| `root.offline` | `root`, `message`, `frozen` (tickets frozen) |
| `root.online` | `root` |
| `lease.lost` | `message`; another daemon took over the folder, rules are off |
| `lease.acquired` | no fields |
| `state.error` | `message`; a state file next to the tickets could not be updated |
| `update.available` | `current`, `latest`, `url` |

Within one scan the order is: `config.*`, `migration.suggested`, `root.*`, the ticket events found in the files as they were scanned (`source` `api` or `scan`), `rule.applied` and the ticket events caused by rules (`source` `rule`), `warning.raised`, `state.error`, then `scan`.

New event types and new payload fields can appear in any `v1` release; clients ignore types and fields they do not know.
