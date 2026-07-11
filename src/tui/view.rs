use super::app::{App, HitAction, LogLevel, Tab, ToolbarAction, is_pending};
use super::components::{
    ACCENT, BG, BORDER, ButtonHit, ButtonVariant, DANGER, FG, MUTED, PANEL, PANEL_HOVER, PRIMARY,
    SUCCESS, WARNING, centered, contains, draw_button, panel, popup,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Row, Table, Wrap};

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
    app.hits.clear();

    if area.width < 72 || area.height < 20 {
        render_too_small(frame, area);
        return;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    render_header(frame, app, layout[0]);
    render_tabs(frame, app, layout[1]);
    match app.tab {
        Tab::Review => render_review(frame, app, layout[2]),
        Tab::Activity => render_activity(frame, app, layout[2]),
        Tab::Workspace => render_workspace(frame, app, layout[2]),
    }
    render_toolbar(frame, app, layout[3]);
    render_command(frame, app, layout[4]);
    render_status(frame, app, layout[5]);

    if app.command_mode {
        render_command_suggestions(frame, app, layout[4]);
    }
    if app.show_help {
        render_help(frame, app, area);
    }
    if app.confirmation.is_some() {
        render_confirmation(frame, app, area);
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(22),
            Constraint::Min(20),
            Constraint::Length(24),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " code",
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "-sanity",
                Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(BORDER)),
        )
        .alignment(Alignment::Left),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(app.root.display().to_string())
            .style(Style::default().fg(MUTED))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(BORDER)),
            )
            .alignment(Alignment::Center),
        chunks[1],
    );
    let badge = if let Some(job) = &app.job {
        format!(" {} {} ", spinner(app.tick), job.label)
    } else if app.workspace.initialized {
        format!(
            " {} files | {} pending ",
            app.workspace.tracked_files,
            pending_count(app)
        )
    } else {
        " not initialized ".to_string()
    };
    frame.render_widget(
        Paragraph::new(badge)
            .style(
                Style::default()
                    .fg(if app.job.is_some() { WARNING } else { SUCCESS })
                    .add_modifier(Modifier::BOLD),
            )
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(BORDER)),
            )
            .alignment(Alignment::Right),
        chunks[2],
    );
}

fn render_tabs(frame: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Min(0),
        ])
        .split(area);
    for (index, tab) in Tab::ALL.iter().copied().enumerate() {
        let selected = tab == app.tab;
        let count = if tab == Tab::Review {
            format!("  {}", app.filtered.len())
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(format!("{}{}", tab.title(), count))
                .alignment(Alignment::Center)
                .style(
                    Style::default()
                        .fg(if selected { FG } else { MUTED })
                        .bg(if selected { PANEL_HOVER } else { BG })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                )
                .block(
                    Block::default()
                        .borders(Borders::BOTTOM)
                        .border_style(Style::default().fg(if selected { PRIMARY } else { BORDER })),
                ),
            chunks[index],
        );
        app.hits.push(ButtonHit {
            area: chunks[index],
            action: HitAction::Tab(tab),
        });
    }
}

fn render_review(frame: &mut Frame, app: &mut App, area: Rect) {
    let areas = if area.width >= 78 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area)
    };
    render_review_list(frame, app, areas[0]);
    render_review_detail(frame, app, areas[1]);
}

fn render_review_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.filter.is_empty() {
        format!("Queue ({})", app.filtered.len())
    } else {
        format!("Queue ({}) /{}", app.filtered.len(), app.filter)
    };
    frame.render_widget(panel(title, true), area);
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let row_heights = app
        .filtered
        .iter()
        .map(|index| {
            review_row_text(&app.reviews[*index], inner.width as usize, false)
                .0
                .len()
        })
        .collect::<Vec<_>>();
    let viewport_height = inner.height.max(1) as usize;
    app.list_offset = app.list_offset.min(app.filtered.len().saturating_sub(1));
    if app.selected < app.list_offset {
        app.list_offset = app.selected;
    }
    while app.list_offset < app.selected
        && row_heights[app.list_offset..=app.selected]
            .iter()
            .sum::<usize>()
            > viewport_height
    {
        app.list_offset += 1;
    }

    if app.filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.reviews.is_empty() {
                "No pending proposals\n\nRun :propose [path] to scan indexed source."
            } else {
                "No proposals match this filter\n\nRun :filter to clear it."
            })
            .style(Style::default().fg(MUTED))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let mut visible_rows = Vec::new();
    let mut used_height = 0;
    for (index, filtered_index) in app.filtered.iter().enumerate().skip(app.list_offset) {
        let row_height = row_heights[index].min(viewport_height);
        if !visible_rows.is_empty() && used_height + row_height > viewport_height {
            break;
        }
        visible_rows.push((index, *filtered_index, used_height, row_height));
        used_height += row_height;
        if used_height >= viewport_height {
            break;
        }
    }
    app.list_rows = visible_rows.len().max(1);

    for (index, filtered_index, row_y, row_height) in visible_rows {
        let item = &app.reviews[filtered_index];
        let selected = index == app.selected;
        let (text, flagged) = review_row_text(item, inner.width as usize, selected);
        let row_area = Rect::new(
            inner.x,
            inner.y + row_y as u16,
            inner.width,
            row_height as u16,
        );
        app.hits.push(ButtonHit {
            area: row_area,
            action: HitAction::Review(index),
        });
        frame.render_widget(
            Paragraph::new(text.join("\n")).style(
                Style::default()
                    .fg(if selected {
                        FG
                    } else if flagged {
                        WARNING
                    } else {
                        MUTED
                    })
                    .bg(if selected { PANEL_HOVER } else { PANEL })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            row_area,
        );
    }
}

fn render_review_detail(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(3)])
        .split(area);
    let Some(item) = app.selected_review() else {
        frame.render_widget(panel("Proposal", false), area);
        return;
    };
    let flagged = item.flag != "clean";
    let status_color = if is_pending(item) { WARNING } else { MUTED };
    let metadata = vec![
        Line::from(vec![
            Span::styled(
                &item.proposal.original_text,
                Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ->  ", Style::default().fg(MUTED)),
            Span::styled(
                &item.proposal.sanitized_text,
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}  ", item.proposal.category),
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{:.0}% confidence  ", item.proposal.confidence * 100.0),
                Style::default().fg(PRIMARY),
            ),
            Span::styled(
                format!("{:?}", item.status).to_lowercase(),
                Style::default().fg(status_color),
            ),
        ]),
        Line::from(Span::styled(
            if flagged {
                format!("WARNING: {} | {}", item.flag, item.file)
            } else {
                format!("CHECK: clean | {}", item.file)
            },
            Style::default()
                .fg(if flagged { WARNING } else { SUCCESS })
                .add_modifier(if flagged {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        )),
        Line::from(format!(
            "REASON: {}",
            item.proposal
                .rationale
                .as_deref()
                .unwrap_or("No provider rationale.")
        )),
    ];
    frame.render_widget(
        Paragraph::new(metadata)
            .wrap(Wrap { trim: true })
            .block(panel("Proposal", false)),
        chunks[0],
    );

    let source = app.source_context();
    let lines = if source.is_empty() {
        vec![Line::from(Span::styled(
            "Source context unavailable",
            Style::default().fg(MUTED),
        ))]
    } else {
        source
            .iter()
            .map(|line| {
                source_line(
                    line.number,
                    &line.text,
                    &item.proposal.original_text,
                    line.matched,
                )
            })
            .collect()
    };
    let visible_source_rows = chunks[1].height.saturating_sub(2) as usize;
    let source_scroll = source
        .iter()
        .position(|line| line.matched)
        .map(|matched| matched.saturating_sub(visible_source_rows / 2))
        .unwrap_or(0);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((source_scroll as u16, 0))
            .block(panel("Source context", false)),
        chunks[1],
    );
}

fn render_activity(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(3)])
        .split(area);
    if let Some(job) = &app.job {
        let ratio = job
            .progress
            .and_then(|(done, total)| (total > 0).then_some(done as f64 / total as f64))
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        let elapsed = job.started.elapsed().as_secs();
        frame.render_widget(
            Gauge::default()
                .block(panel(format!("{} | {}s", job.label, elapsed), true))
                .gauge_style(
                    Style::default()
                        .fg(PRIMARY)
                        .bg(PANEL_HOVER)
                        .add_modifier(Modifier::BOLD),
                )
                .ratio(ratio)
                .label(format!("{}  {}", spinner(app.tick), job.detail)),
            chunks[0],
        );
    } else {
        frame.render_widget(
            Paragraph::new("No operation running")
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED))
                .block(panel("Activity", false)),
            chunks[0],
        );
    }
    let height = chunks[1].height.saturating_sub(2) as usize;
    let lines = app
        .logs
        .iter()
        .rev()
        .take(height)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|entry| {
            let color = match entry.level {
                LogLevel::Info => ACCENT,
                LogLevel::Success => SUCCESS,
                LogLevel::Warning => WARNING,
                LogLevel::Error => DANGER,
            };
            Line::from(vec![
                Span::styled(format!("{}  ", entry.at), Style::default().fg(MUTED)),
                Span::styled(
                    level_name(entry.level),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(&entry.message),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(panel("Event log", false)),
        chunks[1],
    );
}

fn render_workspace(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(3)])
        .split(area);
    let tracked = app.workspace.tracked_files.to_string();
    let config = app.workspace.config_path.display().to_string();
    let rows = vec![
        Row::new(vec![
            "State",
            if app.workspace.initialized {
                "ready"
            } else {
                "not initialized"
            },
        ]),
        Row::new(vec!["Mode", app.workspace.mode.as_str()]),
        Row::new(vec!["Provider", app.workspace.provider.as_str()]),
        Row::new(vec!["Tracked files", tracked.as_str()]),
        Row::new(vec!["Config", config.as_str()]),
    ];
    frame.render_widget(
        Table::new(rows, [Constraint::Length(18), Constraint::Min(20)])
            .column_spacing(2)
            .style(Style::default().fg(FG))
            .block(panel("Workspace", true)),
        chunks[0],
    );
    let commands = [
        (":init", "initialize workspace state"),
        (":index", "refresh deterministic mirror"),
        (":verify", "check mirror, maps and leak policy"),
        (":propose [path] -j 8", "queue model suggestions"),
        (":review all", "include resolved proposals"),
        (":filter <text>", "search the review queue"),
    ];
    let lines = commands
        .into_iter()
        .map(|(command, description)| {
            Line::from(vec![
                Span::styled(
                    format!("{command:<24}"),
                    Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
                ),
                Span::styled(description, Style::default().fg(MUTED)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(panel("Command palette", false)),
        chunks[1],
    );
}

fn render_toolbar(frame: &mut Frame, app: &mut App, area: Rect) {
    let constraints = [
        Constraint::Length(12),
        Constraint::Length(1),
        Constraint::Length(12),
        Constraint::Length(1),
        Constraint::Length(13),
        Constraint::Length(1),
        Constraint::Length(13),
        Constraint::Length(1),
        Constraint::Length(12),
        Constraint::Min(0),
    ];
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);
    let disabled = app.job.is_some();
    let selected_pending = app.selected_review().is_some_and(is_pending);
    let buttons = [
        (
            0,
            "Index",
            ButtonVariant::Secondary,
            ToolbarAction::Index,
            disabled,
        ),
        (
            2,
            "Verify",
            ButtonVariant::Secondary,
            ToolbarAction::Verify,
            disabled,
        ),
        (
            4,
            "Propose",
            ButtonVariant::Primary,
            ToolbarAction::Propose,
            disabled,
        ),
        (
            6,
            "Approve",
            ButtonVariant::Success,
            ToolbarAction::Approve,
            disabled || !selected_pending,
        ),
        (
            8,
            "Reject",
            ButtonVariant::Destructive,
            ToolbarAction::Reject,
            disabled || !selected_pending,
        ),
    ];
    for (index, label, variant, action, button_disabled) in buttons {
        let hovered = contains(chunks[index], app.mouse.0, app.mouse.1);
        draw_button(
            frame,
            chunks[index],
            label,
            variant,
            hovered,
            button_disabled,
        );
        if !button_disabled {
            app.hits.push(ButtonHit {
                area: chunks[index],
                action: HitAction::Toolbar(action),
            });
        }
    }
}

fn render_command(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.command_mode;
    let text = if focused {
        format!(":{}", app.command)
    } else {
        "Type : for commands or / to filter".to_string()
    };
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(if focused { FG } else { MUTED }))
            .block(panel(
                if focused {
                    "Command"
                } else {
                    "Command palette"
                },
                focused,
            )),
        area,
    );
    app.hits.push(ButtonHit {
        area,
        action: HitAction::CommandFocus,
    });
    if focused {
        let cursor_x = area
            .x
            .saturating_add(2)
            .saturating_add(app.command.chars().count() as u16);
        frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(2)), area.y + 1));
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let (message, color) = app
        .logs
        .back()
        .map(|entry| {
            let color = match entry.level {
                LogLevel::Info => ACCENT,
                LogLevel::Success => SUCCESS,
                LogLevel::Warning => WARNING,
                LogLevel::Error => DANGER,
            };
            (format!(" {} | {}", entry.at, entry.message), color)
        })
        .unwrap_or_else(|| (" Ready".to_string(), MUTED));
    let shortcuts = " ?:help  Tab:views  i:index  v:verify  p:propose  q:quit ";
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(20),
            Constraint::Length(shortcuts.len() as u16),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(message).style(Style::default().fg(color).bg(BG)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(shortcuts)
            .alignment(Alignment::Right)
            .style(Style::default().fg(MUTED).bg(BG)),
        chunks[1],
    );
}

fn render_command_suggestions(frame: &mut Frame, app: &App, input: Rect) {
    let suggestions = app.command_suggestions();
    if suggestions.is_empty() || input.y < 3 {
        return;
    }
    let height = suggestions.len() as u16 + 2;
    let area = Rect::new(
        input.x,
        input.y.saturating_sub(height),
        34.min(input.width),
        height,
    );
    frame.render_widget(ratatui::widgets::Clear, area);
    let lines = suggestions
        .iter()
        .map(|command| {
            Line::from(Span::styled(
                format!(" :{command}"),
                Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(panel("Suggestions | Tab completes", true)),
        area,
    );
}

fn render_help(frame: &mut Frame, app: &mut App, area: Rect) {
    app.hits.clear();
    render_modal_backdrop(frame, area);
    let popup_area = centered(area, 72, 22);
    let inner = popup(frame, popup_area, "Keyboard and mouse");
    let lines = vec![
        Line::from(vec![Span::styled(
            "Navigation",
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        )]),
        Line::from("  j/k or arrows     select proposal"),
        Line::from("  PgUp/PgDn         move one page"),
        Line::from("  Tab                switch view"),
        Line::from("  mouse click/scroll select, switch, and run actions"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Actions",
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        )]),
        Line::from("  i index    v verify    p propose"),
        Line::from("  a approve  r reject    / filter"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Command palette",
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        )]),
        Line::from("  : opens input      Tab completes      Up/Down history"),
        Line::from("  propose [path] -j N | review [all] | filter <text>"),
        Line::from("  tab review|activity|workspace | refresh | clear | quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Press Esc, Enter, or ? to close",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_confirmation(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(confirmation) = app.confirmation.as_ref() else {
        return;
    };
    app.hits.clear();
    render_modal_backdrop(frame, area);
    let popup_area = centered(area, 76, 13);
    let inner = popup(frame, popup_area, &confirmation.title);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(confirmation.message.as_str())
            .style(Style::default().fg(FG))
            .wrap(Wrap { trim: true }),
        chunks[0],
    );
    let buttons = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(15),
            Constraint::Length(2),
            Constraint::Length(15),
        ])
        .split(chunks[1]);
    let cancel_hover = contains(buttons[1], app.mouse.0, app.mouse.1);
    let confirm_hover = contains(buttons[3], app.mouse.0, app.mouse.1);
    draw_button(
        frame,
        buttons[1],
        "Cancel [n]",
        ButtonVariant::Ghost,
        cancel_hover,
        false,
    );
    draw_button(
        frame,
        buttons[3],
        "Confirm [y]",
        ButtonVariant::Primary,
        confirm_hover,
        false,
    );
    app.hits.push(ButtonHit {
        area: buttons[1],
        action: HitAction::Cancel,
    });
    app.hits.push(ButtonHit {
        area: buttons[3],
        action: HitAction::Confirm,
    });
}

fn render_too_small(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(
            "code-sanity\n\nTerminal is too small. Resize to at least 72x20.\n\nPress q to quit.",
        )
        .alignment(Alignment::Center)
        .style(Style::default().fg(FG).bg(BG))
        .block(panel("Interactive workspace", true)),
        area,
    );
}

fn render_modal_backdrop(frame: &mut Frame, area: Rect) {
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
}

fn source_line(number: usize, text: &str, needle: &str, matched: bool) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{number:>5}  "),
        Style::default().fg(MUTED),
    )];
    if let Some(start) = text.find(needle) {
        let end = start + needle.len();
        spans.push(Span::styled(
            text[..start].to_string(),
            Style::default().fg(if matched { FG } else { MUTED }),
        ));
        spans.push(Span::styled(
            needle.to_string(),
            Style::default().fg(WARNING).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            text[end..].to_string(),
            Style::default().fg(if matched { FG } else { MUTED }),
        ));
    } else {
        spans.push(Span::styled(text.to_string(), Style::default().fg(MUTED)));
    }
    Line::from(spans).style(Style::default().bg(if matched { PANEL_HOVER } else { PANEL }))
}

fn pending_count(app: &App) -> usize {
    app.reviews.iter().filter(|item| is_pending(item)).count()
}

fn spinner(tick: usize) -> &'static str {
    ["|", "/", "-", "\\"][tick % 4]
}

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "INFO ",
        LogLevel::Success => "OK   ",
        LogLevel::Warning => "WARN ",
        LogLevel::Error => "ERROR",
    }
}

fn wrap_with_prefix(
    value: &str,
    first_prefix: &str,
    next_prefix: &str,
    width: usize,
) -> Vec<String> {
    let first_width = width.saturating_sub(first_prefix.chars().count()).max(1);
    let next_width = width.saturating_sub(next_prefix.chars().count()).max(1);
    let mut remaining = value.chars().peekable();
    let mut lines = Vec::new();
    let mut first = true;

    while remaining.peek().is_some() {
        let (prefix, content_width) = if first {
            (first_prefix, first_width)
        } else {
            (next_prefix, next_width)
        };
        let chunk = remaining.by_ref().take(content_width).collect::<String>();
        lines.push(format!("{prefix}{chunk}"));
        first = false;
    }

    if lines.is_empty() {
        lines.push(first_prefix.to_string());
    }
    lines
}

fn review_row_text(
    item: &crate::proposal::ReviewItem,
    width: usize,
    selected: bool,
) -> (Vec<String>, bool) {
    let flagged = item.flag != "clean";
    let marker = if selected { ">" } else { " " };
    let warning = if flagged { "!" } else { " " };
    let confidence = (item.proposal.confidence * 100.0).round() as usize;
    let inline = format!(
        "{marker}{warning} {} -> {} {confidence:>3}%",
        item.proposal.original_text, item.proposal.sanitized_text
    );
    if inline.chars().count() <= width {
        return (vec![inline], flagged);
    }

    let mut lines = wrap_with_prefix(
        &item.proposal.original_text,
        &format!("{marker}{warning} "),
        "   ",
        width,
    );
    let confidence = format!(" {confidence:>3}%");
    let sanitized_width = width.saturating_sub(confidence.chars().count());
    let mut sanitized = wrap_with_prefix(
        &item.proposal.sanitized_text,
        "   -> ",
        "      ",
        sanitized_width,
    );
    if let Some(last) = sanitized.last_mut() {
        last.push_str(&confidence);
    }
    lines.extend(sanitized);
    (lines, flagged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Action, Confirmation};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn confirmation_modal_exposes_only_modal_mouse_targets() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(temp.path());
        app.confirmation = Some(Confirmation {
            action: Action::Reject("proposal".to_string()),
            title: "Confirm".to_string(),
            message: "Confirm action".to_string(),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.hits.len(), 2);
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.action, HitAction::Confirm))
        );
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.action, HitAction::Cancel))
        );
    }

    #[test]
    fn review_rows_mark_flagged_items() {
        use crate::proposal::{Proposal, ReviewItem, ReviewStatus};
        let mut item = ReviewItem {
            id: "id".to_string(),
            file: "src/main.rs".to_string(),
            proposal: Proposal {
                category: "identifier".to_string(),
                original_text: "Trezor".to_string(),
                sanitized_text: "HardwareWallet".to_string(),
                confidence: 0.7,
                rationale: Some("API name".to_string()),
            },
            status: ReviewStatus::Pending,
            flag: "public API name".to_string(),
            created_at: String::new(),
        };
        let (flagged, warning) = review_row_text(&item, 50, false);
        assert!(warning);
        assert!(flagged[0].starts_with(" !"));

        item.flag = "clean".to_string();
        let (clean, warning) = review_row_text(&item, 50, false);
        assert!(!warning);
        assert!(clean[0].starts_with("  "));
    }

    #[test]
    fn review_rows_preserve_long_identifiers_without_ellipsis() {
        use crate::proposal::{Proposal, ReviewItem, ReviewStatus};
        let item = ReviewItem {
            id: "id".to_string(),
            file: "src/main.mm".to_string(),
            proposal: Proposal {
                category: "identifier".to_string(),
                original_text: "beginCursorSuppression".to_string(),
                sanitized_text: "beginCursorHandling".to_string(),
                confidence: 0.78,
                rationale: None,
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: String::new(),
        };

        let (wide, _) = review_row_text(&item, 64, false);
        assert_eq!(wide.len(), 1);
        assert!(wide[0].contains("beginCursorSuppression"));
        assert!(wide[0].contains("beginCursorHandling"));

        let (narrow, _) = review_row_text(&item, 42, true);
        let rendered = narrow.join("\n");
        assert!(narrow.len() >= 2);
        assert!(rendered.contains("beginCursorSuppression"));
        assert!(rendered.contains("beginCursorHandling"));
        assert!(!rendered.contains('~'));
    }
}
