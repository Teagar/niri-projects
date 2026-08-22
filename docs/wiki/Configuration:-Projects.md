### Overview

You can define **projects** — named groups of workspaces — in the config. Projects act as a third spatial axis alongside the existing workspace and column dimensions, allowing you to switch between entire desktop environments instantly.

This is similar to tmux sessions or tmuxinator: each project is a self-contained set of workspaces, and switching between them parks the outgoing project's windows alive off-screen while the incoming project's workspaces slide into view.

```kdl
projects {
    project "neovim" {
        keep-open

        workspace "editor" {
        }

        workspace "docs" {
        }
    }

    project "writing" {
        workspace "notes" {
        }
    }
}
```

When no projects are defined, niri behaves exactly as before — there is zero overhead and no behavioral change.

### Concepts

| State | Meaning |
|---|---|
| **Dormant** (default) | No workspaces resident. Zero cost beyond the stored config definition. |
| **Warm** | Workspaces are parked off-screen; windows and processes are alive and reachable, but invisible. |
| **Active** | This project's workspaces are attached to the current monitor. Only one project is active at a time. |

Switching to a project transitions it to Active (parking the previous project to Warm).
An explicit close returns a Warm project to Dormant, destroying its parked workspaces.

### Configuration

#### `projects {}` block

Contains one or more `project` children. Each project has:

- **name** (string argument, required) — unique identifier, e.g. `"neovim"`.
- **`keep-open`** (flag, optional) — when set, the project's workspaces remain warm even when idle.
- **`workspace`** children — each defines a workspace belonging to the project. Workspaces accept the same structure as top-level named workspaces (name, spawn-at-startup commands).

```kdl
projects {
    project "my-project" {
        keep-open

        workspace "main" {
            spawn-at-startup "foot" "-e" "nvim" "."
            spawn-at-startup "foot" "-e" "lazygit"
        }

        workspace "browser" {
            spawn-at-startup "firefox"
        }
    }
}
```

Workspaces defined inside a project use the same global layout settings unless overridden.

#### `project-switch` animation

Configure the animation used when switching between projects:

```kdl
animations {
    project-switch {
        spring damping-ratio=1.0 stiffness=800 epsilon=0.0001
        // or: off
    }
}
```

Accepts the same parameters as [`workspace-switch`](./Configuration:-Animations.md).

### Key binds

| Action | Description |
|---|---|
| `switch-project "name"` | Switch to the named project (activates it if dormant/warm, no-op if already active). |
| `close-project "name"` | Close a project, destroying its parked workspaces. |
| `keep-project-open "name"` | Pre-warm a project (future use — runs startup commands without activating). |
| `toggle-project-overview` | Open/close the project overview with staggered depth display. |

Example binds:

```kdl
binds {
    Mod+Shift+P { switch-project "neovim"; }
    Mod+Alt+P   { switch-project "writing"; }
    Mod+Ctrl+P  { close-project "neovim"; }
    Mod+P       { toggle-project-overview; }
}
```

### IPC

New request and event types extend the niri IPC protocol:

**Requests:**
- `niri msg projects` — list all projects and their current state.

**Actions:**
- `niri msg action switch-project "name"` — switch to a project.
- `niri msg action close-project "name"` — close a project.
- `niri msg action keep-project-open "name"` — pre-warm a project.
- `niri msg action toggle-project-overview` — toggle the project overview.

**Events** (event stream):
- `ProjectsChanged { projects }` — full replacement of project state.
- `ProjectActivated { project_name }` — a project became active.
- `ProjectClosed { project_name }` — a project was closed.

### Notes

- Switching away from a project always parks its workspaces. Windows and processes remain alive.
- Only an explicit `close-project` action destroys a project's resources.
- Project workspace names are part of the global named-workspace namespace; avoid name collisions across projects.
- Per-output project placement (`open-on-output`) is deferred to a future version.
