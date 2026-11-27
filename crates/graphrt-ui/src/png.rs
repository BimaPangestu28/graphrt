//! Charts as PNG images, for channels that send media but cannot draw blocks (feature `png`).
//!
//! [`chart_png`] draws a validated `chart` block on a white surface with the fixed
//! 8-colour series palette graphrt hosts use, so a customer on WhatsApp sees the same
//! chart a desktop renders. Text is set in a built-in subset of DejaVu Sans (Latin,
//! Greek, Cyrillic); a chart whose labels need other scripts returns
//! [`PngError::UnsupportedText`], and the host sends [`crate::fallback::to_text`] instead.
//!
//! ```
//! use serde_json::json;
//!
//! let chart = graphrt_ui::validate(&json!({
//!     "$ui": "chart", "version": 1, "type": "bar", "title": "Sales",
//!     "x": {"values": ["Jan", "Feb"]}, "series": [{"name": "2026", "values": [120, 150]}],
//! })).unwrap();
//! let png = graphrt_ui::png::chart_png(&chart, &graphrt_ui::png::PngOptions::default()).unwrap();
//! assert!(png.starts_with(b"\x89PNG"));
//! ```
//!
//! Pure Rust (tiny-skia, fontdue), no system fonts or libraries, a few milliseconds per chart.

use serde_json::Value as Json;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PremultipliedColorU8, Stroke, Transform};

use crate::fallback::{TextOptions, compact, currency_prefix, format_number};
use crate::{UiBlock, UiKind};

const FONT: &[u8] = include_bytes!("../assets/DejaVuSans-subset.ttf");
/// Series colours, in order (the light-surface palette of graphrt hosts).
const SERIES: [[u8; 3]; 8] = [
    [0x25, 0x7D, 0xE0],
    [0xF4, 0x72, 0x30],
    [0x00, 0xB8, 0x94],
    [0xF2, 0xA6, 0x00],
    [0xEE, 0x7F, 0xAC],
    [0x00, 0x8A, 0x17],
    [0x6A, 0x4C, 0xE2],
    [0xEC, 0x46, 0x47],
];
/// The colour of series `index`; past the eighth the palette repeats.
fn series_colour(index: usize) -> [u8; 3] {
    SERIES.get(index % SERIES.len()).copied().unwrap_or(INK)
}

const INK: [u8; 3] = [0x18, 0x18, 0x1B];
const MUTED: [u8; 3] = [0x5F, 0x5F, 0x67];
const GRID: [u8; 3] = [0xE4, 0xE4, 0xE7];
/// More bars than this many per 3 pixels are drawn as an area instead.
const MIN_BAR_SLOT: f32 = 3.0;
/// Most pie slices drawn; the rest are folded into "Other".
const MAX_SLICES: usize = 8;

/// Size and number style of [`chart_png`].
#[derive(Clone, Debug, PartialEq)]
pub struct PngOptions {
    /// Width in points, 320 to 2000.
    pub width: u32,
    /// Height in points, 240 to 2000.
    pub height: u32,
    /// Pixels per point, 1 to 3 (2 stays sharp on phones).
    pub scale: f32,
    /// Thousands separator of axis labels.
    pub thousands: char,
    /// Decimal separator of axis labels.
    pub decimal: char,
}

impl Default for PngOptions {
    fn default() -> Self {
        Self { width: 720, height: 440, scale: 2.0, thousands: ',', decimal: '.' }
    }
}

/// Why a chart could not be drawn.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PngError {
    #[error("only chart blocks can be drawn as an image, this is a {0} block")]
    NotAChart(UiKind),
    #[error("the built-in font has no glyph for `{0}`")]
    UnsupportedText(char),
    #[error("could not draw the chart: {0}")]
    Render(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Anchor {
    Start,
    Middle,
    End,
}

struct Canvas {
    pixmap: Pixmap,
    font: fontdue::Font,
    scale: f32,
}

impl Canvas {
    fn new(options: &PngOptions) -> Result<Self, PngError> {
        let scale = options.scale.clamp(1.0, 3.0);
        let width = (options.width.clamp(320, 2000) as f32 * scale) as u32;
        let height = (options.height.clamp(240, 2000) as f32 * scale) as u32;
        let mut pixmap = Pixmap::new(width, height).ok_or_else(|| PngError::Render("empty image".to_owned()))?;
        pixmap.fill(Color::WHITE);
        let font = fontdue::Font::from_bytes(FONT, fontdue::FontSettings::default())
            .map_err(|error| PngError::Render(error.to_owned()))?;
        Ok(Self { pixmap, font, scale })
    }

    fn check_text(&self, text: &str) -> Result<(), PngError> {
        match text.chars().find(|c| !c.is_whitespace() && self.font.lookup_glyph_index(*c) == 0) {
            Some(missing) => Err(PngError::UnsupportedText(missing)),
            None => Ok(()),
        }
    }

    /// Width of `text` at `size` points.
    fn text_width(&self, text: &str, size: f32) -> f32 {
        text.chars().map(|c| self.font.metrics(c, size).advance_width).sum()
    }

    /// `text` cut with `…` to fit `max_width` points.
    fn fit(&self, text: &str, size: f32, max_width: f32) -> String {
        if self.text_width(text, size) <= max_width {
            return text.to_owned();
        }
        let mut kept = String::new();
        for c in text.chars() {
            let candidate = format!("{kept}{c}…");
            if self.text_width(&candidate, size) > max_width {
                break;
            }
            kept.push(c);
        }
        format!("{}…", kept.trim_end())
    }

    /// Draws `text` with its baseline at `y`, anchored at `x`.
    fn text(&mut self, text: &str, x: f32, y: f32, size: f32, colour: [u8; 3], anchor: Anchor) {
        let offset = match anchor {
            Anchor::Start => 0.0,
            Anchor::Middle => self.text_width(text, size) / 2.0,
            Anchor::End => self.text_width(text, size),
        };
        let pixel_size = size * self.scale;
        let mut pen = (x - offset) * self.scale;
        let baseline = (y * self.scale).round() as i32;
        let (width, height) = (self.pixmap.width() as i32, self.pixmap.height() as i32);
        for c in text.chars() {
            let (metrics, coverage) = self.font.rasterize(c, pixel_size);
            let left = pen.round() as i32 + metrics.xmin;
            let top = baseline - metrics.ymin - metrics.height as i32;
            let pixels = self.pixmap.pixels_mut();
            for (i, alpha) in coverage.iter().enumerate().filter(|(_, alpha)| **alpha > 0) {
                let px = left + (i % metrics.width.max(1)) as i32;
                let py = top + (i / metrics.width.max(1)) as i32;
                if px < 0 || py < 0 || px >= width || py >= height {
                    continue;
                }
                if let Some(pixel) = pixels.get_mut((py * width + px) as usize) {
                    let mix = |ink: u8, under: u8| {
                        ((u32::from(ink) * u32::from(*alpha) + u32::from(under) * (255 - u32::from(*alpha))) / 255)
                            as u8
                    };
                    let blended = PremultipliedColorU8::from_rgba(
                        mix(colour[0], pixel.red()),
                        mix(colour[1], pixel.green()),
                        mix(colour[2], pixel.blue()),
                        255,
                    );
                    if let Some(blended) = blended {
                        *pixel = blended;
                    }
                }
            }
            pen += metrics.advance_width;
        }
    }

    fn paint(colour: [u8; 3], alpha: u8) -> Paint<'static> {
        let mut paint = Paint::default();
        paint.set_color_rgba8(colour[0], colour[1], colour[2], alpha);
        paint.anti_alias = true;
        paint
    }

    fn fill(&mut self, points: &[(f32, f32)], colour: [u8; 3], alpha: u8) {
        if let Some(path) = polygon(points, true) {
            let transform = Transform::from_scale(self.scale, self.scale);
            self.pixmap.fill_path(&path, &Self::paint(colour, alpha), FillRule::Winding, transform, None);
        }
    }

    fn stroke(&mut self, points: &[(f32, f32)], colour: [u8; 3], width: f32) {
        if let Some(path) = polygon(points, false) {
            let stroke = Stroke {
                width,
                line_join: tiny_skia::LineJoin::Round,
                line_cap: tiny_skia::LineCap::Round,
                ..Stroke::default()
            };
            let transform = Transform::from_scale(self.scale, self.scale);
            self.pixmap.stroke_path(&path, &Self::paint(colour, 255), &stroke, transform, None);
        }
    }

    fn rect(&mut self, x: f32, y: f32, width: f32, height: f32, colour: [u8; 3]) {
        let (x2, y2) = (x + width, y + height);
        self.fill(&[(x, y), (x2, y), (x2, y2), (x, y2)], colour, 255);
    }
}

fn polygon(points: &[(f32, f32)], close: bool) -> Option<tiny_skia::Path> {
    let (first, rest) = points.split_first()?;
    let mut builder = PathBuilder::new();
    builder.move_to(first.0, first.1);
    for point in rest {
        builder.line_to(point.0, point.1);
    }
    if close {
        builder.close();
    }
    builder.finish()
}

struct Series {
    name: String,
    values: Vec<Option<f64>>,
}

struct Chart {
    kind: String,
    title: Option<String>,
    subtitle: Option<String>,
    categories: Vec<String>,
    series: Vec<Series>,
    stacked: bool,
    horizontal: bool,
    format: Option<String>,
    currency: Option<String>,
    min: Option<f64>,
    max: Option<f64>,
    numbers: TextOptions,
}

impl Chart {
    fn read(block: &Json, options: &PngOptions) -> Self {
        let text = |value: &Json, key: &str| value.get(key).and_then(Json::as_str).map(str::to_owned);
        let list = |value: Option<&Json>| value.and_then(Json::as_array).cloned().unwrap_or_default();
        let y = block.get("y").cloned().unwrap_or(Json::Null);
        Self {
            kind: text(block, "type").unwrap_or_default(),
            title: text(block, "title"),
            subtitle: text(block, "subtitle"),
            categories: list(block.get("x").and_then(|x| x.get("values")))
                .iter()
                .map(|value| value.as_str().map_or_else(|| value.to_string(), str::to_owned))
                .collect(),
            series: list(block.get("series"))
                .iter()
                .map(|series| Series {
                    name: text(series, "name").unwrap_or_default(),
                    values: list(series.get("values")).iter().map(Json::as_f64).collect(),
                })
                .collect(),
            stacked: block.get("stacked") == Some(&Json::Bool(true)),
            horizontal: block.get("horizontal") == Some(&Json::Bool(true)),
            format: text(&y, "format"),
            currency: text(&y, "currency"),
            min: y.get("min").and_then(Json::as_f64),
            max: y.get("max").and_then(Json::as_f64),
            numbers: TextOptions::default().with_separators(options.thousands, options.decimal),
        }
    }

    fn is_round(&self) -> bool {
        matches!(self.kind.as_str(), "pie" | "donut")
    }

    /// An axis label: large numbers are shortened (1.2M, Rp 60M) so the axis stays narrow.
    fn axis_label(&self, value: f64) -> String {
        let is_large = value.abs() >= 10_000.0;
        match self.format.as_deref() {
            None | Some("number") if is_large => compact(value, &self.numbers),
            Some("currency") if is_large => {
                let (prefix, _) = self.currency.as_deref().map(currency_prefix).unwrap_or_default();
                format!("{}{prefix}{}", if value < 0.0 { "-" } else { "" }, compact(value.abs(), &self.numbers))
            }
            _ => self.value_label(value),
        }
    }

    fn value_label(&self, value: f64) -> String {
        format_number(value, self.format.as_deref(), self.currency.as_deref(), &self.numbers)
    }

    /// The value range the axis covers: the data (stacks summed), zero, and `y.min` / `y.max`.
    fn domain(&self) -> (f64, f64) {
        let (mut low, mut high) = (0.0_f64, 0.0_f64);
        for i in 0..self.categories.len() {
            let at = self.series.iter().filter_map(|series| series.values.get(i).copied().flatten());
            if self.stacked {
                let (mut up, mut down) = (0.0, 0.0);
                for value in at {
                    if value >= 0.0 { up += value } else { down += value }
                }
                low = low.min(down);
                high = high.max(up);
            } else {
                for value in at {
                    low = low.min(value);
                    high = high.max(value);
                }
            }
        }
        let low = self.min.unwrap_or(low);
        let high = self.max.unwrap_or(high);
        if high > low { (low, high) } else { (low, low + 1.0) }
    }
}

/// Round tick values covering `low..=high` (about five of them).
fn ticks(low: f64, high: f64) -> Vec<f64> {
    let rough = (high - low) / 5.0;
    let power = 10f64.powf(rough.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * power).find(|step| *step >= rough).unwrap_or(rough);
    if !step.is_finite() || step <= 0.0 {
        return vec![low, high];
    }
    let mut ticks = Vec::new();
    let mut tick = (low / step).ceil() * step;
    while tick <= high + step * 1e-9 && ticks.len() < 12 {
        ticks.push(if tick.abs() < step * 1e-9 { 0.0 } else { tick });
        tick += step;
    }
    ticks
}

/// Draws `block` (a `chart`) as a PNG image.
///
/// # Errors
/// [`PngError::NotAChart`] for another kind, [`PngError::UnsupportedText`] when a label
/// uses a script the built-in font lacks, [`PngError::Render`] when encoding fails.
pub fn chart_png(block: &UiBlock, options: &PngOptions) -> Result<Vec<u8>, PngError> {
    if block.kind() != UiKind::Chart {
        return Err(PngError::NotAChart(block.kind()));
    }
    let chart = Chart::read(block.as_json(), options);
    let mut canvas = Canvas::new(options)?;
    for text in chart.title.iter().chain(&chart.subtitle).chain(&chart.categories) {
        canvas.check_text(text)?;
    }
    for series in &chart.series {
        canvas.check_text(&series.name)?;
    }
    let width = canvas.pixmap.width() as f32 / canvas.scale;
    let height = canvas.pixmap.height() as f32 / canvas.scale;
    let margin = 20.0;
    let mut top = margin;
    if let Some(title) = &chart.title {
        let title = canvas.fit(title, 17.0, width - 2.0 * margin);
        canvas.text(&title, margin, top + 15.0, 17.0, INK, Anchor::Start);
        top += 26.0;
    }
    if let Some(subtitle) = &chart.subtitle {
        let subtitle = canvas.fit(subtitle, 12.0, width - 2.0 * margin);
        canvas.text(&subtitle, margin, top + 11.0, 12.0, MUTED, Anchor::Start);
        top += 20.0;
    }
    let area = Rect { left: margin, top: top + 6.0, right: width - margin, bottom: height - margin };
    if chart.is_round() {
        draw_round(&mut canvas, &chart, area);
    } else {
        draw_axes_chart(&mut canvas, &chart, area);
    }
    canvas.pixmap.encode_png().map_err(|error| PngError::Render(error.to_string()))
}

#[derive(Clone, Copy)]
struct Rect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

/// The legend above the plot (two or more series); returns the height it used.
fn draw_legend(canvas: &mut Canvas, names: &[String], area: Rect) -> f32 {
    let (mut x, mut y) = (area.left, area.top);
    for (i, name) in names.iter().enumerate() {
        let label = canvas.fit(name, 12.0, 180.0);
        let entry = 18.0 + canvas.text_width(&label, 12.0) + 16.0;
        if x + entry > area.right && x > area.left {
            x = area.left;
            y += 20.0;
        }
        canvas.rect(x, y + 2.0, 10.0, 10.0, series_colour(i));
        canvas.text(&label, x + 16.0, y + 11.5, 12.0, MUTED, Anchor::Start);
        x += entry;
    }
    y - area.top + 24.0
}

fn draw_axes_chart(canvas: &mut Canvas, chart: &Chart, mut area: Rect) {
    if chart.series.len() > 1 {
        let names: Vec<String> = chart.series.iter().map(|series| series.name.clone()).collect();
        area.top += draw_legend(canvas, &names, area);
    }
    let count = chart.categories.len().max(1);
    let (low, high) = chart.domain();
    let tick_values = ticks(low, high);
    let tick_labels: Vec<String> = tick_values.iter().map(|tick| chart.axis_label(*tick)).collect();
    let horizontal = chart.horizontal && chart.kind == "bar";
    let label_size = 11.0;

    // The plot rectangle: room for value labels on one side and category labels on the other.
    let value_gutter = tick_labels.iter().map(|label| canvas.text_width(label, label_size)).fold(0.0, f32::max) + 10.0;
    let plot = if horizontal {
        let widest = chart.categories.iter().map(|c| canvas.text_width(c, label_size)).fold(0.0, f32::max);
        let gutter = widest.min((area.right - area.left) * 0.35) + 10.0;
        Rect { left: area.left + gutter, top: area.top + 4.0, right: area.right - 14.0, bottom: area.bottom - 20.0 }
    } else {
        Rect {
            left: area.left + value_gutter,
            top: area.top + 8.0,
            right: area.right - 4.0,
            bottom: area.bottom - 22.0,
        }
    };
    let (plot_width, plot_height) = ((plot.right - plot.left).max(1.0), (plot.bottom - plot.top).max(1.0));
    let band_length = if horizontal { plot_height } else { plot_width };
    let slot = band_length / count as f32;
    // Value -> position along the value axis.
    let along = |value: f64| ((value - low) / (high - low)).clamp(0.0, 1.0) as f32;
    let value_pos = |value: f64| {
        if horizontal { plot.left + along(value) * plot_width } else { plot.bottom - along(value) * plot_height }
    };
    let band_start = |i: usize| if horizontal { plot.top + slot * i as f32 } else { plot.left + slot * i as f32 };

    // Grid and value labels.
    for (tick, label) in tick_values.iter().zip(&tick_labels) {
        let position = value_pos(*tick);
        if horizontal {
            canvas.stroke(&[(position, plot.top), (position, plot.bottom)], GRID, 1.0);
            canvas.text(label, position, plot.bottom + 15.0, label_size, MUTED, Anchor::Middle);
        } else {
            canvas.stroke(&[(plot.left, position), (plot.right, position)], GRID, 1.0);
            canvas.text(label, plot.left - 8.0, position + 4.0, label_size, MUTED, Anchor::End);
        }
    }

    // Category labels, skipping those that would overlap.
    if horizontal {
        let step = (14.0 / slot).ceil().max(1.0) as usize;
        for (i, category) in chart.categories.iter().enumerate().step_by(step) {
            let label = canvas.fit(category, label_size, plot.left - area.left - 10.0);
            canvas.text(&label, plot.left - 8.0, band_start(i) + slot / 2.0 + 4.0, label_size, MUTED, Anchor::End);
        }
    } else {
        let widest = chart.categories.iter().map(|c| canvas.text_width(c, label_size)).fold(0.0, f32::max).min(90.0);
        let step = ((widest + 10.0) / slot).ceil().max(1.0) as usize;
        for (i, category) in chart.categories.iter().enumerate().step_by(step) {
            let label = canvas.fit(category, label_size, 90.0);
            canvas.text(&label, band_start(i) + slot / 2.0, plot.bottom + 16.0, label_size, MUTED, Anchor::Middle);
        }
    }

    let dense_bars = chart.kind == "bar" && slot < MIN_BAR_SLOT;
    match chart.kind.as_str() {
        "bar" if !dense_bars => {
            let group = slot * 0.72;
            let stacked = chart.stacked;
            let bar = if stacked { group } else { group / chart.series.len().max(1) as f32 };
            for i in 0..count {
                let (mut up, mut down) = (0.0_f64, 0.0_f64);
                for (s, series) in chart.series.iter().enumerate() {
                    let Some(value) = series.values.get(i).copied().flatten() else { continue };
                    let (from, to) = if stacked {
                        let base = if value >= 0.0 { &mut up } else { &mut down };
                        let from = *base;
                        *base += value;
                        (from, *base)
                    } else {
                        (0.0_f64.clamp(low, high), value)
                    };
                    let (a, b) = (value_pos(from), value_pos(to));
                    let offset = band_start(i) + (slot - group) / 2.0 + if stacked { 0.0 } else { bar * s as f32 };
                    let thickness = (bar - if stacked { 0.0 } else { 1.0 }).max(1.0);
                    let colour = series_colour(s);
                    if horizontal {
                        canvas.rect(a.min(b), offset, (a - b).abs().max(1.0), thickness, colour);
                    } else {
                        canvas.rect(offset, a.min(b), thickness, (a - b).abs().max(1.0), colour);
                    }
                }
            }
        }
        kind => {
            let fills = kind == "area" || dense_bars;
            let mut base = vec![0.0_f64; count];
            for (s, series) in chart.series.iter().enumerate() {
                let colour = series_colour(s);
                let stack = chart.stacked && fills;
                // Runs of known values; a null breaks the line.
                let mut run: Vec<(f32, f32, f32)> = Vec::new();
                let flush = |canvas: &mut Canvas, run: &mut Vec<(f32, f32, f32)>| {
                    if fills && run.len() > 1 {
                        let mut outline: Vec<(f32, f32)> = run.iter().map(|(x, y, _)| (*x, *y)).collect();
                        outline.extend(run.iter().rev().map(|(x, _, floor)| (*x, *floor)));
                        canvas.fill(&outline, colour, if stack { 200 } else { 60 });
                    }
                    let line: Vec<(f32, f32)> = run.iter().map(|(x, y, _)| (*x, *y)).collect();
                    match line.as_slice() {
                        [(x, y)] => canvas.fill(&circle(*x, *y, 2.5), colour, 255),
                        _ => canvas.stroke(&line, colour, 2.0),
                    }
                    run.clear();
                };
                for i in 0..count {
                    match series.values.get(i).copied().flatten() {
                        Some(value) => {
                            let floor =
                                if stack { base.get(i).copied().unwrap_or(0.0) } else { 0.0_f64.clamp(low, high) };
                            let top = if stack { floor + value } else { value };
                            if let (true, Some(slot_base)) = (stack, base.get_mut(i)) {
                                *slot_base = top;
                            }
                            run.push((band_start(i) + slot / 2.0, value_pos(top), value_pos(floor)));
                        }
                        None => flush(canvas, &mut run),
                    }
                }
                flush(canvas, &mut run);
            }
        }
    }
    // The zero line on top of the marks.
    let zero = value_pos(0.0_f64.clamp(low, high));
    if horizontal {
        canvas.stroke(&[(zero, plot.top), (zero, plot.bottom)], MUTED, 1.0);
    } else {
        canvas.stroke(&[(plot.left, zero), (plot.right, zero)], MUTED, 1.0);
    }
}

fn circle(cx: f32, cy: f32, radius: f32) -> Vec<(f32, f32)> {
    arc(cx, cy, radius, 0.0, std::f32::consts::TAU)
}

/// Points of the arc from angle `from` to `to` (radians, clockwise from 12 o'clock).
fn arc(cx: f32, cy: f32, radius: f32, from: f32, to: f32) -> Vec<(f32, f32)> {
    let steps = (((to - from).abs() / 0.03).ceil() as usize).clamp(2, 256);
    (0..=steps)
        .map(|i| {
            let angle = from + (to - from) * i as f32 / steps as f32;
            (cx + radius * angle.sin(), cy - radius * angle.cos())
        })
        .collect()
}

fn draw_round(canvas: &mut Canvas, chart: &Chart, area: Rect) {
    let values = chart.series.first().map(|series| series.values.as_slice()).unwrap_or_default();
    let mut slices: Vec<(String, f64)> = chart
        .categories
        .iter()
        .zip(values)
        .map(|(name, value)| (name.clone(), value.unwrap_or(0.0).max(0.0)))
        .collect();
    if slices.len() > MAX_SLICES {
        let rest: f64 = slices.iter().skip(MAX_SLICES - 1).map(|(_, value)| value).sum();
        slices.truncate(MAX_SLICES - 1);
        slices.push(("Other".to_owned(), rest));
    }
    let total: f64 = slices.iter().map(|(_, value)| value).sum();
    let height = area.bottom - area.top;
    let radius = (height / 2.0).min((area.right - area.left) * 0.25) - 4.0;
    let (cx, cy) = (area.left + radius + 8.0, area.top + height / 2.0);
    let inner = if chart.kind == "donut" { radius * 0.58 } else { 0.0 };
    let mut angle = 0.0_f32;
    for (i, (_, value)) in slices.iter().enumerate() {
        if total <= 0.0 || *value <= 0.0 {
            continue;
        }
        let sweep = (*value / total) as f32 * std::f32::consts::TAU;
        let mut outline = arc(cx, cy, radius, angle, angle + sweep);
        if inner > 0.0 {
            outline.extend(arc(cx, cy, inner, angle + sweep, angle));
        } else {
            outline.push((cx, cy));
        }
        canvas.fill(&outline, series_colour(i), 255);
        // A white gap between slices.
        if slices.len() > 1 {
            let edge = arc(cx, cy, radius, angle, angle);
            if let Some((x, y)) = edge.first() {
                let start = (cx + inner * angle.sin(), cy - inner * angle.cos());
                canvas.stroke(&[start, (*x, *y)], [255, 255, 255], 2.0);
            }
        }
        angle += sweep;
    }
    if total <= 0.0 {
        canvas.stroke(&circle(cx, cy, radius), GRID, 2.0);
    }
    // Legend on the right: name, value and share.
    let left = cx + radius + 28.0;
    let line_height = 24.0;
    let mut y = cy - (slices.len() as f32 * line_height) / 2.0 + 8.0;
    for (i, (name, value)) in slices.iter().enumerate() {
        let share = if total > 0.0 { format!(" ({:.0}%)", value / total * 100.0) } else { String::new() };
        let amount = format!("{}{share}", chart.value_label(*value));
        let amount_width = canvas.text_width(&amount, 12.0);
        canvas.rect(left, y - 9.0, 10.0, 10.0, series_colour(i));
        let label = canvas.fit(name, 12.0, (area.right - left - 16.0 - amount_width - 12.0).max(40.0));
        canvas.text(&label, left + 16.0, y, 12.0, INK, Anchor::Start);
        canvas.text(&amount, area.right, y, 12.0, MUTED, Anchor::End);
        y += line_height;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate;
    use serde_json::json;

    fn png(block: Json) -> Result<Vec<u8>, PngError> {
        chart_png(&validate(&block).unwrap(), &PngOptions::default())
    }

    /// Decodes the PNG and returns (width, height, RGBA pixels).
    fn decode(bytes: &[u8]) -> (u32, u32, Vec<[u8; 4]>) {
        let pixmap = Pixmap::decode_png(bytes).unwrap();
        let pixels = pixmap.pixels().iter().map(|p| [p.red(), p.green(), p.blue(), p.alpha()]).collect();
        (pixmap.width(), pixmap.height(), pixels)
    }

    fn count(pixels: &[[u8; 4]], colour: [u8; 3]) -> usize {
        pixels.iter().filter(|p| p[0] == colour[0] && p[1] == colour[1] && p[2] == colour[2]).count()
    }

    #[test]
    fn draws_every_chart_fixture() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/valid");
        let mut drawn = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let block: Json = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if block["$ui"] != "chart" {
                continue;
            }
            let bytes = png(block).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let (width, height, pixels) = decode(&bytes);
            assert_eq!((width, height), (1440, 880), "{}", path.display());
            assert!(count(&pixels, SERIES[0]) > 500, "{}: the first series is drawn", path.display());
            assert!(pixels.iter().all(|p| p[3] == 255), "{}: the image is opaque", path.display());
            // Save with GRAPHRT_UI_PNG_DIR=/some/dir to look at them.
            if let Ok(out) = std::env::var("GRAPHRT_UI_PNG_DIR") {
                let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                std::fs::write(std::path::Path::new(&out).join(format!("{name}.png")), &bytes).unwrap();
            }
            drawn += 1;
        }
        assert!(drawn >= 4);
    }

    #[test]
    fn bars_grow_with_their_values_and_series_keep_their_colours() {
        let chart = |values: [i64; 2]| {
            json!({"$ui": "chart", "version": 1, "type": "bar", "x": {"values": ["A", "B"]},
                "series": [{"name": "One", "values": values}, {"name": "Two", "values": [5, 5]}]})
        };
        let (_, _, small) = decode(&png(chart([1, 1])).unwrap());
        let (_, _, large) = decode(&png(chart([10, 10])).unwrap());
        assert!(count(&large, SERIES[0]) > 3 * count(&small, SERIES[0]));
        assert!(count(&large, SERIES[1]) > 500);
        assert_eq!(count(&large, SERIES[2]), 0);
    }

    #[test]
    fn handles_gaps_negatives_stacks_and_long_series() {
        let gaps = json!({"$ui": "chart", "version": 1, "type": "line", "x": {"values": [1, 2, 3, 4]},
            "series": [{"name": "A", "values": [null, 5, null, -3]}]});
        assert!(png(gaps).is_ok());
        let stacked = json!({"$ui": "chart", "version": 1, "type": "area", "stacked": true, "x": {"values": ["a", "b", "c"]},
            "y": {"format": "currency", "currency": "IDR"},
            "series": [{"name": "A", "values": [1000000, 2000000, 1500000]}, {"name": "B", "values": [500000, null, 700000]}]});
        assert!(png(stacked).is_ok());
        let categories: Vec<i64> = (0..10_000).collect();
        let values: Vec<f64> = (0..10_000).map(|i| (f64::from(i) / 300.0).sin() * 1e6).collect();
        let dense = json!({"$ui": "chart", "version": 1, "type": "bar", "x": {"values": categories},
            "series": [{"name": "Signal", "values": values}]});
        assert!(png(dense).is_ok());
        let empty_pie = json!({"$ui": "chart", "version": 1, "type": "pie", "x": {"values": ["a"]},
            "series": [{"name": "A", "values": [0]}]});
        assert!(png(empty_pie).is_ok());
    }

    #[test]
    fn refuses_other_kinds_and_scripts_the_font_lacks() {
        let stats = validate(&json!({"$ui": "stats", "version": 1, "items": [{"label": "A", "value": 1}]})).unwrap();
        assert_eq!(chart_png(&stats, &PngOptions::default()), Err(PngError::NotAChart(UiKind::Stats)));
        let cjk = json!({"$ui": "chart", "version": 1, "type": "bar", "title": "売上", "x": {"values": ["A"]},
            "series": [{"name": "S", "values": [1]}]});
        assert_eq!(png(cjk), Err(PngError::UnsupportedText('売')));
        let accents = json!({"$ui": "chart", "version": 1, "type": "bar", "title": "Café – Продажи €", "x": {"values": ["Ä"]},
            "series": [{"name": "S", "values": [1]}]});
        assert!(png(accents).is_ok());
    }

    #[test]
    fn ticks_are_round_numbers() {
        assert_eq!(ticks(0.0, 100.0), [0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
        assert_eq!(ticks(0.0, 7.0), [0.0, 2.0, 4.0, 6.0]);
        assert_eq!(ticks(-50.0, 50.0), [-40.0, -20.0, 0.0, 20.0, 40.0]);
        assert!(ticks(0.0, 0.0).len() <= 2);
    }

    #[test]
    fn the_size_follows_the_options() {
        let chart = validate(&json!({"$ui": "chart", "version": 1, "type": "line", "x": {"values": ["a", "b"]},
            "series": [{"name": "A", "values": [1, 2]}]}))
        .unwrap();
        let bytes =
            chart_png(&chart, &PngOptions { width: 400, height: 300, scale: 1.0, ..PngOptions::default() }).unwrap();
        let (width, height, _) = decode(&bytes);
        assert_eq!((width, height), (400, 300));
    }
}
