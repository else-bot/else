//! Alone in its binary: it fills the process-wide render queue.

use std::os::unix::fs::PermissionsExt as _;

use else_visual::{Error, Theme, isolate};

/// A worker that stays busy, so both worker threads hold their jobs while
/// the queue fills, whatever the renderer does with a given page.
fn busy_worker() {
    let path = std::env::temp_dir().join(format!("else-visual-busy-{}", std::process::id()));
    std::fs::write(&path, "#!/bin/sh\nexec sleep 3\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    isolate::use_worker(path);
}

#[test]
fn a_full_queue_refuses_at_once_instead_of_blocking() {
    busy_worker();
    let theme = Theme::standard(true);
    let mut pending: Vec<_> = (0..2)
        .map(|_| isolate::submit("<p>x</p>", 300, 1.0, &theme))
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(300));
    pending.extend((0..16).map(|_| isolate::submit("<p>x</p>", 300, 1.0, &theme)));
    let started = std::time::Instant::now();
    let refused = futures_executor::block_on(isolate::submit("<p>x</p>", 300, 1.0, &theme));
    assert!(matches!(refused, Err(Error::Limit(_))), "{refused:?}");
    assert!(started.elapsed() < std::time::Duration::from_millis(100));
    drop(pending);
}
