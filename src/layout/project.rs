//! Projects: a third spatial dimension for workspaces.
//!
//! A project is a named collection of workspaces. Exactly one project may be
//! *active* (its workspaces resident on monitors); others are either *warm*
//! (workspaces parked off-screen, windows and processes alive) or *dormant*
//! (nothing resident, zero cost beyond the stored snapshot).
//!
//! The layout tracks runtime state only; the source of truth for project
//! definitions is the config, mirrored here as a snapshot (same pattern as
//! named workspaces keeping their [`niri_config::WorkspaceConfig`]).

use niri_config::ProjectConfig;

use super::workspace::{Workspace, WorkspaceId};

/// Runtime state of one configured project.
#[derive(Debug)]
pub(super) struct Project<W: super::LayoutElement> {
    /// Snapshot of the config this project was created from. Updated on
    /// config reload while preserving runtime state.
    pub config: ProjectConfig,
    /// Stable color index for overview card rendering (assigned once).
    #[allow(dead_code)]
    pub color_index: usize,
    /// Runtime residency state.
    pub kind: ProjectKind<W>,
}

#[derive(Debug)]
pub(super) enum ProjectKind<W: super::LayoutElement> {
    /// Nothing resident.
    Dormant,
    /// Workspaces parked off-screen, alive and invisible.
    Warm {
        /// Parked workspaces, in their original order.
        workspaces: Vec<Workspace<W>>,
        /// Index of the workspace that was active when parked.
        active_workspace_idx: usize,
        /// Ids of the parked workspaces for quick lookup.
        #[allow(dead_code)]
        ids: Vec<WorkspaceId>,
    },
}

#[allow(dead_code)]
impl<W: super::LayoutElement> Project<W> {
    pub fn new(config: ProjectConfig, color_index: usize) -> Self {
        Self {
            config,
            color_index,
            kind: ProjectKind::Dormant,
        }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn keep_open(&self) -> bool {
        self.config.keep_open
    }

    pub fn is_dormant(&self) -> bool {
        matches!(self.kind, ProjectKind::Dormant)
    }

    pub fn is_warm(&self) -> bool {
        matches!(self.kind, ProjectKind::Warm { .. })
    }

    /// True if the given workspace id is parked in this project.
    pub fn owns_workspace(&self, id: WorkspaceId) -> bool {
        match &self.kind {
            ProjectKind::Dormant => false,
            ProjectKind::Warm { ids, .. } => ids.contains(&id),
        }
    }

    /// Take all parked workspaces out (project becomes dormant).
    ///
    /// Returns the workspaces in their original order along with the parked
    /// active index.
    pub fn take_workspaces(&mut self) -> (Vec<Workspace<W>>, usize) {
        let old = std::mem::replace(&mut self.kind, ProjectKind::Dormant);
        match old {
            ProjectKind::Dormant => (vec![], 0),
            ProjectKind::Warm {
                workspaces,
                active_workspace_idx,
                ..
            } => (workspaces, active_workspace_idx),
        }
    }

    /// Park workspaces into this project (from an active state).
    pub fn park_workspaces(&mut self, workspaces: Vec<Workspace<W>>, active_workspace_idx: usize) {
        let ids = workspaces.iter().map(|ws| ws.id()).collect();
        self.kind = ProjectKind::Warm {
            workspaces,
            active_workspace_idx,
            ids,
        };
    }
}

/// Information about what happened when switching to a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSwitchResult {
    /// Name of the project that became active.
    pub activated_name: String,
    /// Startup commands to run (non-empty only if the project was dormant
    /// and just transitioned to active).
    pub startup_commands: Vec<Vec<String>>,
    /// Whether the project had warm (parked) workspaces (false for a
    /// freshly-created dormant activation).
    pub was_warm: bool,
}
