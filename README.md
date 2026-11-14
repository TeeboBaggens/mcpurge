# mcpurge

Find every MCP server configured on a machine, see which ones are actually
running, stop them, and purge them — the config entry and, if you want, the
files on disk.

A single self-contained binary for macOS, Windows and Linux. No webview, no
Node toolchain.

```
┌─ mcpurge ──────────────────────────────┬─ Detail ─────────────┐
│ [Rescan] [Filter: all ▾]  ☐ show stale │ rea                  │
│                                        │ Transport: stdio     │
│ ● rea          Kiro·user   RUNNING pid │ Cmd: npx -y          │
│ ● rea          Claude Code RUNNING ×3  │   rea-agents@6.7.0   │
│ ○ godot-ai     Kiro·user   stopped     │ Env: 2 vars (hidden) │
│ ● solidcraft   Claude Code RUNNING pid │ Found in: ~/.kiro/…  │
│ ○ crawl4ai     Claude Code stopped     │ Files: 223 MB        │
│                                        │ [Kill tree] [Remove] │
└────────────────────────────────────────┴──────────────────────┘
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

**3. Not everything near a server is the server's.** SolidCraft's real config
on the development machine was:

```
solidcraft-cli mcp --allow-path ~/development/projects/switch2coffee
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

## Install

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
  libxcb-xfixes0-dev libxkbcommon-dev
```

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

### The three tabs

- **Servers** — every declared server, with its client, scope, transport, live
  PIDs and reclaimable size. Select a row for the full detail pane: resolved
  command, masked env, source file and JSON pointer, process tree with match
  confidence, and classified artifacts.
- **Orphans** — processes that look like MCP servers but match no config entry.
  These are the interesting ones: servers still running after their config was
  edited away, or started by a client mcpurge does not recognise.
- **Coverage** — what was actually examined. Lists unrecognised MCP-shaped
  configs, detected local-model runtimes, every known client with a hit count,
  and files that could not be read.

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
cargo test --all                              # 87 tests
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
fonts lack gets caught before it ships as an empty box. The config rewriter is
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
