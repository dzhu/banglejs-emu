use tui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::Color,
    widgets::{Block, StatefulWidget, Widget},
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

impl<'a> StatefulWidget for TuiScreen<'a> {
    type State = (u16, u16);

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        if area.height < 1 {
            return;
        }

        let x0 = get_line_offset(176, area.width, Alignment::Center);
        let y0 = 0;

        *state = (x0, y0);

        for y in (0..176.min(2 * area.height)).step_by(2) {
            for x in 0..176.min(area.width) {
                buf.get_mut(area.left() + x0 + x, area.top() + y0 + y / 2)
                    .set_symbol("\u{2584}")
                    .set_bg(color(self.screen.0[y as usize][x as usize]))
                    .set_fg(color(self.screen.0[y as usize + 1][x as usize]));
            }
        }
    }
}

pub struct Blocked<'a, W> {
    block: Block<'a>,
    inner: W,
}

impl<'a, W> Blocked<'a, W> {
    pub fn new(block: Block<'a>, inner: W) -> Self {
        Self { block, inner }
    }
}

impl<'a, W: Widget> Widget for Blocked<'a, W> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let inner = self.block.inner(area);
        self.block.render(area, buf);
        self.inner.render(inner, buf);
    }
}

impl<'a, W: StatefulWidget> StatefulWidget for Blocked<'a, W> {
    type State = W::State;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let inner = self.block.inner(area);
        self.block.render(area, buf);
        self.inner.render(inner, buf, state);
    }
}
