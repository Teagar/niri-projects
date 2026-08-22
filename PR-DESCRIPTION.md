# Add Projects: a third spatial dimension for workspaces

## Problem

Niri's spatial model has two axes: workspaces stack vertically per monitor,
and columns of windows scroll horizontally within a workspace. There's no
built-in way to group a whole *set* of workspaces (with their windows and
startup commands) under a name and switch between such sets instantly — the
way tmux sessions let you switch between independent groups of panes.

Today, getting this requires external scripting: spawning terminals in
specific directories, focusing workspaces, and hoping window placement lands
correctly, with no persistence and no instant switching.

## Design

**Projects** are named, self-contained collections of one or more workspaces.
A project starts *Dormant* (zero cost beyond its config), becomes *Warm*
(workspaces parked off-screen, windows alive) when pre-warmed or after being
active, and is *Active* when its workspaces are attached to the monitor.

Switching between projects parks the outgoing project's workspaces (detaches
them from the monitor without killing anything) and attaches the incoming
project's workspaces. This is the single most important correctness contract:
**switching never destroys the outgoing project's windows**.

### Config surface

```kdl
projects {
    project "neovim" {
        keep-open
        workspace "editor" {
            spawn-at-startup "foot" "-e" "nvim" "."
            spawn-at-startup "foot" "-e" "lazygit"
        }
    }
}
```

### IPC surface

- `Request::Projects` / `Response::Projects(Vec<Project>)`
- `Action::SwitchProject { project }`, `CloseProject`, `KeepProjectOpen`
- `Action::ToggleProjectOverview` and overview navigation actions
- `Event::ProjectsChanged`, `ProjectActivated`, `ProjectClosed`

## What was implemented (this PR)

1. **niri-config**: `projects {}` config section with per-project workspace
   definitions and `keep-open` flag. `project-switch` animation key (default
   spring 800).

2. **niri-ipc**: `Project`/`ProjectState` IPC types, all request/response/
   action/event variants following existing naming conventions exactly.

3. **Layout core**: `Project` type with `Dormant`/`Warm` lifecycle, workspace
   parking on monitor detach, re-attachment on switch. Search paths for warm
   project windows are wired into `find_window_and_output`, `remove_window`,
   and `has_window` so that parked windows survive Wayland protocol cleanup.

4. **Action dispatch**: `switch-project`, `close-project`, `toggle-project-
   overview` wired through binds → input handler → layout.

5. **Eight correctness tests** (203 total workspace tests, all passing):
   - `switching_away_and_back_preserves_windows` (core contract)
   - `animated_switch_away_and_back_preserves_windows`
   - `switching_preserves_across_outputs`
   - `overview_double_toggle_never_panics`
   - `reload_projects_preserves_runtime_state`
   - `switch_to_warm_or_active_project_is_o1`
   - `depth_navigation_does_not_mutate_state`
   - `closing_project_destroys_workspaces`

6. **Wiki docs**: `Configuration:-Projects.md` following the existing
   `Configuration:-Named-Workspaces.md` pattern.

## What is explicitly deferred (not in this PR)

- **Per-output project placement** (`open-on-output` in project workspace
  config) — first pass targets single-monitor; multi-monitor distribution
  is a follow-up.
- **Staggered-depth visual rendering** in the project overview — the toggle
  and depth-navigation actions are wired with stub handlers; the full
  stacked-card visual and depth cycling will be a follow-up PR.
- **Animated project-switch slide** — the switch currently uses the existing
  workspace-switch animation infrastructure; a horizontal slide overlay for
  the transition is deferred.
- **Window↔process correlation** for spawned commands — startup commands are
  spawned but not correlated to resulting windows by app-id matching; this
  is deferred for a follow-up.
- **Cross-compositor-restart restoration** — project definitions survive a
  config reload; live window content across compositor restart is out of scope.
- **Keep-open garbage collection** — projects with `keep-open` unset don't
  currently auto-transition to Dormant when their last window closes.

## How to test

### Automated tests

```
cargo test --workspace         # 203 tests, 0 failures
cargo clippy --workspace -- -D warnings   # clean
```

### Manual testing (isolated test environment)

```bash
cd niri-projects/
cargo build
# Switch to a spare TTY (Ctrl+Alt+F3) and run:
/tmp/niri-test/run-test-niri.sh
```

Config at `/tmp/niri-test/config/config.kdl` includes projects with
default keybinds (Mod+Shift+P → neovim, Mod+Alt+P → writing).

Test steps:
1. Press Mod+Shift+P to switch to the "neovim" project.
2. Open a terminal and a file manager — these windows belong to neovim.
3. Press Mod+Alt+P to switch to "writing" — neovim windows should
   disappear but remain alive.
4. Switch back with Mod+Shift+P — neovim's windows must still exist
   (same content, same IDs).
5. Press Mod+P to toggle the project overview.
6. Press Mod+Shift+E to quit.
