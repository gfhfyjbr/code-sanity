use super::app::{App, HitAction, LogLevel, ProposeFocus, Tab, ToolbarAction, is_pending};
use super::change_preview;
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
    } else if app.propose_dialog.is_some() {
        render_propose_dialog(frame, app, area);
    } else if app.confirmation.is_some() {
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
    if area.width >= 96 {
        let areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(area);
        render_review_queue(frame, app, areas[0]);
        change_preview::render(frame, app, areas[1]);
    } else {
        let queue_height = area
            .height
            .saturating_mul(3)
            .div_ceil(10)
            .clamp(4, 9)
            .min(area.height.saturating_sub(3));
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(queue_height), Constraint::Min(3)])
            .split(area);
        render_review_queue(frame, app, areas[0]);
        change_preview::render(frame, app, areas[1]);
    }
}

fn render_review_queue(frame: &mut Frame, app: &mut App, area: Rect) {
    let button_height = if area.height >= 6 { 3 } else { 1 };
    let bottom_margin = u16::from(area.height >= 6);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(button_height),
            Constraint::Length(bottom_margin),
        ])
        .split(area);
    render_review_list(frame, app, areas[0]);
    render_select_all_button(frame, app, areas[1]);
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
            let item = &app.reviews[*index];
            review_row_text(
                item,
                inner.width as usize,
                false,
                app.is_review_checked(item),
            )
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
        let checked = app.is_review_checked(item);
        let (text, flagged) = review_row_text(item, inner.width as usize, selected, checked);
        let row_area = Rect::new(
            inner.x,
            inner.y + row_y as u16,
            inner.width,
            row_height as u16,
        );
        if is_pending(item) {
            app.hits.push(ButtonHit {
                area: Rect::new(
                    row_area.x,
                    row_area.y,
                    row_area.width.min(3),
                    row_area.height,
                ),
                action: HitAction::ToggleReview(index),
            });
        }
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

fn render_select_all_button(frame: &mut Frame, app: &mut App, area: Rect) {
    let all_checked = app.all_filtered_pending_checked();
    let label = if all_checked {
        "Deselect All"
    } else {
        "Select All"
    };
    let disabled = app.job.is_some() || !app.has_filtered_pending();
    let hovered = contains(area, app.mouse.0, app.mouse.1);
    if area.height >= 3 {
        draw_button(
            frame,
            area,
            label,
            ButtonVariant::Secondary,
            hovered,
            disabled,
        );
    } else {
        let style = if disabled {
            Style::default().fg(BORDER).bg(PANEL)
        } else {
            Style::default()
                .fg(if hovered { FG } else { PRIMARY })
                .bg(PANEL)
                .add_modifier(if hovered {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                })
        };
        frame.render_widget(
            Paragraph::new(format!("[ {label} ]"))
                .alignment(Alignment::Center)
                .style(style),
            area,
        );
    }
    if !disabled {
        app.hits.push(ButtonHit {
            area,
            action: HitAction::ToggleAllReviews,
        });
    }
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
        Constraint::Length(15),
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
    let checked = app.checked_pending_count();
    let approve_label = if checked == 0 {
        "Approve".to_string()
    } else {
        format!("Approve ({checked})")
    };
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
            approve_label.as_str(),
            ButtonVariant::Success,
            ToolbarAction::Approve,
            disabled || (checked == 0 && !selected_pending),
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
        Line::from("  j/k or arrows     focus proposal"),
        Line::from("  PgUp/PgDn         move one page"),
        Line::from("  Space/click [ ]   toggle approval selection"),
        Line::from("  Tab                switch view"),
        Line::from("  mouse click/scroll select, switch, and run actions"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Actions",
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        )]),
        Line::from("  i index    v verify    p proposal setup"),
        Line::from("  a approve checked (or focused)    r reject focused"),
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

fn render_propose_dialog(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(dialog) = app.propose_dialog.clone() else {
        return;
    };
    app.hits.clear();
    render_modal_backdrop(frame, area);
    let popup_area = centered(area, 82, 22);
    let inner = popup(frame, popup_area, "Configure proposal scan");

    let option_rows = if dialog.dropdown_open {
        inner
            .height
            .saturating_sub(10)
            .saturating_sub(2)
            .min(6)
            .min(dialog.scopes.len() as u16)
    } else {
        0
    };
    let option_height = if option_rows == 0 { 0 } else { option_rows + 2 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(option_height),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(inner);

    let selected_label = dialog
        .selected()
        .map(|scope| scope.label.as_str())
        .unwrap_or("Entire workspace");
    let directory_focused = dialog.focus == ProposeFocus::Directory;
    let dropdown_hovered = contains(chunks[0], app.mouse.0, app.mouse.1);
    frame.render_widget(
        Paragraph::new(format!(
            " {selected_label}  {}",
            if dialog.dropdown_open { "▲" } else { "▼" }
        ))
        .style(
            Style::default()
                .fg(if directory_focused { FG } else { MUTED })
                .bg(if dropdown_hovered { PANEL_HOVER } else { PANEL })
                .add_modifier(if directory_focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        )
        .block(
            Block::default()
                .title(" Directory scope ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if directory_focused {
                    PRIMARY
                } else {
                    BORDER
                })),
        ),
        chunks[0],
    );
    app.hits.push(ButtonHit {
        area: chunks[0],
        action: HitAction::ProposeDropdown,
    });

    if option_rows > 0 {
        let rows = option_rows as usize;
        let start = dialog
            .selected_scope
            .saturating_sub(rows / 2)
            .min(dialog.scopes.len().saturating_sub(rows));
        let lines = dialog
            .scopes
            .iter()
            .enumerate()
            .skip(start)
            .take(rows)
            .map(|(index, scope)| {
                let selected = index == dialog.selected_scope;
                Line::from(Span::styled(
                    format!("{} {}", if selected { ">" } else { " " }, scope.label),
                    Style::default()
                        .fg(if selected { FG } else { MUTED })
                        .bg(if selected { PANEL_HOVER } else { PANEL })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(PRIMARY)),
            ),
            chunks[1],
        );
        for row in 0..rows {
            let index = start + row;
            app.hits.push(ButtonHit {
                area: Rect::new(
                    chunks[1].x.saturating_add(1),
                    chunks[1].y.saturating_add(1 + row as u16),
                    chunks[1].width.saturating_sub(2),
                    1,
                ),
                action: HitAction::ProposeScope(index),
            });
        }
    }

    let endpoint_focused = dialog.focus == ProposeFocus::Endpoint;
    let endpoint_label = if dialog.endpoint_required {
        format!(
            "[{}] Allow provider endpoint (required)",
            if dialog.allow_endpoint { "X" } else { " " }
        )
    } else {
        "[ ] Allow provider endpoint (not required for this provider)".to_string()
    };
    frame.render_widget(
        Paragraph::new(endpoint_label).style(
            Style::default()
                .fg(if dialog.endpoint_required {
                    if endpoint_focused { FG } else { WARNING }
                } else {
                    MUTED
                })
                .bg(if endpoint_focused { PANEL_HOVER } else { PANEL })
                .add_modifier(if endpoint_focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        chunks[2],
    );
    if dialog.endpoint_required {
        app.hits.push(ButtonHit {
            area: chunks[2],
            action: HitAction::ProposeEndpoint,
        });
    }

    frame.render_widget(
        Paragraph::new(format!("Destination: {}", dialog.destination))
            .style(Style::default().fg(MUTED))
            .wrap(Wrap { trim: true }),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(
            "Real source in the selected scope may be sent to this provider. The provider can only queue proposals. Tab moves focus; Up/Down selects a directory; Space toggles permission.",
        )
        .style(Style::default().fg(MUTED))
        .wrap(Wrap { trim: true }),
        chunks[4],
    );

    let buttons = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(15),
            Constraint::Length(2),
            Constraint::Length(15),
        ])
        .split(chunks[5]);
    let cancel_hovered = contains(buttons[1], app.mouse.0, app.mouse.1);
    let run_hovered = contains(buttons[3], app.mouse.0, app.mouse.1);
    let run_disabled = !dialog.can_run() || app.job.is_some();
    draw_button(
        frame,
        buttons[1],
        "Cancel [n]",
        ButtonVariant::Ghost,
        cancel_hovered || dialog.focus == ProposeFocus::Cancel,
        false,
    );
    draw_button(
        frame,
        buttons[3],
        "Run [y]",
        ButtonVariant::Primary,
        run_hovered || dialog.focus == ProposeFocus::Run,
        run_disabled,
    );
    app.hits.push(ButtonHit {
        area: buttons[1],
        action: HitAction::ProposeCancel,
    });
    if !run_disabled {
        app.hits.push(ButtonHit {
            area: buttons[3],
            action: HitAction::ProposeRun,
        });
    }
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
    checked: bool,
) -> (Vec<String>, bool) {
    let flagged = item.flag != "clean";
    let checkbox = if checked { "[X]" } else { "[ ]" };
    let marker = if selected { ">" } else { " " };
    let warning = if flagged { "!" } else { " " };
    let confidence = (item.proposal.confidence * 100.0).round() as usize;
    let inline = format!(
        "{checkbox} {marker}{warning} {} -> {} {confidence:>3}%",
        item.proposal.original_text, item.proposal.sanitized_text
    );
    if inline.chars().count() <= width {
        return (vec![inline], flagged);
    }

    let first_prefix = format!("{checkbox} {marker}{warning} ");
    let continuation = " ".repeat(first_prefix.chars().count());
    let mut lines = wrap_with_prefix(
        &item.proposal.original_text,
        &first_prefix,
        &continuation,
        width,
    );
    let confidence = format!(" {confidence:>3}%");
    let sanitized_width = width.saturating_sub(confidence.chars().count());
    let sanitized_prefix = format!("{continuation}-> ");
    let sanitized_continuation = " ".repeat(sanitized_prefix.chars().count());
    let mut sanitized = wrap_with_prefix(
        &item.proposal.sanitized_text,
        &sanitized_prefix,
        &sanitized_continuation,
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
    use crate::tui::app::{Action, Confirmation, ProposeDialog, ProposeScope};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in buffer.area.y..buffer.area.bottom() {
            for x in buffer.area.x..buffer.area.right() {
                text.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            text.push('\n');
        }
        text
    }

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
    fn propose_modal_renders_scope_dropdown_and_gates_endpoint_permission() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(temp.path());
        app.propose_dialog = Some(ProposeDialog {
            scopes: vec![
                ProposeScope {
                    path: None,
                    label: "Entire workspace".to_string(),
                },
                ProposeScope {
                    path: Some("src".into()),
                    label: "src".to_string(),
                },
                ProposeScope {
                    path: Some("src/worker".into()),
                    label: "src/worker".to_string(),
                },
            ],
            selected_scope: 0,
            dropdown_open: false,
            focus: ProposeFocus::Directory,
            endpoint_required: true,
            allow_endpoint: false,
            destination: "https://provider.example/v1 using test-model".to_string(),
            jobs: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let blocked = screen_text(&terminal);
        assert!(blocked.contains("Configure proposal scan"));
        assert!(blocked.contains("Directory scope"));
        assert!(blocked.contains("Entire workspace"));
        assert!(blocked.contains("[ ] Allow provider endpoint (required)"));
        assert!(blocked.contains("provider.example"));
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.action, HitAction::ProposeDropdown))
        );
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.action, HitAction::ProposeEndpoint))
        );
        assert!(
            !app.hits
                .iter()
                .any(|hit| matches!(hit.action, HitAction::ProposeRun))
        );

        let dialog = app.propose_dialog.as_mut().unwrap();
        dialog.allow_endpoint = true;
        dialog.dropdown_open = true;
        dialog.selected_scope = 2;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let enabled = screen_text(&terminal);
        assert!(enabled.contains("[X] Allow provider endpoint (required)"));
        assert!(enabled.contains("src/worker"));
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.action, HitAction::ProposeScope(2)))
        );
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.action, HitAction::ProposeRun))
        );
    }

    #[test]
    fn review_queue_renders_checkboxes_and_swapping_select_all_button() {
        use crate::proposal::{Proposal, ReviewItem, ReviewStatus};
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(temp.path());
        app.reviews = vec![ReviewItem {
            id: "proposal".to_string(),
            file: "src/main.rs".to_string(),
            proposal: Proposal {
                target: None,
                category: "identifier".to_string(),
                original_text: "private_name".to_string(),
                sanitized_text: "neutral_name".to_string(),
                confidence: 0.91,
                rationale: None,
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: String::new(),
        }];
        app.filtered = vec![0];
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let unchecked = screen_text(&terminal);
        assert!(unchecked.contains("[ ]"));
        assert!(unchecked.contains("Select All"));
        let lines = unchecked.lines().collect::<Vec<_>>();
        let select_all = lines
            .iter()
            .position(|line| line.contains("Select All"))
            .unwrap();
        let toolbar = lines
            .iter()
            .enumerate()
            .skip(select_all + 1)
            .find_map(|(index, line)| line.contains("Index").then_some(index))
            .unwrap();
        assert!(
            lines[select_all + 1..toolbar]
                .iter()
                .any(|line| line.chars().take(36).all(char::is_whitespace)),
            "expected a blank row below the queue action"
        );
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.action, HitAction::ToggleReview(0)))
        );
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.action, HitAction::ToggleAllReviews))
        );

        app.checked_reviews.insert("proposal".to_string());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let checked = screen_text(&terminal);
        assert!(checked.contains("[X]"));
        assert!(checked.contains("Deselect All"));
        assert!(checked.contains("Approve (1)"));
    }

    #[test]
    fn review_rows_mark_flagged_items() {
        use crate::proposal::{Proposal, ReviewItem, ReviewStatus};
        let mut item = ReviewItem {
            id: "id".to_string(),
            file: "src/main.rs".to_string(),
            proposal: Proposal {
                target: None,
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
        let (flagged, warning) = review_row_text(&item, 50, false, false);
        assert!(warning);
        assert!(flagged[0].starts_with("[ ]  !"));

        item.flag = "clean".to_string();
        let (clean, warning) = review_row_text(&item, 50, false, true);
        assert!(!warning);
        assert!(clean[0].starts_with("[X]   "));
    }

    #[test]
    fn review_rows_preserve_long_identifiers_without_ellipsis() {
        use crate::proposal::{Proposal, ReviewItem, ReviewStatus};
        let item = ReviewItem {
            id: "id".to_string(),
            file: "src/main.mm".to_string(),
            proposal: Proposal {
                target: None,
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

        let (wide, _) = review_row_text(&item, 64, false, false);
        assert_eq!(wide.len(), 1);
        assert!(wide[0].contains("beginCursorSuppression"));
        assert!(wide[0].contains("beginCursorHandling"));

        let (narrow, _) = review_row_text(&item, 42, true, true);
        let rendered = narrow.join("\n");
        assert!(narrow.len() >= 2);
        assert!(narrow[0].starts_with("[X] >"));
        assert!(rendered.contains("beginCursorSuppression"));
        assert!(rendered.contains("beginCursorHandling"));
        assert!(!rendered.contains('~'));
    }
}
