//! Render a visual to PNG: `cargo run -p else-visual --example render -- in.html out.png [width] [light|email]`.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output, rest @ ..] = args.as_slice() else {
        eprintln!("usage: render IN.html OUT.png [WIDTH] [light|email]");
        std::process::exit(2);
    };
    let width = rest
        .first()
        .and_then(|w| w.parse().ok())
        .unwrap_or(else_visual::REFERENCE_WIDTH);
    let dark = !rest.iter().any(|a| a == "light");
    let html = std::fs::read_to_string(input).expect("readable input");
    let start = std::time::Instant::now();
    let theme = else_visual::Theme::standard(dark);
    let rendered = if rest.iter().any(|a| a == "email") {
        let source = else_visual::email::Source {
            html,
            inline: Vec::new(),
        };
        else_visual::email::render(&source, width, 2.0)
    } else {
        else_visual::render(&html, width, 2.0, &theme)
    }
    .unwrap_or_else(|e| panic!("{e}"))
    .over(theme.color(else_visual::Color::Background));
    eprintln!(
        "{}x{} device px, content {} css px, clipped {}, blocked {:?}, links {:?}, {:?}",
        rendered.width,
        rendered.height,
        rendered.content_height,
        rendered.clipped,
        rendered.blocked,
        rendered.links,
        start.elapsed()
    );
    std::fs::write(output, rendered.png().expect("png")).expect("writable output");
}
