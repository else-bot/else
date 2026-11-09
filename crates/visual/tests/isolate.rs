use else_visual::{Error, Theme, isolate};

fn worker() {
    isolate::use_worker(env!("CARGO_BIN_EXE_visual-worker").into());
}

#[test]
fn a_worker_returns_the_same_render_as_in_process() {
    worker();
    let html = r#"<div style="height: 30px; background: var(--chart-1)"></div>"#;
    let theme = Theme::standard(true);
    let isolated = isolate::render(html, 300, 2.0, &theme).unwrap();
    let local = else_visual::render(html, 300, 2.0, &theme).unwrap();
    assert_eq!(
        (isolated.width, isolated.height),
        (local.width, local.height)
    );
    assert_eq!(isolated.rgba, local.rgba);
    assert_eq!(isolated.content_height, 30);
}

#[test]
fn deep_nesting_fails_as_an_error_instead_of_crashing_the_host() {
    worker();
    let html = format!("{}x{}", "<div>".repeat(511), "</div>".repeat(511));
    let error = isolate::render(&html, 680, 1.0, &Theme::standard(true)).unwrap_err();
    assert!(matches!(error, Error::Html(_)), "{error:?}");
}

#[test]
fn nested_layers_past_the_memory_limit_are_stopped() {
    worker();
    let layer = r#"<div style="height: 4000px; opacity: 0.99">"#;
    let html = format!("{}x{}", layer.repeat(100), "</div>".repeat(100));
    let started = std::time::Instant::now();
    let error = isolate::render(&html, 680, 2.0, &Theme::standard(true)).unwrap_err();
    assert!(matches!(error, Error::Limit(_)), "{error:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(11));
}

#[test]
fn render_errors_cross_the_process_boundary() {
    worker();
    assert_eq!(
        isolate::render("", 300, 1.0, &Theme::standard(true)).unwrap_err(),
        Error::Empty
    );
}
