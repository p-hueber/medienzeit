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
    },
};
use heapless::String;
use medienzeit_core::{Snapshot, WARNING_SECS};
use u8g2_fonts::{
    fonts,
    types::{FontColor, HorizontalAlignment, VerticalPosition},
    FontRenderer,
};

/// Panel geometry. The board is fixed at 200x200.
pub const WIDTH: u32 = 200;
pub const HEIGHT: u32 = 200;

/// `BinaryColor::On` is ink. On the EPD that maps to `Color::Black`; the simulator
/// is configured to match, so "On == dark" holds everywhere.
const INK: BinaryColor = BinaryColor::On;
const PAPER: BinaryColor = BinaryColor::Off;

const WEEKDAYS_DE: [&str; 7] = ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"];

/// Everything the screen needs beyond the ledger snapshot.
pub struct Chrome<'a> {
    pub device_names: [&'a str; medienzeit_core::DEVICES],
}

impl Default for Chrome<'_> {
    fn default() -> Self {
        Self { device_names: ["Handy", "Tablet"] }
    }
}

fn hero_font() -> FontRenderer {
    FontRenderer::new::<fonts::u8g2_font_logisoso62_tn>()
}
fn title_font() -> FontRenderer {
    FontRenderer::new::<fonts::u8g2_font_helvB18_tr>()
}
fn label_font() -> FontRenderer {
    FontRenderer::new::<fonts::u8g2_font_helvB10_tr>()
}
fn small_font() -> FontRenderer {
    FontRenderer::new::<fonts::u8g2_font_helvR08_tr>()
}

/// Minutes remaining, rounded *up*, so "1 Minute" shows until the time is truly gone.
fn minutes_ceil(secs: u32) -> u32 {
    secs.div_ceil(60)
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
    // When the budget is gone the whole screen inverts. It is unmissable from across
    // the room, which is the entire point of putting a display on this thing.
    let (bg, fg) = if snap.exhausted { (INK, PAPER) } else { (PAPER, INK) };

    target.clear(bg)?;
    header(target, snap, fg)?;
    hero(target, snap, fg)?;
    progress(target, snap, fg)?;
    dock_row(target, snap, chrome, fg)?;
    Ok(())
}

fn header<D>(target: &mut D, snap: &Snapshot<2>, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let _ = small_font().render_aligned(
        "MEDIENZEIT",
        Point::new(6, 4),
        VerticalPosition::Top,
        HorizontalAlignment::Left,
        FontColor::Transparent(fg),
        target,
    );

    let mut clock: String<16> = String::new();
    let _ = write!(
        clock,
        "{} {:02}:{:02}",
        WEEKDAYS_DE[snap.local.weekday as usize % 7],
        snap.local.hour,
        snap.local.minute
    );
    let _ = small_font().render_aligned(
        clock.as_str(),
        Point::new(WIDTH as i32 - 6, 4),
        VerticalPosition::Top,
        HorizontalAlignment::Right,
        FontColor::Transparent(fg),
        target,
    );

    Line::new(Point::new(6, 18), Point::new(WIDTH as i32 - 7, 18))
        .into_styled(PrimitiveStyle::with_stroke(fg, 1))
        .draw(target)
}

fn hero<D>(target: &mut D, snap: &Snapshot<2>, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let cx = WIDTH as i32 / 2;

    if snap.exhausted {
        let _ = title_font().render_aligned(
            "ZEIT UM",
            Point::new(cx, 74),
            VerticalPosition::Center,
            HorizontalAlignment::Center,
            FontColor::Transparent(fg),
            target,
        );
        let _ = label_font().render_aligned(
            "Morgen wieder",
            Point::new(cx, 106),
            VerticalPosition::Center,
            HorizontalAlignment::Center,
            FontColor::Transparent(fg),
            target,
        );
        return Ok(());
    }

    let mins = minutes_ceil(snap.remaining_secs);
    let mut big: String<8> = String::new();
    if mins >= 60 {
        let _ = write!(big, "{}:{:02}", mins / 60, mins % 60);
    } else {
        let _ = write!(big, "{mins}");
    }

    let _ = hero_font().render_aligned(
        big.as_str(),
        Point::new(cx, 82),
        VerticalPosition::Center,
        HorizontalAlignment::Center,
        FontColor::Transparent(fg),
        target,
    );

    let unit = if mins >= 60 { "Stunden" } else { "Minuten" };
    let _ = label_font().render_aligned(
        unit,
        Point::new(cx, 124),
        VerticalPosition::Center,
        HorizontalAlignment::Center,
        FontColor::Transparent(fg),
        target,
    );

    Ok(())
}

fn progress<D>(target: &mut D, snap: &Snapshot<2>, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let outline = RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(6, 138), Size::new(WIDTH - 12, 16)),
        Size::new(3, 3),
    );

    // Inverted screen, full bar: draw it solid. A 1 px gap between a white outline and
    // a white fill reads as an *empty* bar on black, which is exactly backwards.
    if snap.exhausted {
        return outline.into_styled(PrimitiveStyle::with_fill(fg)).draw(target);
    }

    outline
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(fg)
                .stroke_width(1)
                .stroke_alignment(StrokeAlignment::Inside)
                .build(),
        )
        .draw(target)?;

    // An away-window shows an empty bar with a caption rather than a fill, so
    // "paused" never looks like "you have used none of it".
    if snap.away {
        let _ = small_font().render_aligned(
            "PAUSE - zaehlt nicht",
            Point::new(WIDTH as i32 / 2, 146),
            VerticalPosition::Center,
            HorizontalAlignment::Center,
            FontColor::Transparent(fg),
            target,
        );
        return Ok(());
    }

    if snap.allowance_secs > 0 {
        let inner_w = WIDTH - 16;
        let frac = snap.spent_secs.min(snap.allowance_secs) as u64 * inner_w as u64
            / snap.allowance_secs as u64;
        if frac > 0 {
            Rectangle::new(Point::new(8, 140), Size::new(frac as u32, 12))
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
        let x = 8 + i as i32 * half;

        // Filled square = on the cradle, hollow = in her hands.
        let box_rect = Rectangle::new(Point::new(x, 168), Size::new(12, 12));
        if snap.docked[i] {
            box_rect.into_styled(PrimitiveStyle::with_fill(fg)).draw(target)?;
        } else {
            box_rect.into_styled(PrimitiveStyle::with_stroke(fg, 1)).draw(target)?;
        }

        let _ = label_font().render_aligned(
            *name,
            Point::new(x + 18, 174),
            VerticalPosition::Center,
            HorizontalAlignment::Left,
            FontColor::Transparent(fg),
            target,
        );
    }

    // The rule along the bottom is the clock-state cue, on a display that cannot
    // animate: dashed = picked up but still free, solid = billing, thick = last
    // minutes. Deliberately wordless — no text to translate, no umlauts to render.
    if !snap.exhausted {
        let y = 192;
        let right = WIDTH as i32 - 7;
        if snap.in_grace {
            let style = PrimitiveStyle::with_stroke(fg, 1);
            let mut x = 6;
            while x < right {
                let seg_end = (x + 6).min(right);
                Line::new(Point::new(x, y), Point::new(seg_end, y))
                    .into_styled(style)
                    .draw(target)?;
                x += 12;
            }
        } else if snap.spending {
            let warn = snap.remaining_secs <= WARNING_SECS;
            Line::new(Point::new(6, y), Point::new(right, y))
                .into_styled(PrimitiveStyle::with_stroke(fg, if warn { 3 } else { 1 }))
                .draw(target)?;
        }
    }

    Ok(())
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
}
