use std::io::{self, IsTerminal};
use std::path::PathBuf;

use console::{Key, Term, style};
use laine_core::workspace::{DiscoveredRepo, same_repository_path};

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

/// Comprueba la entrada y la salida que usa el selector antes de descubrir repositorios.
pub fn ensure_selection_terminal() -> Result<(), &'static str> {
    if !io::stdin().is_terminal() || !Term::stderr().is_term() {
        return Err(
            "Repository selection requires an interactive terminal. Pass --select <repo1,repo2> or --all.",
        );
    }
    Ok(())
}

/// Show the repository picker with the configured repository paths preselected.
pub fn select_repositories(
    discovered: &[DiscoveredRepo],
    preselected_paths: &[PathBuf],
    title: &str,
) -> Result<Vec<DiscoveredRepo>, String> {
    if discovered.is_empty() {
        return Ok(Vec::new());
    }

    let preselected: Vec<bool> = discovered
        .iter()
        .map(|repo| {
            preselected_paths
                .iter()
                .any(|path| same_repository_path(path, &repo.path))
        })
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

// Render the visible choices with paths for repositories whose labels collide.
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
        let path = if discovered
            .iter()
            .filter(|candidate| candidate.name == repo.name)
            .count()
            > 1
        {
            format!(" [{}]", repo.path.display())
        } else {
            String::new()
        };
        let kind = style(format!("({})", repo.detected_kind)).dim();
        term.write_line(&format!(" {pointer} {checkbox} {label} {kind}{path}"))?;
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
