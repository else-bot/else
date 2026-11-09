//! Alone in its binary: it fills the process-wide render queue.

use else_visual::{Error, Theme, isolate};

fn worker() {
    isolate::use_worker(env!("CARGO_BIN_EXE_visual-worker").into());
}

#[test]
fn a_full_queue_refuses_at_once_instead_of_blocking() {
    worker();
    let theme = Theme::standard(true);
    let layer = r#"<div style="height: 4000px; opacity: 0.99">"#;
    let heavy = format!("{}x{}", layer.repeat(100), "</div>".repeat(100));
    let mut pending: Vec<_> = (0..2)
        .map(|_| isolate::submit(&heavy, 680, 2.0, &theme))
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(300));
    pending.extend((0..16).map(|_| isolate::submit("<p>x</p>", 300, 1.0, &theme)));
    let started = std::time::Instant::now();
    let refused = futures_executor::block_on(isolate::submit("<p>x</p>", 300, 1.0, &theme));
    assert!(matches!(refused, Err(Error::Limit(_))), "{refused:?}");
    assert!(started.elapsed() < std::time::Duration::from_millis(100));
    drop(pending);
}
