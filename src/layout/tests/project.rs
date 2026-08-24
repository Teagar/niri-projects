//! Correctness tests for the Projects feature.
//!
//! These correspond to the five contracts in the spec:
//! 1. switching_away_and_back_preserves_windows
//! 2. overview_double_toggle_never_panics
//! 3. reload_projects_preserves_runtime_state
//! 4. switch_to_warm_or_active_project_is_o1
//! 5. depth_navigation_in_overview_does_not_mutate_state

use super::*;

use crate::layout::monitor::{tab_shape_path, DRAWER_MAX_VISIBLE_DEPTH, DRAWER_TAB_HEIGHT};
use pangocairo::cairo;

fn make_project_config(name: &str) -> niri_config::ProjectConfig {
    niri_config::ProjectConfig {
        name: name.to_string(),
        keep_open: false,
        overview_border: None,
        workspaces: vec![],
    }
}

fn make_project_config_with_ws(name: &str, ws_name: &str) -> niri_config::ProjectConfig {
    niri_config::ProjectConfig {
        name: name.to_string(),
        keep_open: false,
        overview_border: None,
        workspaces: vec![niri_config::ProjectWorkspaceConfig {
            name: ws_name.to_string(),
            spawn_at_startup: vec![],
        }],
    }
}

fn setup_layout_with_projects() -> Layout<TestWindow> {
    let mut layout = Layout::default();

    // Add an output so workspaces can be attached.
    let ops = [Op::AddOutput(1)];
    for op in &ops.clone() {
        op.clone().apply(&mut layout);
    }

    // Configure two projects.
    layout.ensure_project(&make_project_config("project_a"));
    layout.ensure_project(&make_project_config("project_b"));

    layout
}

#[test]
fn switching_away_and_back_preserves_windows() {
    let mut layout = setup_layout_with_projects();

    // Switch to project_a (dormant → active).
    let result = layout.switch_to_project("project_a");
    assert!(result.is_some(), "project_a should have been activated");
    assert_eq!(result.as_ref().unwrap().activated_name, "project_a");
    assert!(
        !result.unwrap().was_warm,
        "first activation should not be warm"
    );

    // Add a window in project_a's active workspace.
    let win_a = TestWindow::new(TestWindowParams::new(42));
    layout.add_window(
        win_a.clone(),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    assert!(layout.has_window(&42));

    // Switch to project_b.
    let result = layout.switch_to_project("project_b");
    assert!(result.is_some(), "project_b should have been activated");
    assert_eq!(result.as_ref().unwrap().activated_name, "project_b");

    // Window 42 must NOT be visible in the active monitor (it's parked).
    // But project_a should be warm (its workspaces are parked).
    let project_a_idx = layout.project_index("project_a").unwrap();
    assert!(
        layout.projects[project_a_idx].is_warm(),
        "project_a should be warm after switching away"
    );

    // Switch back to project_a.
    let result = layout.switch_to_project("project_a");
    assert!(result.is_some(), "switching back to project_a should work");
    assert!(result.as_ref().unwrap().was_warm, "should be warm");

    // Window 42 must still exist with the same id.
    assert!(
        layout.has_window(&42),
        "window 42 must survive the round-trip: a->b->a"
    );
}

#[test]
fn animated_switch_away_and_back_preserves_windows() {
    let mut layout = setup_layout_with_projects();

    // Switch to project_a.
    layout.switch_to_project("project_a");

    // Add a window.
    let win = TestWindow::new(TestWindowParams::new(99));
    layout.add_window(
        win,
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    assert!(layout.has_window(&99));

    // Switch to project_b.
    layout.switch_to_project("project_b");

    // Advance animations (the project-switch animation runs here).
    layout.advance_animations();

    // Switch back to project_a.
    layout.switch_to_project("project_a");

    // Advance animations again.
    layout.advance_animations();

    // Window 99 must still exist.
    assert!(
        layout.has_window(&99),
        "window 99 must survive animated round-trip"
    );
}

#[test]
fn switching_preserves_across_outputs() {
    let mut layout = Layout::default();

    // Add two outputs.
    Op::AddOutput(1).apply(&mut layout);
    Op::AddOutput(2).apply(&mut layout);

    // Configure two projects.
    layout.ensure_project(&make_project_config("alpha"));
    layout.ensure_project(&make_project_config("beta"));

    // Switch to alpha.
    layout.switch_to_project("alpha");

    // Add a window in alpha's workspace on output 1.
    let win = TestWindow::new(TestWindowParams::new(7));
    layout.add_window(
        win,
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    assert!(layout.has_window(&7));

    // Switch to beta.
    layout.switch_to_project("beta");

    // Switch back to alpha.
    layout.switch_to_project("alpha");

    // Window 7 must still exist.
    assert!(
        layout.has_window(&7),
        "window 7 must survive multi-output switch round-trip"
    );
}

#[test]
fn overview_double_toggle_never_panics() {
    let mut layout = Layout::default();
    Op::AddOutput(1).apply(&mut layout);

    // Toggle overview on.
    layout.toggle_overview();
    assert!(layout.is_overview_open());

    // Toggle off immediately (even while the animation may be in-flight).
    layout.toggle_overview();
    layout.advance_animations();

    // Should be closed.
    assert!(!layout.is_overview_open());

    // Repeat several more times to ensure idempotency.
    for _ in 0..5 {
        layout.toggle_overview();
        layout.toggle_overview();
        layout.advance_animations();
    }

    assert!(
        !layout.is_overview_open(),
        "overview should be closed after toggling pairs"
    );
}

#[test]
fn reload_projects_preserves_runtime_state() {
    let mut layout = setup_layout_with_projects();

    // Switch to project_a and add a window.
    layout.switch_to_project("project_a");
    let win = TestWindow::new(TestWindowParams::new(55));
    layout.add_window(
        win,
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    assert!(layout.has_window(&55));

    // Switch to project_b (parks project_a's workspaces).
    layout.switch_to_project("project_b");
    assert!(layout.has_window(&55), "window must exist while parked");

    // Simulate config reload: re-ensure both projects with the same config.
    // This must NOT destroy project_a's parked workspaces.
    layout.ensure_project(&make_project_config("project_a"));
    layout.ensure_project(&make_project_config("project_b"));

    // Switch back to project_a.
    layout.switch_to_project("project_a");
    assert!(
        layout.has_window(&55),
        "window 55 must survive config reload while parked"
    );
}

#[test]
fn switch_to_warm_or_active_project_is_o1() {
    let mut layout = setup_layout_with_projects();

    // Activate project_a.
    layout.switch_to_project("project_a");

    // Add a window so project_a has content.
    let win = TestWindow::new(TestWindowParams::new(1));
    layout.add_window(
        win,
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    // Switch to project_b.
    layout.switch_to_project("project_b");

    // Switching back to project_a should be instant (warm).
    let result = layout.switch_to_project("project_a");
    assert!(
        result.is_some(),
        "switch to warm project should return a result"
    );
    assert!(
        result.unwrap().was_warm,
        "should report warm path was taken (no respawn)"
    );

    // Window still exists.
    assert!(layout.has_window(&1));
}

#[test]
fn depth_navigation_does_not_mutate_state() {
    let mut layout = setup_layout_with_projects();

    // Open the project overview drawer.
    layout.toggle_project_overview();
    assert!(layout.is_project_overview_open());

    // Navigate selection — wraps around without panic.
    let initial_selected = layout.project_overview_selected;
    layout.project_overview_prev();
    layout.project_overview_next();

    // Selection should return to initial after round-trip.
    assert_eq!(
        layout.project_overview_selected, initial_selected,
        "selection round-trip must return to initial"
    );

    // Close via commit (with no active project change).
    let initial_active = layout.active_project_name.clone();
    layout.project_overview_commit();
    assert!(
        !layout.is_project_overview_open(),
        "drawer must close after commit"
    );
    assert_eq!(layout.active_project_name, initial_active);
}

#[test]
fn closing_project_destroys_workspaces() {
    let mut layout = setup_layout_with_projects();

    // Switch to project_a, add a window.
    layout.switch_to_project("project_a");
    let win = TestWindow::new(TestWindowParams::new(10));
    layout.add_window(
        win,
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    assert!(layout.has_window(&10));

    // Switch to project_b.
    layout.switch_to_project("project_b");

    // Close project_a — this should destroy its workspaces.
    assert!(layout.close_project("project_a"));

    // Window 10 should no longer exist.
    assert!(
        !layout.has_window(&10),
        "window 10 should be destroyed after close_project"
    );
}

#[test]
fn overview_drawer_entries_include_all_projects() {
    let mut layout = setup_layout_with_projects();

    // Closed drawer has no entries.
    assert!(layout.project_overview_entries().is_empty());

    layout.toggle_project_overview();

    // One card per project, in config order.
    let entries = layout.project_overview_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].idx, 0);
    assert_eq!(entries[0].name, "project_a");
    assert_eq!(entries[1].idx, 1);
    assert_eq!(entries[1].name, "project_b");

    // Nothing activated yet: both cards report dormant.
    assert_eq!(entries[0].state_label, "dormant");
    assert_eq!(entries[1].state_label, "dormant");

    // Activating a project marks its card as active.
    layout.switch_to_project("project_b");
    let entries = layout.project_overview_entries();
    assert_eq!(entries[0].state_label, "dormant");
    assert_eq!(entries[1].state_label, "active");

    // Closing the drawer clears the entries again.
    layout.toggle_project_overview();
    assert!(layout.project_overview_entries().is_empty());
}

/// Rule 3 regression test: the regular overview must NEVER show project cards.
#[test]
fn regular_overview_never_shows_project_cards() {
    let mut layout = setup_layout_with_projects();

    // One active project, one warm.
    layout.switch_to_project("project_a");
    layout.ensure_project(&make_project_config_with_ws("project_b", "b0"));

    // Open the regular overview.
    layout.toggle_overview();
    assert!(layout.is_overview_open());
    assert!(
        !layout.is_project_overview_open(),
        "regular overview must not open the project drawer"
    );

    // project_overview_entries() must be empty: projects don't leak in.
    let entries = layout.project_overview_entries();
    assert_eq!(
        entries.len(),
        0,
        "regular overview must never trigger project card rendering"
    );

    layout.toggle_overview();
}

#[test]
fn overview_selection_navigation_rotates_selection() {
    let mut layout = setup_layout_with_projects();

    // Stack is [project_a(0), project_b(1)]; selection starts on project_a.
    assert_eq!(layout.project_overview_selected, 0);

    layout.toggle_project_overview();
    assert_eq!(layout.project_overview_selected, 0);

    // Next moves forward: project_b becomes selected.
    layout.project_overview_next();
    assert_eq!(layout.project_overview_selected, 1);

    // Next again wraps back around to project_a.
    layout.project_overview_next();
    assert_eq!(layout.project_overview_selected, 0);

    // Prev wraps backwards to project_b.
    layout.project_overview_prev();
    assert_eq!(layout.project_overview_selected, 1);

    // Browsing the selection never mutated any project's runtime state.
    assert!(layout.projects[0].is_dormant() || layout.projects[0].is_warm());
}

#[test]
fn overview_entry_order_follows_config_order() {
    let mut layout = setup_layout_with_projects();

    // Three projects: cards must be listed in config order [a, b, c].
    layout.ensure_project(&make_project_config_with_ws("project_c", "c0"));

    layout.toggle_project_overview();
    let entries = layout.project_overview_entries();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["project_a", "project_b", "project_c"]);

    // The selected card always sits at depth 0 (front of the stack).
    assert_eq!(entries[layout.project_overview_selected].depth, 0.);
}

#[test]
fn overview_navigation_wraps_around() {
    let mut layout = setup_layout_with_projects();

    // Three projects so wrap-around is observable beyond a simple swap.
    layout.ensure_project(&make_project_config("project_c"));

    layout.toggle_project_overview();
    assert_eq!(layout.project_overview_selected, 0);

    // Prev from the first project wraps to the last (no clamping).
    layout.project_overview_prev();
    assert_eq!(layout.project_overview_selected, 2);

    // Next wraps back around to the first.
    layout.project_overview_next();
    assert_eq!(layout.project_overview_selected, 0);
}

#[test]
fn windows_created_in_a_project_stay_in_that_project() {
    let mut layout = setup_layout_with_projects();

    let monitor_windows = |layout: &Layout<TestWindow>| {
        let mut ids = Vec::new();
        if let super::super::MonitorSet::Normal { monitors, .. } = &layout.monitor_set {
            for mon in monitors {
                for ws in &mon.workspaces {
                    for win in ws.windows() {
                        ids.push(*win.id());
                    }
                }
            }
        }
        ids
    };

    // Activate project_a and create window 42 there.
    layout.switch_to_project("project_a");
    let win_a = TestWindow::new(TestWindowParams::new(42));
    layout.add_window(
        win_a,
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    assert_eq!(monitor_windows(&layout), vec![42]);

    // Switch to project_b (project_a parks with its window) and create
    // window 43 while project_b is active.
    layout.switch_to_project("project_b");
    let win_b = TestWindow::new(TestWindowParams::new(43));
    layout.add_window(
        win_b,
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    // Exactly one window visible: 43 in project_b. Window 42 stays alive but
    // parked inside project_a.
    assert_eq!(monitor_windows(&layout), vec![43]);
    assert!(layout.has_window(&42), "window 42 must survive parking");

    // Round trip: back to project_a, window 42 is visible again and 43 parks.
    layout.switch_to_project("project_a");
    assert_eq!(monitor_windows(&layout), vec![42]);
    assert!(layout.has_window(&43), "window 43 must survive parking");
}

// ── Drawer geometry regression tests ────────────────────────────────────────
//
// These guard against the "cards drawn off-screen" failure mode where card
// rectangles extended far past the output bounds (only a single corner was
// ever visible) due to double-offset positioning of drawer elements.

/// Every drawer card (including its folder tab above it) must stay within the
/// output's logical bounds for all visible depths and common scales.
#[test]
fn drawer_card_bounds_stay_within_output() {
    let mut layout = setup_layout_with_projects();
    // Two extra projects so the maximum visible depth (3) is exercised.
    layout.ensure_project(&make_project_config("project_c"));
    layout.ensure_project(&make_project_config("project_d"));
    layout.toggle_project_overview();

    let output = layout.outputs().next().unwrap().clone();
    let mon = layout.monitor_for_output(&output).unwrap();
    let scale = mon.scale().fractional_scale();
    let view = Size::<f64, Logical>::from((1280., 720.));

    for stack_pos in 0..=DRAWER_MAX_VISIBLE_DEPTH as usize {
        let rect = mon.project_drawer_card_layout(scale, stack_pos as f64);

        assert!(
            rect.loc.x >= 0.,
            "card {stack_pos} extends past the left edge: {rect:?}"
        );
        assert!(
            rect.loc.y - DRAWER_TAB_HEIGHT >= -0.5,
            "card {stack_pos} tab extends past the top edge: {rect:?}"
        );
        assert!(
            rect.loc.x + rect.size.w <= view.w + 0.5,
            "card {stack_pos} extends past the right edge: {rect:?}"
        );
        assert!(
            rect.loc.y + rect.size.h <= view.h + 0.5,
            "card {stack_pos} extends past the bottom edge: {rect:?}"
        );

        // A point in each card's exposed strip (the part not covered by the
        // card in front of it) must hit-test back to that card.
        let prev_bottom = if stack_pos == 0 {
            rect.loc.y
        } else {
            let prev =
                mon.project_drawer_card_layout(scale, (stack_pos - 1) as f64);
            prev.loc.y + prev.size.h
        };
        let probe = Point::from((
            rect.loc.x + rect.size.w / 2.,
            (prev_bottom + rect.loc.y + rect.size.h) / 2.,
        ));
        let hit = mon.project_drawer_hit_test(probe, 4);
        assert_eq!(
            hit,
            Some(stack_pos),
            "exposed strip of card {stack_pos} must hit-test to itself"
        );
    }
}

/// Stacked cards must remain centered on the front card's horizontal axis
/// (they shrink toward the middle, not toward their top-left corner).
#[test]
fn drawer_stacked_cards_stay_centered() {
    let layout = setup_layout_with_projects();

    let output = layout.outputs().next().unwrap().clone();
    let mon = layout.monitor_for_output(&output).unwrap();
    let scale = mon.scale().fractional_scale();

    let front = mon.project_drawer_card_layout(scale, 0.);
    let back = mon.project_drawer_card_layout(scale, 1.);

    let front_cx = front.loc.x + front.size.w / 2.;
    let back_cx = back.loc.x + back.size.w / 2.;
    assert!(
        (front_cx - back_cx).abs() < 0.5,
        "stacked cards drifted off-center: front cx {front_cx}, back cx {back_cx}"
    );

    // Deeper cards are strictly smaller and strictly lower than the front.
    assert!(back.size.w < front.size.w);
    assert!(back.loc.y > front.loc.y);
}

/// Fractional depths (mid-animation) must produce in-bounds cards too.
#[test]
fn drawer_mid_animation_depths_stay_in_bounds() {
    let layout = setup_layout_with_projects();

    let output = layout.outputs().next().unwrap().clone();
    let mon = layout.monitor_for_output(&output).unwrap();
    let scale = mon.scale().fractional_scale();
    let view = Size::<f64, Logical>::from((1280., 720.));

    let mut depth = -0.4;
    while depth <= DRAWER_MAX_VISIBLE_DEPTH + 0.4 {
        let rect = mon.project_drawer_card_layout(scale, depth);
        assert!(rect.loc.x >= 0. && rect.loc.y >= -DRAWER_TAB_HEIGHT - 0.5);
        assert!(
            rect.loc.x + rect.size.w <= view.w + 0.5
                && rect.loc.y + rect.size.h <= view.h + 0.5,
            "depth {depth} produced an out-of-bounds card: {rect:?}"
        );
        depth += 0.1;
    }
}

/// Regression: the folder tab path must produce a solid rounded-TOP
/// rectangle (square bottom corners), not a degenerate wedge. Guards
/// against arc-center mistakes in the cairo path construction.
#[test]
fn project_tab_shape_is_solid_rounded_rectangle() {
    let (w, h) = (200i32, 30i32);
    let radius = 10f64;
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).unwrap();
    {
        let cr = cairo::Context::new(&surface).unwrap();
        tab_shape_path(&cr, w as f64, h as f64, radius);
        cr.set_source_rgb(1., 1., 1.);
        let _ = cr.fill();
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.data().unwrap();
    // ARgb32 is native-endian; alpha is the 4th byte of each pixel.
    let alpha = |x: i32, y: i32| data[y as usize * stride + x as usize * 4 + 3];
    // Bottom row: fully opaque across the full width (square corners).
    for x in 0..w {
        assert_eq!(
            alpha(x, h - 1),
            255,
            "bottom row must be solid at x={x} — the tab is not a rectangle"
        );
    }

    // Top row: opaque only between the rounded corners.
    for x in 0..w {
        let a = alpha(x, 0);
        if (x as f64) >= radius && x < w - radius as i32 {
            assert_eq!(a, 255, "top edge missing at x={x}");
        } else {
            assert!(a < 250, "top corner must be rounded at x={x}");
        }
    }

    // Nothing outside the shape.
    assert_eq!(alpha(0, 0), 0);
}

/// Regression: the ACTIVE project's card must carry its live workspace
/// (rendered as a thumbnail), not a placeholder fill. The workspaces of the
/// active project live on the monitor, not in the Warm store.
#[test]
fn active_project_card_carries_live_workspace() {
    use crate::layout::{ProjectOverviewItem};

    let mut layout = setup_layout_with_projects();
    layout.switch_to_project("project_a");

    let win = TestWindow::new(TestWindowParams::new(7));
    layout.add_window(
        win,
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    layout.toggle_project_overview();
    let entries = layout.project_overview_entries();

    let active = entries
        .iter()
        .find(|e| e.name == "project_a")
        .expect("project_a entry");
    assert_eq!(active.state_label, "active");
    assert!(
        matches!(active.item, ProjectOverviewItem::Warm(_)),
        "active project card must reference its live workspace"
    );

    // project_b is dormant here: placeholder card.
    let dormant = entries
        .iter()
        .find(|e| e.name == "project_b")
        .expect("project_b entry");
    assert!(
        matches!(dormant.item, ProjectOverviewItem::Placeholder),
        "dormant project card must be a placeholder"
    );

    // After switching away, project_a becomes warm and still carries a real
    // workspace (parked).
    layout.switch_to_project("project_b");
    let entries = layout.project_overview_entries();
    let warm = entries
        .iter()
        .find(|e| e.name == "project_a")
        .expect("project_a entry");
    assert_eq!(warm.state_label, "warm");
    assert!(
        matches!(warm.item, ProjectOverviewItem::Warm(_)),
        "warm parked project card must carry its workspace thumbnail source"
    );
}
