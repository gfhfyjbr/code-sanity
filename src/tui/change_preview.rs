use super::app::{App, SourceLine, is_pending};
use super::components::{
    ACCENT, BORDER, DANGER, FG, MUTED, PANEL, PANEL_HOVER, PRIMARY, SUCCESS, WARNING, panel,
};
use super::syntax::{SyntaxKind, SyntaxSpan, highlight_lines};
use crate::proposal::ReviewItem;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Context,
    Removed,
    Added,
}

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(item) = app.selected_review() else {
        frame.render_widget(panel("Change preview", false), area);
        let inner = area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        frame.render_widget(
            Paragraph::new("No proposal selected").style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    };

    let status = format!("{:?}", item.status).to_lowercase();
    let title = format!(
        "Change preview (local) | {} | {:.0}% {status}",
        item.file,
        item.proposal.confidence * 100.0
    );
    frame.render_widget(panel(title, false), area);
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let header_height = if inner.height >= 8 {
        4
    } else if inner.height >= 4 {
        1
    } else {
        0
    };
    let code_area = if header_height == 0 {
        inner
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(header_height), Constraint::Min(1)])
            .split(inner);
        render_metadata(frame, item, chunks[0]);
        chunks[1]
    };

    let source = app.source_context(code_area.height.max(1) as usize);
    render_source(frame, item, &source, code_area);
}

fn render_metadata(frame: &mut Frame, item: &ReviewItem, area: Rect) {
    let flagged = item.flag != "clean";
    let status_color = if is_pending(item) { WARNING } else { MUTED };
    let scope = if item.proposal.target.is_some() {
        "symbol scope"
    } else {
        "global alias"
    };
    let metadata = vec![
        Line::from(vec![
            Span::styled(
                item.proposal.original_text.clone(),
                Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ->  ", Style::default().fg(MUTED)),
            Span::styled(
                item.proposal.sanitized_text.clone(),
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}  ", item.proposal.category),
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{:.0}%  ", item.proposal.confidence * 100.0),
                Style::default().fg(PRIMARY),
            ),
            Span::styled(
                format!("{:?}  ", item.status).to_lowercase(),
                Style::default().fg(status_color),
            ),
            Span::styled(format!("{scope}  "), Style::default().fg(MUTED)),
            Span::styled(
                if flagged {
                    format!("WARNING: {}", item.flag)
                } else {
                    "CHECK: clean".to_string()
                },
                Style::default()
                    .fg(if flagged { WARNING } else { SUCCESS })
                    .add_modifier(if flagged {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]),
        Line::from(Span::styled(
            format!(
                "REASON: {}",
                item.proposal
                    .rationale
                    .as_deref()
                    .unwrap_or("No provider rationale.")
            ),
            Style::default().fg(MUTED),
        )),
    ];
    let paragraph = Paragraph::new(metadata);
    if area.height >= 4 {
        frame.render_widget(
            paragraph.block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(BORDER)),
            ),
            area,
        );
    } else {
        frame.render_widget(paragraph, area);
    }
}

fn render_source(frame: &mut Frame, item: &ReviewItem, source: &[SourceLine], area: Rect) {
    if source.is_empty() {
        frame.render_widget(
            Paragraph::new("Source context unavailable").style(Style::default().fg(MUTED)),
            area,
        );
        return;
    }

    let original_text = source
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>();
    let revised_text = source
        .iter()
        .map(|line| {
            if line.matched {
                replace_all(
                    &line.text,
                    &item.proposal.original_text,
                    &item.proposal.sanitized_text,
                )
            } else {
                line.text.clone()
            }
        })
        .collect::<Vec<_>>();
    let revised_refs = revised_text.iter().map(String::as_str).collect::<Vec<_>>();
    let path = Path::new(&item.file);
    let original_syntax = highlight_lines(path, &original_text);
    let revised_syntax = highlight_lines(path, &revised_refs);

    let mut rendered = Vec::with_capacity(source.len() + 1);
    let mut first_change = None;
    for (index, line) in source.iter().enumerate() {
        if line.matched && !item.proposal.original_text.is_empty() {
            first_change.get_or_insert(rendered.len());
            rendered.push(diff_line(
                line.number,
                &line.text,
                &original_syntax[index],
                DiffKind::Removed,
                &item.proposal.original_text,
            ));
            rendered.push(diff_line(
                line.number,
                &revised_text[index],
                &revised_syntax[index],
                DiffKind::Added,
                &item.proposal.sanitized_text,
            ));
        } else {
            rendered.push(diff_line(
                line.number,
                &line.text,
                &original_syntax[index],
                DiffKind::Context,
                "",
            ));
        }
    }

    let visible_rows = area.height.max(1) as usize;
    let scroll = first_change
        .map(|index| index.saturating_sub(visible_rows.saturating_sub(2) / 2))
        .unwrap_or(0)
        .min(rendered.len().saturating_sub(visible_rows));
    frame.render_widget(Paragraph::new(rendered).scroll((scroll as u16, 0)), area);
}

fn replace_all(text: &str, original: &str, replacement: &str) -> String {
    if original.is_empty() {
        text.to_string()
    } else {
        text.replace(original, replacement)
    }
}

fn diff_line(
    number: usize,
    text: &str,
    syntax: &[SyntaxSpan],
    kind: DiffKind,
    emphasized: &str,
) -> Line<'static> {
    let (marker, marker_color, background) = match kind {
        DiffKind::Context => (' ', MUTED, PANEL),
        DiffKind::Removed => ('-', DANGER, PANEL_HOVER),
        DiffKind::Added => ('+', SUCCESS, PANEL_HOVER),
    };
    let mut spans = vec![Span::styled(
        format!("{marker} {number:>4} | "),
        Style::default()
            .fg(marker_color)
            .add_modifier(if kind == DiffKind::Context {
                Modifier::empty()
            } else {
                Modifier::BOLD
            }),
    )];
    spans.extend(code_spans(text, syntax, emphasized, marker_color));
    Line::from(spans).style(Style::default().bg(background))
}

fn code_spans(
    text: &str,
    syntax: &[SyntaxSpan],
    emphasized: &str,
    emphasized_color: Color,
) -> Vec<Span<'static>> {
    let matches = if emphasized.is_empty() {
        Vec::new()
    } else {
        text.match_indices(emphasized)
            .map(|(start, value)| start..start + value.len())
            .collect::<Vec<_>>()
    };
    let mut boundaries = vec![0, text.len()];
    boundaries.extend(
        syntax
            .iter()
            .filter(|span| span.start <= span.end && span.end <= text.len())
            .flat_map(|span| [span.start, span.end]),
    );
    boundaries.extend(matches.iter().flat_map(|range| [range.start, range.end]));
    boundaries.retain(|boundary| text.is_char_boundary(*boundary));
    boundaries.sort_unstable();
    boundaries.dedup();

    boundaries
        .windows(2)
        .filter_map(|boundary| {
            let start = boundary[0];
            let end = boundary[1];
            if start == end {
                return None;
            }
            let highlighted_match = matches
                .iter()
                .any(|range| start >= range.start && end <= range.end);
            let kind = syntax
                .iter()
                .find(|span| start >= span.start && end <= span.end)
                .map(|span| span.kind)
                .unwrap_or(SyntaxKind::Plain);
            let style = if highlighted_match {
                Style::default()
                    .fg(emphasized_color)
                    .add_modifier(Modifier::BOLD)
            } else {
                syntax_style(kind)
            };
            Some(Span::styled(text[start..end].to_string(), style))
        })
        .collect()
}

fn syntax_style(kind: SyntaxKind) -> Style {
    match kind {
        SyntaxKind::Plain | SyntaxKind::Variable => Style::default().fg(FG),
        SyntaxKind::Comment => Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        SyntaxKind::Keyword => Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        SyntaxKind::String => Style::default().fg(SUCCESS),
        SyntaxKind::Number | SyntaxKind::Constant => Style::default().fg(WARNING),
        SyntaxKind::Type => Style::default().fg(ACCENT),
        SyntaxKind::Function => Style::default().fg(PRIMARY),
        SyntaxKind::Property | SyntaxKind::Tag => Style::default().fg(ACCENT),
        SyntaxKind::Operator => Style::default().fg(FG).add_modifier(Modifier::BOLD),
        SyntaxKind::Punctuation => Style::default().fg(MUTED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proposal::{Proposal, ReviewStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn review_item() -> ReviewItem {
        ReviewItem {
            id: "id".to_string(),
            file: "main.rs".to_string(),
            proposal: Proposal {
                target: None,
                category: "identifier".to_string(),
                original_text: "hwid".to_string(),
                sanitized_text: "device_id".to_string(),
                confidence: 0.9,
                rationale: Some("replace a sensitive identifier".to_string()),
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: String::new(),
        }
    }

    #[test]
    fn change_rows_keep_syntax_and_use_diff_colors() {
        let old = "let hwid = 3;";
        let new = "let device_id = 3;";
        let old_syntax = highlight_lines(Path::new("main.rs"), &[old]);
        let new_syntax = highlight_lines(Path::new("main.rs"), &[new]);
        let removed = diff_line(7, old, &old_syntax[0], DiffKind::Removed, "hwid");
        let added = diff_line(7, new, &new_syntax[0], DiffKind::Added, "device_id");

        let keyword = removed
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "let")
            .unwrap();
        assert_eq!(keyword.style.fg, Some(PRIMARY));
        let old_target = removed
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "hwid")
            .unwrap();
        assert_eq!(old_target.style.fg, Some(DANGER));
        assert!(old_target.style.add_modifier.contains(Modifier::BOLD));
        let new_target = added
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "device_id")
            .unwrap();
        assert_eq!(new_target.style.fg, Some(SUCCESS));
        assert!(new_target.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn preview_replaces_every_occurrence_on_the_changed_line() {
        assert_eq!(
            replace_all("hwid == hwid", "hwid", "device_id"),
            "device_id == device_id"
        );
        assert_eq!(replace_all("unchanged", "", "value"), "unchanged");
    }

    #[test]
    fn component_renders_full_height_patch_preview() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("main.rs"),
            "one\ntwo\nthree\nfour\nfive\nlet hwid = 3;\nseven\neight\nnine\nten\n",
        )
        .unwrap();
        let mut app = App::new(temp.path());
        app.reviews = vec![review_item()];
        app.filtered = vec![0];

        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| render(frame, &app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                rendered.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            rendered.push('\n');
        }

        assert!(rendered.contains("Change preview"));
        assert!(rendered.contains("-    6 | let hwid = 3;"));
        assert!(rendered.contains("+    6 | let device_id = 3;"));
        assert!(rendered.contains("ten"));

        let mut compact = Terminal::new(TestBackend::new(72, 4)).unwrap();
        compact
            .draw(|frame| render(frame, &app, frame.area()))
            .unwrap();
        let buffer = compact.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                rendered.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            rendered.push('\n');
        }
        assert!(rendered.contains("-    6 | let hwid = 3;"));
        assert!(rendered.contains("+    6 | let device_id = 3;"));
    }
}
