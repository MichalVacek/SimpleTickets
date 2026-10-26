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
| `simpletickets plugins status [--json]` | Checks every enabled plugin: manifest valid, API version supported, required environment variables set, command found. Exits non-zero when one is not ready |
