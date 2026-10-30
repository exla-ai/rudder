use super::*;

#[test]
fn browser_result_waits_for_confirmation_without_changing_the_draft() {
    for target in [board_access::Target::Project, board_access::Target::Usage] {
        let mut app = App::new();
        app.task_input = "continue this task".into();
        app.task_cursor = 8;
        app.delete_pending = Some("another-run".into());
        app.notice = Some("confirm deleting another-run".into());
        let (sender, receiver) = mpsc::sync_channel(1);
        app.board_open_rx = Some((target, receiver));
        sender.send(board_access::OpenResult::Opened).unwrap();

        app.poll_board_open();
        assert_eq!(app.notice.as_deref(), Some("confirm deleting another-run"));
        assert_eq!(app.delete_pending.as_deref(), Some("another-run"));
        assert_eq!(app.task_input, "continue this task");
        assert_eq!(app.task_cursor, 8);
        assert!(app.board_open_rx.is_some());

        app.delete_pending = None;
        app.poll_board_open();
        assert!(app.board_open_rx.is_none());
        assert!(app.notice.as_deref().unwrap().starts_with("opening "));
        assert_eq!(app.task_input, "continue this task");
        assert_eq!(app.task_cursor, 8);
    }
}

#[test]
fn browser_worker_disconnect_settles_without_exposing_an_endpoint() {
    let mut app = App::new();
    let (sender, receiver) = mpsc::sync_channel(1);
    app.board_open_rx = Some((board_access::Target::Project, receiver));
    drop(sender);

    app.poll_board_open();
    assert!(app.board_open_rx.is_none());
    assert_eq!(
        app.notice.as_deref(),
        Some("web UI is unavailable for this session")
    );
}

#[test]
fn browser_engine_result_waits_for_each_views_confirmation() {
    let mut app = App::new();
    app.engine_headless = true;
    app.quit_confirm_pending = true;
    app.notice = Some("confirm quitting".into());
    app.task_input = "keep this draft".into();
    let (sender, receiver) = mpsc::sync_channel(1);
    app.board_open_rx = Some((board_access::Target::Usage, receiver));
    sender.send(board_access::OpenResult::Opened).unwrap();

    app.poll_board_open();
    assert!(app.board_open_rx.is_none());
    app.project_engine_notice();
    assert_eq!(app.notice.as_deref(), Some("confirm quitting"));
    assert_eq!(app.machine_notice_cursor, 0);

    app.quit_confirm_pending = false;
    app.project_engine_notice();
    assert_eq!(
        app.notice.as_deref(),
        Some("opening the usage dashboard in your browser")
    );
    assert_eq!(app.machine_notice_cursor, 1);
    assert_eq!(app.task_input, "keep this draft");
}
