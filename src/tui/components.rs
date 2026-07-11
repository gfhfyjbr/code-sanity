use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

pub const BG: Color = Color::Rgb(5, 7, 11);
pub const PANEL: Color = Color::Rgb(13, 16, 23);
pub const PANEL_HOVER: Color = Color::Rgb(24, 28, 38);
pub const BORDER: Color = Color::Rgb(49, 54, 67);
pub const FG: Color = Color::Rgb(244, 245, 247);
pub const MUTED: Color = Color::Rgb(146, 150, 161);
pub const PRIMARY: Color = Color::Rgb(168, 177, 255);
pub const SUCCESS: Color = Color::Rgb(61, 214, 140);
pub const WARNING: Color = Color::Rgb(249, 180, 78);
pub const DANGER: Color = Color::Rgb(255, 99, 105);
pub const ACCENT: Color = Color::Rgb(92, 205, 255);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Ghost,
    Success,
    Destructive,
}

#[derive(Debug, Clone)]
pub struct ButtonHit<A> {
    pub area: Rect,
    pub action: A,
}

impl<A: Clone> ButtonHit<A> {
    pub fn action_at(&self, column: u16, row: u16) -> Option<A> {
        contains(self.area, column, row).then(|| self.action.clone())
    }
}

pub fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

pub fn panel(title: impl Into<String>, focused: bool) -> Block<'static> {
    Block::default()
        .title(format!(" {} ", title.into()))
        .title_style(
            Style::default()
                .fg(if focused { PRIMARY } else { MUTED })
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { PRIMARY } else { BORDER }))
        .style(Style::default().bg(PANEL).fg(FG))
}

pub fn draw_button(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    variant: ButtonVariant,
    hovered: bool,
    disabled: bool,
) {
    let (fg, bg, border) = match variant {
        ButtonVariant::Primary => (BG, PRIMARY, PRIMARY),
        ButtonVariant::Secondary => (FG, PANEL_HOVER, BORDER),
        ButtonVariant::Ghost => (MUTED, BG, BG),
        ButtonVariant::Success => (BG, SUCCESS, SUCCESS),
        ButtonVariant::Destructive => (FG, Color::Rgb(82, 27, 32), DANGER),
    };
    let mut style = Style::default().fg(fg).bg(bg);
    let mut border_style = Style::default().fg(border);
    if hovered && !disabled {
        style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
        border_style = border_style.fg(FG);
    }
    if disabled {
        style = Style::default().fg(BORDER).bg(PANEL);
        border_style = Style::default().fg(BORDER);
    }
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(style)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(border_style),
            ),
        area,
    );
}

pub fn popup(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .title(format!(" {title} "))
            .title_alignment(Alignment::Center)
            .title_style(Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(PRIMARY))
            .style(Style::default().bg(PANEL).fg(FG)),
        area,
    );
    Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(2),
        area.width.saturating_sub(4),
        area.height.saturating_sub(4),
    )
}

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}
