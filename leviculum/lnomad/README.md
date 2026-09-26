# lnomad

A terminal browser for NomadNet micron (`.mu`) pages. It connects to a running
`lnsd`/`rnsd` shared instance, fetches a page over the Reticulum
request/response path, renders the micron markup to ANSI, and lets you follow
links interactively.

## Usage

```
lnomad [url] [options]
```

A URL names the node's destination address and the request path on it:

```
<address>                     open the node's /page/index.mu
<address>:/page/about.mu      open a specific page
<address>:/page/x.mu`a=1|b=2  carry preset query fields (var_a, var_b)
<address>:/file/manual.pdf    download a file instead of rendering a page
:/page/about.mu              a local URL: the node of the page in view
```

`<address>` is 32 hex characters (the 16-byte truncated destination hash).

Leaving the address out makes the URL local — local to the page in view — but
the `:` stays. A bare request path (`/page/about.mu`, no colon) names no node
and is not a URL at all. It is rejected here exactly as the reference NomadNet
browser rejects it, and the error names the missing `:`.

The URL is optional. Started without one on a terminal, `lnomad` opens its
start screen with the places panel showing — your bookmarks, and the nodes
discovery has turned up. Without a terminal to browse in (piped, redirected, or
`--print`) a URL is required, since there would be nothing to print.

## Discovering nodes

There is no discovery mode and no flag to turn it on: discovery runs
continuously from startup, for the whole life of the browser. Every NomadNet
node announces the `nomadnetwork.node` destination, so its announces can be
recognised and its destination hash and display name collected without knowing
anything about it in advance.

Announces are folded into the places panel (`d`) whether or not a page is
loading, so the list keeps filling while you read, scroll, or have a panel open.
Until the first one arrives the panel says so rather than claiming there are
none. The registry is a bounded FIFO of the 500 most recent nodes (re-announces
update in place; once full, the oldest-seen node is evicted), and is held in RAM
only — a node you want to keep belongs in the bookmarks.

### Options

- `--instance <name>`  shared-instance name (overrides the config file's)
- `--config <dir>`     Reticulum config directory (default: the platform default)
- `--no-color`         disable ANSI colour
- `--theme <t>`        colour theme: `auto` (default), `light`, or `dark`. `auto`
  detects the terminal background (OSC 11, with a `COLORFGBG` fallback) and picks
  the matching theme, defaulting to dark when it cannot tell. Ignored with
  `--print` or non-tty output.
- `--color <d>`        colour depth: `auto` (default), `truecolor`, or `256`.
  `auto` emits 24-bit true colour when `COLORTERM` is `truecolor`/`24bit` and
  otherwise downgrades every colour to the nearest xterm-256 palette index, so
  terminals without true-colour support still render sensibly. `--no-color`
  overrides this and drops to monochrome.
- `--width <n>`        render width (default: detected terminal width, else 80)
- `--timeout <s>`      per-request timeout in seconds (default 30)
- `--output <path>`    where a `/file/` download is saved: an existing directory
  (or a path spelled with a trailing `/`) receives the file under its own name,
  any other path names the exact file to write. Without it the file lands in the
  current working directory
- `--print`            fetch, render and print once, then exit

When stdout is not a terminal, `lnomad` prints once and exits even without
`--print`, so piping and redirection never block on the browser.

## Interactive keys

On a terminal, `lnomad` opens a full-screen browser: a one-row top-bar (the page
title, a `·`, and the URL, with a right-aligned status cluster: a bookmark
star, an identity key marker while identifying, a cache bolt, and the hop count
to the node), the scrollable page, and a footer. The footer is a strip of
clickable button-hints where a keybinding and a button are the same thing: the
navigation trio (`Alt-← back`, `Alt-→ forward`, `R reload`) first, then the
current mode's actions. Each button's key reads bright and bold, its label muted;
press the key or click the button. On a narrow terminal the footer drops the
lowest-priority buttons and, if still too tight, collapses the rest to their
keys. Links carry no `[N]` marker and there is no link legend; a link is set
apart by its underline and colour, and is reached by focus, hint or click:

- `j` / `k`, `↓` / `↑`, `Ctrl-n` / `Ctrl-p`  scroll a line; `Ctrl-f` / `Ctrl-b`,
  `Ctrl-v`, `PageDown` / `PageUp`  page down / up; `Ctrl-d` / `Ctrl-u`  half a
  page; `g` / `G`, `Home` / `End`  top / bottom; the mouse wheel scrolls too
- `Tab` / `Shift-Tab`  move the focus cursor across links AND form fields, in
  document order (auto-scrolls)
- `Enter`     follow the focused link
- form fields, when focused: type to edit a text field, `Space` to toggle a
  checkbox / radio, `Esc` to leave field editing; a click focuses a field too
- `f`         hint mode: type the label shown over a link, a form field or a
  top-bar control (or a link's text). A hinted link is followed, a hinted text
  field is focused for editing, and a hinted checkbox or radio is toggled or
  selected outright
- `/`         in-page search: type a query, `Enter` highlights every match and
  jumps to the first; `n` / `N` cycle to the next / previous match, `Esc` clears
- click       follow a link, activate a top-bar control, or press a footer button
- `:`         enter a URL
- `m`         bookmark the current page (a click on the top-bar star does the same)
- `y`         copy the focused link's or the current page's URL
- `d`         open the places panel (bookmarks and discovered nodes)
- `i`         identify to this node, or go back to anonymous
- `R` / `Ctrl-R` / `F5`  reload the page (always refetches, bypassing the cache)
- `t`         toggle the light / dark theme (correct a wrong auto-detection)
- `Alt-←` / `Alt-→`  back / forward
- mouse back / forward side buttons  back / forward
- `Esc` / `Ctrl-g`  cancel a load
- `?`         toggle the help overlay (grouped keybindings; `Esc`, `?` or a
  click anywhere closes it). When an overlay (help or places) is taller than the
  terminal the same scroll keys and the wheel scroll the focused window, with a
  scrollbar on its border
- `q` / `Ctrl-c`  quit

The focused or hovered link's target appears in a small floating field at the
bottom-left of the content, just above the footer, so it never covers the
clickable button-hints. Local URLs (`:/page/x.mu`) resolve against the page
currently in view; a followed link carries its preset (`f=v`) fields as `var_*`
request variables.

Recently viewed pages are held in an in-RAM cache (the last 50 distinct pages),
so revisiting one, including stepping back and forward through history, renders
instantly from memory with your last scroll position restored. The cache is
transparent: `R` always refetches (bypassing it and refreshing the stored copy),
and non-idempotent form submits are never cached. A shown page served from the
cache carries a subtle `⚡` bolt in the top-bar status cluster.

The places panel (`d`) takes the same up/down motions as the page scroll applied
to its selection: `j` / `k`, `Ctrl-n` / `Ctrl-p`, arrows step a row; `Ctrl-f` /
`Ctrl-b` and `Ctrl-d` / `Ctrl-u` jump several; `g` / `G`, `Home` / `End` go to
the first / last entry. When the list is taller than the terminal the view
follows the selection, with a scrollbar on the border; the wheel scrolls that
view without moving the selection, so every entry stays reachable.

`Enter` opens the selection and a single click on a row opens it straight away;
hovering a row highlights it. A bookmark row that is selected or hovered carries
a right-aligned `×` marker: `x` or `Delete` removes the selected bookmark, and a
click on the marker removes that row's. A deletion is announced in a toast and
can be taken back with `u`, which restores the bookmark to its old position (one
level of undo — a second deletion overwrites the stash). A click inside the panel
but off an entry does nothing, a click outside closes it, and `Esc` / `d` close
it too.

Bookmarks persist in `${XDG_CONFIG_HOME:-~/.config}/lnomad/bookmarks.toml`; the
discovered nodes beside them are the in-RAM registry described above and are
gone at exit.

### Form fields and submitting

A page can carry input fields (`` `<name`> `` text, `` `<?|name`Label> ``
checkbox, `` `<^|name`Label> `` radio). They render as input boxes, initialised
from their prefill; `Tab` reaches them and, once focused, they edit in place. A
link that references a field by name (e.g. `` `[Submit`:/page/s.mu`name] ``, or
`*` for every field) is a submit: following it collects the current values of the
referenced fields and sends them as NomadNet expects, each under a `field_<name>`
request variable, alongside any `var_*` presets. This interoperates with a real
NomadNet node, whose page handler reads the same `field_*` / `var_*` variables.

A link whose target is an external URL (an `http`, `https` or `mailto` scheme)
is not fetched in-mesh: it is handed to the platform default handler (`xdg-open`
on Linux). Any other scheme (`file`, `javascript`, custom schemes) is refused
and reported in a transient toast, since a page comes from an untrusted node and
an arbitrary URI must never reach a system handler.

A link to an LXMF address (`` `[Write me`lxmf@<hash>] ``, or the long
`lxmf.delivery@<hash>` form) is an address, not a page. `lnomad` is a page
browser with no message composer, so following such a link copies the
destination hash to the clipboard and says so in a toast, ready to paste into a
client that can send.

### Identifying to a node

By default `lnomad` browses anonymously: nothing about you reaches the node
serving the page. `i` (or the footer's `identify` button) opts the current node
in, and `i` again goes back to anonymous. While identifying, the top-bar cluster
carries a 🔑 marker with the first eight hex characters of your own fingerprint,
so the state with consequences is the visible one.

The decision is per node and persists across runs in
`${XDG_CONFIG_HOME:-~/.config}/lnomad/identify.toml`. The identity revealed is
`lnomad`'s own, kept as `identity` in the same directory and minted on first
use — not the shared instance's transport identity. Identifying binds to the
link, not to the request, so toggling it reloads the page over a fresh link; a
node that receives the identify sees a `remote_identity` on your requests and
can attribute what you submit to you, which is what NomadNet's
`identify_on_connect` does.

### Tables

A micron `` `t `` table renders with box-drawing separators, columns sized to
their visible width. A cell is inline micron, so a style, a colour or a link
inside one renders (and a link in a cell is followable like any other); a
literal `|` written `\|` stays part of its cell.

### Pictures

Micron has no image construct, so a page can only link to a picture in its
node's file area. `lnomad` recognises such a link by its target — a `/file/`
path ending in `.png`, `.jpg`, `.jpeg` or `.gif` — and, when the link stands
alone on its line, fetches the file and draws it there. A link inside a
sentence stays a link: there is no rectangle to grow into without tearing the
text apart.

How it is drawn depends on what the terminal answered when asked at startup:

| terminal                             | what you get                          |
|--------------------------------------|---------------------------------------|
| Kitty, Ghostty, WezTerm, iTerm2       | the picture, via its graphics protocol |
| xterm, foot, Contour, mlterm          | the picture, via Sixel                 |
| anything else with colour             | Unicode half-blocks                    |
| `--no-color`, or a failed decode      | a line naming the file, format and size |

The last row is the point of the ladder: a reader on a terminal that can show
nothing still learns that a picture is there, what it is and how big it is, and
can still save or open it.

Half-blocks are sized differently from a graphics protocol, and deliberately: a
cell carries one pixel across and two down, so every extra column and row is
extra resolution. A picture there fills the page width and may run past the
bottom of the screen (up to two screenfuls) rather than being fitted into the
font-size box a graphics protocol would use — the same 300x300 portrait is
38x38 pixels the fitted way and 78x78 this way. It is still coarse; a terminal
with Sixel or the Kitty protocol is what makes a photograph a photograph.

A page's pictures are fetched **after** the page is on screen, one at a time,
never more than eight from any one page. Nothing waits for a picture, `Esc`
cancels the queue, and leaving the page drops it. Over LoRa this matters: a
20 kB thumbnail is roughly a minute of airtime at SF7 and ten or more at SF12,
so `--images off` leaves every image link an ordinary link and fetches nothing.

For the same reason a picture that has been paid for once is kept: fetched
payloads stay in an in-memory cache, keyed by the node and path they came from,
so stepping back to a page draws them again without a transfer. The cache is
bounded by bytes rather than by count — pictures differ in size by three orders
of magnitude, and "the last fifty" would say nothing about how much memory the
browser is holding. `--image-cache <megabytes>` sets the budget (default 10);
when it is exceeded, the pictures untouched longest are dropped one at a time
until it fits again, and a single picture larger than the whole budget is never
kept at all. `--image-cache 0` switches it off. Nothing is written to disk:
caching a stranger's pictures into a directory is the reader's decision to
make, not ours.

With a picture focused (`Tab`), `Enter` saves it to the download directory and
`o` opens it in the system image viewer. Neither fetches it a second time — the
bytes are already here.

### Downloads

A `/file/` target is downloaded rather than rendered. Following a `/file/` link
in the browser saves it under `$XDG_DOWNLOAD_DIR` (falling back to
`$HOME/Downloads`) and reports the name, size and directory in a toast; `Esc`
cancels a download in flight, just like a page load. Giving a `/file/` URL on
the command line downloads it directly, without ever opening the TUI, and prints
how many bytes were saved and where.

The filename comes from the server's Resource metadata when it sends one, else
from the URL's last path component. Either source is untrusted, so it is reduced
to a bare basename — a name like `../../etc/passwd` can never write outside the
target directory. An existing file is never overwritten silently: ` (1)`, ` (2)`
and so on are appended until the name is free. The one exception is a `--output`
that names an exact file, which opts into writing exactly there.

The two bottom surfaces split cleanly. The bottom-left floating field carries the
current pointer/page state (a focused/hovered link's target, or the loading
spinner and path during a fetch) and stays as long as it applies. Transient notes
(a fetch error, a refused link, "copied", "bookmarked", "cancelled") appear as an
auto-dismissing toast floated at the bottom-right of the content; a toast clears
after a few seconds or on the next key press. Neither covers the footer, which
always keeps its clickable button-hints.

## Anchors

A `#anchor` in a target (a followed link or the initial URL) is resolved
against the page's anchors and scrolled to on load; an unknown anchor falls back
to the top of the page with a toast note.
