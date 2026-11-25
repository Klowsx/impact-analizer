use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};

use console::{Key, Term, style};
use laine_core::workspace::DiscoveredRepo;

const MAX_VISIBLE_ITEMS: usize = 16;

/// Pure state machine for the interactive multi-select repository picker.
#[derive(Debug, Clone)]
pub struct SelectorState {
    pub cursor: usize,
    pub selected: Vec<bool>,
    pub scroll_offset: usize,
}

impl SelectorState {
    pub fn new(len: usize, preselected: &[bool]) -> Self {
        let mut selected = vec![false; len];
        for (i, flag) in preselected.iter().copied().enumerate().take(len) {
            selected[i] = flag;
        }
        Self {
            cursor: 0,
            selected,
            scroll_offset: 0,
        }
    }

    pub fn move_up(&mut self) {
        if self.selected.is_empty() {
            return;
        }
        if self.cursor == 0 {
            self.cursor = self.selected.len() - 1;
        } else {
            self.cursor -= 1;
        }
        self.sync_scroll();
    }

    pub fn move_down(&mut self) {
        if self.selected.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.selected.len();
        self.sync_scroll();
    }

    pub fn toggle_current(&mut self) {
        if let Some(flag) = self.selected.get_mut(self.cursor) {
            *flag = !*flag;
        }
    }

    pub fn toggle_all(&mut self) {
        let all_selected = self.selected.iter().all(|&b| b);
        self.selected.fill(!all_selected);
    }

    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|&&b| b).count()
    }

    fn sync_scroll(&mut self) {
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        } else if self.cursor >= self.scroll_offset + MAX_VISIBLE_ITEMS {
            self.scroll_offset = self.cursor + 1 - MAX_VISIBLE_ITEMS;
        }
    }
}

/// Prompts the user to select repositories from `discovered`.
///
/// - When attached to an interactive terminal (TTY), renders a live checkbox list
///   supporting `↑/↓`, `j/k` (Vim), and `w/s` navigation (for compact 60% keyboards),
///   `Space` to toggle, `a` to toggle all, `Enter` to confirm, and `Esc`/`q` to cancel.
/// - When stdin/stdout is not a TTY (pipes, CI, AI agents), falls back to reading
///   comma-separated numbers, repository names, or `all` from stdin.
pub fn select_repositories(
    discovered: &[DiscoveredRepo],
    preselected_names: &HashSet<String>,
    title: &str,
) -> Result<Vec<DiscoveredRepo>, String> {
    if discovered.is_empty() {
        return Ok(Vec::new());
    }

    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        run_interactive_multiselect(discovered, preselected_names, title)
    } else {
        run_line_prompt_fallback(discovered, preselected_names, title)
    }
}

fn run_interactive_multiselect(
    discovered: &[DiscoveredRepo],
    preselected_names: &HashSet<String>,
    title: &str,
) -> Result<Vec<DiscoveredRepo>, String> {
    let preselected: Vec<bool> = discovered
        .iter()
        .map(|r| preselected_names.contains(&r.name))
        .collect();
    let mut state = SelectorState::new(discovered.len(), &preselected);

    let term = Term::stderr();
    let _ = term.hide_cursor();

    let mut rendered_lines = 0usize;
    let result = loop {
        if rendered_lines > 0 {
            let _ = term.clear_last_lines(rendered_lines);
        }
        rendered_lines = render_frame(&term, title, discovered, &state)
            .map_err(|e| format!("Failed to render selector: {e}"))?;

        let key = term
            .read_key()
            .map_err(|e| format!("Failed to read key input: {e}"))?;
        match key {
            Key::ArrowUp | Key::Char('k') | Key::Char('K') | Key::Char('w') | Key::Char('W') => {
                state.move_up();
            }
            Key::ArrowDown | Key::Char('j') | Key::Char('J') | Key::Char('s') | Key::Char('S') => {
                state.move_down();
            }
            Key::Char(' ') => {
                state.toggle_current();
            }
            Key::Char('a') | Key::Char('A') => {
                state.toggle_all();
            }
            Key::Enter => {
                let picked: Vec<DiscoveredRepo> = discovered
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| state.selected[*i])
                    .map(|(_, repo)| repo.clone())
                    .collect();
                break Ok(picked);
            }
            Key::Escape | Key::Char('q') | Key::Char('Q') => {
                break Err("Selection cancelled.".to_string());
            }
            _ => {}
        }
    };

    if rendered_lines > 0 {
        let _ = term.clear_last_lines(rendered_lines);
    }
    let _ = term.show_cursor();
    result
}

fn render_frame(
    term: &Term,
    title: &str,
    discovered: &[DiscoveredRepo],
    state: &SelectorState,
) -> io::Result<usize> {
    let mut lines = 0usize;
    term.write_line(&format!(
        "{} ({} selected / {} found)",
        style(title).bold(),
        style(state.selected_count()).cyan().bold(),
        discovered.len()
    ))?;
    lines += 1;

    term.write_line(&format!(
        "{}",
        style("↑/↓ or j/k or w/s: move  ·  Space: toggle  ·  a: all  ·  Enter: confirm  ·  Esc/q: cancel")
            .dim()
    ))?;
    lines += 1;

    let end = (state.scroll_offset + MAX_VISIBLE_ITEMS).min(discovered.len());
    if state.scroll_offset > 0 {
        term.write_line(&format!(
            "  {}",
            style(format!("... {} more above", state.scroll_offset)).dim()
        ))?;
        lines += 1;
    }

    for (idx, repo) in discovered
        .iter()
        .enumerate()
        .take(end)
        .skip(state.scroll_offset)
    {
        let is_cursor = idx == state.cursor;
        let is_checked = state.selected[idx];
        let pointer = if is_cursor {
            style(">").cyan().bold().to_string()
        } else {
            " ".to_string()
        };
        let checkbox = if is_checked {
            style("[x]").green().bold().to_string()
        } else {
            style("[ ]").dim().to_string()
        };
        let label = if is_cursor {
            style(&repo.name).bold().to_string()
        } else {
            repo.name.clone()
        };
        let kind = style(format!("({})", repo.detected_kind)).dim();
        term.write_line(&format!(" {pointer} {checkbox} {label} {kind}"))?;
        lines += 1;
    }

    if end < discovered.len() {
        term.write_line(&format!(
            "  {}",
            style(format!("... {} more below", discovered.len() - end)).dim()
        ))?;
        lines += 1;
    }

    Ok(lines)
}

fn run_line_prompt_fallback(
    discovered: &[DiscoveredRepo],
    preselected_names: &HashSet<String>,
    title: &str,
) -> Result<Vec<DiscoveredRepo>, String> {
    println!("{title} ({} found):", discovered.len());
    for (idx, repo) in discovered.iter().enumerate() {
        let mark = if preselected_names.contains(&repo.name) {
            "x"
        } else {
            " "
        };
        println!(
            "  [{}] ({}) {} ({})",
            mark,
            idx + 1,
            repo.name,
            repo.detected_kind
        );
    }
    print!("Select repositories (comma-separated numbers/names, or 'all'): ");
    let _ = io::stdout().flush();

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("Failed to read selection: {e}"))?;
    let trimmed = line.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(discovered.to_vec());
    }

    let mut selected = Vec::new();
    for token in trimmed.split(',') {
        let item = token.trim();
        if item.is_empty() {
            continue;
        }
        if let Ok(num) = item.parse::<usize>() {
            if num == 0 || num > discovered.len() {
                return Err(format!(
                    "Selection index {num} is out of range (1..={})",
                    discovered.len()
                ));
            }
            let repo = &discovered[num - 1];
            if !selected
                .iter()
                .any(|r: &DiscoveredRepo| r.name == repo.name)
            {
                selected.push(repo.clone());
            }
        } else if let Some(repo) = discovered.iter().find(|r| r.name == item) {
            if !selected
                .iter()
                .any(|r: &DiscoveredRepo| r.name == repo.name)
            {
                selected.push(repo.clone());
            }
        } else {
            return Err(format!("Unknown repository selection: '{item}'"));
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_state_moves_wraps_toggles_and_respects_preselected_defaults() {
        let mut state = SelectorState::new(3, &[false, true, false]);
        assert_eq!(state.cursor, 0);
        assert_eq!(state.selected_count(), 1);
        assert_eq!(state.selected, vec![false, true, false]);

        // Wrapping upward from 0 goes to last item (2)
        state.move_up();
        assert_eq!(state.cursor, 2);
        state.toggle_current();
        assert_eq!(state.selected, vec![false, true, true]);
        assert_eq!(state.selected_count(), 2);

        // Wrapping downward from last item goes back to 0
        state.move_down();
        assert_eq!(state.cursor, 0);

        // Toggle all selects everything, toggling again clears everything
        state.toggle_all();
        assert_eq!(state.selected, vec![true, true, true]);
        state.toggle_all();
        assert_eq!(state.selected, vec![false, false, false]);
    }
}
