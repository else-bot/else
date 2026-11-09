use else_visual::{Color, Error, MAX_BYTES, MAX_HEIGHT, Theme, render};

fn pixel(rendered: &else_visual::Rendered, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * rendered.width + x) * 4) as usize;
    rendered.rgba[at..at + 4].try_into().unwrap()
}

#[test]
fn the_height_follows_the_content_at_the_given_width_and_scale() {
    let html = r#"<div style="height: 120px; background: red"></div>"#;
    let rendered = render(html, 400, 2.0, &Theme::standard(true)).unwrap();
    assert_eq!((rendered.width, rendered.height), (800, 240));
    assert_eq!(rendered.content_height, 120);
    assert!(!rendered.clipped);
}

#[test]
fn theme_variables_follow_the_host_theme() {
    let html = r#"<div style="height: 10px; background: var(--chart-1)"></div>"#;
    let theme = Theme::new(true, 8.0, |c| {
        if c == Color::Chart1 {
            [10, 200, 30, 255]
        } else {
            [0, 0, 0, 255]
        }
    });
    let rendered = render(html, 300, 1.0, &theme).unwrap();
    assert_eq!(pixel(&rendered, 5, 5), [10, 200, 30, 255]);
}

#[test]
fn the_page_is_transparent_so_the_bubble_shows_through() {
    let html = r#"<p style="margin: 0; height: 20px"></p><div style="height: 10px; background: blue"></div>"#;
    let rendered = render(html, 300, 1.0, &Theme::standard(false)).unwrap();
    assert_eq!(pixel(&rendered, 5, 5)[3], 0);
    assert_eq!(pixel(&rendered, 5, 25), [0, 0, 255, 255]);
}

#[test]
fn remote_resources_are_reported_and_never_loaded() {
    let html =
        r#"<div><img src="https://example.com/track.png" width="20" height="20"><p>text</p></div>"#;
    let rendered = render(html, 300, 1.0, &Theme::standard(true)).unwrap();
    assert_eq!(
        rendered.blocked,
        vec!["https://example.com/track.png".to_owned()]
    );
}

#[test]
fn tall_content_is_cut_at_the_limit() {
    let html = r#"<div style="height: 9000px; background: red"></div>"#;
    let rendered = render(html, 300, 1.0, &Theme::standard(true)).unwrap();
    assert_eq!(rendered.height, MAX_HEIGHT);
    assert_eq!(rendered.content_height, 9000);
    assert!(rendered.clipped);
}

#[test]
fn oversized_and_empty_sources_are_rejected() {
    let big = "a".repeat(MAX_BYTES + 1);
    assert_eq!(
        render(&big, 300, 1.0, &Theme::standard(true)).unwrap_err(),
        Error::TooLarge(MAX_BYTES + 1)
    );
    assert_eq!(
        render("", 300, 1.0, &Theme::standard(true)).unwrap_err(),
        Error::Empty
    );
}

#[test]
fn a_fragment_inherits_the_theme_text_color() {
    let html = r#"<div><p style="margin: 0; font-size: 40px; line-height: 40px">█</p></div>"#;
    let theme = Theme::new(true, 8.0, |c| {
        if c == Color::Foreground {
            [250, 250, 250, 255]
        } else {
            [0, 0, 0, 255]
        }
    });
    let rendered = render(html, 300, 1.0, &theme).unwrap();
    let lit = (0..rendered.width).any(|x| pixel(&rendered, x, 20) == [250, 250, 250, 255]);
    assert!(lit, "text uses --foreground");
}
