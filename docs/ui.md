# UI addon

`simpletickets-ui` is a kanban board for the local API. It is a separate process: a small web server bound to `127.0.0.1` that serves a single-page app (plain HTML, CSS and JavaScript embedded in the binary, no build step, nothing loaded from the network) and relays the page's requests to the daemon.

## Running it

Start the daemon, then the board:

```
simpletickets daemon --dir ~/tickets
simpletickets-ui --dir ~/tickets --port 7979
```

It prints `SimpleTickets board: http://127.0.0.1:7979/`; open that address.

| Flag | Meaning |
|---|---|
| `--dir DIR` | Tickets folder; the token is read from `DIR/.simpletickets/token`. Default: `$SIMPLETICKETS_DIR`, else the current folder |
| `--port N` | Port of the board on `127.0.0.1`. Default `7979`; `0` picks a free port |
| `--url URL` | Daemon address. Default: `$SIMPLETICKETS_URL`, else `http://127.0.0.1:7878` |
| `--token-file PATH` | Token file, overriding `--dir` |

The plugin manifest `plugins/ui/plugin.yml` describes it as a `service` plugin. After `scripts/install.sh --addons ui`, or by copying the manifest into a plugins folder (see [plugins.md](plugins.md#discovery)), `simpletickets plugins enable ui` makes the daemon start the board on port 7979 with its own connection settings and restart it if it crashes. The port is fixed by the manifest; to use another, copy the manifest and change the `--port` argument and the health URL.

The token file is read again on every relayed request, so a replaced token is picked up without restarting the board.

## What the board shows

- Four columns by status category: to do, in progress, done, cancelled. A category with several configured statuses is split into one lane per status name; a category with none cannot receive cards.
- Cards show id, title, priority, due date (overdue is marked), assignee, tags, a blocked badge, the parent or the number of sub-tickets, and the number of rule warnings. A derived parent status places the card by its effective category.
- Dragging a card onto a lane sets its status with `PATCH /v1/tickets/{id}`. From the keyboard, focus a card and press Shift+Left or Shift+Right to move it to the neighbouring column, or open it and change the status in the form.
- Filters: assignee, tag, priority, blocked, and a text box that uses `GET /v1/search` (title and body). Filters combine.
- Clicking a card or pressing Enter opens the detail panel: fields, body (shown as plain text), links (parent, sub-tickets, blocked by, open blockers, blocks, related) that open the linked ticket, rule warnings from the doctor report, and an edit form. "New ticket" opens the create form.
- A banner shows when the daemon cannot be reached, when a root is offline (health `degraded`) and when an update is available.
- The page follows `GET /v1/events` by long polling. It takes the cursor first and reloads state through the list routes, and does the same whenever a page reports `gap` (see [api.md](api.md#gap-detection-and-resync)). Events are treated as notifications: any ticket event reloads the list, which is idempotent.
- Colours follow the system light or dark setting; the layout scrolls horizontally with snapping on narrow screens and the detail panel covers the screen.

## Security model

The browser never sees the API token. The page talks to the board server only, and the server adds `Authorization: Bearer <token>` when it calls the daemon.

- Bound to `127.0.0.1` only.
- `Host` must be `127.0.0.1`, `localhost` or `::1`, with an optional port, on every request (blocks DNS rebinding). Others get `403`.
- Every `/api/` request must carry the header `X-SimpleTickets-Session`. The value is 256 random bits generated when the board starts and written into the page it serves (a `<meta name="st-session">` tag). A page on another origin cannot read that page, so it cannot learn the value, and a custom header forces a CORS preflight that the board never answers.
- If an `Origin` header is present it must be exactly `http://` plus the request's own `Host`. State-changing requests (anything but `GET` and `HEAD`) must send one; browsers always do. `Sec-Fetch-Site: cross-site` and `same-site` are refused.
- Only the routes the board needs are relayed: `GET` of health, parents, tickets, ticket by id, search, events, doctor, config and templates; `POST` of tickets and rescan; `PATCH` of a ticket. Config changes, migrations, tombstones and ID reassignment are not reachable through the board. Ticket IDs in paths are limited to letters, digits, `.`, `_` and `-`.
- Responses carry `Content-Security-Policy` (no inline script or style, no external origins, no framing), `X-Frame-Options: DENY`, `nosniff` and `Cache-Control: no-store`. Ticket text is inserted with `textContent` only, never as HTML.
- Daemon failures are reported as `502` with a generic message that includes neither the token nor file paths.

A command line client of the board (curl, for example) must send the session header, and an `Origin` header for writes:

```
curl -s http://127.0.0.1:7979/ | grep st-session          # the session value
curl -s -H "X-SimpleTickets-Session: $S" http://127.0.0.1:7979/api/v1/tickets
```

## Limits

- The board loads all tickets with one list call and renders them all; it is meant for hundreds of tickets, not tens of thousands.
- Dragging needs a pointer; keyboard users use Shift+arrows or the form. Touch drag and drop depends on the browser.
- The board has been exercised through the HTTP layer only; see the test suite in `plugins/ui/tests`. The page itself has not been checked in a browser.
