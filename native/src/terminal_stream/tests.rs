use super::*;

fn cursor(sequence: u64) -> Cursor {
    Cursor {
        generation: 7,
        sequence,
    }
}
fn snapshot(sequence: u64, screen: &[u8]) -> Snapshot {
    Snapshot {
        cursor: cursor(sequence),
        reset: true,
        gap: true,
        rows: 3,
        cols: 20,
        screen: screen.to_vec(),
        history: vec!["old history".into()],
    }
}
fn view() -> View {
    View::new(1024, 20, 1000)
}

#[test]
fn reconnect_fences_callbacks_and_replaces_partial_decoder() {
    let mut view = view();
    let first = view.reconnect();
    assert_eq!(
        view.snapshot(first, snapshot(0, b"hello")),
        Receive::Applied { paint_ready: true }
    );
    assert_eq!(
        view.delta(first, cursor(1), &[0xf0, 0x9f]),
        Receive::Applied { paint_ready: false }
    );
    let second = view.reconnect();
    assert_eq!(view.delta(first, cursor(2), &[0x92, 0xa9]), Receive::Stale);
    assert_eq!(view.snapshot(first, snapshot(2, b"old")), Receive::Stale);
    assert_eq!(view.exit(first, &cursor(1), 0), Receive::Stale);
    assert_eq!(
        view.delta(second, cursor(2), b"bad suffix"),
        Receive::SnapshotRequired
    );
    assert_eq!(
        view.snapshot(second, snapshot(2, b"new")),
        Receive::Applied { paint_ready: true }
    );
    assert_eq!(view.screen().contents(), "new");
}

#[test]
fn gap_requires_complete_screen_and_preserves_history_size_cursor() {
    let mut view = view();
    let generation = view.reconnect();
    view.snapshot(generation, snapshot(0, b"old"));
    assert_eq!(
        view.delta(generation, cursor(2), b"suffix"),
        Receive::SnapshotRequired
    );
    assert_eq!(
        view.delta(generation, cursor(3), b"still suffix"),
        Receive::SnapshotRequired
    );
    let mut complete = snapshot(3, b"\x1b[2;4Hready");
    complete.cols = 30;
    assert_eq!(
        view.snapshot(generation, complete),
        Receive::Applied { paint_ready: true }
    );
    assert_eq!(view.screen().size(), (3, 30));
    assert_eq!(view.screen().cursor_position(), (1, 8));
    assert_eq!(view.history(), &["old history"]);
    assert_eq!(view.resume_cursor(), Some(&cursor(3)));
    assert_eq!(
        view.snapshot(generation, snapshot(2, b"stale")),
        Receive::Stale
    );
    assert!(view.screen().contents().contains("ready"));
}

#[test]
fn sequenced_full_screen_frames_and_explicit_coalescing_gap() {
    let mut view = view();
    let generation = view.reconnect();
    view.snapshot(generation, snapshot(0, b"zero"));
    let mut next = snapshot(1, b"one");
    next.reset = false;
    next.gap = false;
    assert_eq!(
        view.snapshot(generation, next),
        Receive::Applied { paint_ready: true }
    );
    let mut missing = snapshot(4, b"four");
    missing.reset = false;
    missing.gap = false;
    assert_eq!(
        view.snapshot(generation, missing),
        Receive::SnapshotRequired
    );
    assert_eq!(
        view.snapshot(generation, snapshot(4, b"four")),
        Receive::Applied { paint_ready: true }
    );
    assert_eq!(view.screen().contents(), "four");
}

#[test]
fn split_utf8_escape_and_synchronized_output_do_not_complete_early() {
    let mut view = view();
    let generation = view.reconnect();
    view.snapshot(generation, snapshot(0, b""));
    let chunks: &[(&[u8], bool)] = &[
        (b"\xf0\x9f", false),
        (b"\x92\xa9", true),
        (b"\x1b[", false),
        (b"31mR", true),
        (b"\x1b]0;unfinished", false),
        (b"\x1b", false),
        (b"\\", true),
        (b"\x1b[?2026hworking", false),
        (b"\x1b[?2026", false),
        (b"l", true),
    ];
    for (index, (bytes, paint_ready)) in chunks.iter().enumerate() {
        assert_eq!(
            view.delta(generation, cursor(index as u64 + 1), bytes),
            Receive::Applied {
                paint_ready: *paint_ready
            }
        );
    }
    assert!(view.screen().contents().contains('💩'));
}

#[test]
fn invalid_snapshot_is_bounded_and_exit_cannot_hide_missing_output() {
    let mut view = view();
    let generation = view.reconnect();
    let mut huge = snapshot(0, b"bad");
    huge.rows = u16::MAX;
    huge.cols = u16::MAX;
    assert_eq!(view.snapshot(generation, huge), Receive::SnapshotRequired);
    assert_eq!(
        view.snapshot(generation, snapshot(0, b"\x1b[")),
        Receive::SnapshotRequired
    );
    view.snapshot(generation, snapshot(0, b"last output"));
    assert_eq!(
        view.exit(generation, &cursor(1), 0),
        Receive::SnapshotRequired
    );
    view.snapshot(generation, snapshot(1, b"final output"));
    assert_eq!(view.exit(generation, &cursor(1), 5), Receive::Exited(5));
    assert_eq!(
        view.delta(generation, cursor(2), b"late"),
        Receive::SnapshotRequired
    );
    assert_eq!(view.screen().contents(), "final output");
}

#[test]
fn slow_view_has_bounded_queue_and_does_not_block_fast_view() {
    let now = Instant::now();
    let mut slow = OutputQueue::new(8, Duration::from_millis(4));
    let mut fast = OutputQueue::new(8, Duration::from_millis(4));
    assert!(slow.push(b"12345", now));
    assert!(!slow.due(now + Duration::from_millis(3)));
    assert!(slow.due(now + Duration::from_millis(4)));
    assert!(!slow.push(b"6789", now));
    assert_eq!(slow.queued_bytes(), 0);
    assert!(slow.needs_snapshot());
    assert!(!slow.push(b"suffix", now));
    assert_eq!(slow.take(), None);
    assert!(fast.push(b"\xf0\x9f", now));
    assert!(fast.push(b"\x92\xa9", now));
    assert_eq!(fast.take(), Some("💩".as_bytes().to_vec()));
    slow.reset_after_snapshot();
    assert!(slow.push(b"fresh", now));
    assert_eq!(slow.take(), Some(b"fresh".to_vec()));
}

#[test]
fn controller_lease_revocation_prevents_old_interrupt_and_paste_replay() {
    let owner = Owner {
        owner_epoch: 1,
        run_epoch: 3,
    };
    let next_owner = Owner {
        owner_epoch: 2,
        run_epoch: 3,
    };
    let mut input = InputAuthority::default();
    input.replace(1, owner.clone());
    assert!(input.grant(1, &owner, "controller".into()));
    assert_eq!(input.input(1, &owner, "spectator", b"\x03"), None);
    let paste = b"\x1b[200~a\xf0\x9f\x92\xa9\x1b[201~";
    assert_eq!(
        input.input(1, &owner, "controller", paste),
        Some(paste.as_slice())
    );
    input.replace(2, next_owner.clone());
    assert_eq!(input.input(1, &owner, "controller", b"\x03"), None);
    assert_eq!(input.input(2, &next_owner, "controller", b"\x03"), None);
    assert!(!input.grant(2, &owner, "controller".into()));
    assert!(input.grant(2, &next_owner, "controller".into()));
    assert_eq!(
        input.input(2, &next_owner, "controller", b"new"),
        Some(b"new".as_slice())
    );
    assert!(!input.replace(1, owner.clone()));
    assert!(!input.replace(2, owner));
    assert_eq!(
        input.input(2, &next_owner, "controller", b"new"),
        Some(b"new".as_slice())
    );
    input.revoke();
    assert_eq!(input.input(2, &next_owner, "controller", b"\x03"), None);
}

#[test]
fn traces_separate_prediction_and_authoritative_echo_on_local_clock() {
    let start = Instant::now();
    let prediction = trace(
        start,
        start + Duration::from_millis(1),
        "k1".into(),
        "local-paint",
        false,
    );
    let echo = trace(
        start,
        start + Duration::from_millis(9),
        "k1".into(),
        "authoritative-echo",
        true,
    );
    assert_eq!(prediction.correlation_id, echo.correlation_id);
    assert!(!prediction.authoritative && echo.authoritative);
    assert!(prediction.elapsed < echo.elapsed);
}

#[test]
fn exporter_captures_real_parser_state_history_modes_and_cursor() {
    let mut parser = vt100::Parser::new(3, 20, 10);
    let mut exporter = ScreenExporter::new(7, 4096, 10, 1000);
    let output = b"one\r\ntwo\r\nthree\r\nfour\r\nfive\x1b[?2004h\x1b[31m";
    parser.process(output);
    exporter.observe(output);
    parser.screen_mut().set_scrollback(1);
    let snapshot = exporter.capture(parser.screen_mut(), true, true).unwrap();
    assert_eq!(parser.screen().scrollback(), 1);
    assert_eq!(snapshot.history, vec!["one", "two"]);
    let wire = serde_json::to_value(snapshot.wire_screen().unwrap()).unwrap();
    assert_eq!(wire["format"], "vt100-ansi-v1");
    assert_eq!(wire["columns"], 20);
    assert_eq!(wire["scrollback"][0], "one");
    let mut view = View::new(4096, 10, 1000);
    let connection = view.reconnect();
    assert_eq!(
        view.snapshot(connection, snapshot),
        Receive::Applied { paint_ready: true }
    );
    parser.screen_mut().set_scrollback(0);
    assert_eq!(view.screen().contents(), parser.screen().contents());
    assert_eq!(
        view.screen().cursor_position(),
        parser.screen().cursor_position()
    );
    assert_eq!(
        view.screen().bracketed_paste(),
        parser.screen().bracketed_paste()
    );
}

#[test]
fn exporter_waits_for_split_frames_but_keeps_parser_across_reconnects() {
    let mut parser = vt100::Parser::new(3, 20, 10);
    let mut exporter = ScreenExporter::new(7, 4096, 10, 1000);
    for chunk in [
        b"\xf0\x9f".as_slice(),
        b"\x92\xa9\x1b[?2026hworking".as_slice(),
    ] {
        parser.process(chunk);
        exporter.observe(chunk);
        assert_eq!(
            exporter
                .capture(parser.screen_mut(), true, true)
                .unwrap_err(),
            ExportError::IncompleteFrame
        );
    }
    let chunk = b"\x1b[?2026l";
    parser.process(chunk);
    exporter.observe(chunk);
    let first = exporter.capture(parser.screen_mut(), true, true).unwrap();
    let second = exporter.capture(parser.screen_mut(), true, true).unwrap();
    assert_eq!(second.cursor.sequence, first.cursor.sequence);
    let mut view = View::new(4096, 10, 1000);
    let old_connection = view.reconnect();
    view.snapshot(old_connection, first);
    let current = view.reconnect();
    assert_eq!(
        view.snapshot(current, second),
        Receive::Applied { paint_ready: true }
    );
    assert!(view.screen().contents().contains('💩'));
    assert!(view.screen().contents().contains("working"));
}

#[test]
fn final_snapshot_precedes_exit_even_with_truncated_source_escape() {
    let mut parser = vt100::Parser::new(3, 20, 10);
    let mut exporter = ScreenExporter::new(7, 4096, 10, 1000);
    let output = b"final output\x1b[?2026h\xf0\x9f";
    parser.process(output);
    exporter.observe(output);
    assert!(!exporter.complete());
    let snapshot = exporter.capture_final(parser.screen_mut()).unwrap();
    let cursor = snapshot.cursor.clone();
    assert!(snapshot.reset && snapshot.gap);
    let mut view = View::new(4096, 10, 1000);
    let connection = view.reconnect();
    assert_eq!(
        view.snapshot(connection, snapshot),
        Receive::Applied { paint_ready: true }
    );
    assert_eq!(view.screen().contents(), "final output");
    assert_eq!(view.exit(connection, &cursor, 0), Receive::Exited(0));
}

#[test]
fn synchronized_mode_in_private_parameter_lists_holds_until_matching_reset() {
    let mut boundary = Boundary::default();
    for chunk in [b"\x1b[?25;20".as_slice(), b"26;2004hpaint".as_slice()] {
        boundary.process(chunk);
        assert!(!boundary.complete());
    }
    boundary.process(b"\x1b[?25l");
    assert!(!boundary.complete());
    boundary.process(b"\x1b[?2004;2026;25l");
    assert!(boundary.complete());
    boundary.process(b"\x1b[?0002026;25h");
    assert!(!boundary.complete());
    boundary.process(b"\x1b[?25;0002026l");
    assert!(boundary.complete());
}

#[test]
fn private_mode_scanner_is_bounded_without_truncating_parameter_lists() {
    let mut boundary = Boundary::default();
    boundary.process(b"\x1b[?");
    for _ in 0..1000 {
        boundary.process(b"25;");
    }
    boundary.process(b"999999999999999999999999;2026h");
    assert!(!boundary.complete());
    boundary.process(b"\x1b[?25;2026l");
    assert!(boundary.complete());
    // Non-private and numeric-overflow parameters cannot alias private 2026.
    boundary.process(b"\x1b[25;2026h\x1b[?67562h");
    assert!(boundary.complete());
    // A private-mode list with an intermediate is a different CSI function.
    boundary.process(b"\x1b[?2026 h");
    assert!(boundary.complete());
}

#[test]
fn snapshot_screen_and_history_share_one_total_byte_budget() {
    let mut view = View::new(16, 20, 1000);
    let generation = view.reconnect();
    let mut oversized = snapshot(0, b"12345678");
    oversized.history = vec!["123456789".into()];
    assert_eq!(
        view.snapshot(generation, oversized),
        Receive::SnapshotRequired
    );
    let mut exact = snapshot(0, b"12345678");
    exact.history = vec!["12345678".into()];
    assert_eq!(
        view.snapshot(generation, exact),
        Receive::Applied { paint_ready: true }
    );
    assert_eq!(view.screen().contents(), "12345678");
}
