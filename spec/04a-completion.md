**← Previous:** [04 — Prompt editor](04-prompt-editor.md) | **Next:** [05 — Pane modes](05-pane-modes.md) →

# 04a — Tab completion

The headline feature within the headline feature. Tab in [the prompt editor](04-prompt-editor.md) does **not** send `\t` to the PTY; it opens a popup of completion candidates Termica computed locally. This document is the design for the hybrid completion engine that ships post-MVP: **CLI-native drivers** for the modern tools that already expose a completion endpoint (`kubectl __complete`, `aws_completer`, `gh __complete`, cobra `__complete`, …) combined with a **per-pane shell sidecar** for the long tail and user-defined completions (aliases, custom `complete -F` functions, fish abbreviations).

The MVP — local path / `$PATH` / history completion — is the fallback when both upstream sources fail or take too long. It ships first ([Phase 4I](10-roadmap.md#phase-4--editor-at-prompt-block-model-pivot)). This document covers the deeper design that lands behind it once 4I is stable.

## Why not just forward Tab to the shell

Three things go wrong if we let the shell's line editor own Tab.

1. **The shell's line editor is the thing Termica replaces.** [Spec/00 §"Why Termica"](00-overview.md) lists "a real completion popup" as one of the four headline features (alongside multiline editing, history search, and undo). A native popup is part of the product; forwarding Tab means we don't have one.
2. **Multiline commands break.** When a user is editing a 5-line `for` loop in the Termica editor, the shell isn't seeing those lines — they don't exist on the PTY until Enter. The shell's line editor has nothing to complete against.
3. **Mode safety breaks.** Tab routed to the PTY in `ShellPromptEditor` mode would echo through readline / ZLE / fish's line editor, producing visible glyphs in the live `Term` grid that don't correspond to anything the user actually typed. The editor's whole point is that the user types into Termica, not the shell.

So Termica computes completions locally. The question is **where the candidates come from**, and that's the rest of this document.

## Source priority (the hybrid)

A single Tab keystroke fires all three sources in parallel. Results merge into one ranked popup; sources are tagged so the UI can show source-of-truth icons.

| # | Source | Speed | Coverage |
|---|---|---|---|
| 1 | **CLI-native drivers** (`kubectl __complete`, `gh __complete`, `git --list-cmds`, …) | ~50–150 ms first call, ~10–30 ms cached | Excellent for big tools that ship a completion endpoint; nothing for tools that don't |
| 2 | **Shell sidecar** (`bash`/`zsh`/`fish` process with the user's rc) | ~100–500 ms first call, ~30–80 ms cached | Picks up aliases, custom `complete -F`, fish abbreviations, kubectl/aws completions the user installed in their rc — anything the user's shell would complete |
| 3 | **Local heuristics** (paths under cursor, `$PATH` executables, command history) | Synchronous, always under 10 ms | Always available; coarse coverage. The MVP path. |

All three sources race to a 250 ms timeout. Whichever land within the timeout populate the popup; late results stream in afterward without disturbing the user's current selection. If nothing arrives by the timeout, the popup opens with source 3's local heuristics and a faint "searching…" affordance.

> **As built (slice 2 — wait, don't flash).** The original "open instantly with locals, stream the rest in" rule flashed the *wrong* thing for a driver-eligible command: `gh <Tab>` briefly showed the cwd's **files** before the gh subcommands replaced them ~100 ms later, and `git ch<Tab>` (no local match) showed **nothing at all** because an empty local result meant the popup never opened and the driver never fired. So the lifecycle is now: a **driver-eligible** command (leading word maps to a known tool, cursor in argument position) **waits** for the driver result, then opens the popup from the driver⊕locals merge — even when there were no local matches. A command with **no** driver keeps the instant-locals behavior. On a live re-filter the previously-open popup stays visible until the refreshed result swaps in (no blink). The decision is a pure, tested function (`completion::plan_completion` → `Open` / `AwaitDriver` / `Closed`, resolved by `completion::resolve_driver`); the once-per-frame resolution lives in `render_pane`. When the driver source grows a sidecar (slices 3–5) the "stream in late without disturbing selection" wording above becomes the steady-state for *additional* late sources on top of this first result.

The popup ALWAYS opens when there's anything to show. A completion request never fails silently. Even if both sidecar and drivers crash, source 3 returns *something* (the paths under the cursor) — and a driver-eligible command with neither a driver result nor a local match simply shows nothing rather than a misleading file list.

## Source 1 — CLI-native drivers

Modern CLIs expose a "complete this command line" endpoint independent of any shell. Termica calls it directly:

| Tool | Endpoint | Notes |
|---|---|---|
| `kubectl` | `kubectl __complete <args...>` | The args are everything the user has typed so far, split on whitespace. The stdout is one candidate per line, optionally followed by a tab + description. |
| `aws` | `aws_completer <COMP_LINE> <COMP_POINT>` | aws_completer is a small Python script Amazon ships with the AWS CLI; reads `COMP_LINE` env var and `COMP_POINT` (cursor offset). |
| `gh` | `gh __complete <args...>` | cobra-based — same shape as kubectl. |
| `git` | `git --list-cmds=builtins,others,nohelpers` (subcommands), then per-subcommand completions are sidecar territory | git's own completion endpoint is partial; bash-completion's `_git` function is much richer. Source 2 covers what source 1 can't here. |
| `docker` | `docker __complete <args...>` | cobra-based. |
| `gcloud` | (no native endpoint; sidecar only) | gcloud's completion is shell-script-only. |
| `terraform` | `terraform <subcommand> -help-machine` (subcommands) + sidecar | partial native; sidecar fills the gap. |

A small `completion::drivers` module knows about each tool and its endpoint (`completion::drivers::parse`). Adding a new tool is one match arm + a small parse function for that tool's output format (most are "candidate\tdescription" per line — `parse_cobra_complete`, shared by kubectl/gh/docker; aws_completer's output is whitespace-tokenized — `parse_aws_completer`; git's is one subcommand per line — `parse_git_list_cmds`). cobra's `__complete` trailing `:N` directive line is ignored. kubectl's *resource* completion (`kubectl get <resource>`) deviates from the tab convention and pads columns with spaces, so `parse_cobra_complete` accepts a run of two or more spaces as the value/description boundary too (cobra completion values never contain a space).

**Driver detection is implicit.** As-built, there is no separate `--help` probe: a request simply fires for any command whose leading word maps to a known tool, and if the tool isn't installed (or is shadowed), `Command::spawn` fails on the worker thread and the request yields zero candidates. This is strictly better than an explicit probe — it never runs a subprocess on the UI thread, needs no detection cache, and is self-correcting (the local sources still populate the popup). A failure is silent; source 2 / source 3 cover the gap.

**Driver process lifecycle**: each driver call is a one-shot subprocess spawned on the per-pane worker thread, bounded by a **2 s** wall-clock deadline (`spawn` + `try_wait` poll + `kill` on timeout — no `wait-timeout` dependency). No long-running drivers. The cost is ~30–50 ms of fork+exec for a local tool; the 2 s ceiling exists for a real-but-slow endpoint (e.g. `kubectl --context <remote> get` round-tripping to a cold cluster — a too-tight cap there killed the subprocess and showed nothing). Because a driver-eligible command **waits** for its result, the popup shows a **"searching…" spinner** (`completion::popup::paint_searching`) at the popup anchor during the wait, so the delay is legible. A [result cache](#caching) makes repeats free; an outright timeout yields zero candidates (not cached, so it retries next time). Every step is traceable via [`TERMICA_DUMP_EVENTS`](03-shell-integration.md) `completion` records.

**Per-tool cwd / env**: drivers run with `PaneSession::context().cwd` as CWD and the pane's environment. kubectl's context selection lives in `~/.kube/config` and the `KUBECONFIG` env var, both of which propagate naturally.

## Source 2 — shell sidecar

A long-lived companion shell per pane. Spawned on-demand when the user first presses Tab; one of three flavours depending on `PaneSession.shell`:

```text
┌─────────────────────┐                    ┌──────────────────────────┐
│   Termica editor    │  COMPLETE request  │ Sidecar shell (bash/zsh/ │
│   (ShellPromptEditor)│ ─────────────────▶│  fish), --no-pty,        │
│                     │ ◀───────────────── │  user's rc loaded        │
│                     │  candidates (json) │                          │
└─────────────────────┘                    └──────────────────────────┘
```

The sidecar is **not** the user's interactive shell. It's a Termica-controlled sibling that loads the user's rc files in non-interactive mode so it can run `complete -p` (bash), `_main_complete` (zsh), or `complete -C` (fish) on demand. Termica writes RPC requests on its stdin; the sidecar writes JSON responses on its stdout. No PTY involved — it's pipe IPC.

### Sidecar protocol (wire)

Newline-delimited JSON over stdin/stdout. Termica controls the request side; the sidecar's vendored helper script controls the response side.

**Request** (Termica → sidecar):

```jsonc
{ "id": 17, "type": "complete", "line": "kubectl get po", "point": 14 }
{ "id": 18, "type": "complete", "line": "git ", "point": 4 }
{ "id": 19, "type": "ping" }
{ "id": 20, "type": "shutdown" }
```

`id` is a request correlation token; each response carries the same id.

**Response** (sidecar → Termica):

```jsonc
{ "id": 17, "type": "candidates", "items": [
  { "value": "pod", "display": "pod", "description": "Pods are…" },
  { "value": "podsecuritypolicy", "display": "podsecuritypolicy" }
] }
{ "id": 19, "type": "pong" }
{ "id": 20, "type": "shutdown_ack" }
{ "type": "error", "id": 17, "reason": "completion-function-missing" }
```

`items[].description` is optional. Termica truncates descriptions to one line in the popup.

### Bash sidecar

> **As built (slice 4 — live-shell capture, in-process).** Bash ships as live-shell completion, like fish and zsh — answered by the pane's own integration so **runtime-defined** `complete` specs / aliases resolve. It is the *simplest* of the three: unlike zsh's `compadd` (which early-returns outside a real ZLE widget, forcing a captive `zpty` child), **bash completion functions just fill the `COMPREPLY` array and need no readline context**, so the managed bash captures completions **in-process** — no sidecar child at all, the same shape as fish's in-process `complete -C`.
>
> On a request, `__termica_bc_capture <line>` (in [`integration/bash-bootstrap.bash`](../integration/bash-bootstrap.bash)) sets the `COMP_*` globals, triggers bash-completion's lazy loader (`_comp_load`) for the command, reads the registered `-F <function>` from `complete -p` (or the dynamic `-D` default), calls that function, and dedupes `COMPREPLY` into a values array. The `COMP_*` are all `local`, so they can never leak into the next real command's bash-preexec DEBUG trap (which **skips** preexec while `COMP_POINT` is set — a stuck value would silently stop the block model).
>
> The captured **values** go out in a [`completion`](03-shell-integration.md#completion--live-shell-completion) DCS marker, parsed by the same Rust parser as fish/zsh. The dispatch is the shared `__termica_complete` sentinel command, but with a **bash-specific difference**: it carries **no leading space**. bash-preexec reads the running command from `builtin history 1`; under `HISTCONTROL=ignorespace` a leading-space command never enters history, so bash-preexec would hand `termica_preexec` the *previous* command and corrupt the block model. So the bash sentinel enters history normally (where the preexec/precmd guards recognise it and stay inert) and `__termica_complete` **deletes its own history entry**. `$?` is preserved. Mode-inertness is normative in [spec/03 §`completion`](03-shell-integration.md#completion--live-shell-completion).
>
> **Bash-completion is loaded by the bootstrap** if the user's `~/.bashrc` didn't (the bash analog of zsh's `compinit`): `--noprofile` skips the profile.d loader, so the wrapper sources `bash_completion.sh` from the standard locations, interactive-only. **Graceful degradation**: `bash-completion` 2.x needs bash ≥ 4.1; on an old bash (stock macOS ships 3.2) it won't load, so the capture only resolves the few manually-registered specs — the terminal and prompt editor are unaffected. The bootstrap reports capability in `integration_ready` (`{"bash_major":N,"completion":true|false}`); `completion:false` is parsed into `IntegrationState::Confirmed { completion_degraded: true }`, and the pane shows a thin **dismissible** strip at its top ("Tab completion is limited — this bash is too old…"). It's a *notice*, not a `Degraded` pane: the terminal and prompt editor work fully. Dismissal is per-pane (a fresh pane shows it again). The gate is the pure `render_pane::should_show_completion_notice(degraded, dismissed)`.
>
> **v1 emits values only.** **Routing** mirrors zsh exactly: the per-tool **cobra drivers stay authoritative** (`gh`/`git`/`kubectl`/…) and only the **long tail** routes to the live shell (`DriverTool::BashComplete`, command *and* argument position via `fish_segment`); an empty segment never fires.

### Zsh sidecar

> **As built (slice 5 — live-shell capture via a warm `zpty` child).** zsh ships as live-shell completion, like fish — answered by the pane's own integration so **runtime-defined** aliases complete — but the mechanism is necessarily different from fish's clean `complete -C`, and the design below (call `_main_complete` from a plain function) **does not work on modern zsh**: `compadd`/`_main_complete` early-return outside a real ZLE widget, producing zero matches. Empirically (zsh 5.9), the reliable mechanism is the one [fzf-tab](https://github.com/Aloxaf/fzf-tab) / [zsh-autocomplete](https://github.com/marlonrichert/zsh-autocomplete) use:
>
> 1. **A warm captive child.** The pane shell (which runs with `unsetopt zle`, so it can't host a widget) lazily spawns a persistent `zsh/zpty` child — `zsh -f -i`, ZLE *enabled* — on the first Tab and reuses it. The child is seeded **once** with the user's `$fpath` + `compinit` (so their tools' completions load); it is **never** seeded by re-sourcing dotfiles, which on a slow `.zshrc` would make every Tab as slow as shell startup — the constraint that rules out a fresh `zsh -i -c` per request. Setup and per-request data are delivered by `source`-ing a file (a short command line), because a long command *written* to the child's line editor wraps at the pty width and never executes.
> 2. **A `compadd` recorder.** Inside the child, `compadd` is wrapped to capture the would-be matches via `compadd -O <array>` (zsh extracts them itself — no fragile option-parsing) and then delegate to the builtin.
> 3. **A real widget.** A one-shot ZLE widget sets `BUFFER` to the requested line and calls `zle complete-word`; the recorder collects the matches. (Triggered by `^X^X` — **not** `^T`, which BSD/macOS ttys intercept as VSTATUS/SIGINFO before it reaches ZLE.)
> 4. **Runtime aliases.** Each request replays the live shell's `alias` table into the child, so config *and* interactively-defined aliases complete.
>
> The captured **values** go out in a [`completion`](03-shell-integration.md#completion--live-shell-completion) DCS marker, parsed by the same Rust parser as fish's reply. The dispatch (`__termica_complete` sentinel, preexec/precmd guards, history exclusion, `$?` preservation) and mode-inertness are normative in [spec/03 §`completion`](03-shell-integration.md#completion--live-shell-completion); the child's lifetime is tied to the bootstrap process, so it dies with the pane — and it is **idle-dropped** sooner: `termica_precmd` runs `__termica_zc_idle_check` once per prompt, releasing the child (~5 MB) after `$SECONDS - last_use` exceeds `__TERMICA_ZC_IDLE_SECS` (300 s), with the next Tab lazily respawning it. precmd is the only in-shell clock, so a pane left truly idle *at* a prompt keeps the child until its next command; the common "used Tab a while ago, still running commands" case is covered.
>
> **v1 emits values only.** zsh descriptions are gated behind `verbose` / format zstyles and are config-dependent and fragile; deferred. **Routing**: a zsh pane keeps the per-tool **cobra drivers authoritative** for the tools that have them (`gh`/`git`/`kubectl`/…, already reliable) and routes only the **long tail** — aliases, functions, built-ins, shell-installed completions — to the live shell (`DriverTool::ZshComplete`, command *and* argument position via `fish_segment`). An **empty** segment never fires. Known limitation: a function defined *as a command* directly in `.zshrc` (not via `$fpath`) may not complete by name in v1, since the warm child loads `$fpath`, not the dotfiles.

### Fish sidecar

Fish is the cleanest of the three. It has a native `complete -C` CLI that returns completions on stdout:

```fish
$ fish -c 'complete -C "kubectl ge"'
get	Display one or many resources
…
```

No `compsys`-style ZLE state. No `COMPREPLY`. Just one CLI call per request.

> **As built (slice 3 — one-shot, not a persistent process).** Because fish exposes completion as a plain CLI whose stdout is already `value\tdescription` per line — the **same format cobra `__complete` emits** — the fish sidecar does **not** need the persistent-process / JSON-RPC machinery sketched below for bash/zsh. It rides the existing **CLI-native driver engine** ([Source 1](#source-1--cli-native-drivers)) instead: a new `DriverTool::FishComplete` whose invocation is `fish -c 'complete -C $argv[1]' <line>` (the line passed as a positional arg so quotes/spaces can't break out of the `-c` script; **no** `--no-config`, since the user's aliases and `complete` definitions are the whole point) and whose parser is `parse_fish_complete` (tab-only split — fish always separates value/description with a single literal tab, and a completion *value* may contain spaces). This reuses the engine's per-pane worker thread, 2 s deadline, [result cache](#caching), `paint_searching` spinner, and `TERMICA_DUMP_EVENTS` records for free.
>
> **Routing**: in a **fish** pane, `complete -C` is a superset of the per-tool CLI drivers, so `completion::plan_completion` routes *any* completion — in **both command and argument position** — to `FishComplete` (via `drivers::parse::fish_segment`) and never also fires a per-tool driver. Routing the command **name** to fish (not just arguments) is what lets the user's **aliases, functions, and abbreviations** complete, since the local `$PATH` source only knows on-disk executables; the `FishComplete` result is merged with the local sources (the `$PATH` executables at command position, files at argument position) so a command on both collapses to one row. The only thing that does *not* fire the sidecar is an **empty** segment (empty editor, or right after a `|`/`;`), which would otherwise `complete -C ""` the entire command set. Non-fish panes keep the per-tool `driver_target` path unchanged (argument position only; the command name stays local). The shell is read from `PaneSession::shell()`.
>
> The persistent-process / JSON-RPC model sketched below was **not** used by any shell in the end. All three ship as **live-shell capture** answered by the pane's own integration: fish via in-process `complete -C`, **bash** via in-process `COMPREPLY` (see [§"Bash sidecar"](#bash-sidecar)), **zsh** via a warm `zpty` child (see [§"Zsh sidecar"](#zsh-sidecar)). The JSON-RPC sketch is retained below only as historical design context.

> **As built (slice 3b — live-shell completion is now the primary fish path).** The one-shot subprocess above loads the user's *config* but can't see aliases/functions defined **interactively at runtime** (they live in the pane's own fish process). So when a fish pane is **at a prompt with confirmed integration**, completion is answered by the pane's **own live shell**: Termica writes a `complete\t<id>\t<base64-line>` request to the PTY, the bootstrap's read-eval loop runs `complete -C` **in-process** and replies with a [`completion`](03-shell-integration.md#completion--live-shell-completion) DCS marker, and `PaneSession` correlates the reply (by `id`) into the same `AwaitDriver` → `resolve_driver` popup flow the drivers use — so the renderer is unchanged. This reflects the true live shell (runtime aliases, functions, abbreviations, current `cd`) and is *faster* than the one-shot (no `fish -c` startup). The wire/lifecycle/mode-safety is normative in [spec/03 §`completion`](03-shell-integration.md#completion--live-shell-completion); it is **inert to the pane-mode machine** ([spec/05](05-pane-modes.md)).
>
> The one-shot `fish -c 'complete -C'` (`DriverTool::FishComplete` via the engine) is kept as the **degraded-mode fallback** — used when the fish pane is *not* at a prompt or integration is unconfirmed (`PaneSession::fish_live_capable()` is false). Routing (`plan_completion` → `fish_segment`, command + argument position) is identical for both transports. A live request the shell never answers times out (~600 ms) and falls back to the local candidate sources. Remaining fish follow-up: **multi-source racing** (show instant locals, then swap the live reply in, rather than waiting on the spinner).

Fish is the **reference implementation** for the protocol — when in doubt about the wire shape or the lifecycle, look at the fish path first. (For the persistent-process bash/zsh sidecars, that means the wire/lifecycle below; fish itself sidesteps it via the one-shot path above.)

### Sidecar lifecycle

| Phase | Trigger | Action |
|---|---|---|
| Spawn | First Tab press in pane | Spawn the matching **persistent** sidecar (`bash --rcfile`, `zsh -i`) with stdio pipes. Detect shell from `PaneSession.shell`. **Fish is exempt** — it uses the one-shot driver-engine path above (a fresh `fish -c 'complete -C'` per request, no persistent process), so this lifecycle applies to bash/zsh only. |
| Steady state | Subsequent Tab presses | Send `COMPLETE` requests; read responses. ~30–80 ms per call after warm-up. |
| Idle timeout | No request for `SIDECAR_IDLE_SECS = 300` (5 min) | Send `shutdown`; close pipes. Re-spawn on next Tab. |
| Crash | Sidecar process exits unexpectedly | Drop the handle, fall back to source 3 for this request. Re-spawn on next Tab; rate-limit re-spawn attempts at 1/sec to avoid fork bombs. |
| Pane teardown | `PaneSession::drop` | Send `shutdown`; SIGKILL fallback after `SHUTDOWN_GRACE_MS = 500`. |

**Cwd / env tracking**: the sidecar inherits the pane's cwd and env at spawn time. On `PaneSession::observe_lifecycle_event` for `Cwd { cwd }`, Termica sends a `{ "type": "cd", "path": "<cwd>" }` request so the sidecar's view of the filesystem stays aligned with the user's. Env changes (e.g. activating a virtualenv) propagate via the `__termica_envsync` hook in our integration scripts: when the integration sees `prompt_vars` with changed env keys, Termica forwards them to the sidecar as `{ "type": "setenv", "vars": {...} }`.

This is the part that's most likely to drift in real usage and the part the test surface watches most carefully — see [§Testing](#testing) below.

### Why a per-pane sidecar (not one per process)

Could be ONE sidecar shared across all panes, but each pane wants its own cwd + env + kubectl context. Per-pane keeps the protocol simple (no pane-id routing on every request, no cross-pane state corruption) and the cost is modest: a single `bash --rcfile` with stdio pipes consumes ~5 MB and zero CPU when idle. With 5 panes that's 25 MB — fine.

If a future profiling pass shows this is too much, the sidecar can be lazily-spawned (no sidecar until first Tab) and idle-timed-out (already the plan).

## Source 3 — local heuristics

Always available; lives in `completion::local`. No process spawn, no IPC. Pure functions over the editor buffer + the pane's cwd + the recorded history.

| Sub-source | Trigger | Output |
|---|---|---|
| Path completion | Token under cursor matches `^[./~]` or contains `/` | Directory listing filtered by prefix; trailing `/` for dirs |
| `$PATH` executable scan | First token (the command), no `/` in it | Walk `$PATH` once per Tab, filter by prefix; cache the executable list per cwd at 10s TTL |
| History match | First token matches the start of a previous command | Pull from the `runs` table ([spec/07](07-history-and-search.md)) filtered by current cwd; ranks by recency × frequency |

### Quoting and escaping

The token under the cursor is found **quote- and escape-aware** ([`completion::local::completion_context`](../src/completion/local.rs)), so a quoted or escaped filename completes correctly:

- An **opening quote** bounds the token. `ls "my fi⇥` completes against `my fi` (spaces inside the quote do **not** split the token, unlike an unquoted space), and the replaced range starts just after the quote so accepting lands the name inside the quotes. A backslash-escaped space (`ls my\ fi⇥`) is likewise one token; the matched prefix is the **unescaped** literal (`my fi`).
- The **substituted value** is escaped for the context it lands in ([`completion::local::escape_for_context`](../src/completion/local.rs)) while the popup's **display** stays the plain, human-readable name:
  - unquoted → backslash-escape whitespace and shell metacharacters (`my file.txt` → `my\ file.txt`), leaving `/` intact so a path stays a path;
  - inside `"…"` → escape only `"`, `$`, `` ` ``, `\` (spaces and globs are literal there);
  - inside `'…'` → pass through unchanged.
- The `./` disambiguation prefix added to a non-pathish argument (`C⇥` → `./CLAUDE.md`) is **suppressed inside an explicit quote** — a quoted bare name needs no disambiguation.
- A candidate's **`value` is the full replacement for the whole token** (`origin_byte..origin_byte + token_len`), never just the part after the last `/`. The local path source already obeys this — it rewrites a bare directory entry into the full path the user typed (`~/Lib⇥` → `~/Library`, `src/Ca⇥` → `src/Cargo.toml`). Shell sidecars instead return only the **last path segment** (`complete -C "cd ~/Lib"` → `Library`); accepting that verbatim would replace the whole token and drop the `~/` prefix (`cd ~/Lib⇥` → `cd Library`, the wrong directory unless cwd is `~`). [`resolve_driver`](../src/completion/mod.rs) therefore **realigns** every driver/sidecar value to the same convention — prepending the token's directory prefix (everything up to and including its last `/`) when the value doesn't already carry it — before merging with the local sources. The popup's **display** still shows the bare segment.
- For a **path-shaped token** — i.e. when the completion is a **path extension** — completion **must not extend to a bad path**. (Not every completion is a path extension: command names, subcommands, branches, and flags are not, and pass through untouched.) The key asymmetry is that the **local path source is the authoritative, complete listing** of the directory under the cursor, while zsh's path completion is noisy — it emits the typed path's own ancestor components (`cd /usr/⇥` → `usr`; `cd /usr/bin/af⇥` → `usr`, `bin`) and *alternative names for ambiguous intermediate components* (`cd /usr/lib/dtrace/arm/⇥` → `libexec`, `arm64`, from `lib`/`libexec` and `arm`/`arm64`), all verified against the live captive child. Aligned, those become paths that don't exist (`/usr/lib/dtrace/arm/libexec`). This is handled as a **routing decision**, not a post-hoc filter:
  - **When the local listing succeeded** (path-shaped token, local candidates non-empty): [`plan_completion`](../src/completion/mod.rs) **skips the sidecar entirely** and opens the local rows immediately — no `AwaitDriver`, no spinner, no latency. The local rows already carry the trailing `/`, the full-path display, and the correct escaping, and the sidecar could only add redundant or junk rows. This is what collapses the original double-row, ensures a directory accept ends in `/` (not a space), and removes the ancestor / intermediate-component noise — structurally, since the noisy candidates are never requested.
  - **When there is no local listing** (the directory couldn't be read, or it isn't a filesystem path at all): the sidecar still fires — it may be the only source. The driver result then has no authoritative oracle, so [`resolve_driver`](../src/completion/mod.rs) falls back to two cheap heuristics — the value must extend the token (case-insensitive), and its leaf must not be one of the token's own directory components — to cull the obvious junk.

  Non-path tokens (no `/`) always consult the sidecar: fuzzy/substring command completions must survive, and a command on both `$PATH` and the sidecar still collapses via the ranker.
- Every surviving driver/sidecar value is **canonicalised then escaped for the shell context** ([`resolve_driver`](../src/completion/mod.rs)), exactly matching how the local path source escapes its own values — this matters for the no-local-listing path case and for non-path argument filenames:
  - **Canonicalise** — unescape the value to its literal ([`unescape`](../src/completion/local.rs)). zsh's `compadd` capture emits the SAME match in BOTH forms (verified live): raw `Application Support` *and* pre-escaped `Application\ Support`. Unescaping both to the literal lets them collapse to one candidate; without it the pre-escaped form would double-escape to `Application\\\ Support`.
  - **Escape** — re-escape the literal for the cursor's quote state ([`escape_for_context`](../src/completion/local.rs)), so a completion with a space round-trips (`Application Support` → `Application\ Support`). Escaping applies even for a non-path token, so an argument-position filename with a space (`vim my\ fi⇥`) round-trips too.

Local heuristics are **synchronous**. They run on the main thread, never spawn anything, and complete in well under 5 ms. They always populate the popup before sources 1 and 2 have a chance to respond — which is the point: the popup opens instantly, then the more-expensive sources stream in candidates as they arrive.

## Popup widget

A native egui popup, anchored to the editor's caret. Same widget surface as the [Ctrl+R history overlay](04-prompt-editor.md#history-popup) — they share the popup chrome helpers in [`src/render.rs`](../src/render.rs).

```
                        ┌──────────────────────────────────────┐
   $ kubectl get po    │  ▷ pod                  k8s • Pods…  │  ← driver source
                       │    podsecuritypolicy    k8s          │
                       │  ▷ podlist              alias        │  ← sidecar (custom alias)
                       │  ▷ podman                            │  ← local $PATH
                       │                                      │
                       │     [Tab] / [Enter]   ↑/↓   Esc      │
                       └──────────────────────────────────────┘
```

### Visible affordances per row

- **Source tag** (`k8s` / `alias` / `local` / `git` / …) on the right, dim. Tells the user where the candidate came from when the same prefix matches multiple sources.
- **Description** (one line, truncated with ellipsis at viewport width) when the source provides one. None of the heuristics provide descriptions; drivers and sidecars often do.
- **Prefix-match highlight** — the typed prefix is bold within each candidate's display string.

### Keystrokes inside the popup

| Key | Action |
|---|---|
| `Tab` | Accept the highlighted candidate (replace the token under the cursor). On a single-candidate result, the popup may auto-accept — configurable, default off. |
| `Enter` | Same as Tab. |
| `↑` / `↓` | Walk the candidate list. Wraps at the ends. Live-extends the highlighted candidate's preview into the editor (inline ghost text) — same convention as fish's autosuggestions. |
| `Esc` | Dismiss without accepting. The editor buffer is restored to whatever it was before the popup opened. |
| `Backspace` | Trim the last char from the partial typed prefix; the popup re-filters. If the prefix becomes empty AND the popup was opened by Tab (not by typing), the popup closes. |
| Any printable char | Inserted into the editor; the popup re-filters on the new prefix. |
| `Ctrl+R` | The history overlay takes precedence — close the completion popup, open the history overlay. |

### Ranking

The merged candidate list is ranked by a small score:

```
score = source_weight              // 1.0 for driver, 0.7 for sidecar, 0.5 for local
      + 0.5 * prefix_match_density // typed-chars-matched / candidate-length
      + 0.3 * recency_bonus        // 1 if this candidate has been chosen in this pane within `RECENT_WINDOW_SECS`; 0 otherwise
      + 0.2 * cwd_bonus            // 1 if the candidate came from a source that knew the cwd (drivers + sidecar always; local for paths)
```

Ties broken alphabetically. The constants live in `src/completion/ranking.rs` and have unit tests over hand-crafted candidate lists so future tuning doesn't accidentally regress an established preference.

**As built (slice 2):** `ranking::source_weight` is the only term wired up so far — the prefix-density / recency / cwd bonuses land with the shell-sidecar slices. Drivers use weight **`1.2`**, not the nominal `1.0` above, so they sort above the already-tuned local triad (History `1.0`, `$PATH` `0.8`, path `0.6`) without re-tuning it. The full re-weighting (driver `1.0` / sidecar `0.7` / local `0.5` plus the bonus terms) is part of the sidecar work, where all three source tiers coexist.

### Source merge

If two sources return the same `value`, they collapse into one row (preserving the longer description and the higher-priority source tag). This avoids "kubectl pod" appearing twice when both the kubectl driver and the user's `alias kubectl` sidecar entry produce it.

## Caching

> **Status (as built):** the **driver result cache shipped** in its own fast-follow PR (`completion::drivers::cache`). Key `(tool, cwd, line)`, 10 s TTL, pane-scoped (dropped on pane close), keyed pure-functionally on monotonic milliseconds from an injected `Clock` (`SystemClock` in prod; a fake that advances on demand in tests, so the TTL logic needs no `Instant::now`). A hit is served synchronously by `CompletionDriverEngine::request` (stashed for the same frame's `poll`) with **no subprocess**; a non-empty miss response is stored by `poll` on its way out. **Empty results are not cached** — an absent tool re-fails cheaply and a transient timeout stays free to retry. Expired entries are swept on insert so the map can't grow unbounded. The other rows below (`$PATH`, history, sidecar) remain target design for later slices. `(source, …)` in the key prose is conceptual; the driver cache key is `(tool, cwd, line)`. Explicit refresh (Cmd+Shift+R) is not yet wired — a future polish.

The expensive sources (drivers, sidecar) cache aggressively. The cache key is `(source, tool, cwd, partial_line)` — a kubectl driver call for `kubectl get pods` in `/home/tim` is cached separately from the same call in `/home/tim/work`.

| Cache | TTL | Invalidation |
|---|---|---|
| Driver detection (kubectl exists, gh exists, …) | Per-process | None — re-detected on next process start |
| Driver call results (`kubectl __complete pod`) | 10 seconds | Cwd change; explicit refresh (Cmd+Shift+R inside popup) |
| Sidecar call results | 5 seconds | Cwd change; env change; explicit refresh |
| `$PATH` executable list | 10 seconds | Cwd change (yes — some env mutations change `$PATH`) |
| History matches | 1 second | Submit (new history entry) |

`$PATH` deserves its own TTL because some users use `direnv` / `asdf` / `nvm` / `mise` to install per-directory tool versions; the executable list changes on every `cd`.

All caches are pane-scoped. Closing a pane drops its caches.

## Integration with the editor

The popup is a state on the editor — see the `completion: Option<CompletionPopup>` field in [the editor model](04-prompt-editor.md#the-editor-model). When the popup is open:

- The editor still owns text + cursor + selection. The popup is a view layer over a snapshot of those.
- Arrow keys route to the popup, not the editor's history walk (spec/04 §"History walk").
- Backspace edits the editor buffer (and re-filters the popup), not the popup directly.
- Submit (Enter) accepts the popup — does NOT submit the command. To submit the original buffer, press Esc first.
- Click outside the popup closes it. Click on a candidate accepts it.

The popup's "open" event is what creates the candidate request. Reopening (close → open) re-requests; we don't cache the popup's own state.

## Trigger semantics

Tab opens the popup. **Inside an active popup**, Tab cycles the highlighted candidate forward (with `Shift+Tab` for backward) — same convention as VS Code, JetBrains, etc. A second bare Tab (no candidate visible because all sources returned empty) inserts a literal `\t` only if `bash`/`zsh` style "expand-tabs" is configured ON; default is to do nothing (a tab in a command line is almost always a mistake).

Auto-trigger on typing — like fish's autosuggestions popping up as you type — is **post-completion-spec**: a follow-up design that piggybacks on this engine. We don't promise it; we don't preclude it.

## What this spec does NOT cover

These are intentionally out of scope and have their own follow-ups:

- **Inline ghost-text suggestions** as the user types (fish-style autosuggestions). Same engine, different render path. Future work.
- **Multi-shell coordination**. Each pane has its own sidecar; we don't sync state across them. If a user has two zsh panes with different `KUBECONFIG`, each gets its own completions. That's correct, not a bug.
- **Sandboxing**. The sidecar runs the user's rc with the user's privileges. Same trust model as the existing managed shell integration ([spec/03](03-shell-integration.md)).
- **Completion of arguments to programs that take a DSL** (e.g. `git rebase -i HEAD~3` — the `HEAD~3` is git-revision syntax). The driver (`git --list-cmds` or the sidecar's `_git` function) handles it; we don't parse git revisions ourselves.
- **Snippet expansion / template macros**. Future work; orthogonal to completion.
- **Custom user-defined completion sources via plugins.** Phase 10+. The plugin API doesn't exist yet.

## Testing

The strict-layer rule ([CLAUDE.md](../CLAUDE.md)) applies to the whole completion stack: ranking, parsing, caching, request/response framing, and the popup state machine. Tests-first, same commit.

- **Unit (strict)**: per-driver parse functions (`parse_kubectl_complete`, `parse_aws_completer`, …) take a recorded stdout string and a partial line and produce a `Vec<Candidate>`. Recorded fixtures live under [`testdata/completion/<tool>/`](../testdata/completion/).
- **Unit (strict)**: `ranking::score(candidate, source, history)` is pure. Cover the four bonus components individually.
- **Unit (strict)**: sidecar protocol framing — request / response JSON shape, id correlation, partial-read tolerance.
- **Integration**: spawn a real bash sidecar with our integration helper installed, send a completion request, assert the JSON. Same for zsh. Same for fish (the easy one). Lives in [`tests/`](../tests/).
- **Snapshot**: completion popup at various states (empty, single-source, multi-source, with descriptions, prefix-highlight) renders deterministically. `egui_kittest` per [spec/09](09-testing.md).
- **Failure-mode tests**: driver missing → no kubectl candidates; sidecar crashes → fall through to local; sidecar slow (>250 ms) → popup opens with locals, drivers stream in later; ambiguous candidate from sidecar with malformed JSON → drop and continue.

## Roadmap

This design is targeted at **post-MVP**. The actual implementation slices:

1. **Phase 4I — MVP local completion** ✅ (shipped). Source 3 only. Paths + `$PATH` + history. The popup widget lands here. Tab works for the common cases; advanced commands fall back to `\t`-doesn't-go-to-PTY → no completion.
2. **CLI-native drivers** ✅ (shipped). `kubectl`/`gh`/`docker` (cobra `__complete`), `aws_completer`, `git --list-cmds`. Source 1 enabled; popup gains source tags; candidates stream into the open popup off-thread (per-pane worker + `egui::Context` repaint, mirroring `git_probe`). Detection is implicit (spawn-failure = silent no-op); the result cache is a separate fast-follow PR. `completion::drivers`.
   - **2a — Driver result cache** ✅ (shipped). The 10 s TTL cache in [§Caching](#caching) + the injectable `Clock` its tests need. `completion::drivers::cache`.
3. **Post-MVP — Fish sidecar.** Cleanest of the three sidecars, so it lands first as the reference for the protocol. About 400–600 LOC including the helper script.
4. **Post-MVP — Bash sidecar.** Vendored helper + idle-timeout lifecycle + crash recovery. About 800–1100 LOC.
5. **Post-MVP — Zsh sidecar.** Most fragile; ships after the bash one stabilises so we have a known-good baseline. About 1000–1400 LOC including the helper.

Each slice is a separate PR with its own GH Issue. The protocol is fixed at slice 2 and only versioned forward if a sidecar slice needs a new request kind.

The original v1 stance in [spec/10 §"Post-MVP"](10-roadmap.md#post-mvp-probably-yes) — "Shell completion bridge (zsh + bash) via a private OSC request/response" — is **superseded by this document**. The bridge is no longer "OSC over the PTY"; it's a private stdio sidecar with newline-delimited JSON. Same intent, cleaner mechanism. Spec/10's row will be updated when slice 1 lands.

---

**← Previous:** [04 — Prompt editor](04-prompt-editor.md) | **Next:** [05 — Pane modes](05-pane-modes.md) →
