# topterm

**Activity Monitor for your terminal tabs.** Find which terminal tab is leaking memory — and kill it without restarting anything.

![macOS](https://img.shields.io/badge/platform-macOS-black)
![Rust](https://img.shields.io/badge/built%20with-Rust-orange)
![deps](https://img.shields.io/badge/dependencies-3-green)

<img src="assets/demo.svg" alt="topterm TUI" width="100%">

And the one-shot plain table:

```
topterm  5 terminal sessions, 641 processes  |  memory 18.54 GB of 16.00 GB physical
memory = physical footprint (incl. compressed + swapped), so the sum can exceed RAM

TTY               MEM      ΔMEM   %CPU PROCS TAB                    SHELL/RECENT        CWD
ttys004       6.21 GB   +227 MB  188.4     4 ml-training run 42     zsh / python train… /Users/demo/dev/vision-…   ← GROWING
ttys001       1.38 GB     -3 MB   24.7     4 api-server (dev)       zsh / node server.… /Users/demo/dev/api-ser…
ttys007        817 MB     -0 MB    9.3     3 docker compose up      zsh / docker compo… /Users/demo/dev/edgecast
ttys012        514 MB     +1 MB    5.1     2 vite dev server        zsh / vite dev ser… /Users/demo/dev/storefr…
ttys019         93 MB     +0 MB    0.2     2 -                      zsh                 /Users/demo

terminal emulator apps (scrollback buffers)
Terminal                1.15 GB    1.9  pid 812  (app itself, not a tab)

detached (reparented to launchd - no tab to blame, kill by pid)
pid:6188       650 MB    3.6     1  node /Users/demo/dev/storefront/node_modules/next/dist/server/next-server.js
                                    --port 3000
```

<sub>Screenshot and table are real `topterm` output rendered against a synthetic machine (`--fake-data`), so nobody's actual tab titles ship in this repo.</sub>

## The problem

You have a MacBook with 18 GB of RAM and your terminal is using 25 GB. Activity Monitor shows dozens of anonymous `node` and `python` processes, but not which tab owns them or which one is leaking. `top`, `htop`, and Activity Monitor all show a flat process list — none of them know what a terminal tab is.

`topterm` does. Every tab gets a pseudo-terminal (`ttys000`, `ttys001`, ...), and every process a tab spawns hangs off that tty's process tree. `topterm` groups the entire process table by tab, sums memory and CPU per tab, looks up each tab's actual *title* and working directory, and sorts the hogs to the top. The leaky tab identifies itself in one screen.

## Install

```sh
cargo install --git https://github.com/Comradery64/topterm
```

That builds and drops the `topterm` binary in `~/.cargo/bin`, which a standard
`rustup` install already puts on your `PATH` — so `topterm` just works in a new
shell. (If it doesn't, add `~/.cargo/bin` to your `PATH`.)

Or build from a clone:

```sh
git clone https://github.com/Comradery64/topterm && cd topterm
cargo build --release
ln -s "$PWD/target/release/topterm" ~/.local/bin/topterm
```

Requires macOS and Rust. Three dependencies (`ratatui`, `crossterm`, `libc`) — no daemon, no root, no kernel extension. It reads the same `ps`/`lsof`/`sysctl` output you could read yourself, just ten thousand times faster.

## Use

```sh
topterm                          # one-shot table, sorted by memory, pipe-friendly
topterm -t                       # the TUI
topterm --index > tabs.md        # read-only map of every tab and its Claude session
topterm --summarize > tabs.md    # same, plus a short recap per session
```

### Memory details

- **CLAUDE column**: `idle 4d 16h` / `busy` for the Claude Code session in each tab, read from
  `~/.claude/sessions/<pid>.json` (and cc-mirror variants).
- **Swap and SSD writes** in the header: swap used, and bytes written to the internal SSD since
  boot with a per-day average, from the disk's own counter (`ioreg`). Sustained swapping shows up
  here as hundreds of GB a day.
- **MCP servers**: every Claude session starts its own copy of each MCP server. The one-shot table
  lists each server with how many tabs run it, its processes, total memory, and any copies
  orphaned to `launchd` after their session ended.
- **`--scrollback`**: each tab's Terminal.app scrollback in characters, with an estimate of its
  share of Terminal's memory (proportional to characters, so it is an estimate). Takes ~10s with
  100+ tabs; the TUI refreshes it every 5 minutes.

### Hibernate (TUI, `H`)

Exit an idle Claude session without closing its tab. Select a row and press `H` for a preview:

- window/tab position, folder, session name, idle time, and the memory exiting would free
- the last prompt you typed and Claude's last reply, read from the transcript
- the newest cached summary, labelled with its age and how many messages came after it
  (`S` refreshes it with one Haiku call)
- the last lines currently on that tab's screen
- `R` brings the tab to the front so you can look before deciding

`y` hibernates. It refuses if Claude is busy or is running anything besides MCP servers,
language servers, and `caffeinate`, and right before acting it re-checks that the same Claude
process is still on that tab, on the same conversation, and idle. It then types `/exit` into the
tab, waits for Claude to quit, and prints a card in the tab with the folder, last ask, summary,
and the `claude --resume <id>` line. The tab and its history stay. Nothing is done in bulk.

### The tab index

Too many tabs to close safely? `topterm --index` writes a markdown map you can
keep, so closing a tab never means losing track of it. Per tab, in window/tab order:

- the tab's title and working directory
- the Claude Code session it holds: live sessions are read from
  `~/.claude/sessions/<pid>.json`; exited ones from the `claude --resume <id>`
  line Claude prints on exit (last 6,000 characters of scrollback)
- cc-mirror variants
  (`~/.cc-mirror/<name>/config`) are recognised, and their resume command uses
  the variant's own launcher, e.g. `work --resume <id>`

`--summarize` adds one or two sentences per session. Each recap is a one-off
`claude -p --model haiku --effort low --no-session-persistence` call with tools
and MCP disabled, fed a trimmed copy of the transcript. It never resumes the
original session, adds nothing to it, and saves no new session. Recaps are
cached in `~/Library/Caches/topterm/summaries` by session id and transcript
size, so re-runs only pay for sessions that changed. While it runs, each Haiku
call briefly appears as a live Claude session to your other sessions.

Both modes only read. Nothing is closed, cleared, or signalled.

### The TUI

`topterm -t` is interactive:

- **Live gradient RAM gauge** across the top — summed terminal memory vs. physical, with a sparkline of the machine-wide trend.
- **Click any row** (mouse support) or press `Enter` to expand a session into its full process tree with memory bars and a four-minute memory trend. Leaks show up as a line that only goes up.
- **`GROWING` flags pulse** on sessions that gained 50 MB+ across recent samples. That's the leak signature.
- **Press `K`** on a session to kill it: you get a confirmation overlay showing every process group it will SIGTERM, and nothing happens until you press `y`.
- Panel expansion animates at 30 fps while something is moving. Idle, it wakes once per sampling interval.

| Key | Action |
|---|---|
| click / `↑↓` / `j k` | select session |
| `Enter` / click again | expand / collapse detail |
| `H` | hibernate preview for the selected Claude session (`R` reveal, `S` summary, `y` hibernate) |
| `K` then `y` | SIGTERM every process group in the session |
| `Esc` | collapse / cancel |
| `q` / `Ctrl-C` | quit |

### Flags

| Flag | |
|---|---|
| `-t`, `--tui` | interactive TUI (default is a one-shot plain table) |
| `--scrollback` | also measure each tab's Terminal.app scrollback (~10s) |
| `--interval SECS` | TUI sampling interval (default 2) |
| `--wide` | never truncate commands or paths |
| `--no-color` | plain output |
| `--demo-frame` | render one styled TUI frame as ANSI to stdout and exit (used by the test suite to assert on the visuals) |
| `--index` | print a read-only markdown map of every Terminal.app tab (see above) |
| `--summarize` | `--index` plus a Haiku recap per Claude session |
| `--fake-data` | render a synthetic machine instead of sampling this one — how the screenshot and table above were generated |

## How it knows which tab is which

Three tricks, all boring and reliable:

1. **TTY grouping.** `ps -axo pid,ppid,pgid,tty,%cpu` gives every process's controlling terminal. Processes that detached from their tty (`??`) get attributed by walking their ancestry back to a tab's shell — so a runaway child your dev server forked three levels deep still counts against the right tab.
2. **Tab titles via AppleScript.** Terminal.app and iTerm2 both expose the tty of every tab, so one `osascript` call per refresh maps `ttys012` to the title you actually see. (First run may ask for Automation permission — approve once. Fun fact discovered building this: inside a `tell application "Terminal"` block, AppleScript's `tab` means the *tab object*, not the tab character.)
3. **Working directories** via one batched `lsof -d cwd` call — many tabs lack titles, and the path is how you recognize them. A tab whose folder was deleted shows `(folder deleted)`.

Memory is each process's **physical footprint** (`proc_pid_rusage`, the number Activity Monitor shows), not RSS. RSS only counts pages currently in RAM, so on a machine that is swapping it hides most of what a tab really holds. Processes you may not inspect fall back to RSS.

Two categories fall outside per-tab grouping:

- **Detached hogs** — subtrees that lost their tab and reparented to `launchd`. No tab to blame, but they're listed with full command lines and PIDs, because they are frequently the actual leak.
- **The emulator itself** — Terminal.app / iTerm2 / kitty / Alacritty / WezTerm / Ghostty, since gigabytes of scrollback live there, not in any tab.

## FAQ

**Why does the memory total exceed my physical RAM?**
Physical footprint includes memory macOS has compressed or swapped to disk. If the total is well above your RAM, you are swapping, which is exactly when a slow terminal starts to hang.

**Why does the "out of application memory" dialog blame a tiny app for gigabytes?**
macOS groups memory by responsible app. A program launched from a terminal tab, including one Claude Code starts by running its binary, is counted with Terminal, so the dialog can show Terminal's whole total next to it. Launch apps with `open MyApp.app` to give them their own row.

**Does it only track dev tools?**
No — grouping is completely tool-agnostic. Anything a tab runs gets counted: dev servers, docker, ssh, a stuck `npm install`. Only the detached-orphan section filters to dev-tool-looking commands, to keep macOS system daemons out of your face.

**tmux?**
tmux panes get their own ttys under the tmux server, so they group under the server rather than the tab that launched tmux. Known limitation.

**Linux?**
macOS only for now — the `ps`/`lsof`/`sysctl` invocations and the AppleScript tab-title lookup are Darwin-specific. The grouping logic would port; PRs welcome.

**Is the kill feature safe?**
It SIGTERMs (never SIGKILL) each process group after an explicit per-group confirmation, and the overlay auto-cancels if the target session disappears between refreshes. It will never signal a group it didn't show you first.

## Contributing

PRs welcome. The project is MIT, there's no CLA, and the only hard gate is that
`cargo test` passes — CI runs `cargo build` and `cargo test` on macOS.

**A Linux port is the most-wanted contribution.** `src/collector.rs` is the only
Darwin-specific file in the tree: everything platform-dependent lives in its
`ps`/`lsof`/`sysctl` invocations and the AppleScript tab-title lookup. The
grouping, diffing, and rendering logic above it is portable as-is, so a port is
mostly a matter of teaching the collector a second way to fill the same structs
(`/proc` plus your terminal's own tty mapping).

Working on the visuals? `topterm --fake-data --demo-frame` renders a single
styled TUI frame against the synthetic machine in `src/fake.rs`, and `tools/`
holds the stdlib-only scripts that turn that frame into `assets/demo.svg` and
regenerate the plain-text table in this README. See [tools/README.md](tools/README.md)
for the two commands. Please keep demo assets generated from `--fake-data` so no
real machine's tab titles or paths land in the repo — `cargo test` enforces that
the fake data only ever references `/Users/demo`.

## License

MIT — see [LICENSE](LICENSE).

---

Built because a 25 GB terminal on an 18 GB laptop is a solvable problem.
