use super::{
    App, ContainerPickerState, Focus, SidebarItem, move_wrapping_cursor,
    should_force_remote_build_failure_frame, should_force_remote_full_frame,
    should_handle_key_event,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[test]
fn copy_shortcuts_are_recognized_without_confusing_quit() {
    let command_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER);
    let control_shift_c = KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    let control_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    assert!(super::is_copy_key(&command_c));
    assert!(super::is_copy_key(&control_shift_c));
    assert!(!super::is_copy_key(&control_c));
}

#[test]
fn container_picker_steps_are_distinct() {
    assert_ne!(
        ContainerPickerState::NewSessionWorkspace { cursor: 0 },
        ContainerPickerState::NewSessionTemplate {
            workspace_idx: 0,
            cursor: 0,
            templates: Vec::new(),
        }
    );
}

#[test]
fn picker_cursor_wraps_at_boundaries() {
    let mut cursor = 0;
    move_wrapping_cursor(&mut cursor, 3, -1);
    assert_eq!(cursor, 2);

    move_wrapping_cursor(&mut cursor, 3, 1);
    assert_eq!(cursor, 0);

    cursor = 99;
    move_wrapping_cursor(&mut cursor, 3, 1);
    assert_eq!(cursor, 0);

    move_wrapping_cursor(&mut cursor, 0, 1);
    assert_eq!(cursor, 0);
}

#[test]
fn sidebar_items_cover_session_and_creation_entries() {
    assert_eq!(SidebarItem::NewSession, SidebarItem::NewSession);
    assert_eq!(SidebarItem::Session(1), SidebarItem::Session(1));
    assert_ne!(SidebarItem::Session(1), SidebarItem::Session(2));
}

#[test]
fn key_release_events_are_not_handled_as_repeated_input() {
    let key = |kind| KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, kind);

    assert!(should_handle_key_event(&key(KeyEventKind::Press)));
    assert!(should_handle_key_event(&key(KeyEventKind::Repeat)));
    assert!(!should_handle_key_event(&key(KeyEventKind::Release)));
}

#[test]
fn entering_terminal_focus_forces_a_complete_remote_frame() {
    assert!(should_force_remote_full_frame(
        Some(&Focus::Sidebar),
        &Focus::Terminal,
        false,
    ));
    assert!(!should_force_remote_full_frame(
        Some(&Focus::Terminal),
        &Focus::Terminal,
        false,
    ));
    assert!(should_force_remote_full_frame(
        Some(&Focus::Sidebar),
        &Focus::Sidebar,
        true,
    ));
}

#[test]
fn a_new_remote_build_failure_forces_a_complete_frame() {
    assert!(should_force_remote_build_failure_frame(false, true));
    assert!(!should_force_remote_build_failure_frame(true, true));
    assert!(!should_force_remote_build_failure_frame(true, false));
}

#[test]
fn rules_watch_detects_template_and_policy_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness-rules.toml");
    std::fs::write(
        &path,
        "version = 1\n[network]\nallowlist = [\"domain=example.com\"]\n",
    )
    .expect("write rules");
    let original = App::watched_file_stamp(&path);

    std::fs::write(
        &path,
        "version = 1\ntemplate = \"rust\"\n[network]\nallowlist = [\"domain=example.com\"]\n",
    )
    .expect("write template");
    assert_ne!(original, App::watched_file_stamp(&path));

    std::fs::write(
        &path,
        "version = 1\ntemplate = \"rust\"\n[network]\nallowlist = [\"domain=changed.example.com\"]\n",
    )
    .expect("write policy change");
    assert_ne!(original, App::watched_file_stamp(&path));

    std::fs::write(
        &path,
        "# changed comment\nversion = 1\ntemplate = \"rust\"\n[network]\nallowlist = [\"domain=example.com\"]\n",
    )
    .expect("write non-template change");
    assert_ne!(original, App::watched_file_stamp(&path));
}
