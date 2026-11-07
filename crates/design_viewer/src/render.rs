use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Dump {
    #[serde(default)]
    pub pages: Vec<DumpPage>,
}

#[derive(Debug, Deserialize)]
pub struct DumpPage {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub background: Option<Color>,
    #[serde(default)]
    pub children: Vec<Node>,
    #[serde(default)]
    pub screens: Vec<DumpScreen>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpScreen {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "box")]
    pub bounds: Bounds,
    #[serde(default)]
    pub node_count: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, serde::Serialize)]
pub struct Bounds {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Color {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    #[serde(default = "one")]
    pub a: f64,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paint {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    color: Option<Color>,
    #[serde(default = "one")]
    opacity: f64,
    #[serde(default)]
    stops: Vec<GradientStop>,
    #[serde(default)]
    transform: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct GradientStop {
    color: Color,
    #[serde(default)]
    position: f64,
}

#[derive(Debug, Deserialize)]
pub struct Effect {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    color: Option<Color>,
    #[serde(default)]
    offset: Option<Offset>,
    #[serde(default)]
    radius: Option<f64>,
    #[serde(default)]
    spread: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct Offset {
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Radius {
    Uniform(f64),
    Corners(Vec<Option<f64>>),
}

#[derive(Debug, Deserialize)]
pub struct FontName {
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    style: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Measure {
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    value: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Size {
    width: f64,
    height: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "box", default)]
    bounds: Bounds,
    #[serde(default)]
    opacity: Option<f64>,
    #[serde(default)]
    blend_mode: Option<String>,
    #[serde(default)]
    fills: Vec<Paint>,
    #[serde(default)]
    strokes: Vec<Paint>,
    #[serde(default)]
    stroke_weight: Option<f64>,
    #[serde(default)]
    radius: Option<Radius>,
    #[serde(default)]
    clips: bool,
    #[serde(default)]
    effects: Vec<Effect>,
    #[serde(default)]
    characters: Option<String>,
    #[serde(default)]
    font_name: Option<FontName>,
    #[serde(default)]
    font_size: Option<f64>,
    #[serde(default)]
    line_height: Option<Measure>,
    #[serde(default)]
    letter_spacing: Option<Measure>,
    #[serde(default)]
    align: Option<String>,
    #[serde(default)]
    auto_resize: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    size: Option<Size>,
    #[serde(default)]
    children: Vec<Node>,
}

/// Lives in the cache directory, one level above the pages that load it.
pub const VIEWER_SCRIPT_FILE_NAME: &str = "viewer.js";

pub struct RenderedPage {
    pub html: String,
    pub fonts: BTreeSet<String>,
    /// Top-left of the rendered canvas in document coordinates, so screen boxes can be
    /// translated into the page's own coordinate space.
    pub origin: (f64, f64),
    pub width: f64,
    pub height: f64,
}

fn html_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn number(value: f64) -> String {
    let rounded = (value * 1000.0).round() / 1000.0;
    if rounded == 0.0 {
        "0".to_string()
    } else {
        rounded.to_string()
    }
}

fn channel(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn rgba(color: &Color, opacity: f64) -> String {
    format!(
        "rgba({},{},{},{:.3})",
        channel(color.r),
        channel(color.g),
        channel(color.b),
        color.a * opacity
    )
}

fn gradient_stops(paint: &Paint) -> String {
    paint
        .stops
        .iter()
        .map(|stop| {
            format!(
                "{} {:.1}%",
                rgba(&stop.color, paint.opacity),
                stop.position * 100.0
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The dump carries the gradient transform either as `{m00, m01, ...}` or as a
/// `[[m00, m01, m02], [m10, m11, m12]]` matrix depending on the CLI version.
fn transform_row(transform: Option<&serde_json::Value>) -> (f64, f64) {
    let Some(transform) = transform else {
        return (1.0, 0.0);
    };
    if let Some(object) = transform.as_object() {
        let read = |key: &str, default: f64| {
            object
                .get(key)
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(default)
        };
        return (read("m00", 1.0), read("m01", 0.0));
    }
    let first_row = transform
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(serde_json::Value::as_array);
    match first_row {
        Some(row) => (
            row.first()
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(1.0),
            row.get(1)
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
        ),
        None => (1.0, 0.0),
    }
}

fn css_paint(paint: &Paint) -> Option<String> {
    match paint.kind.as_str() {
        "SOLID" => paint.color.as_ref().map(|color| rgba(color, paint.opacity)),
        "GRADIENT_LINEAR" => {
            let (m00, m01) = transform_row(paint.transform.as_ref());
            let angle = m01.atan2(m00).to_degrees() + 90.0;
            Some(format!(
                "linear-gradient({:.1}deg, {})",
                angle,
                gradient_stops(paint)
            ))
        }
        "GRADIENT_RADIAL" | "GRADIENT_DIAMOND" => Some(format!(
            "radial-gradient(closest-side, {})",
            gradient_stops(paint)
        )),
        "GRADIENT_ANGULAR" => Some(format!("conic-gradient({})", gradient_stops(paint))),
        _ => None,
    }
}

fn first_solid(paints: &[Paint]) -> Option<String> {
    paints
        .iter()
        .find(|paint| paint.kind == "SOLID")
        .and_then(css_paint)
}

fn radius(node: &Node) -> String {
    match &node.radius {
        Some(Radius::Uniform(value)) => format!("{}px", number(*value)),
        Some(Radius::Corners(corners)) => corners
            .iter()
            .map(|corner| format!("{}px", number(corner.unwrap_or(0.0))))
            .collect::<Vec<_>>()
            .join(" "),
        None => "0px".to_string(),
    }
}

fn shadows(node: &Node) -> Vec<String> {
    node.effects
        .iter()
        .filter(|effect| effect.kind == "DROP_SHADOW" || effect.kind == "INNER_SHADOW")
        .filter_map(|effect| {
            let color = effect.color.as_ref()?;
            let inset = if effect.kind == "INNER_SHADOW" {
                "inset "
            } else {
                ""
            };
            let offset = effect.offset.unwrap_or_default();
            Some(format!(
                "{inset}{}px {}px {}px {}px {}",
                number(offset.x),
                number(offset.y),
                number(effect.radius.unwrap_or(0.0)),
                number(effect.spread.unwrap_or(0.0)),
                rgba(color, 1.0)
            ))
        })
        .collect()
}

/// Figma's blur radius is roughly twice the CSS standard deviation.
fn blur_radius(node: &Node, kinds: &[&str]) -> Option<f64> {
    node.effects
        .iter()
        .find(|effect| kinds.contains(&effect.kind.as_str()))
        .map(|effect| effect.radius.unwrap_or(0.0) / 2.0)
}

fn blend_mode(node: &Node) -> Option<&'static str> {
    match node.blend_mode.as_deref()? {
        "MULTIPLY" => Some("multiply"),
        "SCREEN" => Some("screen"),
        "OVERLAY" => Some("overlay"),
        "DARKEN" => Some("darken"),
        "LIGHTEN" => Some("lighten"),
        "COLOR_DODGE" | "LINEAR_DODGE" => Some("color-dodge"),
        "COLOR_BURN" | "LINEAR_BURN" => Some("color-burn"),
        "HARD_LIGHT" => Some("hard-light"),
        "SOFT_LIGHT" => Some("soft-light"),
        "DIFFERENCE" => Some("difference"),
        "EXCLUSION" => Some("exclusion"),
        "HUE" => Some("hue"),
        "SATURATION" => Some("saturation"),
        "COLOR" => Some("color"),
        "LUMINOSITY" => Some("luminosity"),
        _ => None,
    }
}

fn text_align(node: &Node) -> &'static str {
    match node.align.as_deref() {
        Some("CENTER") => "center",
        Some("RIGHT") => "right",
        Some("JUSTIFIED") => "justify",
        _ => "left",
    }
}

/// The family is interpolated into a quoted CSS value and the Google Fonts URL, so only
/// characters that can't end the string or the declaration are kept.
fn sanitize_font_family(family: &str) -> String {
    let sanitized: String = family
        .chars()
        .filter(|character| {
            character.is_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.')
        })
        .collect();
    let sanitized = sanitized.trim();
    if sanitized.is_empty() {
        "sans-serif".to_string()
    } else {
        sanitized.to_string()
    }
}

fn font_weight(style_name: &str) -> u16 {
    match style_name.replace("Italic", "").trim() {
        "Thin" => 100,
        "ExtraLight" | "Extra Light" => 200,
        "Light" => 300,
        "Medium" => 500,
        "SemiBold" | "Semi Bold" => 600,
        "Bold" => 700,
        "ExtraBold" | "Extra Bold" => 800,
        "Black" => 900,
        _ => 400,
    }
}

fn base_style(node: &Node, origin: (f64, f64), include_height: bool) -> String {
    let bounds = node.bounds;
    let mut style = format!(
        "position:absolute;left:{}px;top:{}px;width:{}px;",
        number(bounds.x - origin.0),
        number(bounds.y - origin.1),
        number(bounds.width),
    );
    if include_height {
        write!(style, "height:{}px;", number(bounds.height)).ok();
    }
    if let Some(opacity) = node.opacity.filter(|opacity| *opacity < 1.0) {
        write!(style, "opacity:{};", number(opacity)).ok();
    }
    if let Some(mode) = blend_mode(node) {
        write!(style, "mix-blend-mode:{mode};").ok();
    }
    if let Some(blur) = blur_radius(node, &["LAYER_BLUR", "FOREGROUND_BLUR"]) {
        write!(style, "filter:blur({}px);", number(blur)).ok();
    }
    style
}

fn render_text(node: &Node, origin: (f64, f64), fonts: &mut BTreeSet<String>, out: &mut String) {
    let auto_resize = node.auto_resize.as_deref();
    let grows_vertically = matches!(auto_resize, Some("HEIGHT") | Some("WIDTH_AND_HEIGHT"));
    let mut style = base_style(node, origin, !grows_vertically);
    style.push_str(if auto_resize == Some("WIDTH_AND_HEIGHT") {
        "white-space:pre;"
    } else {
        "white-space:pre-wrap;"
    });

    let family = node
        .font_name
        .as_ref()
        .and_then(|font| font.family.as_deref())
        .map(sanitize_font_family)
        .unwrap_or_else(|| "sans-serif".to_string());
    let style_name = node
        .font_name
        .as_ref()
        .and_then(|font| font.style.clone())
        .unwrap_or_else(|| "Regular".to_string());
    fonts.insert(family.clone());

    let color = first_solid(&node.fills)
        .or_else(|| node.fills.iter().find_map(css_paint))
        .unwrap_or_else(|| "#000".to_string());
    write!(
        style,
        "font-family:'{family}';font-weight:{};color:{color};",
        font_weight(&style_name)
    )
    .ok();
    if let Some(size) = node.font_size {
        write!(style, "font-size:{}px;", number(size)).ok();
    }
    if style_name.contains("Italic") {
        style.push_str("font-style:italic;");
    }
    if let Some(Measure {
        unit: Some(unit),
        value: Some(value),
    }) = &node.line_height
    {
        match unit.as_str() {
            "PIXELS" => write!(style, "line-height:{}px;", number(*value)).ok(),
            "PERCENT" => write!(style, "line-height:{}%;", number(*value)).ok(),
            _ => None,
        };
    }
    if let Some(Measure {
        unit: Some(unit),
        value: Some(value),
    }) = &node.letter_spacing
        && *value != 0.0
    {
        match unit.as_str() {
            "PIXELS" => write!(style, "letter-spacing:{}px;", number(*value)).ok(),
            "PERCENT" => write!(style, "letter-spacing:{}em;", number(value / 100.0)).ok(),
            _ => None,
        };
    }
    write!(style, "text-align:{};", text_align(node)).ok();

    write!(
        out,
        "<div style=\"{}\">{}</div>",
        html_escape(&style),
        html_escape(node.characters.as_deref().unwrap_or_default())
    )
    .ok();
}

fn render_vector(node: &Node, origin: (f64, f64), out: &mut String) {
    let size = node.size.unwrap_or(Size {
        width: node.bounds.width,
        height: node.bounds.height,
    });
    let style = base_style(node, origin, true) + "overflow:visible;";
    let fill = first_solid(&node.fills).unwrap_or_else(|| "none".to_string());
    let stroke = first_solid(&node.strokes).unwrap_or_else(|| "none".to_string());
    let paths: String = node
        .paths
        .iter()
        .map(|path| format!("<path d=\"{}\"/>", html_escape(path)))
        .collect();
    write!(
        out,
        "<svg style=\"{}\" viewBox=\"0 0 {} {}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{}\" \
         stroke-linecap=\"round\" stroke-linejoin=\"round\">{paths}</svg>",
        html_escape(&style),
        number(size.width),
        number(size.height),
        html_escape(&fill),
        html_escape(&stroke),
        number(node.stroke_weight.unwrap_or(0.0)),
    )
    .ok();
}

fn render_node(node: &Node, origin: (f64, f64), fonts: &mut BTreeSet<String>, out: &mut String) {
    if node.kind == "TEXT" {
        render_text(node, origin, fonts, out);
        return;
    }
    // Ellipses stay on the div path: it is the one that supports gradient fills and
    // keeps the layer blur of glow shapes.
    if !node.paths.is_empty() && node.children.is_empty() && node.kind != "ELLIPSE" {
        render_vector(node, origin, out);
        return;
    }

    let mut style = base_style(node, origin, true);
    let layers: Vec<String> = node
        .fills
        .iter()
        .rev()
        .filter_map(|paint| {
            let css = css_paint(paint)?;
            Some(if paint.kind == "SOLID" {
                format!("linear-gradient({css},{css})")
            } else {
                css
            })
        })
        .collect();
    if !layers.is_empty() {
        write!(style, "background:{};", layers.join(", ")).ok();
    }

    let mut box_shadows = Vec::new();
    if let Some(weight) = node.stroke_weight.filter(|weight| *weight > 0.0)
        && let Some(stroke) = first_solid(&node.strokes)
    {
        box_shadows.push(format!("inset 0 0 0 {}px {stroke}", number(weight)));
    }
    box_shadows.extend(shadows(node));
    if !box_shadows.is_empty() {
        write!(style, "box-shadow:{};", box_shadows.join(", ")).ok();
    }
    if let Some(blur) = blur_radius(node, &["BACKGROUND_BLUR"]) {
        write!(style, "backdrop-filter:blur({}px);", number(blur)).ok();
    }
    if node.kind == "ELLIPSE" {
        style.push_str("border-radius:50%;");
    } else {
        write!(style, "border-radius:{};", radius(node)).ok();
    }
    if node.clips {
        style.push_str("overflow:hidden;");
    }

    let mut children = String::new();
    for child in &node.children {
        render_node(child, (node.bounds.x, node.bounds.y), fonts, &mut children);
    }
    write!(
        out,
        "<div title=\"{}\" style=\"{}\">{children}</div>",
        html_escape(&node.name),
        html_escape(&style)
    )
    .ok();
}

fn canvas_bounds(children: &[Node]) -> Bounds {
    let extent = children.iter().fold(None, |extent, node| {
        let bounds = node.bounds;
        let (left, top, right, bottom) = (
            bounds.x,
            bounds.y,
            bounds.x + bounds.width,
            bounds.y + bounds.height,
        );
        Some(match extent {
            None => (left, top, right, bottom),
            Some((min_x, min_y, max_x, max_y)) => (
                f64::min(min_x, left),
                f64::min(min_y, top),
                f64::max(max_x, right),
                f64::max(max_y, bottom),
            ),
        })
    });
    match extent {
        Some((min_x, min_y, max_x, max_y)) => Bounds {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        },
        None => Bounds::default(),
    }
}

fn font_link(fonts: &BTreeSet<String>) -> String {
    let families: Vec<String> = fonts
        .iter()
        .filter(|family| !family.is_empty() && family.as_str() != "sans-serif")
        .map(|family| {
            format!(
                "family={}:ital,wght@0,400;0,500;0,600;0,700;1,400",
                url::form_urlencoded::byte_serialize(family.as_bytes()).collect::<String>()
            )
        })
        .collect();
    if families.is_empty() {
        return String::new();
    }
    format!(
        "<link rel=\"stylesheet\" href=\"{}\">",
        html_escape(&format!(
            "https://fonts.googleapis.com/css2?{}&display=block",
            families.join("&")
        ))
    )
}

pub fn render_page(page: &DumpPage) -> RenderedPage {
    let canvas = canvas_bounds(&page.children);
    let origin = (canvas.x, canvas.y);
    let mut fonts = BTreeSet::new();
    let mut body = String::new();
    for node in &page.children {
        render_node(node, origin, &mut fonts, &mut body);
    }
    let screens: Vec<serde_json::Value> = page
        .screens
        .iter()
        .map(|screen| {
            serde_json::json!({
                "id": screen.id,
                "name": screen.name,
                "x": screen.bounds.x - origin.0,
                "y": screen.bounds.y - origin.1,
                "width": screen.bounds.width,
                "height": screen.bounds.height,
            })
        })
        .collect();
    // `<` can only occur inside JSON strings, where `\u003c` is the same character, so no
    // screen name can close the script element early.
    let screens = serde_json::Value::Array(screens)
        .to_string()
        .replace('<', "\\u003c");
    let background = page
        .background
        .as_ref()
        .map(|color| rgba(color, 1.0))
        .unwrap_or_else(|| "transparent".to_string());
    let html = format!(
        "<!doctype html><html data-page-bg=\"{background}\" style=\"--design-page-bg:{background}\">\
         <head><meta charset=\"utf-8\"><title>{title}</title>{fonts}\
         <style>html,body{{margin:0}}body{{background:var(--design-viewer-bg, var(--design-page-bg))}}</style>\
         </head><body><div id=\"asylum-design-stage\" style=\"position:relative;width:{width}px;height:{height}px\">{body}</div>\
         <script id=\"asylum-design-screens\" type=\"application/json\">{screens}</script>\
         <script src=\"../{VIEWER_SCRIPT_FILE_NAME}\"></script></body></html>",
        title = html_escape(&page.name),
        fonts = font_link(&fonts),
        width = number(canvas.width),
        height = number(canvas.height),
    );
    RenderedPage {
        html,
        fonts,
        origin,
        width: canvas.width,
        height: canvas.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(children: serde_json::Value) -> DumpPage {
        serde_json::from_value(serde_json::json!({
            "id": "1:1",
            "name": "Page",
            "background": { "r": 0.1, "g": 0.2, "b": 0.3, "a": 1 },
            "children": children,
        }))
        .expect("fixture is valid")
    }

    fn text(auto_resize: &str, characters: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "TEXT",
            "name": "Label",
            "box": { "x": 10, "y": 20, "width": 120, "height": 40 },
            "fills": [{ "type": "SOLID", "color": { "r": 1, "g": 1, "b": 1, "a": 1 }, "opacity": 1 }],
            "characters": characters,
            "fontName": { "family": "DM Sans", "style": "Semi Bold Italic" },
            "fontSize": 14,
            "lineHeight": { "unit": "PIXELS", "value": 20 },
            "letterSpacing": { "unit": "PERCENT", "value": 2 },
            "align": "CENTER",
            "autoResize": auto_resize,
        })
    }

    #[test]
    fn fixed_width_text_wraps_with_its_width_and_line_height() {
        let rendered = render_page(&page(serde_json::json!([text("HEIGHT", "Hello")])));
        assert!(rendered.html.contains("white-space:pre-wrap;"));
        assert!(rendered.html.contains("width:120px;"));
        assert!(!rendered.html.contains("height:40px;"));
        assert!(rendered.html.contains("line-height:20px;"));
        assert!(rendered.html.contains("letter-spacing:0.02em;"));
        assert!(rendered.html.contains("font-weight:600;"));
        assert!(rendered.html.contains("font-style:italic;"));
        assert!(rendered.html.contains("text-align:center;"));
        assert!(rendered.fonts.contains("DM Sans"));
        assert!(rendered.html.contains("family=DM+Sans"));
    }

    #[test]
    fn auto_width_text_does_not_wrap_and_has_no_height() {
        let rendered = render_page(&page(serde_json::json!([text("WIDTH_AND_HEIGHT", "Hi")])));
        assert!(rendered.html.contains("white-space:pre;"));
        assert!(!rendered.html.contains("height:40px;"));
    }

    #[test]
    fn fixed_size_text_keeps_its_height() {
        let rendered = render_page(&page(serde_json::json!([text("NONE", "Hi")])));
        assert!(rendered.html.contains("height:40px;"));
        assert!(rendered.html.contains("white-space:pre-wrap;"));
    }

    #[test]
    fn linear_gradient_angle_and_stops() {
        let rendered = render_page(&page(serde_json::json!([{
            "type": "RECTANGLE",
            "name": "Card",
            "box": { "x": 0, "y": 0, "width": 100, "height": 50 },
            "fills": [{
                "type": "GRADIENT_LINEAR",
                "opacity": 0.5,
                "transform": [[0, 1, 0], [-1, 0, 1]],
                "stops": [
                    { "color": { "r": 1, "g": 0, "b": 0, "a": 1 }, "position": 0 },
                    { "color": { "r": 0, "g": 0, "b": 1, "a": 1 }, "position": 1 }
                ]
            }],
            "radius": [8, 8, 0, null],
        }])));
        assert!(
            rendered.html.contains(
                "linear-gradient(180.0deg, rgba(255,0,0,0.500) 0.0%, rgba(0,0,255,0.500) 100.0%)"
            ),
            "{}",
            rendered.html
        );
        assert!(rendered.html.contains("border-radius:8px 8px 0px 0px;"));
    }

    #[test]
    fn gradient_transform_as_object() {
        let rendered = render_page(&page(serde_json::json!([{
            "type": "RECTANGLE",
            "box": { "x": 0, "y": 0, "width": 10, "height": 10 },
            "fills": [{
                "type": "GRADIENT_LINEAR",
                "transform": { "m00": 1, "m01": 0, "m02": 0, "m10": 0, "m11": 1, "m12": 0 },
                "stops": [{ "color": { "r": 0, "g": 0, "b": 0 }, "position": 0.5 }]
            }],
        }])));
        assert!(
            rendered
                .html
                .contains("linear-gradient(90.0deg, rgba(0,0,0,1.000) 50.0%)")
        );
    }

    #[test]
    fn vector_paths_become_svg_with_view_box() {
        let rendered = render_page(&page(serde_json::json!([{
            "type": "VECTOR",
            "name": "Icon",
            "box": { "x": 5, "y": 5, "width": 20, "height": 20 },
            "fills": [{ "type": "SOLID", "color": { "r": 0, "g": 0, "b": 0, "a": 1 }, "opacity": 1 }],
            "strokeWeight": 1.5,
            "paths": ["M0 0L20 20Z"],
            "size": { "width": 24, "height": 24 },
        }])));
        assert!(rendered.html.contains("viewBox=\"0 0 24 24\""));
        assert!(rendered.html.contains("<path d=\"M0 0L20 20Z\"/>"));
        assert!(rendered.html.contains("stroke-width=\"1.5\""));
    }

    #[test]
    fn text_and_titles_are_escaped() {
        let mut children = vec![text("HEIGHT", "<b>&\"x'</b>")];
        children.push(serde_json::json!({
            "type": "FRAME",
            "name": "a\"><script>",
            "box": { "x": 0, "y": 0, "width": 10, "height": 10 },
        }));
        let rendered = render_page(&page(serde_json::Value::Array(children)));
        assert!(
            rendered
                .html
                .contains("&lt;b&gt;&amp;&quot;x&#39;&lt;/b&gt;")
        );
        assert!(
            rendered
                .html
                .contains("title=\"a&quot;&gt;&lt;script&gt;\"")
        );
        assert!(!rendered.html.contains("<script>"));
    }

    #[test]
    fn page_background_is_exposed_not_baked_in() {
        let rendered = render_page(&page(serde_json::json!([])));
        assert!(
            rendered
                .html
                .contains("data-page-bg=\"rgba(26,51,77,1.000)\"")
        );
        assert!(
            rendered
                .html
                .contains("--design-page-bg:rgba(26,51,77,1.000)")
        );
        assert!(
            rendered
                .html
                .contains("background:var(--design-viewer-bg, var(--design-page-bg))")
        );
        assert!(!rendered.html.contains("#1e1e1e"));
        assert_eq!(rendered.width, 0.0);
    }

    #[test]
    fn screens_are_embedded_for_the_viewer_script() {
        let page: DumpPage = serde_json::from_value(serde_json::json!({
            "id": "1:1",
            "name": "Page",
            "children": [{
                "type": "FRAME",
                "name": "Home",
                "box": { "x": 100, "y": 50, "width": 390, "height": 844 },
            }],
            "screens": [{
                "id": "2:1",
                "name": "</script><b>",
                "box": { "x": 100, "y": 50, "width": 390, "height": 844 },
                "nodeCount": 12,
            }],
        }))
        .expect("fixture is valid");
        assert_eq!(page.screens[0].node_count, 12);
        let rendered = render_page(&page);
        assert!(
            rendered
                .html
                .contains("<script src=\"../viewer.js\"></script>")
        );
        assert!(rendered.html.contains("id=\"asylum-design-stage\""));
        assert!(rendered.html.contains("\"x\":0.0,\"y\":0.0"));
        assert!(rendered.html.contains("\\u003c/script>\\u003cb>"));
        assert_eq!(rendered.html.matches("</script>").count(), 2);
    }

    #[test]
    fn blurred_ellipse_and_blended_overlay() {
        let rendered = render_page(&page(serde_json::json!([
            {
                "type": "ELLIPSE",
                "name": "glow",
                "box": { "x": 0, "y": 0, "width": 100, "height": 100 },
                "fills": [{ "type": "SOLID", "color": { "r": 1, "g": 0, "b": 0, "a": 1 }, "opacity": 1 }],
                "effects": [{ "type": "LAYER_BLUR", "radius": 80 }],
                "paths": ["M0 0Z"],
            },
            {
                "type": "FRAME",
                "name": "scrim",
                "box": { "x": 0, "y": 0, "width": 100, "height": 100 },
                "opacity": 0.6,
                "blendMode": "MULTIPLY",
                "fills": [{ "type": "SOLID", "color": { "r": 0, "g": 0, "b": 0, "a": 1 }, "opacity": 0.5 }],
            }
        ])));
        assert!(rendered.html.contains("filter:blur(40px);"));
        assert!(rendered.html.contains("border-radius:50%;"));
        assert!(rendered.html.contains("opacity:0.6;"));
        assert!(rendered.html.contains("mix-blend-mode:multiply;"));
        assert!(rendered.html.contains("rgba(0,0,0,0.500)"));
    }

    #[test]
    fn malicious_font_family_and_enums_cannot_escape_the_style() {
        let rendered = render_page(&page(serde_json::json!([{
            "type": "TEXT",
            "name": "Label",
            "box": { "x": 0, "y": 0, "width": 100, "height": 20 },
            "characters": "hi",
            "fontName": {
                "family": "Evil';}</style><script>alert(1)</script>&x=\"",
                "style": "Regular"
            },
            "align": "LEFT;background:url(https://evil.example)",
            "blendMode": "NORMAL;background:red",
        }])));
        assert!(rendered.fonts.contains("Evilstylescriptalert1scriptx"));
        assert!(
            rendered
                .html
                .contains("font-family:&#39;Evilstylescriptalert1scriptx&#39;;")
        );
        assert!(rendered.html.contains("text-align:left;"));
        assert!(!rendered.html.contains("evil.example"));
        assert!(!rendered.html.contains("mix-blend-mode"));
        assert!(!rendered.html.contains("<script>alert"));
        assert!(
            rendered
                .html
                .contains("family=Evilstylescriptalert1scriptx:")
        );

        let fonts = BTreeSet::from(["Bear Sans UI".to_string(), "A&B".to_string()]);
        let link = font_link(&fonts);
        assert!(link.contains("family=Bear+Sans+UI:"));
        assert!(link.contains("family=A%26B:"));
    }
}
