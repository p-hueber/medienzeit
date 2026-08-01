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

/// Numbers and colon only, which is why anything with a minus sign or a letter uses
/// [`title_font`] instead.
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
    header(target, snap, fg)?;

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

/// Night, or out of balance. Says what is happening and what to do about it.
fn lockout<D>(target: &mut D, snap: &Snapshot<2>, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let cx = WIDTH as i32 / 2;
    let title = if snap.night { "NACHT" } else { "ZEIT UM" };

    let _ = title_font().render_aligned(
        title,
        Point::new(cx, 70),
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
        let _ = sub.push_str("laedt wieder auf");
    } else {
        let _ = sub.push_str("in die Box legen");
    }
    let _ = label_font().render_aligned(
        sub.as_str(),
        Point::new(cx, 102),
        VerticalPosition::Center,
        HorizontalAlignment::Center,
        FontColor::Transparent(fg),
        target,
    );

    if snap.balance_secs < 0 && !snap.docked.iter().all(|d| *d) {
        let _ = small_font().render_aligned(
            "in die Box legen",
            Point::new(cx, 124),
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

/// Balance against the cap, so saving toward something is visible.
fn gauge<D>(target: &mut D, snap: &Snapshot<2>, fg: BinaryColor) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let outline = RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(6, 138), Size::new(WIDTH - 12, 16)),
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
            Rectangle::new(Point::new(8, 140), Size::new(filled as u32, 12))
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

        // Filled square = on the cradle, hollow = out of the box.
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
    Ok(())
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
    let y = 192;
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
