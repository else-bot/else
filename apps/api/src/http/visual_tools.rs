//! `visual_preview`: a bot sees its HTML visual as the owner's chat will.

use serde_json::{Value, json};

use super::AppState;
use super::mcp::{Outcome, tool_error, tool_image};
use crate::grants::Grant;

pub const NAME: &str = "visual_preview";

pub(super) fn schema() -> Value {
    json!({
        "name": NAME,
        "description": "Without path: the rules for writing an HTML visual (theme variables, \
            limits); read them first. With path: render that .html file as the owner's chat \
            shows it and see the image, with content_height and any remote URLs that never \
            load. Preview each visual before linking it with ![Title](path.html).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1, "maxLength": 1024,
                          "description": "The .html file, project-relative or under ~/work/." },
                "width": { "type": "integer", "minimum": else_visual::MIN_WIDTH,
                           "maximum": else_visual::MAX_WIDTH,
                           "description": "CSS pixels. Default 680, the chat width. \
                               Try 400 to check a narrow window." },
                "theme": { "type": "string", "enum": ["dark", "light"],
                           "description": "Default dark." },
            },
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) async fn call(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Some(path) = args["path"].as_str().map(str::to_owned) else {
        return (super::mcp::tool_result(json!({ "rules": rules() })), None);
    };
    let width = args["width"]
        .as_u64()
        .map_or(else_visual::REFERENCE_WIDTH, |w| w as u32);
    let dark = args["theme"].as_str() != Some("light");
    let Some(project) = state
        .scheduler
        .registry()
        .all()
        .iter()
        .find_map(|provider| provider.project_dir(grant.bot_id))
    else {
        return (tool_error("Your project directory is unavailable."), None);
    };
    let rendered = tokio::task::spawn_blocking(move || {
        let html = crate::providers::output_files::read_visual(&project, &path)?;
        let theme = else_visual::Theme::standard(dark);
        else_visual::isolate::render(&html, width, 1.0, &theme)
            .map(|rendered| rendered.over(theme.color(else_visual::Color::Background)))
            .and_then(|rendered| Ok((rendered.png()?, rendered)))
            .map_err(|err| err.to_string())
    })
    .await;
    let (png, rendered) = match rendered {
        Ok(Ok(done)) => done,
        Ok(Err(message)) => return (tool_error(&message), None),
        Err(_) => return (tool_error("Rendering failed."), None),
    };
    let next = if !rendered.blocked.is_empty() {
        "Remote images and fonts never load. Inline images as data: URIs or draw them in SVG."
            .to_owned()
    } else if rendered.clipped {
        format!(
            "The chat cuts the visual at {}px. Shorten it or split it into several visuals.",
            else_visual::MAX_HEIGHT
        )
    } else {
        "If it looks right, link it in your reply with ![Title](path).".to_owned()
    };
    let meta = json!({
        "width": width,
        "content_height": rendered.content_height,
        "clipped": rendered.clipped,
        "blocked": rendered.blocked,
        "next": next,
    });
    (tool_image(meta, &png), None)
}

fn rules() -> String {
    let colors: Vec<String> = else_visual::Color::ALL
        .iter()
        .map(|color| format!("--{}", color.name()))
        .collect();
    format!(
        "Use a visual when numbers, trends, or comparisons read better as a chart, KPI tiles, \
         or a table. Write one self-contained HTML+CSS file in your project (a fragment with a \
         <style> block is enough) and show it with ![Title](viz/name.html). The chat draws it \
         inline in the owner's current theme at their window width: design for {width}px and \
         keep it fluid down to 360px with flex-wrap, minmax(), and percentages. It renders \
         without a browser: no JavaScript or interaction, and nothing remote loads, so draw \
         charts in inline SVG or with sized divs, and embed any image as a data: URI. \
         Flexbox, grid, tables, gradients, shadows, and SVG work. Color only with the theme \
         variables, never fixed colors, so it follows light and dark: {colors}. Also \
         --radius, --font-sans (Inter), and --font-mono. Leave the page background \
         transparent, add no outer padding or page title (the reply text carries the title), \
         give charts fixed pixel heights, and stay under {max_height}px tall. Say in text \
         what it shows.",
        width = else_visual::REFERENCE_WIDTH,
        colors = colors.join(", "),
        max_height = else_visual::MAX_HEIGHT,
    )
}
