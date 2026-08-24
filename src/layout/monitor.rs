use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use std::iter::zip;
use std::rc::Rc;
use std::time::Duration;

use niri_config::{CornerRadius, LayoutPart};
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use super::insert_hint_element::{InsertHintElement, InsertHintRenderElement};
use super::scrolling::{Column, ColumnWidth};
use super::tile::Tile;
use super::workspace::{
    compute_working_area, OutputId, Workspace, WorkspaceAddWindowTarget, WorkspaceId,
    WorkspaceRenderElement,
};
use super::{
    compute_overview_zoom, ActivateWindow, HitType, LayoutElement, Options, ProjectOverviewEntry,
    ProjectOverviewItem,
};
use crate::animation::{Animation, Clock, Curve};
use crate::input::swipe_tracker::SwipeTracker;
use crate::layout::RenderLayer;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::{AsGlesRenderer, NiriRenderer};
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::rubber_band::RubberBand;
use crate::utils::transaction::Transaction;
use crate::utils::{
    output_size, round_logical_in_physical, round_logical_in_physical_max1, ResizeEdge,
};

/// Amount of touchpad movement to scroll the height of one workspace.
const WORKSPACE_GESTURE_MOVEMENT: f64 = 300.;

const WORKSPACE_GESTURE_RUBBER_BAND: RubberBand = RubberBand {
    stiffness: 0.5,
    limit: 0.05,
};

/// Amount of DnD edge scrolling to scroll the height of one workspace.
///
/// This constant is tied to the default dnd-edge-workspace-switch max-speed setting.
const WORKSPACE_DND_EDGE_SCROLL_MOVEMENT: f64 = 1500.;

// ── Project overview drawer geometry (screen pixels, multiplied by scale) ──

/// Card width as a fraction of the output view width.
const DRAWER_CARD_WIDTH_FRAC: f64 = 0.62;
/// Vertical center of the card block, as a fraction of the view height.
const DRAWER_CARD_CENTER_Y_FRAC: f64 = 0.52;
/// Downward offset per depth step behind the front card.
const DRAWER_STEP_Y: f64 = 26.;
/// Scale reduction per depth step.
const DRAWER_SCALE_STEP: f64 = 0.06;
/// Darkening overlay alpha added per depth step.
const DRAWER_DIM_STEP: f64 = 0.16;
/// Maximum visible depth steps behind the front card.
const DRAWER_MAX_VISIBLE_DEPTH: f64 = 3.;
/// Card corner radius and border width.
const DRAWER_CARD_RADIUS: f64 = 14.;
const DRAWER_BORDER_WIDTH: f64 = 2.;
/// Folder tab geometry.
const DRAWER_TAB_HEIGHT: f64 = 30.;
const DRAWER_TAB_RADIUS: f64 = 10.;
const DRAWER_TAB_INSET_X: f64 = 22.;
const DRAWER_TAB_STAGGER_X: f64 = 46.;
/// Drawer backdrop color.
const DRAWER_BACKDROP_COLOR: [f32; 4] = [0.043, 0.051, 0.071, 1.];
/// Placeholder (dormant) card fill.
const DRAWER_PLACEHOLDER_COLOR: [f32; 4] = [0.071, 0.082, 0.11, 1.];
/// Tab text color (dark on colored tab).
const DRAWER_TAB_TEXT_COLOR: [f64; 4] = [0.02, 0.027, 0.039, 1.];
/// Duration of the drawer glide animations, in ms.
const DRAWER_ANIM_MS: u64 = 320;

/// Cache key and value for folder tab textures.
type TabCache = HashMap<(String, &'static str, u32, u64), TextureBuffer<GlesTexture>>;

#[derive(Debug)]
pub struct Monitor<W: LayoutElement> {
    /// Output for this monitor.
    pub(super) output: Output,
    /// Cached name of the output.
    output_name: String,
    /// Latest known scale for this output.
    scale: smithay::output::Scale,
    /// Latest known size for this output.
    view_size: Size<f64, Logical>,
    /// Latest known working area for this output.
    ///
    /// Not rounded to physical pixels.
    // FIXME: since this is used for things like DnD scrolling edges in the overview, ideally this
    // should only consider overlay and top layer-shell surfaces. However, Smithay doesn't easily
    // let you do this at the moment.
    working_area: Rectangle<f64, Logical>,
    // Must always contain at least one.
    pub(super) workspaces: Vec<Workspace<W>>,
    /// Index of the currently active workspace.
    pub(super) active_workspace_idx: usize,
    /// ID of the previously active workspace.
    pub(super) previous_workspace_id: Option<WorkspaceId>,
    /// In-progress switch between workspaces.
    pub(super) workspace_switch: Option<WorkspaceSwitch>,
    /// Indication where an interactively-moved window is about to be placed.
    pub(super) insert_hint: Option<InsertHint>,
    /// Insert hint element for rendering.
    insert_hint_element: InsertHintElement,
    /// Location to render the insert hint element.
    insert_hint_render_loc: Option<InsertHintRenderLoc>,
    /// Whether the overview is open.
    pub(super) overview_open: bool,
    /// Progress of the overview zoom animation, 1 is fully in overview.
    overview_progress: Option<OverviewProgress>,
    /// Animated drawer depth per project card (project index → depth).
    project_drawer_depths: HashMap<usize, f64>,
    /// In-flight glide animations per project card.
    project_drawer_anims: HashMap<usize, Animation>,
    /// Cached folder tab textures, keyed by (name, state, color bits, scale bits).
    project_tab_cache: RefCell<TabCache>,
    /// Cached rounded border overlay textures, keyed by
    /// (color bits, width px, height px).
    project_border_cache: RefCell<HashMap<(u64, i32, i32), TextureBuffer<GlesTexture>>>,
    /// Regular (non-project) workspaces saved aside while a project is active.
    ///
    /// While a project is active, this monitor holds ONLY the project's
    /// workspaces; the regular set is parked here and restored when the
    /// project deactivates. This guarantees windows cannot land outside the
    /// active project while it is resident.
    pub(super) saved_regular_workspaces: Option<Vec<Workspace<W>>>,
    /// Clock for driving animations.
    pub(super) clock: Clock,
    /// Configurable properties of the layout as received from the parent layout.
    pub(super) base_options: Rc<Options>,
    /// Configurable properties of the layout.
    pub(super) options: Rc<Options>,
    /// Layout config overrides for this monitor.
    layout_config: Option<niri_config::LayoutPart>,
}

#[derive(Debug)]
pub enum WorkspaceSwitch {
    Animation(Animation),
    Gesture(WorkspaceSwitchGesture),
}

#[derive(Debug)]
pub struct WorkspaceSwitchGesture {
    /// Index of the workspace where the gesture was started.
    center_idx: usize,
    /// Fractional workspace index where the gesture was started.
    ///
    /// Can differ from center_idx when starting a gesture in the middle between workspaces, for
    /// example by "catching" an animation.
    start_idx: f64,
    /// Current, fractional workspace index.
    pub(super) current_idx: f64,
    /// Animation for the extra offset to the current position.
    ///
    /// For example, if there's a workspace switch during a DnD scroll.
    animation: Option<Animation>,
    tracker: SwipeTracker,
    /// Whether the gesture is controlled by the touchpad.
    is_touchpad: bool,
    /// Whether the gesture is clamped to +-1 workspace around the center.
    is_clamped: bool,

    // If this gesture is for drag-and-drop scrolling, this is the last event's unadjusted
    // timestamp.
    dnd_last_event_time: Option<Duration>,
    // Time when the drag-and-drop scroll delta became non-zero, used for debouncing.
    //
    // If `None` then the scroll delta is currently zero.
    dnd_nonzero_start_time: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InsertPosition {
    NewColumn(usize),
    InColumn(usize, usize),
    Floating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InsertWorkspace {
    Existing(WorkspaceId),
    NewAt(usize),
}

#[derive(Debug)]
pub(super) struct InsertHint {
    pub workspace: InsertWorkspace,
    pub position: InsertPosition,
    pub corner_radius: CornerRadius,
}

#[derive(Debug, Clone, Copy)]
struct InsertHintRenderLoc {
    workspace: InsertWorkspace,
    location: Point<f64, Logical>,
}

#[derive(Debug)]
pub(super) enum OverviewProgress {
    Animation(Animation),
    Value(f64),
}

/// Where to put a newly added window.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum MonitorAddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// On this workspace.
    Workspace {
        /// Id of the target workspace.
        id: WorkspaceId,
        /// Override where the window will open as a new column.
        column_idx: Option<usize>,
    },
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

impl<'a, W: LayoutElement> Copy for MonitorAddWindowTarget<'a, W> {}

impl<'a, W: LayoutElement> Clone for MonitorAddWindowTarget<'a, W> {
    fn clone(&self) -> Self {
        *self
    }
}

niri_render_elements! {
    MonitorInnerRenderElement<R> => {
        Workspace = CropRenderElement<WorkspaceRenderElement<R>>,
        InsertHint = CropRenderElement<InsertHintRenderElement>,
        UncroppedInsertHint = InsertHintRenderElement,
        Shadow = ShadowRenderElement,
        SolidColor = SolidColorRenderElement,
        Texture = PrimaryGpuTextureRenderElement,
    }
}

pub type MonitorRenderElement<R> =
    RelocateRenderElement<RescaleRenderElement<MonitorInnerRenderElement<R>>>;

impl WorkspaceSwitch {
    pub fn current_idx(&self) -> f64 {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.value(),
            WorkspaceSwitch::Gesture(gesture) => {
                gesture.current_idx + gesture.animation.as_ref().map_or(0., |anim| anim.value())
            }
        }
    }

    pub fn target_idx(&self) -> f64 {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.to(),
            WorkspaceSwitch::Gesture(gesture) => gesture.current_idx,
        }
    }

    pub fn offset(&mut self, delta: isize) {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.offset(delta as f64),
            WorkspaceSwitch::Gesture(gesture) => {
                if delta >= 0 {
                    gesture.center_idx += delta as usize;
                } else {
                    gesture.center_idx -= (-delta) as usize;
                }
                gesture.start_idx += delta as f64;
                gesture.current_idx += delta as f64;
            }
        }
    }

    fn is_animation_ongoing(&self) -> bool {
        match self {
            WorkspaceSwitch::Animation(_) => true,
            WorkspaceSwitch::Gesture(gesture) => gesture.animation.is_some(),
        }
    }
}

impl WorkspaceSwitchGesture {
    fn min_max(&self, workspace_count: usize) -> (f64, f64) {
        if self.is_clamped {
            let min = self.center_idx.saturating_sub(1) as f64;
            let max = (self.center_idx + 1).min(workspace_count - 1) as f64;
            (min, max)
        } else {
            (0., (workspace_count - 1) as f64)
        }
    }

    fn animate_from(&mut self, from: f64, clock: Clock, config: niri_config::Animation) {
        let current = self.animation.as_ref().map_or(0., Animation::value);
        self.animation = Some(Animation::new(clock, from + current, 0., 0., config));
    }
}

impl InsertWorkspace {
    fn existing_id(self) -> Option<WorkspaceId> {
        match self {
            InsertWorkspace::Existing(id) => Some(id),
            InsertWorkspace::NewAt(_) => None,
        }
    }
}

impl OverviewProgress {
    pub fn value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.value(),
            OverviewProgress::Value(v) => *v,
        }
    }

    pub fn clamped_value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.clamped_value(),
            OverviewProgress::Value(v) => *v,
        }
    }
}

impl From<&super::OverviewProgress> for OverviewProgress {
    fn from(value: &super::OverviewProgress) -> Self {
        match value {
            super::OverviewProgress::Animation(anim) => Self::Animation(anim.clone()),
            super::OverviewProgress::Gesture(gesture) => Self::Value(gesture.value),
            super::OverviewProgress::Open => Self::Value(1.),
        }
    }
}

impl<W: LayoutElement> Monitor<W> {
    pub fn new(
        output: Output,
        mut workspaces: Vec<Workspace<W>>,
        ws_id_to_activate: Option<WorkspaceId>,
        clock: Clock,
        base_options: Rc<Options>,
        layout_config: Option<LayoutPart>,
    ) -> Self {
        let options =
            Rc::new(Options::clone(&base_options).with_merged_layout(layout_config.as_ref()));

        let scale = output.current_scale();
        let view_size = output_size(&output);
        let working_area = compute_working_area(&output);

        // Prepare the workspaces: set output, empty first, empty last.
        let mut active_workspace_idx = 0;

        for (idx, ws) in workspaces.iter_mut().enumerate() {
            assert!(ws.has_windows_or_name());

            ws.set_output(Some(output.clone()));
            ws.update_config(options.clone());

            if ws_id_to_activate.is_some_and(|id| ws.id() == id) {
                active_workspace_idx = idx;
            }
        }

        if options.layout.empty_workspace_above_first && !workspaces.is_empty() {
            let ws = Workspace::new(output.clone(), clock.clone(), options.clone());
            workspaces.insert(0, ws);
            active_workspace_idx += 1;
        }

        let ws = Workspace::new(output.clone(), clock.clone(), options.clone());
        workspaces.push(ws);

        Self {
            output_name: output.name(),
            output,
            scale,
            view_size,
            working_area,
            workspaces,
            active_workspace_idx,
            previous_workspace_id: None,
            insert_hint: None,
            insert_hint_element: InsertHintElement::new(options.layout.insert_hint),
            insert_hint_render_loc: None,
            overview_open: false,
            overview_progress: None,
            project_drawer_depths: HashMap::new(),
            project_drawer_anims: HashMap::new(),
            project_tab_cache: RefCell::new(HashMap::new()),
            project_border_cache: RefCell::new(HashMap::new()),
            saved_regular_workspaces: None,
            workspace_switch: None,
            clock,
            base_options,
            options,
            layout_config,
        }
    }

    pub fn into_workspaces(mut self) -> Vec<Workspace<W>> {
        self.workspaces.retain(|ws| ws.has_windows_or_name());

        for ws in &mut self.workspaces {
            ws.set_output(None);
        }

        self.workspaces
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    pub fn output_name(&self) -> &String {
        &self.output_name
    }

    pub fn active_workspace_idx(&self) -> usize {
        self.active_workspace_idx
    }

    pub fn active_workspace_ref(&self) -> &Workspace<W> {
        &self.workspaces[self.active_workspace_idx]
    }

    pub fn find_named_workspace(&self, workspace_name: &str) -> Option<&Workspace<W>> {
        self.workspaces.iter().find(|ws| {
            ws.name
                .as_ref()
                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
        })
    }

    pub fn find_named_workspace_index(&self, workspace_name: &str) -> Option<usize> {
        self.workspaces.iter().position(|ws| {
            ws.name
                .as_ref()
                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
        })
    }

    pub fn active_workspace(&mut self) -> &mut Workspace<W> {
        &mut self.workspaces[self.active_workspace_idx]
    }

    pub fn idx_of_ws(&self, id: WorkspaceId) -> Option<usize> {
        self.workspaces.iter().position(|ws| ws.id() == id)
    }

    pub fn has_ws(&self, id: WorkspaceId) -> bool {
        self.idx_of_ws(id).is_some()
    }

    pub fn windows(&self) -> impl Iterator<Item = &W> {
        self.workspaces.iter().flat_map(|ws| ws.windows())
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.windows().any(|win| win.id() == window)
    }

    pub fn add_workspace_at(&mut self, idx: usize) {
        let ws = Workspace::new(
            self.output.clone(),
            self.clock.clone(),
            self.options.clone(),
        );

        self.workspaces.insert(idx, ws);
        if idx <= self.active_workspace_idx {
            self.active_workspace_idx += 1;
        }

        if let Some(switch) = &mut self.workspace_switch {
            if idx as f64 <= switch.target_idx() {
                switch.offset(1);
            }
        }
    }

    pub fn add_workspace_top(&mut self) {
        self.add_workspace_at(0);
    }

    pub fn add_workspace_bottom(&mut self) {
        self.add_workspace_at(self.workspaces.len());
    }

    /// Swap in a whole new workspace list, resetting view state.
    ///
    /// Used by project activation/parking where the entire residency set of
    /// this monitor changes atomically.
    pub(super) fn replace_workspaces(&mut self, workspaces: Vec<Workspace<W>>) {
        self.workspaces = workspaces;
        self.active_workspace_idx = 0;
        self.workspace_switch = None;
        self.previous_workspace_id = None;
    }

    pub fn activate_workspace(&mut self, idx: usize) {
        self.activate_workspace_with_anim_config(idx, None);
    }

    pub fn activate_workspace_with_anim_config(
        &mut self,
        idx: usize,
        config: Option<niri_config::Animation>,
    ) {
        // FIXME: also compute and use current velocity.
        let current_idx = self.workspace_render_idx();

        if self.active_workspace_idx != idx {
            self.previous_workspace_id = Some(self.workspaces[self.active_workspace_idx].id());
        }

        let prev_active_idx = self.active_workspace_idx;
        self.active_workspace_idx = idx;

        let config = config.unwrap_or(self.options.animations.workspace_switch.0);

        match &mut self.workspace_switch {
            // During a DnD scroll, we want to visually animate even if idx matches the active idx.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                gesture.center_idx = idx;

                // Adjust start_idx to make current_idx point at idx.
                let current_pos = gesture.current_idx - gesture.start_idx;
                gesture.start_idx = idx as f64 - current_pos;
                let prev_current_idx = gesture.current_idx;
                gesture.current_idx = idx as f64;

                let current_idx_delta = gesture.current_idx - prev_current_idx;
                gesture.animate_from(-current_idx_delta, self.clock.clone(), config);
            }
            _ => {
                // Don't animate if nothing changed.
                if prev_active_idx == idx {
                    return;
                }

                self.workspace_switch = Some(WorkspaceSwitch::Animation(Animation::new(
                    self.clock.clone(),
                    current_idx,
                    idx as f64,
                    0.,
                    config,
                )));
            }
        }
    }

    pub(super) fn resolve_add_window_target<'a>(
        &mut self,
        target: MonitorAddWindowTarget<'a, W>,
    ) -> (usize, WorkspaceAddWindowTarget<'a, W>) {
        match target {
            MonitorAddWindowTarget::Auto => {
                (self.active_workspace_idx, WorkspaceAddWindowTarget::Auto)
            }
            MonitorAddWindowTarget::Workspace { id, column_idx } => {
                let idx = self.idx_of_ws(id).unwrap();
                let target = if let Some(column_idx) = column_idx {
                    WorkspaceAddWindowTarget::NewColumnAt(column_idx)
                } else {
                    WorkspaceAddWindowTarget::Auto
                };
                (idx, target)
            }
            MonitorAddWindowTarget::NextTo(win_id) => {
                let idx = self
                    .workspaces
                    .iter_mut()
                    .position(|ws| ws.has_window(win_id))
                    .unwrap();
                (idx, WorkspaceAddWindowTarget::NextTo(win_id))
            }
        }
    }

    pub fn add_window(
        &mut self,
        window: W,
        target: MonitorAddWindowTarget<W>,
        activate: ActivateWindow,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
    ) {
        // Currently, everything a workspace sets on a Tile is the same across all workspaces of a
        // monitor. So we can use any workspace, not necessarily the exact target workspace.
        let tile = self.workspaces[0].make_tile(window);

        self.add_tile(
            tile,
            target,
            activate,
            true,
            width,
            is_full_width,
            is_floating,
            None,
        );
    }

    pub fn add_column(
        &mut self,
        mut workspace_idx: usize,
        column: Column<W>,
        activate: bool,
        anim: Option<niri_config::Animation>,
    ) {
        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_column(column, activate, anim);

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_output = OutputId::new(&self.output);
        }

        if workspace_idx == self.workspaces.len() - 1 {
            self.add_workspace_bottom();
        }
        if self.options.layout.empty_workspace_above_first && workspace_idx == 0 {
            self.add_workspace_top();
            workspace_idx += 1;
        }

        if activate {
            self.activate_workspace(workspace_idx);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_tile(
        &mut self,
        tile: Tile<W>,
        target: MonitorAddWindowTarget<W>,
        activate: ActivateWindow,
        // FIXME: Refactor ActivateWindow enum to make this better.
        allow_to_activate_workspace: bool,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
        anim: Option<niri_config::Animation>,
    ) {
        let (mut workspace_idx, target) = self.resolve_add_window_target(target);

        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_tile(
            tile,
            target,
            activate,
            width,
            is_full_width,
            is_floating,
            anim,
        );

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_output = OutputId::new(&self.output);
        }

        if workspace_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        if self.options.layout.empty_workspace_above_first && workspace_idx == 0 {
            self.add_workspace_top();
            workspace_idx += 1;
        }

        if allow_to_activate_workspace && activate.map_smart(|| false) {
            self.activate_workspace(workspace_idx);
        }
    }

    pub fn add_tile_to_column(
        &mut self,
        workspace_idx: usize,
        column_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        // FIXME: Refactor ActivateWindow enum to make this better.
        allow_to_activate_workspace: bool,
    ) {
        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_tile_to_column(column_idx, tile_idx, tile, activate);

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_output = OutputId::new(&self.output);
        }

        // Since we're adding window to an existing column, the workspace isn't empty, and
        // therefore cannot be the last one, so we never need to insert a new empty workspace.

        if allow_to_activate_workspace && activate {
            self.activate_workspace(workspace_idx);
        }
    }

    pub fn clean_up_workspaces(&mut self) {
        assert!(self.workspace_switch.is_none());

        let range_start = if self.options.layout.empty_workspace_above_first {
            1
        } else {
            0
        };
        for idx in (range_start..self.workspaces.len() - 1).rev() {
            if self.active_workspace_idx == idx {
                continue;
            }

            if !self.workspaces[idx].has_windows_or_name() {
                self.workspaces.remove(idx);
                if self.active_workspace_idx > idx {
                    self.active_workspace_idx -= 1;
                }
            }
        }

        // Special case handling when empty_workspace_above_first is set and all workspaces
        // are empty.
        if self.options.layout.empty_workspace_above_first && self.workspaces.len() == 2 {
            assert!(!self.workspaces[0].has_windows_or_name());
            assert!(!self.workspaces[1].has_windows_or_name());
            self.workspaces.remove(1);
            self.active_workspace_idx = 0;
        }
    }

    pub fn unname_workspace(&mut self, id: WorkspaceId) -> bool {
        let Some(idx) = self.idx_of_ws(id) else {
            return false;
        };
        let ws = &mut self.workspaces[idx];

        ws.unname();

        if self.workspace_switch.is_none() {
            self.clean_up_workspaces();
        }

        true
    }

    /// Removes the workspaces with the given ids from this monitor, returning
    /// them in their original order.
    ///
    /// Used for parking project workspaces when switching projects. Adjusts
    /// the active workspace index and cancels any in-flight workspace switch.
    #[allow(dead_code)]
    pub(super) fn take_workspaces_by_id(&mut self, ids: &[WorkspaceId]) -> Vec<Workspace<W>> {
        let mut removed = Vec::new();

        // Remove from highest index first so earlier indices stay stable.
        let mut i = self.workspaces.len();
        while i > 0 {
            i -= 1;
            if ids.contains(&self.workspaces[i].id()) {
                let mut ws = self.workspaces.remove(i);
                ws.set_output(None);
                removed.push(ws);

                if i <= self.active_workspace_idx && self.active_workspace_idx > 0 {
                    self.active_workspace_idx -= 1;
                }
            }
        }

        if !removed.is_empty() {
            // The view may have been animating towards a workspace we just
            // parked; reset the animation rather than trying to correct it.
            self.workspace_switch = None;

            if self
                .previous_workspace_id
                .is_some_and(|prev| ids.contains(&prev))
            {
                self.previous_workspace_id = None;
            }
        }

        removed.reverse();
        removed
    }

    pub fn remove_workspace_by_idx(&mut self, mut idx: usize) -> Workspace<W> {
        if idx == self.workspaces.len() - 1 {
            self.add_workspace_bottom();
        }
        if self.options.layout.empty_workspace_above_first && idx == 0 {
            self.add_workspace_top();
            idx += 1;
        }

        let mut ws = self.workspaces.remove(idx);
        ws.set_output(None);

        // For monitor current workspace removal, we focus previous rather than next (<= rather
        // than <). This is different from columns and tiles, but it lets move-workspace-to-monitor
        // back and forth to preserve position.
        if idx <= self.active_workspace_idx && self.active_workspace_idx > 0 {
            self.active_workspace_idx -= 1;
        }

        self.workspace_switch = None;
        self.clean_up_workspaces();

        ws
    }

    pub fn insert_workspace(&mut self, mut ws: Workspace<W>, mut idx: usize, activate: bool) {
        ws.set_output(Some(self.output.clone()));
        ws.update_config(self.options.clone());

        // Don't insert past the last empty workspace.
        if idx == self.workspaces.len() {
            idx -= 1;
        }
        if idx == 0 && self.options.layout.empty_workspace_above_first {
            // Insert a new empty workspace on top to prepare for insertion of new workspace.
            self.add_workspace_top();
            idx += 1;
        }

        self.workspaces.insert(idx, ws);

        if idx <= self.active_workspace_idx {
            self.active_workspace_idx += 1;
        }

        if activate {
            self.workspace_switch = None;
            self.activate_workspace(idx);
        }

        self.workspace_switch = None;
        self.clean_up_workspaces();
    }

    pub fn append_workspaces(&mut self, mut workspaces: Vec<Workspace<W>>) {
        if workspaces.is_empty() {
            return;
        }

        for ws in &mut workspaces {
            ws.set_output(Some(self.output.clone()));
            ws.update_config(self.options.clone());
        }

        let empty_was_focused = self.active_workspace_idx == self.workspaces.len() - 1;

        // Push the workspaces from the removed monitor in the end, right before the
        // last, empty, workspace.
        let empty = self.workspaces.remove(self.workspaces.len() - 1);
        self.workspaces.extend(workspaces);
        self.workspaces.push(empty);

        // If empty_workspace_above_first is set and the first workspace is now no longer empty,
        // add a new empty workspace on top.
        if self.options.layout.empty_workspace_above_first
            && self.workspaces[0].has_windows_or_name()
        {
            self.add_workspace_top();
        }

        // If the empty workspace was focused on the primary monitor, keep it focused.
        if empty_was_focused {
            self.active_workspace_idx = self.workspaces.len() - 1;
        }

        // FIXME: if we're adding workspaces to currently invisible positions
        // (outside the workspace switch), we don't need to cancel it.
        self.workspace_switch = None;
        self.clean_up_workspaces();
    }

    pub fn move_down_or_to_workspace_down(&mut self) {
        if !self.active_workspace().move_down() {
            self.move_to_workspace_down(ActivateWindow::Smart);
        }
    }

    pub fn move_up_or_to_workspace_up(&mut self) {
        if !self.active_workspace().move_up() {
            self.move_to_workspace_up(ActivateWindow::Smart);
        }
    }

    pub fn focus_window_or_workspace_down(&mut self) {
        if !self.active_workspace().focus_down() {
            self.switch_workspace_down();
        }
    }

    pub fn focus_window_or_workspace_up(&mut self) {
        if !self.active_workspace().focus_up() {
            self.switch_workspace_up();
        }
    }

    pub fn move_to_workspace_up(&mut self, activate: ActivateWindow) {
        let new_idx = self.active_workspace_idx.saturating_sub(1);
        self.move_to_workspace(None, new_idx, activate);
    }

    pub fn move_to_workspace_down(&mut self, activate: ActivateWindow) {
        let new_idx = min(self.active_workspace_idx + 1, self.workspaces.len() - 1);
        self.move_to_workspace(None, new_idx, activate);
    }

    pub fn move_to_workspace(
        &mut self,
        window: Option<&W::Id>,
        idx: usize,
        activate: ActivateWindow,
    ) {
        let source_workspace_idx = if let Some(window) = window {
            self.workspaces
                .iter()
                .position(|ws| ws.has_window(window))
                .unwrap()
        } else {
            self.active_workspace_idx
        };
        let source_id = self.workspaces[source_workspace_idx].id();

        let new_idx = min(idx, self.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }
        let new_id = self.workspaces[new_idx].id();

        let activate = activate.map_smart(|| {
            window.is_none_or(|win| self.active_window().map(|win| win.id()) == Some(win))
        });

        let workspace = &mut self.workspaces[source_workspace_idx];
        let Some(window) = window.or_else(|| workspace.active_window().map(|win| win.id())) else {
            return;
        };
        let window = window.clone();

        let mut old_render_pos = workspace
            .tiles_with_render_positions()
            .find_map(|(tile, offset, _visible)| (tile.window().id() == &window).then_some(offset))
            .unwrap();

        let transaction = Transaction::new();
        let removed = workspace.remove_tile(&window, transaction);

        // If the view is following the tile, match the animation.
        let config = if activate {
            self.options.animations.workspace_switch.0
        } else {
            self.options.animations.window_movement.0
        };

        self.add_tile(
            removed.tile,
            MonitorAddWindowTarget::Workspace {
                id: new_id,
                column_idx: None,
            },
            if activate {
                ActivateWindow::Yes
            } else {
                ActivateWindow::No
            },
            true,
            removed.width,
            removed.is_full_width,
            removed.is_floating,
            Some(config),
        );

        if self.workspace_switch.is_none() {
            self.clean_up_workspaces();
        }

        let new_idx = self.idx_of_ws(new_id).unwrap();

        // Animate vertical movement between workspaces.
        //
        // Recompute the source idx in case some workspace was removed during clean-up. If the
        // source workspace itself was removed, don't bother animating this since the removal is
        // instant anyway.
        if let Some(source_workspace_idx) = self.idx_of_ws(source_id) {
            old_render_pos.y +=
                self.workspace_size_with_gap(1.).h * (source_workspace_idx as f64 - new_idx as f64);
        }

        let (tile, new_render_pos) = self.workspaces[new_idx]
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == &window)
            .unwrap();
        tile.animate_move_from_with_config(old_render_pos - new_render_pos, config);
        tile.set_anim_y_between_workspaces();
    }

    pub fn move_column_to_workspace_up(&mut self, activate: bool) {
        let new_idx = self.active_workspace_idx.saturating_sub(1);
        self.move_column_to_workspace(new_idx, activate);
    }

    pub fn move_column_to_workspace_down(&mut self, activate: bool) {
        let new_idx = min(self.active_workspace_idx + 1, self.workspaces.len() - 1);
        self.move_column_to_workspace(new_idx, activate);
    }

    pub fn move_column_to_workspace(&mut self, idx: usize, activate: bool) {
        let source_workspace_idx = self.active_workspace_idx;

        let new_idx = min(idx, self.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }

        let workspace = &mut self.workspaces[source_workspace_idx];
        if workspace.floating_is_active() {
            let activate = if activate {
                ActivateWindow::Smart
            } else {
                ActivateWindow::No
            };
            self.move_to_workspace(None, idx, activate);
            return;
        }

        let Some(id) = workspace.scrolling().active_column().map(Column::id) else {
            return;
        };
        let mut old_render_pos = workspace
            .scrolling()
            .columns_with_render_positions()
            .find_map(|(col, pos)| (col.id() == id).then_some(pos))
            .unwrap();

        let column = workspace.remove_active_column().unwrap();

        // Animate vertical movement between workspaces.
        old_render_pos.y +=
            self.workspace_size_with_gap(1.).h * (source_workspace_idx as f64 - new_idx as f64);

        // If the view is following the column, match the animation.
        let config = if activate {
            self.options.animations.workspace_switch.0
        } else {
            self.options.animations.window_movement.0
        };

        let new_id = self.workspaces[new_idx].id();
        self.add_column(new_idx, column, activate, Some(config));

        let new_idx = self.idx_of_ws(new_id).unwrap();
        let (column, new_render_pos) = self.workspaces[new_idx]
            .scrolling_mut()
            .columns_with_render_positions_mut()
            .find(|(col, _pos)| col.id() == id)
            .unwrap();
        column.animate_move_from_with_config(old_render_pos - new_render_pos, config);
        column.set_anim_y_between_workspaces();
    }

    pub fn switch_workspace_up(&mut self) {
        let new_idx = match &self.workspace_switch {
            // During a DnD scroll, select the prev apparent workspace.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                let current = gesture.current_idx;
                let new = current.ceil() - 1.;
                new.clamp(0., (self.workspaces.len() - 1) as f64) as usize
            }
            _ => self.active_workspace_idx.saturating_sub(1),
        };

        self.activate_workspace(new_idx);
    }

    pub fn switch_workspace_down(&mut self) {
        let new_idx = match &self.workspace_switch {
            // During a DnD scroll, select the next apparent workspace.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                let current = gesture.current_idx;
                let new = current.floor() + 1.;
                new.clamp(0., (self.workspaces.len() - 1) as f64) as usize
            }
            _ => min(self.active_workspace_idx + 1, self.workspaces.len() - 1),
        };

        self.activate_workspace(new_idx);
    }

    fn previous_workspace_idx(&self) -> Option<usize> {
        let id = self.previous_workspace_id?;
        self.idx_of_ws(id)
    }

    pub fn switch_workspace(&mut self, idx: usize) {
        self.activate_workspace(min(idx, self.workspaces.len() - 1));
    }

    pub fn switch_workspace_auto_back_and_forth(&mut self, idx: usize) {
        let idx = min(idx, self.workspaces.len() - 1);

        if idx == self.active_workspace_idx {
            if let Some(prev_idx) = self.previous_workspace_idx() {
                self.switch_workspace(prev_idx);
            }
        } else {
            self.switch_workspace(idx);
        }
    }

    pub fn switch_workspace_previous(&mut self) {
        if let Some(idx) = self.previous_workspace_idx() {
            self.switch_workspace(idx);
        }
    }

    pub fn active_window(&self) -> Option<&W> {
        self.active_workspace_ref().active_window()
    }

    pub fn advance_animations(&mut self) {
        match &mut self.workspace_switch {
            Some(WorkspaceSwitch::Animation(anim)) => {
                if anim.is_done() {
                    self.workspace_switch = None;
                    self.clean_up_workspaces();
                }
            }
            Some(WorkspaceSwitch::Gesture(gesture)) => {
                // Make sure the last event time doesn't go too much out of date (for
                // monitors not under cursor), causing sudden jumps.
                //
                // This happens after any dnd_scroll_gesture_scroll() calls (in
                // Layout::advance_animations()), so it doesn't mess up the time delta there.
                if let Some(last_time) = &mut gesture.dnd_last_event_time {
                    let now = self.clock.now_unadjusted();
                    if *last_time != now {
                        *last_time = now;

                        // If last_time was already == now, then dnd_scroll_gesture_scroll() must've
                        // updated the gesture already. Therefore, when this code runs, the pointer
                        // must be outside the DnD scrolling zone.
                        gesture.dnd_nonzero_start_time = None;
                    }
                }

                if let Some(anim) = &mut gesture.animation {
                    if anim.is_done() {
                        gesture.animation = None;
                    }
                }
            }
            None => (),
        }

        let mut finished = Vec::new();
        for (&idx, anim) in self.project_drawer_anims.iter_mut() {
            if anim.is_done() {
                finished.push(idx);
            }
        }
        for idx in finished {
            let value = self
                .project_drawer_anims
                .get(&idx)
                .map(Animation::clamped_value);
            if let Some(value) = value {
                self.project_drawer_depths.insert(idx, value);
            }
            self.project_drawer_anims.remove(&idx);
        }

        for ws in &mut self.workspaces {
            ws.advance_animations();
        }
    }

    pub(super) fn are_animations_ongoing(&self) -> bool {
        self.workspace_switch
            .as_ref()
            .is_some_and(|s| s.is_animation_ongoing())
            || self
                .project_drawer_anims
                .values()
                .any(|anim| !anim.is_done())
            || self.workspaces.iter().any(|ws| ws.are_animations_ongoing())
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.workspace_switch.is_some()
            || self
                .workspaces
                .iter()
                .any(|ws| ws.are_transitions_ongoing())
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        let mut insert_hint_ws_geo = None;
        let insert_hint_ws_id = self
            .insert_hint
            .as_ref()
            .and_then(|hint| hint.workspace.existing_id());

        for ws in &mut self.workspaces {
            ws.update_render_elements(is_active, RenderLayer::MovingBetweenWorkspaces);
        }

        for (ws, geo) in self.workspaces_with_render_geo_mut(true) {
            ws.update_render_elements(is_active, RenderLayer::Normal);

            if Some(ws.id()) == insert_hint_ws_id {
                insert_hint_ws_geo = Some(geo);
            }
        }

        self.insert_hint_render_loc = None;
        if let Some(hint) = &self.insert_hint {
            match hint.workspace {
                InsertWorkspace::Existing(ws_id) => {
                    if let Some(idx) = self.idx_of_ws(ws_id) {
                        let ws = &self.workspaces[idx];
                        if let Some(mut area) = ws.insert_hint_area(hint.position) {
                            let scale = ws.scale().fractional_scale();
                            let view_size = ws.view_size();

                            // Make sure the hint is at least partially visible.
                            if matches!(hint.position, InsertPosition::NewColumn(_)) {
                                let zoom = self.overview_zoom();
                                let geo = insert_hint_ws_geo.unwrap();
                                let geo = geo.downscale(zoom);

                                area.loc.x = area.loc.x.max(-geo.loc.x - area.size.w / 2.);
                                area.loc.x =
                                    area.loc.x.min(geo.loc.x + geo.size.w - area.size.w / 2.);
                            }

                            // Round to physical pixels.
                            area = area.to_physical_precise_round(scale).to_logical(scale);

                            let view_rect = Rectangle::new(area.loc.upscale(-1.), view_size);
                            self.insert_hint_element.update_render_elements(
                                area.size,
                                view_rect,
                                hint.corner_radius,
                                scale,
                            );
                            self.insert_hint_render_loc = Some(InsertHintRenderLoc {
                                workspace: hint.workspace,
                                location: area.loc,
                            });
                        }
                    } else {
                        error!("insert hint workspace missing from monitor");
                    }
                }
                InsertWorkspace::NewAt(ws_idx) => {
                    let scale = self.scale.fractional_scale();
                    let zoom = self.overview_zoom();
                    let gap = self.workspace_gap(zoom);

                    let hint_gap = round_logical_in_physical(scale, gap * 0.1);
                    let hint_height = gap - hint_gap * 2.;

                    let next_ws_geo = self.workspaces_render_geo().nth(ws_idx).unwrap();
                    let hint_width = round_logical_in_physical(scale, next_ws_geo.size.w * 0.75);
                    let hint_x =
                        round_logical_in_physical(scale, (next_ws_geo.size.w - hint_width) / 2.);

                    let hint_loc_diff = Point::from((-hint_x, hint_height + hint_gap));
                    let hint_loc = next_ws_geo.loc - hint_loc_diff;
                    let hint_size = Size::from((hint_width, hint_height));

                    // Sometimes the hint ends up 1 px wider than necessary and/or 1 px
                    // narrower than necessary. The values here seem correct. Might have to do with
                    // how zooming out currently doesn't round to output scale properly.

                    // Compute view rect as if we're above the next workspace (rather than below
                    // the previous one).
                    let view_rect = Rectangle::new(hint_loc_diff, next_ws_geo.size);

                    self.insert_hint_element.update_render_elements(
                        hint_size,
                        view_rect,
                        CornerRadius::default(),
                        scale,
                    );
                    self.insert_hint_render_loc = Some(InsertHintRenderLoc {
                        workspace: hint.workspace,
                        location: hint_loc,
                    });
                }
            }
        }
    }

    pub fn update_config(&mut self, base_options: Rc<Options>) {
        let options =
            Rc::new(Options::clone(&base_options).with_merged_layout(self.layout_config.as_ref()));

        if self.options.layout.empty_workspace_above_first
            != options.layout.empty_workspace_above_first
            && self.workspaces.len() > 1
        {
            if options.layout.empty_workspace_above_first {
                self.add_workspace_top();
            } else if self.workspace_switch.is_none() && self.active_workspace_idx != 0 {
                self.workspaces.remove(0);
                self.active_workspace_idx = self.active_workspace_idx.saturating_sub(1);
            }
        }

        for ws in &mut self.workspaces {
            ws.update_config(options.clone());
        }

        self.insert_hint_element
            .update_config(options.layout.insert_hint);

        self.base_options = base_options;
        self.options = options;
    }

    pub fn update_layout_config(&mut self, layout_config: Option<niri_config::LayoutPart>) -> bool {
        if self.layout_config == layout_config {
            return false;
        }

        self.layout_config = layout_config;
        self.update_config(self.base_options.clone());

        true
    }

    pub fn update_shaders(&mut self) {
        for ws in &mut self.workspaces {
            ws.update_shaders();
        }

        self.insert_hint_element.update_shaders();
    }

    pub fn update_output_size(&mut self) {
        self.scale = self.output.current_scale();
        self.view_size = output_size(&self.output);
        self.working_area = compute_working_area(&self.output);

        for ws in &mut self.workspaces {
            ws.update_output_size();
        }
    }

    pub fn move_workspace_down(&mut self) {
        let mut new_idx = min(self.active_workspace_idx + 1, self.workspaces.len() - 1);
        if new_idx == self.active_workspace_idx {
            return;
        }

        self.workspaces.swap(self.active_workspace_idx, new_idx);

        if new_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        if self.options.layout.empty_workspace_above_first && self.active_workspace_idx == 0 {
            self.add_workspace_top();
            new_idx += 1;
        }

        let previous_workspace_id = self.previous_workspace_id;
        self.activate_workspace(new_idx);
        self.workspace_switch = None;
        self.previous_workspace_id = previous_workspace_id;

        self.clean_up_workspaces();
    }

    pub fn move_workspace_up(&mut self) {
        let mut new_idx = self.active_workspace_idx.saturating_sub(1);
        if new_idx == self.active_workspace_idx {
            return;
        }

        self.workspaces.swap(self.active_workspace_idx, new_idx);

        if self.active_workspace_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        if self.options.layout.empty_workspace_above_first && new_idx == 0 {
            self.add_workspace_top();
            new_idx += 1;
        }

        let previous_workspace_id = self.previous_workspace_id;
        self.activate_workspace(new_idx);
        self.workspace_switch = None;
        self.previous_workspace_id = previous_workspace_id;

        self.clean_up_workspaces();
    }

    pub fn move_workspace_to_idx(&mut self, old_idx: usize, new_idx: usize) {
        if self.workspaces.len() <= old_idx {
            return;
        }

        let mut new_idx = new_idx.clamp(0, self.workspaces.len() - 1);
        if old_idx == new_idx {
            return;
        }

        let ws = self.workspaces.remove(old_idx);
        self.workspaces.insert(new_idx, ws);

        if new_idx > old_idx {
            if new_idx == self.workspaces.len() - 1 {
                // Insert a new empty workspace.
                self.add_workspace_bottom();
            }

            if self.options.layout.empty_workspace_above_first && old_idx == 0 {
                self.add_workspace_top();
                new_idx += 1;
            }
        } else {
            if old_idx == self.workspaces.len() - 1 {
                // Insert a new empty workspace.
                self.add_workspace_bottom();
            }

            if self.options.layout.empty_workspace_above_first && new_idx == 0 {
                self.add_workspace_top();
                new_idx += 1;
            }
        }

        // Only refocus the workspace if it was already focused
        if self.active_workspace_idx == old_idx {
            self.active_workspace_idx = new_idx;
        // If the workspace order was switched so that the current workspace moved down the
        // workspace stack, focus correctly
        } else if new_idx <= self.active_workspace_idx && old_idx > self.active_workspace_idx {
            self.active_workspace_idx += 1;
        } else if new_idx >= self.active_workspace_idx && old_idx < self.active_workspace_idx {
            self.active_workspace_idx = self.active_workspace_idx.saturating_sub(1);
        }

        self.workspace_switch = None;

        self.clean_up_workspaces();
    }

    /// Returns the geometry of the active window relative to and clamped to the output.
    ///
    /// During animations, assumes the final view position.
    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        if self.overview_open {
            return None;
        }

        self.active_workspace_ref().active_window_visual_rectangle()
    }

    fn workspace_size(&self, zoom: f64) -> Size<f64, Logical> {
        let ws_size = self.view_size.upscale(zoom);
        let scale = self.scale.fractional_scale();
        ws_size.to_physical_precise_ceil(scale).to_logical(scale)
    }

    fn workspace_gap(&self, zoom: f64) -> f64 {
        let scale = self.scale.fractional_scale();
        let gap = self.view_size.h * 0.1 * zoom;
        round_logical_in_physical_max1(scale, gap)
    }

    fn workspace_size_with_gap(&self, zoom: f64) -> Size<f64, Logical> {
        let gap = self.workspace_gap(zoom);
        self.workspace_size(zoom) + Size::from((0., gap))
    }

    pub fn overview_zoom(&self) -> f64 {
        let progress = self.overview_progress.as_ref().map(|p| p.value());
        compute_overview_zoom(&self.options, progress)
    }

    pub(super) fn set_overview_progress(&mut self, progress: Option<&super::OverviewProgress>) {
        let prev_render_idx = self.workspace_render_idx();
        self.overview_progress = progress.map(OverviewProgress::from);
        let new_render_idx = self.workspace_render_idx();

        // If the view jumped (can happen when going from corrected to uncorrected render_idx, for
        // example when toggling the overview in the middle of an overview animation), then restart
        // the workspace switch to avoid jumps.
        if prev_render_idx != new_render_idx {
            if let Some(WorkspaceSwitch::Animation(anim)) = &mut self.workspace_switch {
                // FIXME: maintain velocity.
                *anim = anim.restarted(prev_render_idx, anim.to(), 0.);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn overview_progress_value(&self) -> Option<f64> {
        self.overview_progress.as_ref().map(|p| p.value())
    }

    pub fn workspace_render_idx(&self) -> f64 {
        // If workspace switch and overview progress are matching animations, then compute a
        // correction term to make the movement appear monotonic.
        if let (
            Some(WorkspaceSwitch::Animation(switch_anim)),
            Some(OverviewProgress::Animation(progress_anim)),
        ) = (&self.workspace_switch, &self.overview_progress)
        {
            if switch_anim.start_time() == progress_anim.start_time()
                && (switch_anim.duration().as_secs_f64() - progress_anim.duration().as_secs_f64())
                    .abs()
                    <= 0.001
            {
                #[rustfmt::skip]
                // How this was derived:
                //
                // - Assume we're animating a zoom + switch. Consider switch "from" and "to".
                //   These are render_idx values, so first workspace to second would have switch
                //   from = 0. and to = 1. regardless of the zoom level.
                //
                // - At the start, the point at "from" is at Y = 0. We're moving the point at "to"
                //   to Y = 0. We want this to be a monotonic motion in apparent coordinates (after
                //   zoom).
                //
                // - Height at the start:
                //   from_height = (size.h + gap) * from_zoom.
                //
                // - Current height:
                //   current_height = (size.h + gap) * zoom.
                //
                // - We're moving the "to" point to Y = 0:
                //   to_y = 0.
                //
                // - The initial position of the point we're moving:
                //   from_y = (to - from) * from_height.
                //
                // - We want this point to travel monotonically in apparent coordinates:
                //   current_y = from_y + (to_y - from_y) * progress,
                //   where progress is from 0 to 1, equals to the animation progress (switch and
                //   zoom are the same since they are synchronized).
                //
                // - Derive the Y of the first workspace from this:
                //   first_y = current_y - to * current_height.
                //
                // Now, let's substitute and rearrange the terms.
                //
                // - current_y = from_y + (0 - (to - from) * from_height) * progress
                // - progress = (switch_anim.value() - from) / (to - from)
                // - current_y = from_y - (to - from) * from_height * (switch_anim.value() - from) / (to - from)
                // - current_y = from_y - from_height * (switch_anim.value() - from)
                // - first_y = from_y - from_height * (switch_anim.value() - from) - to * current_height
                // - first_y = (to - from) * from_height - from_height * (switch_anim.value() - from) - to * current_height
                // - first_y = to * from_height - switch_anim.value() * from_height - to * current_height
                // - first_y = -switch_anim.value() * from_height + to * (from_height - current_height)
                let from = progress_anim.from();
                let from_zoom = compute_overview_zoom(&self.options, Some(from));
                let from_ws_height_with_gap = self.workspace_size_with_gap(from_zoom).h;

                let zoom = self.overview_zoom();
                let ws_height_with_gap = self.workspace_size_with_gap(zoom).h;

                let first_ws_y = -switch_anim.value() * from_ws_height_with_gap
                    + switch_anim.to() * (from_ws_height_with_gap - ws_height_with_gap);

                return -first_ws_y / ws_height_with_gap;
            }
        };

        if let Some(switch) = &self.workspace_switch {
            switch.current_idx()
        } else {
            self.active_workspace_idx as f64
        }
    }

    pub fn workspaces_render_geo(&self) -> impl Iterator<Item = Rectangle<f64, Logical>> {
        let scale = self.scale.fractional_scale();
        let zoom = self.overview_zoom();

        let ws_size = self.workspace_size(zoom);
        let gap = self.workspace_gap(zoom);
        let ws_height_with_gap = ws_size.h + gap;

        let static_offset = (self.view_size.to_point() - ws_size.to_point()).downscale(2.);
        let static_offset = static_offset
            .to_physical_precise_round(scale)
            .to_logical(scale);

        let first_ws_y = -self.workspace_render_idx() * ws_height_with_gap;
        let first_ws_y = round_logical_in_physical(scale, first_ws_y);

        // Return position for one-past-last workspace too.
        (0..=self.workspaces.len()).map(move |idx| {
            let y = first_ws_y + idx as f64 * ws_height_with_gap;
            let loc = Point::from((0., y)) + static_offset;

            // Even though all components that go into loc are rounded to physical pixels, the
            // floating point addition may lose precision. This can result for example in the
            // current workspace having y = 0.0000000000002 and thus missing pointer hits at the
            // monitor edge with y = 0. So, post-round the location too.
            let loc = loc.to_physical_precise_round(scale).to_logical(scale);

            Rectangle::new(loc, ws_size)
        })
    }

    pub fn workspaces_with_render_geo_cull(
        &self,
        cull: bool,
    ) -> impl Iterator<Item = (&Workspace<W>, Rectangle<f64, Logical>)> {
        let output_geo = Rectangle::from_size(self.view_size);

        let geo = self.workspaces_render_geo();
        zip(self.workspaces.iter(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| !cull || geo.intersection(output_geo).is_some())
    }

    pub fn workspaces_with_render_geo(
        &self,
    ) -> impl Iterator<Item = (&Workspace<W>, Rectangle<f64, Logical>)> {
        self.workspaces_with_render_geo_cull(true)
    }

    pub fn workspaces_with_render_geo_idx(
        &self,
    ) -> impl Iterator<Item = ((usize, &Workspace<W>), Rectangle<f64, Logical>)> {
        let output_geo = Rectangle::from_size(self.view_size);

        let geo = self.workspaces_render_geo();
        zip(self.workspaces.iter().enumerate(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| geo.intersection(output_geo).is_some())
    }

    pub fn workspaces_with_render_geo_mut(
        &mut self,
        cull: bool,
    ) -> impl Iterator<Item = (&mut Workspace<W>, Rectangle<f64, Logical>)> {
        let output_geo = Rectangle::from_size(self.view_size);

        let geo = self.workspaces_render_geo();
        zip(self.workspaces.iter_mut(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| !cull || geo.intersection(output_geo).is_some())
    }

    pub fn workspace_under(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&Workspace<W>, Rectangle<f64, Logical>)> {
        let (ws, geo) = self.workspaces_with_render_geo().find_map(|(ws, geo)| {
            // Extend width to entire output.
            let loc = Point::from((0., geo.loc.y));
            let size = Size::from((self.view_size.w, geo.size.h));
            let bounds = Rectangle::new(loc, size);

            bounds.contains(pos_within_output).then_some((ws, geo))
        })?;
        Some((ws, geo))
    }

    pub fn workspace_under_narrow(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<&Workspace<W>> {
        self.workspaces_with_render_geo()
            .find_map(|(ws, geo)| geo.contains(pos_within_output).then_some(ws))
    }

    pub fn window_under(&self, pos_within_output: Point<f64, Logical>) -> Option<(&W, HitType)> {
        let (ws, geo) = self.workspace_under(pos_within_output)?;

        if self.overview_progress.is_some() {
            let zoom = self.overview_zoom();
            let pos_within_workspace = (pos_within_output - geo.loc).downscale(zoom);
            let (win, hit) = ws.window_under(pos_within_workspace)?;
            // During the overview animation, we cannot do input hits because we cannot really
            // represent scaled windows properly.
            Some((win, hit.to_activate()))
        } else {
            let (win, hit) = ws.window_under(pos_within_output - geo.loc)?;
            Some((win, hit.offset_win_pos(geo.loc)))
        }
    }

    pub fn resize_edges_under(&self, pos_within_output: Point<f64, Logical>) -> Option<ResizeEdge> {
        if self.overview_progress.is_some() {
            return None;
        }

        let (ws, geo) = self.workspace_under(pos_within_output)?;
        ws.resize_edges_under(pos_within_output - geo.loc)
    }

    pub(super) fn insert_position(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> (InsertWorkspace, Rectangle<f64, Logical>) {
        let mut iter = self.workspaces_with_render_geo_idx();

        let dummy = Rectangle::default();

        // Monitors always have at least one workspace.
        let ((idx, ws), geo) = iter.next().unwrap();

        // Check if above first.
        if pos_within_output.y < geo.loc.y {
            return (InsertWorkspace::NewAt(idx), dummy);
        }

        let contains = move |geo: Rectangle<f64, Logical>| {
            geo.loc.y <= pos_within_output.y && pos_within_output.y < geo.loc.y + geo.size.h
        };

        // Check first.
        if contains(geo) {
            return (InsertWorkspace::Existing(ws.id()), geo);
        }

        let mut last_geo = geo;
        let mut last_idx = idx;
        for ((idx, ws), geo) in iter {
            // Check gap above.
            let gap_loc = Point::from((last_geo.loc.x, last_geo.loc.y + last_geo.size.h));
            let gap_size = Size::from((geo.size.w, geo.loc.y - gap_loc.y));
            let gap_geo = Rectangle::new(gap_loc, gap_size);
            if contains(gap_geo) {
                return (InsertWorkspace::NewAt(idx), dummy);
            }

            // Check workspace itself.
            if contains(geo) {
                return (InsertWorkspace::Existing(ws.id()), geo);
            }

            last_geo = geo;
            last_idx = idx;
        }

        // Anything below.
        (InsertWorkspace::NewAt(last_idx + 1), dummy)
    }

    pub fn render_above_top_layer(&self) -> bool {
        // Render above the top layer only if the view is stationary.
        if self.workspace_switch.is_some() || self.overview_progress.is_some() {
            return false;
        }

        let ws = &self.workspaces[self.active_workspace_idx];
        ws.render_above_top_layer()
    }

    pub fn render_insert_hint_between_workspaces<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        if self.options.layout.insert_hint.off {
            return;
        }
        let Some(render_loc) = self.insert_hint_render_loc else {
            return;
        };
        let InsertWorkspace::NewAt(_) = render_loc.workspace else {
            return;
        };

        self.insert_hint_element
            .render(renderer, render_loc.location, &mut |elem| {
                let elem = MonitorInnerRenderElement::UncroppedInsertHint(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                let elem =
                    RelocateRenderElement::from_element(elem, Point::default(), Relocate::Relative);
                push(elem);
            });
    }

    pub fn render_workspaces<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        focus_ring: bool,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let _span = tracy_client::span!("Monitor::render_workspaces");

        let scale = self.scale.fractional_scale();
        // Ceil the height in physical pixels.
        let height = (self.view_size.h * scale).ceil() as i32;

        let zoom = self.overview_zoom();

        let insert_hint_render_loc = self
            .insert_hint_render_loc
            .filter(|_| !self.options.layout.insert_hint.off);

        let scale_relocate = move |geo: Rectangle<f64, Logical>, elem| {
            let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
            RelocateRenderElement::from_element(
                elem,
                // The offset we get from workspaces_with_render_geo() is already
                // rounded to physical pixels, but it's in the logical coordinate
                // space, so we need to convert it to physical.
                geo.loc.to_physical_precise_round(scale),
                Relocate::Relative,
            )
        };

        // Draw in passes for correct Z ordering during window movement between workspaces:
        // - floating windows moving between workspaces
        // - normal floating windows
        // - scrolling windows moving between workspaces
        // - normal scrolling windows
        for pass in 0..4 {
            // Don't cull when drawing windows moving between workspaces so that windows moving to
            // workspaces off-screen will still render.
            let cull = matches!(pass, 1 | 3);

            // Crop the elements to prevent them overflowing, currently visible during a workspace
            // switch.
            //
            // HACK: crop to infinite bounds at least horizontally where we
            // know there's no workspace joining or monitor bounds, otherwise
            // it will cut pixel shaders and mess up the coordinate space.
            // There's also a damage tracking bug which causes glitched
            // rendering for maximized GTK windows.
            //
            // FIXME: use proper bounds after fixing the Crop element.
            //
            // Also, check cull here to avoid cropping windows moving between workspaces.
            //
            // FIXME: for cull=true, it might be better visually to crop to a workspace-high region
            // anchored to the window/column as it moves between workspaces, to prevent overflowing
            // windows from appearing and disappearing.
            let crop_bounds =
                if cull && (self.workspace_switch.is_some() || self.overview_progress.is_some()) {
                    Rectangle::new(
                        Point::from((-i32::MAX / 2, 0)),
                        Size::from((i32::MAX, height)),
                    )
                } else {
                    Rectangle::new(
                        Point::from((-i32::MAX / 2, -i32::MAX / 2)),
                        Size::from((i32::MAX, i32::MAX)),
                    )
                };

            for (ws, geo) in self.workspaces_with_render_geo_cull(cull) {
                // Macro instead of closure because ws and insert hint have different elem types.
                macro_rules! push {
                    () => {{
                        &mut |elem| {
                            let elem = CropRenderElement::from_element(elem, scale, crop_bounds);
                            if let Some(elem) = elem {
                                let elem = MonitorInnerRenderElement::from(elem);
                                push(scale_relocate(geo, elem));
                            }
                        }
                    }};
                }

                let xray_pos = XrayPos::new(geo.loc, zoom);

                match pass {
                    0 => {
                        ws.render_floating(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::MovingBetweenWorkspaces,
                            push!(),
                        );
                    }
                    1 => {
                        ws.render_floating(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::Normal,
                            push!(),
                        );

                        if let Some(loc) = insert_hint_render_loc {
                            if loc.workspace == InsertWorkspace::Existing(ws.id()) {
                                self.insert_hint_element.render(
                                    ctx.renderer,
                                    loc.location,
                                    push!(),
                                );
                            }
                        }
                    }
                    2 => {
                        ws.render_scrolling(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::MovingBetweenWorkspaces,
                            push!(),
                        );
                    }
                    _ => {
                        ws.render_scrolling(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::Normal,
                            push!(),
                        );
                    }
                }
            }
        }
    }

    /// Reset all project overview drawer animation state.
    pub(super) fn project_overview_reset_anims(&mut self) {
        self.project_drawer_depths.clear();
        self.project_drawer_anims.clear();
    }

    /// Snap every card to its settled depth instantly.
    pub(super) fn project_drawer_settle(&mut self) {
        self.project_overview_reset_anims();
    }

    /// Animate each card gliding to its depth relative to the new selection.
    ///
    /// Cards keep continuous depths: the wrapping card slides the long way
    /// through the intermediate poses instead of teleporting.
    pub(super) fn project_drawer_animate_to(&mut self, targets: &HashMap<usize, f64>) {
        for (&idx, &target) in targets {
            let current = match self.project_drawer_anims.get(&idx) {
                Some(anim) => anim.clamped_value(),
                None => *self.project_drawer_depths.get(&idx).unwrap_or(&target),
            };
            if (current - target).abs() < 0.001 {
                self.project_drawer_depths.insert(idx, target);
                continue;
            }
            let anim = Animation::ease(
                self.clock.clone(),
                current,
                target,
                0.,
                DRAWER_ANIM_MS,
                Curve::EaseOutCubic,
            );
            self.project_drawer_depths.insert(idx, current);
            self.project_drawer_anims.insert(idx, anim);
        }
    }

    /// Base geometry of the drawer card stack (before depth offsets).
    ///
    /// Returns the card rectangle and its pose function mapping fractional
    /// depth to (y-offset in physical px, size factor).
    fn project_drawer_geometry(
        &self,
        scale: f64,
    ) -> (
        Rectangle<f64, Logical>,
        impl Fn(f64) -> (i32, f64) + '_,
    ) {
        let view = self.view_size;
        let card_w = view.w * DRAWER_CARD_WIDTH_FRAC;
        let card_h = card_w * view.h / view.w;
        let x = (view.w - card_w) / 2.;
        let y = view.h * DRAWER_CARD_CENTER_Y_FRAC - card_h / 2.;
        let card_rect = Rectangle::new(Point::from((x, y)), Size::from((card_w, card_h)));

        let pose = move |depth: f64| -> (i32, f64) {
            let d = depth.clamp(0., DRAWER_MAX_VISIBLE_DEPTH);
            let dy = DRAWER_STEP_Y * d * scale;
            let size_factor = 1. - DRAWER_SCALE_STEP * d;
            (dy.round() as i32, size_factor)
        };

        (card_rect, pose)
    }

    /// Drawer card under the given point, front-most first.
    ///
    /// Returns the stack position: 0 is the front (selected) card.
    pub(super) fn project_drawer_hit_test(
        &self,
        pos_within_output: Point<f64, Logical>,
        len: usize,
    ) -> Option<usize> {
        let scale = self.scale.fractional_scale();
        let (rect, pose) = self.project_drawer_geometry(scale);

        let max_visible = (len as f64).min(DRAWER_MAX_VISIBLE_DEPTH + 1.) as usize;
        for stack_pos in 0..max_visible {
            let (dy, size_factor) = pose(stack_pos as f64);
            let hit_rect = Rectangle::new(
                Point::from((rect.loc.x, rect.loc.y + dy as f64 / scale)),
                Size::from((rect.size.w * size_factor, rect.size.h * size_factor)),
            );
            if hit_rect.contains(pos_within_output) {
                return Some(stack_pos);
            }
        }
        None
    }

    /// Folder tab texture for one project card, cached by content.
    fn project_tab(
        &self,
        renderer: &mut GlesRenderer,
        name: &str,
        state: &'static str,
        color: [f32; 4],
        scale: f64,
    ) -> TextureBuffer<GlesTexture> {
        let key = (
            name.to_string(),
            state,
            color.iter().fold(0u32, |acc, c| acc.wrapping_mul(31).wrapping_add((c * 255.).round() as u32)),
            scale.to_bits(),
        );

        let mut cache = self.project_tab_cache.borrow_mut();
        cache
            .entry(key)
            .or_insert_with(|| {
                generate_project_tab(renderer, name, state, color, scale)
                    .unwrap_or_else(|err| {
                        warn!("failed to render project tab {name:?}: {err:?}");
                        panic!("tab generation failed")
                    })
            })
            .clone()
    }

    /// Rounded border overlay texture for one card size, cached by geometry.
    fn project_border(
        &self,
        renderer: &mut GlesRenderer,
        color: [f32; 4],
        width_px: i32,
        height_px: i32,
        scale: f64,
    ) -> TextureBuffer<GlesTexture> {
        let color_bits = color.iter().fold(0u64, |acc, c| {
            acc.wrapping_mul(31).wrapping_add((c * 255.).round() as u64)
        });
        let key = (color_bits, width_px, height_px);

        let mut cache = self.project_border_cache.borrow_mut();
        cache
            .entry(key)
            .or_insert_with(|| {
                generate_project_border(
                    renderer,
                    color,
                    width_px,
                    height_px,
                    DRAWER_CARD_RADIUS * scale,
                    DRAWER_BORDER_WIDTH * scale,
                )
                .unwrap_or_else(|err| {
                    warn!("failed to render project border: {err:?}");
                    panic!("border generation failed")
                })
            })
            .clone()
    }

    /// Render the project overview drawer: a folder-stack of one card per
    /// project over a dark backdrop.
    pub(super) fn render_project_overview<R>(
        &self,
        mut ctx: RenderCtx<R>,
        focus_ring: bool,
        entries: &[ProjectOverviewEntry<W>],
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) where
        R: NiriRenderer + AsGlesRenderer,
    {
        if entries.is_empty() {
            return;
        }

        let _span = tracy_client::span!("Monitor::render_project_overview");

        let scale = self.scale.fractional_scale();
        let zoom = self.overview_zoom();

        // NOTE: smithay renders pushed elements in REVERSE order — the first
        // pushed element ends up on top. So within the frame we push:
        //   per card (front to back): dim, tab, border, content
        // and the backdrop LAST so it lands at the bottom of the stack.

        let (card_rect, pose) = self.project_drawer_geometry(scale);
        let card_base_loc = card_rect.loc.to_physical_precise_round(scale);

        // Front card first so it lands on top.
        let mut ordered: Vec<&ProjectOverviewEntry<W>> = entries.iter().collect();
        ordered.sort_by(|a, b| a.depth.total_cmp(&b.depth));

        for entry in ordered {
            // Animated depth if gliding, static target otherwise.
            let depth = match self.project_drawer_anims.get(&entry.idx) {
                Some(anim) => anim.clamped_value(),
                None => entry.depth,
            };
            if depth > DRAWER_MAX_VISIBLE_DEPTH + 0.4 || depth < -0.4 {
                continue;
            }

            let (dy_phys, size_factor) = pose(depth.max(0.));
            let dim_alpha = (DRAWER_DIM_STEP * depth.max(0.)).min(0.6);

            // Card content region (physical px).
            let card_w_px = ((card_rect.size.w * size_factor) * scale).round() as i32;
            let card_h_px = ((card_rect.size.h * size_factor) * scale).round() as i32;
            let content_loc =
                Point::<i32, Physical>::from((card_base_loc.x, card_base_loc.y + dy_phys));
            let tab_h_phys = (DRAWER_TAB_HEIGHT * scale).round() as i32;

            let gles = ctx.as_gles();

            // ── Depth dimming overlay (topmost within this card) ─────
            if dim_alpha > 0.001 {
                let total_h = tab_h_phys + card_h_px;
                let dim_size = Size::from((
                    card_w_px.max(1) as f64 / scale,
                    total_h.max(1) as f64 / scale,
                ));
                let dim = SolidColorBuffer::new(dim_size, [0., 0., 0., dim_alpha as f32]);
                let dim_loc =
                    Point::<i32, Physical>::from((content_loc.x, content_loc.y - tab_h_phys));
                let elem = MonitorInnerRenderElement::SolidColor(
                    SolidColorRenderElement::from_buffer(
                        &dim,
                        Point::from((0., 0.)),
                        1.,
                        Kind::Unspecified,
                    ),
                );
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                let elem =
                    RelocateRenderElement::from_element(elem, dim_loc, Relocate::Relative);
                push(elem);
            }

            // ── Folder tab ───────────────────────────────────────────
            let tab = self.project_tab(
                gles.renderer,
                &entry.name,
                entry.state_label,
                entry.color,
                scale,
            );
            let tab_size = tab.logical_size();
            let max_stagger = (card_rect.size.w - tab_size.w).max(0.);
            let stagger = ((DRAWER_TAB_INSET_X
                + DRAWER_TAB_STAGGER_X * entry.idx as f64)
            .min(max_stagger))
            .round();
            let tab_loc = Point::<i32, Physical>::from((
                card_base_loc.x + (stagger * scale).round() as i32,
                content_loc.y - tab_h_phys,
            ));
            let tab_logical = Point::from((
                tab_loc.x as f64 / scale,
                tab_loc.y as f64 / scale,
            ));
            let elem = MonitorInnerRenderElement::Texture(PrimaryGpuTextureRenderElement(
                TextureRenderElement::from_texture_buffer(
                    tab,
                    tab_logical,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ),
            ));
            let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
            let elem = RelocateRenderElement::from_element(elem, tab_loc, Relocate::Relative);
            push(elem);

            // ── Border overlay ───────────────────────────────────────
            let border = self.project_border(
                gles.renderer,
                entry.color,
                card_w_px.max(1),
                card_h_px.max(1),
                scale,
            );
            let border_logical = Point::from((
                content_loc.x as f64 / scale,
                content_loc.y as f64 / scale,
            ));
            let elem = MonitorInnerRenderElement::Texture(PrimaryGpuTextureRenderElement(
                TextureRenderElement::from_texture_buffer(
                    border,
                    border_logical,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ),
            ));
            let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
            let elem =
                RelocateRenderElement::from_element(elem, content_loc, Relocate::Relative);
            push(elem);

            // ── Content (bottom-most within this card) ───────────────
            match &entry.item {
                ProjectOverviewItem::Warm(ws) => {
                    // Fit the workspace render into the card.
                    let fit_factor =
                        card_rect.size.w * size_factor / self.view_size.w * zoom;
                    let crop_bounds = Rectangle::new(
                        Point::from((-i32::MAX / 2, 0)),
                        Size::from((i32::MAX, i32::MAX)),
                    );
                    let xray_pos = XrayPos::new(card_rect.loc, zoom);

                    let mut push_card = |elem: WorkspaceRenderElement<R>| {
                        if let Some(cropped) =
                            CropRenderElement::from_element(elem, scale, crop_bounds)
                        {
                            let inner = MonitorInnerRenderElement::Workspace(cropped);
                            let scaled = RescaleRenderElement::from_element(
                                inner,
                                Point::default(),
                                fit_factor,
                            );
                            push(RelocateRenderElement::from_element(
                                scaled,
                                content_loc,
                                Relocate::Relative,
                            ));
                        }
                    };

                    ws.render_scrolling(
                        ctx.r(),
                        xray_pos,
                        focus_ring,
                        RenderLayer::Normal,
                        &mut push_card,
                    );
                    ws.render_floating(
                        ctx.r(),
                        xray_pos,
                        focus_ring,
                        RenderLayer::Normal,
                        &mut push_card,
                    );
                }
                ProjectOverviewItem::Placeholder => {
                    let fill_size = Size::from((
                        card_w_px.max(1) as f64 / scale,
                        card_h_px.max(1) as f64 / scale,
                    ));
                    let fill = SolidColorBuffer::new(fill_size, DRAWER_PLACEHOLDER_COLOR);
                    let elem = MonitorInnerRenderElement::SolidColor(
                        SolidColorRenderElement::from_buffer(
                            &fill,
                            Point::from((0., 0.)),
                            1.,
                            Kind::Unspecified,
                        ),
                    );
                    let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                    let elem = RelocateRenderElement::from_element(
                        elem,
                        content_loc,
                        Relocate::Relative,
                    );
                    push(elem);
                }
            }
        }

        // Dark backdrop behind all cards — pushed LAST so it lands at the bottom.
        let backdrop_buffer = SolidColorBuffer::new(self.view_size, DRAWER_BACKDROP_COLOR);
        let elem = MonitorInnerRenderElement::SolidColor(SolidColorRenderElement::from_buffer(
            &backdrop_buffer,
            Point::from((0., 0.)),
            1.,
            Kind::Unspecified,
        ));
        let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
        let elem = RelocateRenderElement::from_element(elem, Point::default(), Relocate::Relative);
        push(elem);
    }

    pub fn render_workspace_shadows<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let Some(progress) = self.overview_progress.as_ref().map(|p| p.clamped_value()) else {
            return;
        };
        let alpha = progress.clamp(0., 1.) as f32;

        let _span = tracy_client::span!("Monitor::render_workspace_shadows");

        let scale = self.scale.fractional_scale();
        let zoom = self.overview_zoom();

        for (ws, geo) in self.workspaces_with_render_geo() {
            ws.render_shadow(renderer, &mut |elem| {
                let elem = elem.with_alpha(alpha);
                let elem = MonitorInnerRenderElement::Shadow(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
                let elem = RelocateRenderElement::from_element(
                    elem,
                    geo.loc.to_physical_precise_round(scale),
                    Relocate::Relative,
                );
                push(elem);
            });
        }
    }

    pub fn workspace_switch_gesture_begin(&mut self, is_touchpad: bool) {
        let center_idx = self.active_workspace_idx;
        let current_idx = self.workspace_render_idx();

        let gesture = WorkspaceSwitchGesture {
            center_idx,
            start_idx: current_idx,
            current_idx,
            animation: None,
            tracker: SwipeTracker::new(),
            is_touchpad,
            is_clamped: !self.overview_open,
            dnd_last_event_time: None,
            dnd_nonzero_start_time: None,
        };
        self.workspace_switch = Some(WorkspaceSwitch::Gesture(gesture));
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        if let Some(WorkspaceSwitch::Gesture(WorkspaceSwitchGesture {
            dnd_last_event_time: Some(_),
            ..
        })) = &self.workspace_switch
        {
            // Already active.
            return;
        }

        if !self.overview_open {
            // This gesture is only for the overview.
            return;
        }

        let center_idx = self.active_workspace_idx;
        let current_idx = self.workspace_render_idx();

        let gesture = WorkspaceSwitchGesture {
            center_idx,
            start_idx: current_idx,
            current_idx,
            animation: None,
            tracker: SwipeTracker::new(),
            is_touchpad: false,
            is_clamped: false,
            dnd_last_event_time: Some(self.clock.now_unadjusted()),
            dnd_nonzero_start_time: None,
        };
        self.workspace_switch = Some(WorkspaceSwitch::Gesture(gesture));
    }

    pub fn workspace_switch_gesture_update(
        &mut self,
        delta_y: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        let Some(WorkspaceSwitch::Gesture(gesture)) = &self.workspace_switch else {
            return None;
        };

        if gesture.is_touchpad != is_touchpad || gesture.dnd_last_event_time.is_some() {
            return None;
        }

        let zoom = self.overview_zoom();
        let total_height = if gesture.is_touchpad {
            WORKSPACE_GESTURE_MOVEMENT
        } else {
            self.workspace_size_with_gap(1.).h
        };

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return None;
        };

        // Reduce the effect of zoom on the touchpad somewhat.
        let delta_scale = if gesture.is_touchpad {
            (zoom - 1.) / 2.5 + 1.
        } else {
            zoom
        };

        let delta_y = delta_y / delta_scale;
        let mut rubber_band = WORKSPACE_GESTURE_RUBBER_BAND;
        rubber_band.limit /= zoom;

        gesture.tracker.push(delta_y, timestamp);

        let pos = gesture.tracker.pos() / total_height;

        let (min, max) = gesture.min_max(self.workspaces.len());
        let new_idx = gesture.start_idx + pos;
        let new_idx = rubber_band.clamp(min, max, new_idx);

        if gesture.current_idx == new_idx {
            return Some(false);
        }

        gesture.current_idx = new_idx;
        Some(true)
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, pos: Point<f64, Logical>, speed: f64) -> bool {
        let zoom = self.overview_zoom();

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return false;
        };

        let Some(last_time) = gesture.dnd_last_event_time else {
            // Not a DnD scroll.
            return false;
        };

        let config = &self.options.gestures.dnd_edge_workspace_switch;
        let trigger_height = config.trigger_height;

        // Restrict the scrolling horizontally to the strip of workspaces to avoid unwanted trigger
        // after using the hot corner or during horizontal scroll.
        let width = self.view_size.w * zoom;
        let x = pos.x - (self.view_size.w - width) / 2.;

        // Consider the working area so layer-shell docks and such don't prevent scrolling.
        let y = pos.y - self.working_area.loc.y;
        let height = self.working_area.size.h;

        let y = y.clamp(0., height);
        let trigger_height = trigger_height.clamp(0., height / 2.);

        let delta = if x < 0. || width <= x {
            // Outside the bounds horizontally.
            0.
        } else if y < trigger_height {
            -(trigger_height - y)
        } else if height - y < trigger_height {
            trigger_height - (height - y)
        } else {
            0.
        };

        let delta = if trigger_height < 0.01 {
            // Sanity check for trigger-height 0 or small window sizes.
            0.
        } else {
            // Normalize to [0, 1].
            delta / trigger_height
        };
        let delta = delta * speed;

        let now = self.clock.now_unadjusted();
        gesture.dnd_last_event_time = Some(now);

        if delta == 0. {
            // We're outside the scrolling zone.
            gesture.dnd_nonzero_start_time = None;
            return false;
        }

        let nonzero_start = *gesture.dnd_nonzero_start_time.get_or_insert(now);

        // Delay starting the gesture a bit to avoid unwanted movement when dragging across
        // monitors.
        let delay = Duration::from_millis(u64::from(config.delay_ms));
        if now.saturating_sub(nonzero_start) < delay {
            return true;
        }

        let time_delta = now.saturating_sub(last_time).as_secs_f64();

        let delta = delta * time_delta * config.max_speed;

        gesture.tracker.push(delta, now);

        let total_height = WORKSPACE_DND_EDGE_SCROLL_MOVEMENT;
        let pos = gesture.tracker.pos() / total_height;
        let unclamped = gesture.start_idx + pos;

        let (min, max) = gesture.min_max(self.workspaces.len());
        let clamped = unclamped.clamp(min, max);

        // Make sure that DnD scrolling too much outside the min/max does not "build up".
        gesture.start_idx += clamped - unclamped;
        gesture.current_idx = clamped;

        true
    }

    pub fn workspace_switch_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        let Some(WorkspaceSwitch::Gesture(gesture)) = &self.workspace_switch else {
            return false;
        };

        if is_touchpad.is_some_and(|x| gesture.is_touchpad != x) {
            return false;
        }

        let zoom = self.overview_zoom();
        let total_height = if gesture.dnd_last_event_time.is_some() {
            WORKSPACE_DND_EDGE_SCROLL_MOVEMENT
        } else if gesture.is_touchpad {
            WORKSPACE_GESTURE_MOVEMENT
        } else {
            self.workspace_size_with_gap(1.).h
        };

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return false;
        };

        // Take into account any idle time between the last event and now.
        let now = self.clock.now_unadjusted();
        gesture.tracker.push(0., now);

        let mut rubber_band = WORKSPACE_GESTURE_RUBBER_BAND;
        rubber_band.limit /= zoom;

        let mut velocity = gesture.tracker.velocity() / total_height;
        let current_pos = gesture.tracker.pos() / total_height;
        let pos = gesture.tracker.projected_end_pos() / total_height;

        let (min, max) = gesture.min_max(self.workspaces.len());
        let new_idx = gesture.start_idx + pos;

        let new_idx = new_idx.clamp(min, max);
        let new_idx = new_idx.round() as usize;

        velocity *= rubber_band.clamp_derivative(min, max, gesture.start_idx + current_pos);

        if self.active_workspace_idx != new_idx {
            self.previous_workspace_id = Some(self.workspaces[self.active_workspace_idx].id());
        }

        self.active_workspace_idx = new_idx;
        self.workspace_switch = Some(WorkspaceSwitch::Animation(Animation::new(
            self.clock.clone(),
            gesture.current_idx,
            new_idx as f64,
            velocity,
            self.options.animations.workspace_switch.0,
        )));

        true
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        if !matches!(
            self.workspace_switch,
            Some(WorkspaceSwitch::Gesture(WorkspaceSwitchGesture {
                dnd_last_event_time: Some(_),
                ..
            }))
        ) {
            // Not a DnD scroll.
            return;
        };

        self.workspace_switch_gesture_end(None);
    }

    pub fn scale(&self) -> smithay::output::Scale {
        self.scale
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn layout_config(&self) -> Option<&niri_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub(super) fn verify_invariants(&self) {
        use approx::assert_abs_diff_eq;

        let options =
            Options::clone(&self.base_options).with_merged_layout(self.layout_config.as_ref());
        assert_eq!(&*self.options, &options);

        assert!(
            !self.workspaces.is_empty(),
            "monitor must have at least one workspace"
        );
        assert!(self.active_workspace_idx < self.workspaces.len());

        if let Some(WorkspaceSwitch::Animation(anim)) = &self.workspace_switch {
            let before_idx = anim.from() as usize;
            let after_idx = anim.to() as usize;

            assert!(before_idx < self.workspaces.len());
            assert!(after_idx < self.workspaces.len());
        }

        assert!(
            !self.workspaces.last().unwrap().has_windows(),
            "monitor must have an empty workspace in the end"
        );
        if self.options.layout.empty_workspace_above_first {
            assert!(
                !self.workspaces.first().unwrap().has_windows(),
                "first workspace must be empty when empty_workspace_above_first is set"
            )
        }

        assert!(
            self.workspaces.last().unwrap().name.is_none(),
            "monitor must have an unnamed workspace in the end"
        );
        if self.options.layout.empty_workspace_above_first {
            assert!(
                self.workspaces.first().unwrap().name.is_none(),
                "first workspace must be unnamed when empty_workspace_above_first is set"
            )
        }

        if self.options.layout.empty_workspace_above_first {
            assert!(
                self.workspaces.len() != 2,
                "if empty_workspace_above_first is set there must be just 1 or 3+ workspaces"
            )
        }

        // If there's no workspace switch in progress, there can't be any non-last non-active
        // empty workspaces. If empty_workspace_above_first is set then the first workspace
        // will be empty too.
        let pre_skip = if self.options.layout.empty_workspace_above_first {
            1
        } else {
            0
        };
        if self.workspace_switch.is_none() {
            for (idx, ws) in self
                .workspaces
                .iter()
                .enumerate()
                .skip(pre_skip)
                .rev()
                // skip last
                .skip(1)
            {
                if idx != self.active_workspace_idx {
                    assert!(
                        ws.has_windows_or_name(),
                        "non-active workspace can't be empty and unnamed except the last one"
                    );
                }
            }
        }

        for workspace in &self.workspaces {
            assert_eq!(self.clock, workspace.clock);

            assert_eq!(
                self.scale().integer_scale(),
                workspace.scale().integer_scale()
            );
            assert_eq!(
                self.scale().fractional_scale(),
                workspace.scale().fractional_scale()
            );
            assert_eq!(self.view_size, workspace.view_size());
            assert_eq!(self.working_area, workspace.working_area());

            assert_eq!(
                workspace.base_options, self.options,
                "workspace options must be synchronized with monitor"
            );
        }

        let scale = self.scale().fractional_scale();
        let iter = self.workspaces_with_render_geo();
        for (_ws, ws_geo) in iter {
            let pos = ws_geo.loc;
            let rounded_pos = pos.to_physical_precise_round(scale).to_logical(scale);

            // Workspace positions must be rounded to physical pixels.
            assert_abs_diff_eq!(pos.x, rounded_pos.x, epsilon = 1e-5);
            assert_abs_diff_eq!(pos.y, rounded_pos.y, epsilon = 1e-5);
        }
    }
}

// ── Drawer texture generation ─────────────────────────────────────────────

/// Draw a pango text layout and return its pixel size.
fn measure_text(text: &str, font: &pango::FontDescription) -> (i32, i32) {
    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0).unwrap();
    let cr = cairo::Context::new(&surface).unwrap();
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_single_paragraph_mode(true);
    layout.set_font_description(Some(font));
    layout.set_text(text);
    layout.pixel_size()
}
/// Render a folder tab (rounded top rect + dot + name + state) into a texture.
///
/// The returned texture's logical size is the tab size in logical pixels.
fn generate_project_tab(
    renderer: &mut GlesRenderer,
    name: &str,
    state: &'static str,
    color: [f32; 4],
    scale: f64,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let _span = tracy_client::span!("monitor::generate_project_tab");

    let s = |v: f64| v * scale;

    // Fonts.
    let mut name_font = pango::FontDescription::from_string("sans-serif Bold 13");
    name_font.set_absolute_size(s(13.) * pango::SCALE as f64);
    let mut state_font = pango::FontDescription::from_string("sans-serif 11");
    state_font.set_absolute_size(s(11.) * pango::SCALE as f64);

    // Measure.
    let (name_w, name_h) = measure_text(name, &name_font);
    let state_upper = state.to_uppercase();
    let (state_w, _) = measure_text(&state_upper, &state_font);

    let pad_x = s(16.).round() as i32;
    let dot_d = s(6.).round() as i32;
    let dot_gap = s(8.).round() as i32;
    let state_gap = s(10.).round() as i32;
    let width = pad_x * 2 + dot_d + dot_gap + name_w + state_gap + state_w;
    let height = (s(DRAWER_TAB_HEIGHT)).round() as i32;
    if width <= 0 || height <= 0 {
        anyhow::bail!("empty project tab");
    }
    let width = min(width, 16383);
    let height = min(height, 16383);

    // Draw.
    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;

    // Rounded-top-rect path.
    let radius = s(DRAWER_TAB_RADIUS).min(height as f64 / 2.);
    cr.new_sub_path();
    cr.arc(
        radius,
        height as f64,
        radius,
        std::f64::consts::PI,
        1.5 * std::f64::consts::PI,
    );
    cr.line_to(width as f64 - radius, 0.);
    cr.arc(
        width as f64 - radius,
        height as f64,
        radius,
        1.5 * std::f64::consts::PI,
        2. * std::f64::consts::PI,
    );
    cr.line_to(width as f64, height as f64);
    cr.line_to(0., height as f64);
    cr.close_path();
    cr.set_source_rgb(color[0] as f64, color[1] as f64, color[2] as f64);
    let _ = cr.fill();

    // State dot.
    let ink_y = (height as f64 - name_h as f64) / 2.;
    cr.arc(
        pad_x as f64 + dot_d as f64 / 2.,
        ink_y + name_h as f64 / 2.,
        dot_d as f64 / 2.,
        0.,
        2. * std::f64::consts::PI,
    );
    cr.set_source_rgba(
        DRAWER_TAB_TEXT_COLOR[0],
        DRAWER_TAB_TEXT_COLOR[1],
        DRAWER_TAB_TEXT_COLOR[2],
        0.55,
    );
    let _ = cr.fill();

    // Name.
    let mut x = pad_x + dot_d + dot_gap;
    pangocairo_draw_text(
        &cr,
        &name_font,
        DRAWER_TAB_TEXT_COLOR,
        1.,
        x as f64,
        ink_y,
    );

    // State label.
    x += name_w + state_gap;
    pangocairo_draw_text(
        &cr,
        &state_font,
        DRAWER_TAB_TEXT_COLOR,
        0.75,
        x as f64,
        (height as f64 - name_h as f64) / 2. + s(1.),
    );

    drop(cr);
    let data = surface.take_data().unwrap();
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(buffer)
}

/// Render a rounded-rect border overlay into a texture.
fn generate_project_border(
    renderer: &mut GlesRenderer,
    color: [f32; 4],
    width_px: i32,
    height_px: i32,
    radius: f64,
    border_w: f64,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let _span = tracy_client::span!("monitor::generate_project_border");

    let width = width_px.clamp(1, 16383);
    let height = height_px.clamp(1, 16383);

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;

    let inset = border_w / 2.;
    let radius = radius.min((width as f64 - inset * 2.) / 2.)
        .min((height as f64 - inset * 2.) / 2.);
    rounded_rect_path(
        &cr,
        inset,
        inset,
        width as f64 - border_w,
        height as f64 - border_w,
        radius,
    );
    cr.set_line_width(border_w);
    cr.set_source_rgba(
        color[0] as f64,
        color[1] as f64,
        color[2] as f64,
        color[3] as f64,
    );
    let _ = cr.stroke();

    drop(cr);
    let data = surface.take_data().unwrap();
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        1.,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(buffer)
}

/// Append a rounded rectangle path to the cairo context.
fn rounded_rect_path(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    r: f64,
) {
    cr.new_sub_path();
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
    cr.arc(x + w - r, y + r, r, 1.5 * std::f64::consts::PI, 2. * std::f64::consts::PI);
    cr.arc(x + w - r, y + h - r, r, 0., 0.5 * std::f64::consts::PI);
    cr.arc(x + r, y + h - r, r, 0.5 * std::f64::consts::PI, std::f64::consts::PI);
    cr.close_path();
}

/// Draw a text layout onto the cairo context at the given position.
fn pangocairo_draw_text(
    cr: &cairo::Context,
    font: &pango::FontDescription,
    color: [f64; 4],
    alpha: f64,
    x: f64,
    y: f64,
) {
    let layout = pangocairo::functions::create_layout(cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_single_paragraph_mode(true);
    layout.set_font_description(Some(font));
    let _ = cr.save();
    cr.set_source_rgba(color[0], color[1], color[2], alpha);
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, &layout);
    let _ = cr.restore();
}
