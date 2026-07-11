use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io::{self, IsTerminal};
use std::time::Duration;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const MUTED: &str = "\x1b[38;2;146;150;161m";
const PRIMARY: &str = "\x1b[38;2;168;177;255m";
const SUCCESS: &str = "\x1b[38;2;61;214;140m";

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

    #[test]
    fn clip_keeps_table_width_stable() {
        assert_eq!(clip("abcdefgh", 5), "abcd~");
        assert_eq!(clip("abc", 5), "abc");
    }
}
