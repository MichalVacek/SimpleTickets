# Plugins

Everything beyond ticket management is a plugin. A plugin is a separate process that talks to the daemon over the [HTTP API and event stream](api.md); the core has no in-process hooks. This document describes how a plugin is described, found, enabled and run.

## Manifest

A plugin is a folder named after the plugin, holding a `plugin.yml`.

The manifest is YAML for the same reason the config is: the project already reads YAML, hand-editing is common, and comments are allowed. TOML would need a second parser for a file with one level of nesting, and JSON has no comments.

```yaml
name: mcp
version: 0.1.0
description: MCP server so a client can list, create and update tickets
api_version: 1
kind: stdio-client
command: simpletickets-mcp
capabilities:
  - tickets.read
  - tickets.write
install:
  package: simpletickets-mcp
  binary: simpletickets-mcp
```

| Key | Required | Meaning |
|---|---|---|
| `name` | yes | Lower-case letters, digits, `-` and `_`. Must equal the folder name |
| `version` | yes | The plugin's own version, free text without spaces |
| `description` | no | One line for listings |
| `api_version` | yes | The API versions the plugin works with: `1` (exactly 1), `1-2` (inclusive) or `1-` (1 or newer). The daemon serves version 1; a plugin whose range excludes it is not started |
| `kind` | yes | `stdio-client` or `service`, see below |
| `command` | yes | The program to run. A bare name is looked for in the plugin folder, then next to the `simpletickets` executable, then on the `PATH`. A path is used as written; a relative one is relative to the plugin folder |
| `args` | no | Arguments, with the placeholders below |
| `working_dir` | no | Working directory, relative to the plugin folder (no `..`). Default: the plugin folder |
| `env` | no | Names of environment variables the plugin needs. A service is started only when the daemon's own environment has each of them set and non-empty. Names starting with `SIMPLETICKETS_` are set by the daemon and cannot be listed |
| `token` | no | How the token is delivered: `file` (default) or `env`, see [Token handling](#token-handling) |
| `health` | no | Services only: `url` (http or https, must answer `2xx`), `interval_seconds` (default 10), `timeout_seconds` (default 3) |
| `capabilities` | no | What the plugin intends to do, as lower-case words such as `tickets.read` or `events`. See [Capabilities](#capabilities) |
| `install` | no | `package` and `binary`: how `scripts/install.sh` builds the plugin from this repository. A manifest with an `install` block is offered as an addon |

An unknown key is an error, so a typo is reported instead of ignored.

### Placeholders

`args` and `health.url` may contain `${tickets_dir}` (the tickets folder), `${api_url}` (for example `http://127.0.0.1:7878`), `${token_file}` (path of the token file) and `${plugin_dir}`. Any other `${...}` is an error.

### Kinds

- `stdio-client`: started by an external client that speaks to it over stdin and stdout, such as an MCP host. The daemon lists it and the config can enable it, but the daemon never runs it. The client's own configuration launches `command`; the manifest tells installers and tools what the plugin is.
- `service`: a long-running process the daemon starts, supervises and stops.

### Token handling

The API token never appears on a command line, where other users' `ps` could read it. The daemon always provides the token's file path in `SIMPLETICKETS_TOKEN_FILE`; with `token: env` it also provides the token itself in `SIMPLETICKETS_TOKEN`. A manifest whose `args` contain `--token` or `--token=...` is rejected. A client-started plugin reads the same variables, or the `--token-file` option of the plugin, and finds the token file at `<tickets folder>/.simpletickets/token`.

### Capabilities

Capabilities are declarations, shown in listings so a user can see what a plugin means to do before enabling it. They are not enforced: a plugin that runs as the user with the API token can do anything the API allows. Treat enabling a plugin like installing a program.

## Discovery

The daemon and the `plugins` commands look in two places, in this order:

1. `<tickets folder>/.simpletickets/plugins/<name>/plugin.yml`: plugins for this tickets folder only.
2. The per-user plugins folder: `$SIMPLETICKETS_PLUGINS_DIR`, else `$XDG_CONFIG_HOME/simpletickets/plugins` or `~/.config/simpletickets/plugins`, and on Windows `%APPDATA%\simpletickets\plugins`.

A name found in both places resolves to the first. A folder without a `plugin.yml` is ignored; a folder whose manifest is invalid, or whose `name` differs from the folder name, is listed with the error and cannot be enabled. `scripts/install.sh` copies the manifests of the addons it installs into the per-user folder (`--plugins-dir` changes it).

## Enabling

Plugins are off until listed in `simpletickets.yml`:

```yaml
plugins:
  enabled:
    - mcp
```

`simpletickets plugins enable NAME` and `disable NAME` edit that block and leave the rest of the file as written. Names that are enabled but not installed are reported by `plugins list` and `plugins status`.

## Command line

| Command | Does |
|---|---|
| `simpletickets plugins list [--json]` | Every plugin found, with version, kind, enabled or disabled, source (`tickets` or `user`) and, for a broken manifest, the problem |
| `simpletickets plugins enable NAME` | Adds the plugin to `plugins.enabled`; refuses an unknown name or an invalid manifest |
| `simpletickets plugins disable NAME` | Removes it |
| `simpletickets plugins status [--url URL] [--json]` | Checks every enabled plugin: manifest valid, API version supported, required environment variables set, command found. Exits non-zero when one is not ready. When a daemon answers at `--url` (default `$SIMPLETICKETS_URL` or `http://127.0.0.1:7878`) it also shows the daemon's view: state, pid, restarts, crashes |

## Service plugins

The daemon starts every enabled `service` plugin once the API is listening, and stops them first when it shuts down. Plugins are found when the plugin is started; a changed manifest takes effect the next time the plugin is enabled or the daemon starts.

### Starting

Before starting, the daemon checks that the manifest is valid, its `api_version` includes the daemon's, every name in `env` is set, and the command exists. A plugin that fails a check is not started: its state is `failed`, the reason is in `error`, and a `plugin.failed` event is published. Such a plugin is checked again when the enabled list changes.

The process is started with a cleared environment, to keep the daemon's own variables (and any secrets in them) away from plugins. It receives:

- a small base set: `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `TMPDIR`, `TMP`, `TEMP`, `TZ`, `SHELL` and the Windows system variables (`SYSTEMROOT`, `SYSTEMDRIVE`, `WINDIR`, `COMSPEC`, `PATHEXT`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `PROGRAMDATA`), when set;
- the variables named in the manifest's `env`;
- the daemon's connection info:

| Variable | Value |
|---|---|
| `SIMPLETICKETS_URL` | API address, for example `http://127.0.0.1:7878` |
| `SIMPLETICKETS_TOKEN_FILE` | Path of the token file; the token is read from it |
| `SIMPLETICKETS_TOKEN` | The token itself, only with `token: env` |
| `SIMPLETICKETS_DIR` | The tickets folder |
| `SIMPLETICKETS_PLUGIN_NAME`, `SIMPLETICKETS_PLUGIN_DIR` | The plugin's name and folder |
| `SIMPLETICKETS_ACTOR` | `plugin:<name>`, the value to send as [`X-SimpleTickets-Actor`](api.md#actors) |
| `SIMPLETICKETS_API_VERSION` | The API version the daemon serves |

Standard input is closed. Because `${` introduces a placeholder in `args` and `health.url`, shell variable syntax like `${VAR}` cannot be used there; `$VAR` is fine.

### Output

Each line the plugin writes to standard output or standard error goes to the daemon log at `info` level, prefixed with `[plugin:<name>]`; standard error lines carry `(stderr)` after the prefix. Lines longer than 8 KiB are cut. The token is never logged.

### Crashes and restarts

A service is expected to keep running, so any exit the daemon did not ask for, with any exit code, is a crash.

- After a crash the daemon waits and starts it again. The wait is 1 second after the first crash, doubling with each crash inside the window up to 30 seconds.
- Five crashes within 60 seconds mark the plugin `failed`: it is not restarted again until it is disabled and enabled, or the daemon restarts.
- Failing to start the process at all counts as a crash.

### Stopping

When a plugin is disabled, or the daemon shuts down (`SIGINT`, `SIGTERM`, Ctrl-C), the daemon asks the process to exit and waits up to 5 seconds before killing it. On Unix the request is `SIGTERM`; on Windows there is no equivalent for a console-less child, so the process is terminated at once. Plugins stop before the API stops accepting requests, so they can finish their last calls. Only the plugin's own process is signalled, not processes it started.

### Health checks

With `health.url` set, the daemon sends a GET every `interval_seconds` while the plugin runs. A `2xx` answer within `timeout_seconds` is healthy. The result is the `healthy` field of the status (`null` until the first check); a failing check is logged at `warn`. A health check never restarts the process: a plugin that hangs but stays alive reports `healthy: false`.

### Status and events

`GET /v1/plugins` and `simpletickets plugins status` report each plugin's state:

| State | Meaning |
|---|---|
| `disabled` | Not in `plugins.enabled` |
| `client` | Enabled `stdio-client` plugin; its client runs it |
| `pending` | Enabled service the supervisor has not looked at yet |
| `starting`, `running` | |
| `backoff` | Crashed, waiting to restart |
| `failed` | Could not be started, or crashed too often |
| `stopped` | Stopped by the supervisor and about to disappear from the list |

The events `plugin.started`, `plugin.stopped`, `plugin.crashed` and `plugin.failed` are published on the [event stream](api.md#event-types).

## Building workflow automation

Workflow automation ("when X happens, do Y") is not part of the core and the core has no rule language. A workflow plugin is an ordinary plugin built from the pieces the core already offers. The core's own rules (an unblocked ticket, a parent that follows its sub-tickets) are reconciled after every scan; a workflow plugin reacts to the same facts after the fact.

There is deliberately no pre-write hook: nothing can veto or rewrite a change before it is made, because the files are also edited by people, editors and git, and the daemon only ever sees the result. A plugin that needs to veto something does it afterwards, by writing a correcting change under its own actor, which the audit log then shows.

### The pieces

| Need | Use |
|---|---|
| Learn that something happened | The [event stream](api.md#events) by long polling (`GET /v1/events?since=N&wait=20`), or a [webhook](api.md#webhooks) for a receiver that is not a child of the daemon |
| Read the current state | `GET /v1/tickets`, `GET /v1/tickets/{id}`, `GET /v1/search` |
| Act | `POST /v1/tickets`, `PATCH /v1/tickets/{id}` |
| Be identified | `X-SimpleTickets-Actor`, recorded in the [audit log and in event payloads](api.md#actors) |
| Act once | `Idempotency-Key`, see [Idempotency](api.md#idempotency) |
| Explain afterwards | `GET /v1/audit?actor=plugin:<name>` |

### Rules of thumb

- Events are at-least-once and not durable. Handlers must be idempotent: derive an idempotency key from the intended effect (`follow-up:<ticket id>`), not from the event's `seq`, so a replayed event, a retry and a restart all collapse into one effect.
- Keep a cursor (`next` of the last processed page) and send back the `stream` of the page. On a `gap` (the daemon restarted, or the plugin fell behind the backlog) take `next`, reload the state you care about through the list routes, and continue. A plugin that only reacts to new changes may simply continue from `next`.
- Start from the whole retained backlog (call without `since`) when missing a recent event would matter; with idempotent effects, reprocessing is harmless.
- Check the event's `source` and `actor` to avoid loops: an event caused by your own write carries your actor, and one caused by a rule has `source` `rule`. A plugin that reacts to its own output must also end the chain by content (the example skips tickets that it created itself).
- Write only through the API. A plugin that edits ticket files directly loses the compare-and-swap safety, the audit entry and the actor.
- Webhooks suit a plugin that runs elsewhere or that is not a child of the daemon; a `service` plugin started by the daemon can simply long poll, which needs no secret and no listening port.

### Example: follow-up tickets

[`examples/plugins/follow-up`](../examples/plugins/follow-up) is a complete `service` plugin in about 200 lines of Rust with no dependencies beyond `serde_json`. When a ticket's status changes to one in the `done` category (`ticket.status_changed` with `to_category` `done`), it creates a ticket titled `Follow up: <title>`, tagged `follow-up` and related to the finished ticket:

```
POST /v1/tickets
X-SimpleTickets-Actor: plugin:follow-up
Idempotency-Key: follow-up:ST-1

{"title": "Follow up: Ship release", "related": ["ST-1"], "tags": ["follow-up"]}
```

It reads its connection info from the environment the daemon provides (`SIMPLETICKETS_URL`, `SIMPLETICKETS_TOKEN_FILE`, `SIMPLETICKETS_ACTOR`), long polls the events, processes the retained backlog on start, and relies on the key to ignore repeats. A ticket that is reopened and finished again, a replayed event and a plugin restart all produce no second follow-up (the key names the ticket, and the daemon answers the repeat from its stored answer for 24 hours; `422` on a changed title is read as "already handled"). Finishing a follow-up does not create a follow-up of the follow-up.

To try it:

```
cargo build --release -p simpletickets-follow-up
mkdir -p <tickets folder>/.simpletickets/plugins/follow-up
cp examples/plugins/follow-up/plugin.yml <tickets folder>/.simpletickets/plugins/follow-up/
cp target/release/simpletickets-follow-up <tickets folder>/.simpletickets/plugins/follow-up/
simpletickets --dir <tickets folder> plugins enable follow-up
simpletickets --dir <tickets folder> daemon
```

Then finish a ticket (`simpletickets set-status ST-1 done`; the daemon picks the change up on its next scan or file event) and list the tickets. `simpletickets plugins status` shows the plugin running, and `GET /v1/audit?actor=plugin:follow-up` shows what it did. The integration test `examples/plugins/follow-up/tests/follow_up.rs` runs exactly this against a real daemon.
