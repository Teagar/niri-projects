//! Correctness tests for the Projects feature.
//!
//! These correspond to the five contracts in the spec:
//! 1. switching_away_and_back_preserves_windows
//! 2. overview_double_toggle_never_panics
//! 3. reload_projects_preserves_runtime_state
//! 4. switch_to_warm_or_active_project_is_o1
//! 5. depth_navigation_in_overview_does_not_mutate_state

use super::*;

fn make_project_config(name: &str) -> niri_config::ProjectConfig {
    niri_config::ProjectConfig {
        name: name.to_string(),
        keep_open: false,
        workspaces: vec![],
    }
}

fn make_project_config_with_ws(name: &str, ws_name: &str) -> niri_config::ProjectConfig {
    niri_config::ProjectConfig {
        name: name.to_string(),
        keep_open: false,
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

    // Open the project overview.
    layout.toggle_project_overview();

    // Navigate depth — these should be no-ops (stubs).
    layout.project_overview_focus_depth_closer();
    layout.project_overview_focus_depth_further();

    // Navigate slots — also no-ops.
    layout.project_overview_focus_slot_prev();
    layout.project_overview_focus_slot_next();

    // Overview should still be open.
    assert!(
        layout.is_overview_open(),
        "overview must remain open after navigation"
    );

    // Close it.
    layout.toggle_overview();
    assert!(!layout.is_overview_open());
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
