//! The screen.
//!
//! Generic over any `embedded-graphics` [`DrawTarget`] with [`BinaryColor`], so the
//! host simulator and the real 200x200 SSD1681 panel render the same pixels. Keep it
//! that way — a "just for the simulator" branch here defeats the whole point.

#![no_std]

use core::fmt::Write;

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::{
    prelude::*,
    primitives::{
        Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, RoundedRectangle, StrokeAlignment,
        Triangle,
    },
};
use heapless::String;
use medienzeit_core::{Flow, Snapshot, WARNING_SECS};
use u8g2_fonts::{
    fonts,
    types::{FontColor, HorizontalAlignment, VerticalPosition},
    FontRenderer,
};

/// Panel geometry. The board is fixed at 200x200.
pub const WIDTH: u32 = 200;
pub const HEIGHT: u32 = 200;

/// `BinaryColor::On` is ink. On the EPD that maps to `Color::Black`; the simulator is
/// configured to match, so "On == dark" holds everywhere.
const INK: BinaryColor = BinaryColor::On;
const PAPER: BinaryColor = BinaryColor::Off;

/// Vertical layout. With the header gone the whole screen moves up, and the space it
/// freed goes to the dock glyphs — the one thing on here that gets read from across a
/// room rather than up close.
const HERO_BASELINE: i32 = 84;
const GAUGE_Y: i32 = 100;
/// Half-width of a dock glyph. They are deliberately large: at a glance from the door
/// the only question is whether both devices are back, and that answer should not need
/// squinting at a 12 px square.
const GLYPH_R: i32 = 22;
const GLYPH_CY: i32 = 146;
const GLYPH_STROKE: u32 = 7;
const NAME_Y: i32 = 181;

/// Everything the screen needs beyond the ledger snapshot.
pub struct Chrome<'a> {
    pub device_names: [&'a str; medienzeit_core::DEVICES],
}

impl Default for Chrome<'_> {
    fn default() -> Self {
        Self { device_names: ["Handy", "Tablet"] }
    }
}

/// Numbers and colon only, which is why anything with a minus sign or a letter uses
/// [`title_font`] instead.
///
/// The label fonts are the `_tf` (Latin-1) variants rather than `_tr` (ASCII), so German
/// umlauts render properly. Worth the extra flash: "zurücklegen" spelled "zuruecklegen"
/// on a device a child reads every day is a small daily insult.
fn hero_font() -> FontRenderer {
    FontRenderer::new::<fonts::u8g2_font_logisoso62_tn>()
}
fn title_font() -> FontRenderer {
    FontRenderer::new::<fonts::u8g2_font_helvB18_tf>()
}
fn label_font() -> FontRenderer {
    FontRenderer::new::<fonts::u8g2_font_helvB10_tf>()
}
fn small_font() -> FontRenderer {
    FontRenderer::new::<fonts::u8g2_font_helvR08_tf>()
}

/// Minutes, rounded *up*, so "1" shows until the time is truly gone.
fn minutes_ceil(secs: i32) -> u32 {
    if secs <= 0 {
        return 0;
    }
    (secs as u32).div_ceil(60)
}

/// Draw the whole screen. Callers present/refresh afterwards.
pub fn render<D>(
    target: &mut D,
    snap: &Snapshot<{ medienzeit_core::DEVICES }>,
    chrome: &Chrome<'_>,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    // Locked out inverts the whole screen. It is unmissable from across the room,
    // which is the entire point of putting a display on this thing.
    let locked = snap.night || snap.exhausted();
    let (bg, fg) = if locked { (INK, PAPER) } else { (PAPER, INK) };

    target.clear(bg)?;

    if locked {
        lockout(target, snap, fg)?;
    } else {
        hero(target, snap, fg)?;
        gauge(target, snap, fg)?;
    }

    dock_row(target, snap, chrome, fg)?;
    flow_cue(target, snap, fg)?;
    Ok(())
}

/// Night, or out of balance. Says what is happening and what to do about it.
fn lockout<D>(target: &mut D, snap: &Snapshot<2>, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let cx = WIDTH as i32 / 2;
    let title = if snap.night { "NACHT" } else { "ZEIT UM" };

    let _ = title_font().render_aligned(
        title,
        Point::new(cx, 52),
        VerticalPosition::Center,
        HorizontalAlignment::Center,
        FontColor::Transparent(fg),
        target,
    );

    // The actionable line matters more than the status one: docking is the only thing
    // that changes the situation, so say so.
    let mut sub: String<32> = String::new();
    if snap.balance_secs < 0 {
        let _ = write!(sub, "Minus {} Min", minutes_ceil(-snap.balance_secs));
    } else if snap.docked.iter().all(|d| *d) {
        let _ = sub.push_str("lädt wieder auf");
    } else {
        let _ = sub.push_str("zurücklegen");
    }
    let _ = label_font().render_aligned(
        sub.as_str(),
        Point::new(cx, 84),
        VerticalPosition::Center,
        HorizontalAlignment::Center,
        FontColor::Transparent(fg),
        target,
    );

    if snap.balance_secs < 0 && !snap.docked.iter().all(|d| *d) {
        let _ = small_font().render_aligned(
            "zurücklegen",
            Point::new(cx, 106),
            VerticalPosition::Center,
            HorizontalAlignment::Center,
            FontColor::Transparent(fg),
            target,
        );
    }
    Ok(())
}

fn hero<D>(target: &mut D, snap: &Snapshot<2>, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let cx = WIDTH as i32 / 2;
    let mins = minutes_ceil(snap.balance_secs);

    let mut big: String<8> = String::new();
    if mins >= 60 {
        let _ = write!(big, "{}:{:02}", mins / 60, mins % 60);
    } else {
        let _ = write!(big, "{mins}");
    }

    // The unit rides on the end of the number instead of taking a line of its own, so
    // the glyphs below can have the height instead. Measured rather than guessed: the
    // pair is centred as a unit, or the number drifts left as it gains digits.
    let unit = if mins >= 60 { "h" } else { "min" };
    let num_w = hero_font()
        .get_rendered_dimensions(big.as_str(), Point::zero(), VerticalPosition::Baseline)
        .map(|d| d.advance.x)
        .unwrap_or(0);
    let unit_w = title_font()
        .get_rendered_dimensions(unit, Point::zero(), VerticalPosition::Baseline)
        .map(|d| d.advance.x)
        .unwrap_or(0);
    const GAP: i32 = 6;
    let left = cx - (num_w + GAP + unit_w) / 2;

    let _ = hero_font().render_aligned(
        big.as_str(),
        Point::new(left, HERO_BASELINE),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(fg),
        target,
    );
    let _ = title_font().render_aligned(
        unit,
        Point::new(left + num_w + GAP, HERO_BASELINE),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(fg),
        target,
    );
    Ok(())
}

/// Balance against the cap, so saving toward something is visible.
fn gauge<D>(target: &mut D, snap: &Snapshot<2>, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let outline = RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(6, GAUGE_Y), Size::new(WIDTH - 12, 16)),
        Size::new(3, 3),
    );
    outline
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(fg)
                .stroke_width(1)
                .stroke_alignment(StrokeAlignment::Inside)
                .build(),
        )
        .draw(target)?;

    if snap.cap_secs > 0 && snap.balance_secs > 0 {
        let inner_w = WIDTH - 16;
        let filled = (snap.balance_secs as u64).min(snap.cap_secs as u64) * inner_w as u64
            / snap.cap_secs as u64;
        if filled > 0 {
            Rectangle::new(Point::new(8, GAUGE_Y + 2), Size::new(filled as u32, 12))
                .into_styled(PrimitiveStyle::with_fill(fg))
                .draw(target)?;
        }
    }
    Ok(())
}

fn dock_row<D>(
    target: &mut D,
    snap: &Snapshot<2>,
    chrome: &Chrome<'_>,
    fg: BinaryColor,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let half = WIDTH as i32 / 2;
    for (i, name) in chrome.device_names.iter().enumerate() {
        let cx = half / 2 + i as i32 * half;
        if snap.docked[i] {
            check(target, cx, GLYPH_CY, fg)?;
        } else {
            cross(target, cx, GLYPH_CY, fg)?;
        }
        let _ = label_font().render_aligned(
            *name,
            Point::new(cx, NAME_Y),
            VerticalPosition::Center,
            HorizontalAlignment::Center,
            FontColor::Transparent(fg),
            target,
        );
    }
    Ok(())
}

fn glyph_style(fg: BinaryColor) -> PrimitiveStyle<BinaryColor> {
    PrimitiveStyle::with_stroke(fg, GLYPH_STROKE)
}

/// Put back. The long arm runs up to the right, which is what makes it read as a tick
/// rather than as an angle.
fn check<D>(target: &mut D, cx: i32, cy: i32, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let style = glyph_style(fg);
    let elbow = Point::new(cx - GLYPH_R / 4, cy + GLYPH_R * 3 / 5);
    Line::new(Point::new(cx - GLYPH_R, cy + GLYPH_R / 8), elbow)
        .into_styled(style)
        .draw(target)?;
    Line::new(elbow, Point::new(cx + GLYPH_R, cy - GLYPH_R * 3 / 4))
        .into_styled(style)
        .draw(target)
}

/// Taken away.
fn cross<D>(target: &mut D, cx: i32, cy: i32, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let style = glyph_style(fg);
    let r = GLYPH_R * 4 / 5;
    Line::new(Point::new(cx - r, cy - r), Point::new(cx + r, cy + r))
        .into_styled(style)
        .draw(target)?;
    Line::new(Point::new(cx + r, cy - r), Point::new(cx - r, cy + r))
        .into_styled(style)
        .draw(target)
}

/// The bottom of the screen says what the balance is doing, wordlessly — no text to
/// translate and no umlauts to render.
///
/// - upward triangle: filling
/// - dashed rule: held (inside grace, or undocked at night)
/// - solid rule: draining, thick in the last minutes
fn flow_cue<D>(target: &mut D, snap: &Snapshot<2>, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let y = 195;
    let right = WIDTH as i32 - 7;
    let cx = WIDTH as i32 / 2;

    match snap.flow {
        Flow::Filling => Triangle::new(
            Point::new(cx, y - 7),
            Point::new(cx - 7, y),
            Point::new(cx + 7, y),
        )
        .into_styled(PrimitiveStyle::with_fill(fg))
        .draw(target),
        Flow::Held => {
            let style = PrimitiveStyle::with_stroke(fg, 1);
            let mut x = 6;
            while x < right {
                let seg_end = (x + 6).min(right);
                Line::new(Point::new(x, y), Point::new(seg_end, y))
                    .into_styled(style)
                    .draw(target)?;
                x += 12;
            }
            Ok(())
        }
        Flow::Draining => {
            let warn = snap.balance_secs <= WARNING_SECS;
            Line::new(Point::new(6, y), Point::new(right, y))
                .into_styled(PrimitiveStyle::with_stroke(fg, if warn { 3 } else { 1 }))
                .draw(target)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_round_up_so_the_last_minute_stays_visible() {
        assert_eq!(minutes_ceil(0), 0);
        assert_eq!(minutes_ceil(1), 1);
        assert_eq!(minutes_ceil(59), 1);
        assert_eq!(minutes_ceil(60), 1);
        assert_eq!(minutes_ceil(61), 2);
        assert_eq!(minutes_ceil(3_600), 60);
    }

    #[test]
    fn a_negative_balance_reads_as_zero_minutes_remaining() {
        // The magnitude is shown separately on the lockout screen; the hero never
        // needs a minus sign, which the numbers-only hero font could not render.
        assert_eq!(minutes_ceil(-1), 0);
        assert_eq!(minutes_ceil(-1_800), 0);
    }
}
