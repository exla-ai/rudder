//! An agent in a pane asking the terminal for its colors gets the real
//! terminal's answer (term_colors.rs), and hears about an appearance change.

use std::time::{Duration, Instant};

use rudder_native::pty_terminal::{TerminalCommand, TerminalPane, TerminalPaneOptions};
use rudder_native::term_colors::{set_terminal_colors, TerminalColors};

fn wait_for(pane: &mut TerminalPane, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if pane.live_screen_contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn an_agent_asking_for_the_background_gets_the_terminal_s_and_hears_a_switch() {
    set_terminal_colors(TerminalColors { fg: Some([0x11, 0x11, 0x11]), bg: [0xff, 0xff, 0xff] });
    // Ask for the background (BEL-terminated), turn on mode 2031, then print
    // what came back, raw, as hex so the pane shows it as plain text.
    let script = r#"stty raw -echo
printf 'ready\r\n'
printf '\033]11;?\007\033[?2031h'
dd bs=1 count=24 2>/dev/null | od -An -tx1 | tr -d ' \n'; printf '\r\nGOT1\r\n'
dd bs=1 count=9 2>/dev/null | od -An -tx1 | tr -d ' \n'; printf '\r\nGOT2\r\n'
sleep 5"#;
    let mut pane = TerminalPane::spawn_shell_or_command(
        Some(TerminalCommand::with_args("/bin/sh", ["-c", script])),
        TerminalPaneOptions::default(),
    )
    .unwrap();
    assert!(wait_for(&mut pane, "GOT1"), "{:?}", pane.live_screen_lines());
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let screen = pane.live_screen_lines().join("\n");
    assert!(screen.contains(&hex(b"\x1b]11;rgb:ffff/ffff/ffff\x07")), "{screen}");

    // The laptop switched to dark: the agent that asked for 2031 is told.
    set_terminal_colors(TerminalColors { fg: None, bg: [0x10, 0x10, 0x10] });
    pane.drain_output();
    assert!(wait_for(&mut pane, "GOT2"), "{:?}", pane.live_screen_lines());
    let screen = pane.live_screen_lines().join("\n");
    assert!(screen.contains(&hex(b"\x1b[?997;1n")), "{screen}");
}
