# Synchronization

Synchronization to other trackers is an addon, built against Jira. The core knows nothing about it: it reserves no frontmatter keys and has no `external:` map. Three kinds of process take part, and each is a plugin in the sense of [plugins.md](plugins.md):

```
 daemon  --events, tickets API-->  sync plugin  --connector protocol (stdio)-->  connector  -->  remote tracker
                                   (owns all sync                                (Jira, archive, ...)
                                    state and config)
```

- The **sync plugin** (`simpletickets-sync`, a supervised `service`) reads tickets and events from the daemon's API, decides what each target should hold, and calls connectors. It owns the mapping between tickets and remote items, the last synced state, the cursors and the conflict records, all in its own folder under `.simpletickets/sync/`. It does not know which tracker sits behind a target, so the number of targets does not matter.
- A **connector** is a separate program that knows one remote. The sync plugin starts it as a child process and talks to it in line-delimited JSON over standard input and output ([protocol](#connector-protocol)). Connectors are `stdio-client` plugins: the daemon never runs them.
- Two connectors ship: [archive](#archive-connector), the reference implementation and test double, and [Jira](#jira-connector).

Status: the sync plugin, the protocol and the archive connector are exercised end to end by the test suite against a live in-process daemon. The Jira connector is tested only against an in-process mock server that imitates the endpoints it uses. **No real Jira site was available, so behaviour against live Jira is unverified.**

## Quick start

```
cargo build --release -p simpletickets-sync -p simpletickets-connector-archive
simpletickets plugins enable sync                  # after installing the manifest, see below
```

`scripts/install.sh --addons sync,connector-archive,connector-jira` builds and installs them and copies their manifests. Write `<tickets folder>/.simpletickets/sync/config.yml`:

```yaml
targets:
  backup:
    connector:
      command: simpletickets-connector-archive
    options:
      dir: archive            # relative to the tickets folder
    unmapped: pass            # send statuses and priorities as they are
```

Start the daemon; it starts and supervises the sync plugin. Check on it with:

```
simpletickets-sync status
simpletickets-sync dry-run
```

Everything under `.simpletickets/` is excluded from git by the folder's own `.gitignore`, the configuration included. Keep a copy elsewhere if you want it versioned.

Secrets never go into the configuration. A connector reads its credentials from environment variables, and the configuration lists only the **names** (`connector.env`). The daemon starts plugins with a cleared environment, so a variable reaches the sync plugin only when its name is also listed in the plugin manifest's `env` (see [plugins.md](plugins.md#manifest)); copy `plugins/sync/plugin.yml` into `.simpletickets/plugins/sync/` and add the names there. The sync plugin then passes exactly the names in `connector.env` on to the connector.

## Configuration

`.simpletickets/sync/config.yml` (`--config FILE` overrides). Unknown keys are an error.

```yaml
reconcile_interval_seconds: 300     # full reconcile; catches what events missed
pull_interval_seconds: 60           # polling of remotes, targets that pull only
targets:
  jira:                             # target name: lower-case letters, digits, - and _
    connector:
      command: simpletickets-connector-jira   # bare names are found next to the sync binary, then on PATH
      args: []
      env: [JIRA_SITE_URL, JIRA_EMAIL, JIRA_API_TOKEN]   # names only
      timeout_seconds: 120          # a call that takes longer kills the connector
    options:                        # handed to the connector's `configure`; see its section
      project: ST
    direction: both                 # push (default) | both
    on_conflict: local-wins         # local-wins (default) | remote-wins | manual
    on_delete: close                # close (default) | ignore
    unmapped: skip                  # skip (default) | pass
    filter:
      tags: [sync]                  # only tickets with one of these tags
      categories: [todo, in-progress]
    map:
      status:   {todo: To Do, doing: In Progress}   # local status name -> remote status
      category: {done: Done, cancelled: Won't Do}   # category -> remote status, for statuses not listed above
      priority: {high: High, low: Low}
      assignee: {ann: 5b10a2844c20165700ede21g}     # local assignee -> remote user id
    pull:
      fields: [status]              # any of status, priority, assignee, title, tags, due
      status: {Blocked: doing}      # remote -> local, overrides the inverse of map.status
```

Mapping rules:

- Keys are matched case-insensitively. A status is mapped by its name first, then by its category (`effective_category`, so a parent's derived state counts).
- `unmapped: skip` sends nothing for a value with no mapping, so the remote keeps whatever it has. `pass` sends the local value as it is. An absent value (no assignee, no priority) is sent as a clear only where the field has a mapping or `unmapped` is `pass`.
- `title`, `tags` and `due` are always sent as they are; the body is sent as the description.
- Reading back inverts the maps. A remote value that no local value maps to, or that several do (without a `pull` entry to decide), is not written: it is reported as a warning and the ticket is left alone.

## State model

All state is in `.simpletickets/sync/`, written through temporary files and renames. Nothing is ever written into a ticket file or its frontmatter.

| File | Contents |
|---|---|
| `<target>.json` | Per target: the map of ticket id to `ref` (the remote item), `url`, `hash` (digest of what was last pushed), `base` (the remote's values at the last sync), `links` (remote refs of blockers linked), `phase`, `error`, `attempts`, `next_attempt`, `warnings`; the connector's `cursor`; the `conflicts` list; `last_pass`, `last_error` |
| `config.yml` | The configuration above |
| `lock` | Held while a sync process runs; stale after 60 seconds without a heartbeat |
| `request.json` | A `push-now` queued for the running service |

`phase` is `synced`, `error`, `conflict`, `remote-missing`, `closed` or `deleted`.

The `hash` covers everything a push would send: the ticket snapshot (without its file path), the mapped values and the resolved link references. Equal hash and phase `synced` mean nothing to do, so a restart, a replayed event or a second reconcile pushes nothing. Changing a mapping changes the hashes and re-pushes the affected tickets.

Deleting `<target>.json` loses the map. A connector that can find its own items again (Jira does, through a label; archive does through the file name) will adopt the existing items instead of duplicating them.

## Direction

- `push` (default): local changes go to the remote. The remote is never read, and remote edits are overwritten the next time the ticket's payload changes.
- `both`: the plugin also polls the connector (`fetch`) every `pull_interval_seconds` and on every reconcile, and writes remote changes of the `pull.fields` back with `PATCH /v1/tickets/{id}`. Such writes are made as the actor `plugin:sync` (`X-SimpleTickets-Actor`, from `SIMPLETICKETS_ACTOR`), so the audit log (`GET /v1/audit?actor=plugin:sync`) and the event payloads show them. The connector must offer `fetch`; otherwise the target is reported as failed.

A write-back never touches the body: the API's update has no body field, and the remote description is never read. A remote change that arrives with the local value unchanged since the last sync is applied; the new value becomes the base, so it is not pushed back.

## Conflicts

For every pulled field the plugin compares three values: the **base** (the remote's value at the last sync), the **local** value (mapped) and the **remote** value.

| Local vs base | Remote vs base | Result |
|---|---|---|
| unchanged | changed | the remote value is written to the ticket |
| changed | unchanged | the local value is pushed (the normal case) |
| changed | changed, equal | converged; the base moves, nothing is written |
| changed | changed, different | **conflict**, settled by `on_conflict` |

`on_conflict`:

- `local-wins` (default): the local value is pushed over the remote one.
- `remote-wins`: the remote value is written to the ticket.
- `manual`: nothing is written on either side; the ticket is skipped until someone decides.

Whatever the policy, a conflict is recorded in the state and shown by `list-conflicts` (resolved ones with `--all`, newest 100 kept), so nothing is overwritten silently. Settle a waiting conflict with `simpletickets-sync push-now ST-4 --prefer local` or `--prefer remote`. The ticket body is never overwritten by a pull, under any policy.

A remote item that no longer exists (`not_found`) is also a conflict. Under `local-wins` it is recreated; otherwise the ticket is left in phase `remote-missing` until `push-now --prefer local` (recreate) is run.

## Deletion

Syncing never deletes anything remotely; the protocol has no delete operation. A ticket that is gone is confirmed with `GET /v1/tickets/{id}` (`404`), because a list can be a snapshot taken during a scan. Then:

- `on_delete: close` (default): the connector's `close` operation is called (Jira moves the issue to its `close_status`, archive marks the file `"closed": true`). The ticket's state is kept as phase `closed`, a tombstone, so it is not closed twice. If the same id comes back, it is pushed again like any ticket (Jira transitions the issue back according to the status mapping).
- `on_delete: ignore`: the remote item is left as it is; the state records phase `deleted`.

Tickets in an offline root are frozen: the daemon keeps them in the list with `frozen: true`, and sync skips them until the root returns.

## Idempotency

- Pushes are idempotent by design: the connector receives the remote `ref` when one exists, and a create is found again by the connector when it can (see each connector). A crash between a remote create and saving the `ref` therefore does not produce a duplicate on the next run.
- Write-backs use `Idempotency-Key: sync:<target>:<ticket id>:<digest of the remote version and the change>`, so a retry after a crash replays the stored answer instead of writing twice, while a remote value that flips back later (a new version) is a new key.
- Every request carries `X-SimpleTickets-Actor`. The service ignores events that carry its own actor.
- A transient failure (`rate_limited`, `unavailable`) sets `next_attempt` with a doubling backoff (5 s, 10 s, ... capped at 15 minutes, or the remote's `Retry-After` when longer). A permanent refusal records the digest of the refused payload and is not repeated until the ticket changes or `push-now --force` is used. A refused credential (`auth`) stops the target for the rest of the pass and shows in `status`.

## Event stream, gaps and reconcile

The service follows the [event stream](api.md#events) by long polling, applying the documented rules:

1. At start it takes the stream's cursor, then runs a full reconcile, so changes during the reconcile are seen again as events.
2. For `ticket.created`, `ticket.updated`, `ticket.status_changed`, `ticket.deleted` and `ticket.id_reassigned` it syncs the named tickets (events with its own actor are skipped).
3. On `gap` (`backlog_exceeded`, `ahead_of_stream`, `stream_changed`; a daemon restart is one) it takes the page's `next` and runs a full reconcile.
4. Every `reconcile_interval_seconds` it runs a full reconcile regardless, which also catches anything that arrived while the stream was unreachable. A reconcile lists all tickets and reads each synced ticket (the list has no bodies), so it costs one request per ticket; it is meant for thousands of tickets, not hundreds of thousands.

Tickets are pushed parents and blockers first. A sub-ticket whose parent has no remote item yet waits for the parent. Blockers that are not yet synced are linked later: the resolved references are part of the hash, so the dependent is pushed again once the blocker exists.

## Command line

```
simpletickets-sync [--dir DIR] [--config FILE] [--url URL] [--token-file FILE] COMMAND
```

The folder defaults to `$SIMPLETICKETS_DIR` or the current folder; the daemon's address and token come from `--url` and `--token-file`, else from `SIMPLETICKETS_URL`, `SIMPLETICKETS_TOKEN_FILE` and the token file of the folder.

| Command | Does |
|---|---|
| `run` | The service. The daemon starts it with the `sync` plugin manifest; it can also be run by hand |
| `status [--json] [--check]` | Per target: synced count, conflicts, errors, warnings, closed items, last pass and last error, and whether a service is running. `--check` asks each connector to verify the remote and the credentials. Exits 1 when there is a conflict or an error |
| `dry-run [--target T] [--json] [ID...]` | Lists what a pass would create, update, close, pull or find in conflict. Calls only read-only connector operations (`fetch` when pulling), writes no state, no cursor, no ticket and nothing remote. A dry-run of a pull is shown against the current state |
| `push-now [--target T] [--force] [--prefer local\|remote] [ID...]` | Syncs now. When the service is running the request is queued and picked up within seconds; otherwise it runs in this process. `--force` pushes even when nothing changed |
| `list-conflicts [--json] [--all]` | Conflicts waiting for a decision; `--all` adds resolved ones |

Exit codes: `0`, `1` (a problem was reported), `2` (usage or configuration error).

## Connector protocol

Version 1. The transport is the connector's standard input and output; each message is one JSON object on one line (UTF-8, no embedded newlines). Standard error is free text for logs and is shown in the sync plugin's own log; it must never contain credentials. The sync plugin sends one request at a time and waits for its response. Closing standard input ends the connector, as does `shutdown`.

Request and response:

```json
{"v": 1, "id": 7, "op": "push", "params": {}}
{"v": 1, "id": 7, "ok": true,  "result": {}}
{"v": 1, "id": 7, "ok": false, "error": {"code": "rate_limited", "message": "...", "retryable": true, "retry_after_ms": 30000}}
```

`v` is the protocol version, `id` an unsigned integer chosen by the caller and echoed. A request with another `v` is answered with `unsupported`. Unknown request fields and unknown `result` fields are ignored, so versions grow additively; unknown operations are answered with `unsupported`. A response that does not arrive within the target's `timeout_seconds` kills the connector, which is started again on the next pass.

### Versioning and capabilities

`describe` is called first. Its result lists the protocol versions the connector speaks and what it can do; `configure` then returns the capabilities that apply to the given options (archive in Markdown format, for instance, has no `fetch`).

```json
{"name": "jira", "version": "0.1.0", "protocol": [1], "capabilities": ["close", "fetch", "check"]}
```

`configure` and `push` are mandatory. `close`, `fetch` and `check` are optional capabilities; the sync plugin only calls what was announced, and refuses `direction: both` for a connector without `fetch`.

### Operations

| Operation | Params | Result |
|---|---|---|
| `describe` | none | `name`, `version`, `protocol`, `capabilities` |
| `configure` | `target` (the target's name), `options` (the config's `options`), `tickets_dir` | `capabilities`. Validates the options and reads credentials from the environment; may fail with `invalid_request` or `auth`. Called at the start of every connector process |
| `push` | `ticket` (snapshot), `fields` (mapped values), `links`, `ref` | `ref`, `url`, `outcome` (`created`, `updated`, `unchanged`), `fields`, `warnings` |
| `fetch` | `cursor` (opaque, from the last result, or null), `refs` (the remote items this target tracks) | `items`, `cursor`, `more` |
| `close` | `ref`, `ticket_id` | empty |
| `check` | none | `message`. Verifies the remote and the credentials without changing anything |
| `shutdown` | none | empty; the connector exits afterwards |

**`push`.** `ticket` is the ticket as the API returns it, without its file path: `id`, `title`, `status`, `category`, `effective_category`, `priority`, `assignee`, `due`, `tags`, `parent`, `blocked_by`, `related`, `extra`, `body`. `fields` holds the values after mapping, keyed by local field name (`title`, `status`, `priority`, `assignee`, `tags`, `due`): a missing key means leave the remote alone, `null` means clear it. `links` is `{parent_ref, blocked_by_refs, previous_blocked_by_refs}`: the remote refs of the parent and blockers (only those already synced), and the blockers linked by the previous push, so a connector removes only links it made. `ref` is the remote item for this ticket, or `null` to create (a connector may first look for an item it created earlier). The result's `fields` are the remote's actual values after the push (a status that could not be reached is reported as it is); they become the base for conflict detection. A problem that does not prevent the push (a status with no transition, a link target that does not exist) goes into `warnings`, not into an error. A push must be safe to repeat with the same input.

**`fetch`.** Returns items `{ref, version, fields}` for remote items that changed since `cursor`; `fields` uses the same local field names, with remote values (`status`, `priority`, `assignee`, `title`, `tags`, `due`; whatever the connector knows). The cursor is opaque to the sync plugin. A connector may return more than changed, because the sync plugin compares every value with its base and ignores equal ones; it must not return less. `version` identifies this state of the remote item (an `updated` timestamp, a digest) and is used to key write-backs. `more: true` asks for another call with the returned cursor.

**`close`.** Marks the remote item as finished or withdrawn in the way the remote allows. It never deletes. `not_found` means it is already gone.

### Errors

| Code | Meaning | Retried |
|---|---|---|
| `invalid_request` | Malformed request or bad options | no |
| `unsupported` | Unknown operation or protocol version | no |
| `auth` | Credentials missing or refused | no; stops the target for the pass |
| `not_found` | The remote item does not exist | no; handled as a conflict |
| `rate_limited` | The remote asks to slow down; `retry_after_ms` when known | yes, after the backoff |
| `unavailable` | Network or server trouble | yes, after the backoff |
| `rejected` | The remote refused this content | no, until the ticket changes |
| `internal` | A bug or an unexpected state | no |

`retryable` says the same as the table and wins when a connector sets it differently. A connector does its own short retries inside one call (the Jira connector retries 429 and 5xx with backoff); when it gives up it reports `rate_limited` or `unavailable` and the sync plugin schedules the next attempt. A connector must not retry a call that may already have had an effect and cannot be repeated safely, such as an issue create after a network failure: it reports `unavailable` and relies on finding the item next time.

### Writing a connector

Implement `simpletickets_connector_protocol::Connector` (`describe` and `handle`) and call `serve(&mut connector, stdin().lock(), &mut stdout().lock())`; `plugins/connector-archive` is the smallest complete example. The crate `simpletickets-connector-protocol` also has the client used by the sync plugin (`Process`, `Transport`). A connector in another language needs nothing but the line format above. Install its manifest (`kind: stdio-client`) like the others, and give its `command` in the target's `connector.command`.

## Archive connector

`simpletickets-connector-archive` writes one file per ticket into a folder: no network, no credentials. It is the reference implementation of the protocol and the test double of the sync plugin.

| Option | Meaning |
|---|---|
| `dir` | Required. The folder; a relative path is relative to the tickets folder. Created on the first push |
| `format` | `json` (default) or `markdown` |

- `json`: `<id>.json` with `ref`, `closed`, `ticket` (the snapshot), `fields` (the mapped values) and `links`. The `ref` is the file stem (the id with characters other than letters, digits, `-`, `_`, `.` replaced by `_`). Editing `fields` by hand is a remote change as far as the sync plugin is concerned, so `direction: both` works against an archive; `fetch` returns every file, with a digest of the `fields` as its version.
- `markdown`: `<id>.md` with a heading, the fields as a list and the body. Write-only: no `fetch`.
- `close` sets `"closed": true` (JSON) or prefixes `[closed]` (Markdown). Files are never deleted.
- A push with a `ref` whose file is gone is `not_found`; a push without one creates or overwrites the file of that id, so a lost state map causes no duplicates.

## Jira connector

`simpletickets-connector-jira` talks to Jira Cloud over the REST API v3. It uses `POST /rest/api/3/issue`, `PUT` and `GET /rest/api/3/issue/{key}`, `GET` and `POST /rest/api/3/issue/{key}/transitions`, `POST /rest/api/3/issueLink`, `DELETE /rest/api/3/issueLink/{id}`, `POST /rest/api/3/search/jql`, `GET /rest/api/3/myself` and `GET /rest/api/3/project/{key}`.

**Unverified against a live Jira.** The connector is tested against an in-process mock server written from the public API documentation. Field names, workflow behaviour, permission errors and rate-limit details on a real site may differ. Try it on a scratch project first, with `dry-run` and `status --check`.

### Credentials

Read from the environment of the connector process; they are never printed, logged, written to disk or put into an error message, and the sync plugin never sees their values (it only passes the variables on).

| Variable | Value |
|---|---|
| `JIRA_SITE_URL` | `https://<site>.atlassian.net`. Must be `https`; `http` is accepted only for localhost |
| `JIRA_EMAIL` | The account's email |
| `JIRA_API_TOKEN` | An [API token](https://id.atlassian.com/manage-profile/security/api-tokens) of that account (basic authentication) |

List the three names under `connector.env` of the target, and in the `env` of the sync manifest (see [Quick start](#quick-start)).

### Options

| Option | Default | Meaning |
|---|---|---|
| `project` | required | Project key |
| `issue_type` | `Task` | Type of issues made for tickets without a parent |
| `subtask_type` | `Sub-task` | Type for tickets with a parent that is synced |
| `close_status` | `Done` | Status `close` moves an issue to |
| `link_type` | `Blocks` | Issue link type for `blocked_by` |
| `description` | `adf` | `adf` converts Markdown; `text` keeps the body as plain paragraphs |
| `identity_label` | `true` | Label every issue with `simpletickets-<id>` so a lost state map can find it again |
| `retry` | `{attempts: 5, base_ms: 500, max_ms: 30000}` | Retries inside one call |

### Mapping

| Ticket | Jira |
|---|---|
| `title` | `summary` (cut to 255 characters) |
| body | `description`, see below (cut to 30 000 characters, with a note) |
| `tags` | `labels` (characters other than letters, digits and `-_.:/` become `_`, since labels cannot hold spaces). The labels are owned by SimpleTickets: they are set to the tags plus the identity label on every push, and a pull returns them without it |
| `priority` | `priority.name`, as mapped. An absent priority is not cleared (Jira has no empty priority) |
| `assignee` | `assignee.accountId`, from `map.assignee`; an unassigned ticket clears the assignee when the field is mapped |
| `due` | `duedate`; none clears it |
| `status` | the transition whose target status (or name) equals the mapped status, from the issue's current status. No such transition is a warning and the issue keeps its status |
| `parent` | `parent: {key}` with issue type `subtask_type`, when the parent is synced first |
| `blocked_by` | `Blocks` links, the blocked ticket's issue on the inward side ("is blocked by"). Only links to issues the previous push linked are removed; links made by hand stay |

A create is preceded by a search for the identity label, so an issue created before a lost state map is adopted. The create request itself is not retried after a server error or network failure (it may have happened); the next push finds it through the label. Reads, updates, transitions and links are retried.

Pull (`fetch`) returns `status`, `priority` (name), `assignee` (account id), `title`, `tags` and `due`. Without a cursor it reads the tracked issues by key; with one it searches the project for issues updated since the cursor's minute (JQL has minute precision and reads in the caller's time zone, which is also how `updated` is rendered, so the digits are reused as they are) and keeps those the sync plugin tracks.

### Description rendering

Jira's description field takes Atlassian Document Format. With `description: adf` the Markdown body is converted for this subset:

- headings `#` to `######`, paragraphs (single line breaks join into one paragraph, as in Markdown), `-`, `*` and `+` bullet lists, numbered lists, `>` block quotes, fenced code blocks (with their language), horizontal rules;
- inline `` `code` ``, `**bold**`, `*italic*`, `_italic_` (only at word boundaries, so `snake_case` is safe), `[text](url)` for `http`, `https` and `mailto` links, and backslash escapes.

Everything else is kept as plain text, not dropped: tables, images, HTML, footnotes and task-list markers appear as their source text, nested lists are flattened to one level, and an unmatched marker stays literal. Descriptions beyond 30 000 characters are cut and end with `[truncated by SimpleTickets]`. `description: text` keeps blank-line separated paragraphs with line breaks and interprets no Markdown. The description is overwritten from the ticket body whenever the ticket's payload changes; remote edits to it are not read and are lost.

### Rate limits and errors

`429` is retried up to `retry.attempts` times, waiting for `Retry-After` (capped at `max_ms`) or doubling from `base_ms`; after that the call fails as `rate_limited` with `retry_after_ms`, and the sync plugin backs off per ticket. `5xx` and network failures are retried for repeatable requests and reported as `unavailable`. `401` and `403` are `auth`, `404` is `not_found`, other `4xx` are `rejected` with Jira's own messages (`errorMessages` and per-field `errors`).

### Known limits

- No Epic hierarchy: parents are ordinary issues of `issue_type`, children are `subtask_type` sub-tasks. Company-managed and team-managed projects differ in which parent/child combinations they allow.
- Custom fields, components, versions, sprints and comments are not synced. Time zones other than the caller's are not handled in the pull query.
- Assignees need Jira account ids in `map.assignee`; names are not resolved.
- The workflow must offer a direct transition to each mapped status; multi-step paths are not walked.
