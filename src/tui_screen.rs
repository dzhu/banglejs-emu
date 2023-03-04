use tui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::Color,
    widgets::Widget,
};

use crate::emu::{self, Screen};

fn get_line_offset(line_width: u16, text_area_width: u16, alignment: Alignment) -> u16 {
    match alignment {
        Alignment::Center => (text_area_width / 2).saturating_sub(line_width / 2),
        Alignment::Right => text_area_width.saturating_sub(line_width),
        Alignment::Left => 0,
    }
}

#[derive(Clone)]
pub struct TuiScreen<'a> {
    screen: &'a Screen,
}

impl<'a> TuiScreen<'a> {
    pub fn new(screen: &'a emu::Screen) -> TuiScreen<'a> {
        TuiScreen { screen }
    }
}

fn color(c: emu::Color) -> Color {
    match c.rgb() {
        (false, false, false) => Color::Black,
        (false, false, true) => Color::Blue,
        (false, true, false) => Color::Green,
        (false, true, true) => Color::Cyan,
        (true, false, false) => Color::Red,
        (true, false, true) => Color::Magenta,
        (true, true, false) => Color::Yellow,
        (true, true, true) => Color::White,
    }
}

impl<'a> Widget for TuiScreen<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 1 {
            return;
        }

        let x0 = get_line_offset(176, area.width, Alignment::Center);
        for y in (0..176.min(2 * area.height)).step_by(2) {
            for x in 0..176.min(area.width) {
                buf.get_mut(area.left() + x0 + x, area.top() + y / 2)
                    .set_symbol("\u{2584}")
                    .set_bg(color(self.screen.0[y as usize][x as usize]))
                    .set_fg(color(self.screen.0[y as usize + 1][x as usize]));
            }
        }
    }
}
