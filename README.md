# mcpurge

Find every MCP server configured on a machine, see which ones are actually
running, stop them, and purge them — the config entry and, if you want, the
files on disk.

A single self-contained binary for macOS, Windows and Linux. No webview, no
Node toolchain.

```
                    ⌘ mcpurge  Edit  View  Window  Help   ← macOS: up there
┌────────────────────────────────────────────────────────────────┐
│ mcpurge  [Rescan]  42 servers · 6 running · 213 MB reclaimable │
│ Servers | Orphans (1) | Coverage      filter [all ▾] [search] │
├────────────────────────────────────────┬─ Detail ─────────────┤
│ 3 selected  [Stop 2 running…] [Remove 3…]        clear        │
│ ☑ ● rea        Kiro         pid 4242   │ rea                  │
│ ☑ ○ crawl4ai   Kiro         stopped    │ Kiro · user · stdio  │
│ ☑ ○ godot-ai   Kiro         disabled   │ npx -y               │
│ ☐ ● rea        Claude Code  pid 4242   │   rea-agents@6.7.0   │
│                                        │ Env: 1 var (hidden)  │
│                                        │ Files: 213 MB        │
│                                        ├──────────────────────┤
│                                        │ [Stop tree…][Remove…]│
├────────────────────────────────────────┴──────────────────────┤
│ Activity                                  open backups  clear │
└────────────────────────────────────────────────────────────────┘
```

## Why the design looks like this

Three findings drove the architecture. Each came from looking at a real machine
rather than from documentation.

**1. Published config paths are unreliable, so paths alone are not enough.**
Cline's own docs disagree with its code about where its CLI config lives.
Recent Claude Desktop builds no longer keep MCP servers in
`claude_desktop_config.json` at all. Cherry Studio has no config file —
its servers live in `redux-persist` state. A hardcoded path table would start
rotting the day it shipped.

So mcpurge works in two layers: a **data-driven registry** of ~34 clients that
says where to look first, and a **content sweep** that identifies agent config
by its *shape* rather than its filename — any object under `mcpServers`,
`servers`, `context_servers`, `mcp`, `mcp_servers` or `extensions` whose
members carry `command`, `args`, `url` or friends. The sweep finds clients
nobody has told it about, including ones that do not exist yet. The Coverage
tab reports anything MCP-shaped that the registry could not name, so gaps are
visible instead of silent.

**2. Killing one PID is not enough.** A config entry reading
`npx -y rea-agents@6.7.0 mcp` appears in the process table as
`npm exec rea-agents@6.7.0 mcp` with a `node .../rea mcp` child doing the real
work. Signal only the parent and the server keeps running. So every stop walks
the process subtree, signals children before parents (so a wrapper cannot
notice and respawn them), sends `SIGTERM` first, and escalates to `SIGKILL`
only for survivors. On Windows, where there is no `SIGTERM`, it goes straight
to the hard kill.

**3. Not everything near a server is the server's.** A real SolidCraft config
read:

```
solidcraft-cli mcp --allow-path ~/projects/a-firmware-project
```

An early build offered to delete that 1.4 GB path. It is a *project directory
the server was granted access to* — the user's work, not server files. Paths
named in a server's arguments are now classified `data path` and are never
deletable, alongside system paths, app bundles, and shared caches like
`~/.cache/uv` that back every uv tool on the machine. The detail pane marks
them `kept`.

## Safety model

- **Nothing is touched without a backup.** Every removal first copies the
  config file to `~/.mcpurge/backups/<timestamp>/` with an undo manifest.
  `Undo last removal` restores it.
- **Config edits are surgical.** JSON and JSONC are edited by splicing a byte
  range out of the original text, so comments, key order, formatting and
  sibling API tokens survive untouched. TOML goes through `toml_edit` for the
  same reason. YAML has no format-preserving editor available, so it is
  re-serialised and the UI says so before you commit.
- **Edits are atomic.** Files are written to a sibling temp file and renamed,
  so an interrupted write cannot leave a half-written config.
- **An edit that would produce invalid JSON is rejected** and the file is left
  as it was.
- **Files go to the trash**, not `unlink`. Permanent deletion exists but must
  be ticked, and never applies to protected paths.
- **Env values are masked** until you ask to see them. MCP configs are full of
  API tokens.
- **Every destructive action is confirmed** with the exact PIDs, files and
  config keys it will touch.

## Updating itself

mcpurge can replace itself with a newer release, under rules that match what it
does to everything else: nothing happens without being asked, and nothing is
accepted that cannot be verified.

- **Signed, or not installed.** Every release binary carries a detached
  [minisign](https://jedisct1.github.io/minisign/) signature made by the
  mcpurge release key, and the matching public key is compiled into the binary
  rather than fetched. The signature is checked in memory, before a byte is
  written anywhere executable. A download that does not verify is discarded.

  ```
  Release signing key  RWS19zuWF/iFDqf71bMLPIPVNIR8XHNawsUJNNAN1nMJvFbC8ySb3LdI
  Key ID               0E85F817963BF7B5
  ```

  The same key ID is in **Help > About mcpurge**, in
  `assets/release-signing-key.pub`, and in `SIGNING_KEY_ID` in `src/update.rs`.
  Releases can be checked by hand without running mcpurge at all:

  ```sh
  brew install minisign   # or: sudo apt install minisign
  minisign -V -H -P RWS19zuWF/iFDqf71bMLPIPVNIR8XHNawsUJNNAN1nMJvFbC8ySb3LdI \
           -m mcpurge-aarch64-apple-darwin
  ```

- **Ed25519, by format rather than by policy.** minisign has one algorithm, so
  there is no field in a signature to talk the verifier down to something
  weaker. This is why the updater brings in no cryptographic dependency tree
  worth speaking of: `minisign-verify` has no dependencies at all, and
  `cargo deny check advisories` passes with no exceptions. An earlier draft
  used OpenPGP, which meant compiling an RSA implementation — and carrying its
  two open timing advisories — to verify signatures that were never RSA.

- **Three signatures are checked, not one.** The Ed25519 signature over the
  download's BLAKE2b hash, a second one covering that signature together with
  the release's trusted comment, and the key ID matching the built-in key. The
  trusted comment names the version and file, so a valid signature cannot be
  moved onto a different download.

- **Checking is a toggle; installing never is.** **Help > Check for updates on
  launch** controls the one network request mcpurge makes. Turn it off and the
  app talks to nothing at all unless you pick **Check for updates…** yourself.
  Finding a release only opens a window that names the version and asks.

- **Inside `mcpurge.app` it updates the binary, not the bundle.** An update
  replaces `mcpurge.app/Contents/MacOS/mcpurge` with the signed per-architecture
  download, which keeps working - the replacement carries its own ad-hoc
  signature, which is what macOS enforces at launch - but it does leave the
  bundle's own seal stale, so `codesign --verify mcpurge.app` will complain
  afterwards and the binary is no longer universal. Re-download the zip if you
  want either of those back.

- **It will not overwrite a build.** Running from `target/release` during
  development is normal; replacing that binary would clobber your build output,
  so an install from a cargo target directory is declined with an explanation.

Releases are built and signed by `.github/workflows/release.yml` on a `v*` tag,
which refuses to publish if the tag and `Cargo.toml` disagree, and verifies each
signature against the public key committed here — with the same `-H` the app
insists on — before uploading anything. That verification step is where the
committed public key and the signing key are proved to be a pair.

## Install

Everything on the [latest release](https://github.com/TeeboBaggens/mcpurge/releases/latest)
ships with a detached minisign signature beside it, and `-H` below insists the
signature is over a hash of the file rather than the file itself, which is the
only form mcpurge's own verifier accepts:

```sh
brew install minisign   # or: sudo apt install minisign
```

### macOS

Download **`mcpurge-macos-universal.app.zip`** — one app that runs on both
Apple Silicon and Intel. Verify it, unzip it, and drag `mcpurge.app` to
Applications:

```sh
minisign -V -H -P RWS19zuWF/iFDqf71bMLPIPVNIR8XHNawsUJNNAN1nMJvFbC8ySb3LdI \
         -m mcpurge-macos-universal.app.zip
ditto -x -k mcpurge-macos-universal.app.zip .
xattr -dr com.apple.quarantine mcpurge.app
```

That last line is the one to understand rather than paste. mcpurge is signed
with its own release key, not with an Apple Developer ID, and it is not
notarized — so macOS marks the download as quarantined and refuses to open it,
with a dialog that offers only *Done* or *Move to Trash*. Clearing the
quarantine flag is how you say you have checked it yourself, which the
`minisign` line above is you doing exactly that: a signature made by the key in
`assets/release-signing-key.pub` is a stronger statement about where the file
came from than notarization is.

If you would rather not touch `xattr`, open it once and macOS will refuse, then
go to **System Settings > Privacy & Security**, where an **Open Anyway** button
appears for about an hour after that refusal. Control-clicking the app and
choosing *Open* no longer works; Apple removed that route in macOS 15.

The bare `mcpurge-aarch64-apple-darwin` and `mcpurge-x86_64-apple-darwin`
binaries are still published, and are the ones to put on a `PATH` if you want
`mcpurge --list` and `mcpurge --json` in a shell or a script. The `.app` is the
same program; the bundle is what gives it a Dock icon, an application menu, and
privacy prompts that name mcpurge instead of your terminal.

### Linux and Windows

Download the binary for your platform, check its signature, and put it
somewhere on your `PATH`:

```sh
minisign -V -H -P RWS19zuWF/iFDqf71bMLPIPVNIR8XHNawsUJNNAN1nMJvFbC8ySb3LdI \
         -m mcpurge-x86_64-unknown-linux-gnu
chmod +x mcpurge-x86_64-unknown-linux-gnu
```

On Windows the download is `mcpurge-x86_64-pc-windows-msvc.exe`, which needs no
`chmod`. It is not signed with an Authenticode certificate either, so SmartScreen
will ask once.

After that mcpurge keeps itself up to date — see [Updating itself](#updating-itself).

### From source

Requires Rust stable (pinned in `rust-toolchain.toml`).

```sh
git clone https://github.com/TeeboBaggens/mcpurge
cd mcpurge
cargo build --release
./target/release/mcpurge
```

On Linux, the GUI needs the usual X11/Wayland dev packages:

```sh
sudo apt-get install -y libgtk-3-dev libxcb-render0-dev libxcb-shape0-dev \
  libxcb-xfixes0-dev libxkbcommon-dev libssl-dev
```

`libssl-dev` is there for the updater: it uses the platform's own TLS rather
than bundling a copy of Mozilla's root store, so on Linux that means OpenSSL.

## Usage

Launch with no arguments for the GUI. The engine is also usable headlessly,
which makes it scriptable:

```sh
mcpurge --list        # scan and print a table
mcpurge --json        # scan and print JSON
mcpurge --no-sweep    # known config paths only; skip the filesystem sweep
mcpurge --quick       # skip measuring artifact sizes
```

`--list` and `--json` are strictly read-only.

### Selecting several at once

Tick the box on any row, or the one in the header to take everything the
current filter is showing, and the line above the table becomes a batch: stop
every process tree behind them, or remove all of them in one confirmed step.
The confirmation names each server, counts the config files it will back up,
and says plainly which entries it is going to skip and why.

Removals are reflected immediately: a server whose config was removed leaves
the table there and then, and a process confirmed dead stops being shown as
running, without waiting for the next scan.

### Menus

On macOS the menus are the system's own, in the bar at the top of the screen,
because that is where a Mac keeps them: an app that draws Edit/View/Window/Help
inside its own window looks like a port of something else. egui cannot draw
there — that bar is an `NSMenu` owned by `NSApplication` — so the native menu is
built with `muda` and the in-window bar is not drawn at all. Windows and Linux
keep the in-window bar, which is where those platforms expect it.

Both bars run the same list of actions (`MenuAction`), so an item means the same
thing in either, and the half that cannot be seen on the platform you are
building on is still covered by tests.

Menu shortcuts are deliberately few: ⌘F to find, ⌘R to rescan, ⌘1/⌘2/⌘3 for the
tabs. A macOS menu accelerator is handled before the window sees the key, so
⌘Z on "Undo last removal" would undo a *removal* while you were expecting to
undo a word in the search box, and ⌘A would select every server instead of the
text you were typing. Those two items have no shortcut.

### Theme

Taupe, in a light and a dark variant, under **View > Theme**; `Follow system`
tracks the desktop. The choice is remembered between runs. Both palettes are
checked by test: body text clears WCAG AA (4.5:1) on every surface it is drawn
on, and the status inks and selection text clear 3:1.

### Every invented word explains itself

mcpurge prints verdicts in words it made up: a server is `protected`, a path is
`kept`, a process match is `probable`. Each one says on hover what it is *and*
why it is there, and every column heading does the same. `protected`, for
instance, answers the question it begs:

> protected: mcpurge found files for this server but will not delete any of
> them. A path is protected when deleting it would take something other than
> this server with it: your own work handed to the server as an argument, a
> system or package-manager directory, an installed application, or a cache
> shared by every tool that uses the same package manager. Open the server to
> see each path and the rule that kept it.

The explanations for a domain type live on the type (`ArtifactKind::explain`,
`Transport::explain`, `Scope::explain`, `MatchConfidence::explain`) rather than
in the widget that happens to draw it, and a test hovers `protected` in the real
table and reads the tooltip back, so a label that lost its explanation fails the
build rather than quietly becoming jargon again.

### The three tabs

- **Servers** — every declared server, with its client, scope, transport, live
  PIDs and reclaimable size. Select a row for the full detail pane: resolved
  command, masked env, source file and JSON pointer, process tree with match
  confidence, and classified artifacts.
- **Orphans** — processes that look like MCP servers but match no config entry.
  These are the interesting ones: servers still running after their config was
  edited away, or started by a client mcpurge does not recognise.
- **Coverage** — whether anything was missed. Opens with the five numbers that
  answer it (files read, servers declared, clients in use, configs not
  recognised, files that could not be read), then the unrecognised configs,
  then a table of every known client showing **the exact paths probed for it on
  this OS** — which is what answers "but did you check *mine*?" — then the
  local-model runtimes, the read failures, and the full list of files read.

### Match confidence

Attributing a process to a config entry is inference, so it is scored and the
reason is shown:

| Confidence | Meaning |
|---|---|
| `strong` | exact absolute command path, or command plus a distinctive argument, or every identifying argument present |
| `probable` | a non-generic command name, or a partial argument match |
| `loose` | looks MCP-ish but ties to no config entry (orphans) |

A bare generic launcher never matches on its own: two different `uvx` servers
must not be confused for each other.

`(shared)` beside a PID means more than one config entry claims that process —
the same server registered in two clients is indistinguishable at the process
level, so stopping it affects both.

## Clients covered

The registry ships ~34 clients, each tagged with how well its paths are
confirmed: `verified` (vendor docs or seen on a real machine), `reported`
(consistent secondary sources), `guess` (unconfirmed, probed anyway because
probing is cheap).

Anthropic: Claude Code, Claude Desktop.
Editors: Cursor, Windsurf, Devin Desktop, VS Code, VS Code Insiders, Visual
Studio, Zed, JetBrains IDEs, Kiro.
CLI agents: Codex, Gemini CLI, Pi, DeepSeek Harness, OpenCode, Crush, Goose,
GitHub Copilot CLI, Amazon Q Developer, Qwen Code.
Editor extensions: Cline, Roo Code, Kilo Code, Continue.
Local-model desktop apps: LM Studio, Jan, 5ire, Witsy, Chatbox, AnythingLLM,
Cherry Studio.
Servers: LibreChat, Open WebUI (mcpo).

Schema quirks handled per client: VS Code and Visual Studio use `servers`, Zed
uses `context_servers` and is stdio-only, OpenCode uses `mcp` with `command` as
an array, Codex uses TOML `mcp_servers`, Goose uses YAML `extensions` with
`cmd` instead of `command`, Gemini CLI uses `httpUrl` and Windsurf uses
`serverUrl` for remotes. Nesting is handled generically, which is why
`~/.claude.json`'s per-project `projects.<path>.mcpServers` blocks are found
without a special case.

**Ollama, llama.cpp and vLLM** are detected but listed separately as runtimes,
not clients: they are inference servers with no native MCP support. MCP reaches
their models through a separate host such as LM Studio, Goose, Continue or
ollmcp. They appear in the Coverage tab so their absence from the server table
is explained rather than mysterious.

### Adding a client without rebuilding

Copy `assets/clients.toml` to `<config>/mcpurge/clients.toml` and edit it. On
macOS that is `~/Library/Application Support/mcpurge/clients.toml`. The Coverage
tab shows which registry is in use.

## Dependency policy

Dependabot is configured for **major versions and security fixes only**; minor
and patch churn is ignored.

That relies on a subtlety worth stating, because getting it wrong would be
quiet and bad: `update-types` ignore rules apply **only to version updates**,
so security PRs still arrive at any semver level, including patch. An ignore
entry that names a dependency *without* `update-types` expands to `>= 0` and
*would* suppress its security PRs. So `.github/dependabot.yml` always pairs
`dependency-name` with an explicit `update-types` list.

Critical fixes that ship without an advisory are caught separately by a daily
`cargo deny check advisories` run in CI against the RustSec database.

## Development

```sh
cargo test --all                              # 128 tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check advisories licenses bans sources
```

The engine is independent of the GUI — `model`, `registry`, `scan`, `proc`,
`files` and `purge` have no egui dependency, which is what lets `--list` and
`--json` work headlessly and keeps the parsing and editing logic unit-testable.

Tests of note. The UI is rendered headlessly for every tab, dialog and detail
pane, so a layout or id-collision panic fails the build. One test goes further
and drives real pointer input through the real widget tree with `egui_kittest`
(`clicking_a_row_opens_the_detail_pane_with_its_actions`), because a widget that
renders correctly and ignores input looks identical to a passing render test —
see below. Another renders every view with the missing-glyph policy set to panic
(`every_view_renders_with_real_fonts`), which is how a character the bundled
fonts lack gets caught before it ships as an empty box. A third hovers `protected` in the
real table and reads the tooltip that comes up
(`hovering_protected_explains_what_it_means`), because an explanation nobody
wired to a widget is not an explanation. The config rewriter is
checked for comment preservation, dangling commas, deeply nested pointers and
byte-identical sibling tokens; the process matcher against the real
`npx` → `npm exec` wrapper chain; and there are named regression tests for the
bugs the first real scan uncovered (`allow_path_arguments_are_never_deletable`,
`home_directory_is_not_a_project_root`).

### The rows that did nothing

Worth recording, because the tests were green the whole time. `egui_extras`
senses `Sense::hover()` on table cells by default, so `row.response().clicked()`
was permanently false; and egui labels sense click-and-drag for text selection,
so a label inside a click-sensing cell swallows the click before the row sees
it. Both are needed: `TableBuilder::sense(Sense::click())` *and*
`interaction.selectable_labels = false` on the table's `Ui`. With either one
missing, clicking a server did nothing at all — no detail pane, and therefore no
way to reach any per-server action.

Every UI test at the time set `selected` by hand, so all of them passed while
the app could not be used. The lesson is cheap to state and was not cheap to
find: a render test proves a widget draws, not that it responds.

## License

MIT OR Apache-2.0
