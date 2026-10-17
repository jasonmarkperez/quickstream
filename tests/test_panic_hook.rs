//! A panic must restore the terminal before its message is printed (issue #9).
//!
//! Own test binary: it replaces the process-wide panic hook.

mod common;

use std::sync::{Arc, Mutex};

use quickstream::ui;

#[test]
fn test_panic_hook_restores_terminal_before_printing_message() {
    common::isolate();
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (restore_log, prev_log) = (events.clone(), events.clone());

    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(ui::terminal_panic_hook(
        move || restore_log.lock().unwrap().push("restore".to_string()),
        move |info: &std::panic::PanicHookInfo<'_>| {
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .unwrap_or_default();
            prev_log.lock().unwrap().push(format!("prev: {}", msg));
        },
    )));

    let result = std::panic::catch_unwind(|| panic!("boom"));
    std::panic::set_hook(original);

    assert!(result.is_err());
    assert_eq!(*events.lock().unwrap(), vec!["restore".to_string(), "prev: boom".to_string()]);
}

#[test]
fn test_restore_terminal_is_noop_when_tui_never_started() {
    common::isolate();
    assert!(!ui::restore_terminal());
    assert!(!ui::restore_terminal());
}
