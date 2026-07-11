use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io::{self, IsTerminal};
use std::time::Duration;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const MUTED: &str = "\x1b[38;2;146;150;161m";
const PRIMARY: &str = "\x1b[38;2;168;177;255m";
const SUCCESS: &str = "\x1b[38;2;61;214;140m";
const WARNING: &str = "\x1b[38;2;249;180;78m";
const DANGER: &str = "\x1b[38;2;255;99;105m";

pub fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

pub struct TaskProgress {
    progress: Option<ProgressBar>,
}

impl TaskProgress {
    pub fn start(message: impl Into<String>, enabled: bool) -> Self {
        if !enabled || !io::stderr().is_terminal() {
            return Self { progress: None };
        }
        let progress = ProgressBar::new_spinner();
        progress.set_draw_target(ProgressDrawTarget::stderr());
        progress.set_style(
            ProgressStyle::with_template("{spinner:.magenta} {msg:.bold}  {elapsed_precise}")
                .expect("static progress template")
                .tick_strings(&["|", "/", "-", "\\"]),
        );
        progress.set_message(message.into());
        progress.enable_steady_tick(Duration::from_millis(90));
        Self {
            progress: Some(progress),
        }
    }

    pub fn set_message(&self, message: impl Into<String>) {
        if let Some(progress) = &self.progress {
            progress.set_message(message.into());
        }
    }

    pub fn finish(mut self) {
        if let Some(progress) = self.progress.take() {
            progress.finish_and_clear();
        }
    }
}

impl Drop for TaskProgress {
    fn drop(&mut self) {
        if let Some(progress) = self.progress.take() {
            progress.finish_and_clear();
        }
    }
}

pub fn summary(title: &str, rows: &[(&str, String)]) -> bool {
    let table_rows = rows
        .iter()
        .map(|(key, value)| vec![(*key).to_string(), value.clone()])
        .collect::<Vec<_>>();
    table(title, &["Metric", "Value"], &table_rows)
}

pub fn table(title: &str, headers: &[&str], rows: &[Vec<String>]) -> bool {
    table_with_minimums(title, headers, rows, &[])
}

pub fn table_with_minimums(
    title: &str,
    headers: &[&str],
    rows: &[Vec<String>],
    minimums: &[usize],
) -> bool {
    if !stdout_is_tty() {
        return false;
    }
    let columns = headers.len();
    if columns == 0 {
        return true;
    }
    let mut widths = headers
        .iter()
        .map(|header| display_width(header).max(4))
        .collect::<Vec<_>>();
    for row in rows {
        for (column, cell) in row.iter().take(columns).enumerate() {
            widths[column] = widths[column].max(display_width(cell).min(48));
        }
    }
    let terminal_width = crossterm::terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(100)
        .max(40);
    let border_cost = columns + 1 + columns * 2;
    let minimum_width = (0..columns)
        .map(|index| minimums.get(index).copied().unwrap_or(8).max(4))
        .sum::<usize>()
        + border_cost;
    if minimum_width > terminal_width {
        return false;
    }
    while widths.iter().sum::<usize>() + border_cost > terminal_width {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(index, width)| {
                let minimum = minimums.get(*index).copied().unwrap_or(8).max(4);
                **width > minimum
            })
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        widths[index] -= 1;
    }

    let line = widths
        .iter()
        .map(|width| "─".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("┬");
    println!("{PRIMARY}╭{line}╮{RESET}");
    let table_width = widths.iter().sum::<usize>() + border_cost;
    let title_width = table_width.saturating_sub(4);
    println!(
        "{PRIMARY}│{RESET} {BOLD}{:<title_width$}{RESET} {PRIMARY}│{RESET}",
        clip(title, title_width)
    );
    print_separator(&widths, "├", "┼", "┤");
    print_row(
        &headers
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>(),
        &widths,
        BOLD,
    );
    print_separator(&widths, "├", "┼", "┤");
    for row in rows {
        print_row(row, &widths, "");
    }
    let bottom = widths
        .iter()
        .map(|width| "─".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("┴");
    println!("{PRIMARY}╰{bottom}╯{RESET}");
    true
}

pub fn review_queue(items: &[crate::proposal::ReviewItem]) -> bool {
    if !stdout_is_tty() {
        return false;
    }
    let width = crossterm::terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(100)
        .clamp(40, 120);
    let inner = width.saturating_sub(4);
    println!("{PRIMARY}╭{}╮{RESET}", "─".repeat(width - 2));
    print_review_line(
        &format!("Review queue | {} item(s)", items.len()),
        ReviewTone::Heading,
        inner,
    );
    println!("{PRIMARY}├{}┤{RESET}", "─".repeat(width - 2));
    if items.is_empty() {
        print_review_line("No proposals waiting for review.", ReviewTone::Muted, inner);
    }
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            println!("{PRIMARY}├{}┤{RESET}", "─".repeat(width - 2));
        }
        for (tone, line) in review_item_lines(item, inner) {
            print_review_line(&line, tone, inner);
        }
    }
    println!("{PRIMARY}╰{}╯{RESET}", "─".repeat(width - 2));
    true
}

pub fn success(message: &str) {
    if stdout_is_tty() {
        println!("{SUCCESS}ok{RESET}  {message}");
    } else {
        println!("{message}");
    }
}

fn print_row(row: &[String], widths: &[usize], style: &str) {
    print!("{PRIMARY}│{RESET}");
    for (index, width) in widths.iter().enumerate() {
        let value = row.get(index).map(String::as_str).unwrap_or("");
        let clipped = clip(value, *width);
        print!(" {style}{clipped:<width$}{RESET} {PRIMARY}│{RESET}");
    }
    println!();
}

fn print_separator(widths: &[usize], left: &str, middle: &str, right: &str) {
    let line = widths
        .iter()
        .map(|width| "─".repeat(width + 2))
        .collect::<Vec<_>>()
        .join(middle);
    println!("{PRIMARY}{left}{line}{right}{RESET}");
}

fn clip(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        return value.to_string();
    }
    let keep = width.saturating_sub(1);
    format!("{}~", value.chars().take(keep).collect::<String>())
}

fn display_width(value: &str) -> usize {
    value.chars().count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewTone {
    Heading,
    Proposal,
    Warning,
    Danger,
    Muted,
}

fn review_item_lines(
    item: &crate::proposal::ReviewItem,
    width: usize,
) -> Vec<(ReviewTone, String)> {
    let mut lines = vec![
        (
            ReviewTone::Heading,
            format!(
                "{}  {}  {:.0}%",
                item.id,
                format!("{:?}", item.status).to_lowercase(),
                item.proposal.confidence * 100.0
            ),
        ),
        (
            ReviewTone::Proposal,
            format!(
                "{} -> {}",
                item.proposal.original_text, item.proposal.sanitized_text
            ),
        ),
        (
            ReviewTone::Muted,
            format!("FILE: {}  CATEGORY: {}", item.file, item.proposal.category),
        ),
    ];
    if item.flag != "clean" {
        lines.extend(
            wrap_labeled("WARNING: ", &item.flag, width)
                .into_iter()
                .map(|line| (ReviewTone::Danger, line)),
        );
    } else {
        lines.push((ReviewTone::Warning, "CHECK: clean".to_string()));
    }
    let rationale = item
        .proposal
        .rationale
        .as_deref()
        .unwrap_or("No provider rationale.");
    lines.extend(
        wrap_labeled("REASON: ", rationale, width)
            .into_iter()
            .map(|line| (ReviewTone::Warning, line)),
    );
    lines
}

fn wrap_labeled(label: &str, value: &str, width: usize) -> Vec<String> {
    let continuation = " ".repeat(label.chars().count());
    let mut lines = Vec::new();
    let mut current = label.to_string();
    for word in value.split_whitespace() {
        let separator = usize::from(current.chars().count() > label.chars().count());
        if current.chars().count() + separator + word.chars().count() > width
            && current.chars().count() > label.chars().count()
        {
            lines.push(current);
            current = continuation.clone();
        }
        if current.chars().count() > continuation.chars().count() && !current.ends_with(' ') {
            current.push(' ');
        }
        current.push_str(word);
    }
    if current.trim() == label.trim() {
        current.push_str("(none)");
    }
    lines.push(current);
    lines
}

fn print_review_line(value: &str, tone: ReviewTone, width: usize) {
    let color = match tone {
        ReviewTone::Heading => BOLD,
        ReviewTone::Proposal => SUCCESS,
        ReviewTone::Warning => WARNING,
        ReviewTone::Danger => DANGER,
        ReviewTone::Muted => MUTED,
    };
    let value = clip(value, width);
    println!("{PRIMARY}│{RESET} {color}{value:<width$}{RESET} {PRIMARY}│{RESET}");
}

pub fn muted(value: impl AsRef<str>) -> String {
    if stdout_is_tty() {
        format!("{MUTED}{}{RESET}", value.as_ref())
    } else {
        value.as_ref().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proposal::{Proposal, ReviewItem, ReviewStatus};

    #[test]
    fn clip_keeps_table_width_stable() {
        assert_eq!(clip("abcdefgh", 5), "abcd~");
        assert_eq!(clip("abc", 5), "abc");
    }

    #[test]
    fn review_lines_keep_warning_reason_and_full_id() {
        let item = ReviewItem {
            id: "2026-07-11T02-48-35.382110000Z-573f7bc8".to_string(),
            file: "src/client/api.mm".to_string(),
            proposal: Proposal {
                target: None,
                category: "identifier".to_string(),
                original_text: "Trezor".to_string(),
                sanitized_text: "HardwareWallet".to_string(),
                confidence: 0.72,
                rationale: Some("Public vendor API name; replacement may break calls.".to_string()),
            },
            status: ReviewStatus::Pending,
            flag: "touches a protected name (public API or import); needs review".to_string(),
            created_at: String::new(),
        };
        let lines = review_item_lines(&item, 54)
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>();
        assert!(lines.iter().any(|line| line.contains(&item.id)));
        assert!(lines.iter().any(|line| line.starts_with("WARNING:")));
        assert!(lines.iter().any(|line| line.starts_with("REASON:")));
        assert!(lines.join(" ").contains("Public vendor API"));
    }
}
