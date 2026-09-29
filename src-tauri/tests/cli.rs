//! `pluely --action <id>` argv contract. Valid ids start the GUI or hit a live instance, so only rejections are testable headless.
use std::process::Command;

#[test]
fn invalid_argv_exits_2_naming_the_offender() {
    let cases: [(&[&str], &str); 5] = [
        (&["bogus"], "bogus"),
        (&["--action", "bogus"], "bogus"),
        (&["--action", "move_window"], "move_window"),
        (&["--action", "toggle_window", "extra"], "extra"),
        (&["--action"], "--action"),
    ];
    for (args, offender) in cases {
        let out = Command::new(env!("CARGO_BIN_EXE_pluely"))
            .args(args)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains(offender), "{args:?}: {stderr}");
    }
}
