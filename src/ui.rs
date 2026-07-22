//! Tiny ANSI styling layer — zero deps, honors NO_COLOR / CLICOLOR_FORCE /
//! TERM=dumb and only colors real terminals. All user-facing CLI polish goes
//! through here so output stays plain when piped (agents read us!).

use std::io::IsTerminal;
use std::sync::OnceLock;

fn detect() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
        return true;
    }
    if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Whether styled output is active for this process (cached).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(detect)
}

fn paint(code: &str, s: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}

/// Section heading: `▸ title`
pub fn heading(s: &str) -> String {
    format!("{} {}", cyan("▸"), bold(s))
}
/// Green check bullet
pub fn ok(s: &str) -> String {
    format!("{} {}", green("✓"), s)
}
/// Yellow warning bullet
pub fn warn(s: &str) -> String {
    format!("{} {}", yellow("!"), s)
}
/// Inline command highlight
pub fn cmd(s: &str) -> String {
    paint("1;36", s)
}
/// Dim bullet for a removed/changed item: `· msg`
pub fn item(s: &str) -> String {
    format!("{} {}", dim("·"), s)
}

/// Yes/no confirmation prompt (default No). Returns `false` — the safe
/// answer — whenever stdin/stdout is not a terminal, so scripted/piped runs
/// never block or accidentally destroy data.
pub fn confirm(prompt: &str) -> bool {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return false;
    }
    print!("{} {prompt} {} ", yellow("?"), dim("[y/N]"));
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "YES")
}

/// Interactive arrow-key selector over `(name, description)` items — the one
/// raw-mode primitive, so prompts never re-roll terminal handling. Returns the
/// chosen index, or None on cancel (esc/q/ctrl-c). Caller must ensure
/// stdin+stdout are terminals.
pub fn select(title: &str, items: &[(&str, &str)]) -> anyhow::Result<Option<usize>> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::crossterm::terminal;
    use std::io::Write;

    println!(
        "{}  {}",
        heading(title),
        dim("(↑/↓ move, enter select, esc cancel)")
    );

    // raw mode guard — always restored, even on error/panic-unwind via Drop
    struct Raw;
    impl Drop for Raw {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    terminal::enable_raw_mode()?;
    let _raw = Raw;

    let mut sel = 0usize;
    let mut first = true;
    loop {
        // redraw: move back up over our own lines, then repaint each cleared
        let mut out = String::new();
        if !first {
            out.push_str(&format!("\x1b[{}A", items.len()));
        }
        first = false;
        for (i, (name, desc)) in items.iter().enumerate() {
            let line = if i == sel {
                format!("{}  {}", heading(name), dim(desc))
            } else {
                format!("  {name}  {}", dim(desc))
            };
            out.push_str(&format!("\r\x1b[2K{line}\r\n"));
        }
        let mut stdout = std::io::stdout();
        stdout.write_all(out.as_bytes())?;
        stdout.flush()?;

        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => sel = (sel + items.len() - 1) % items.len(),
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => sel = (sel + 1) % items.len(),
                KeyCode::Enter => return Ok(Some(sel)),
                KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                _ => {}
            }
        }
    }
}

/// Raw-mode multi-select checklist. `items` is (name, desc, preselected);
/// returns the indices of the checked rows, or `None` on cancel (esc/ctrl-c).
/// Space toggles the cursor row, enter confirms. Same drop-guard + redraw
/// discipline as `select` — never re-roll the raw-mode handling.
pub fn multiselect(
    title: &str,
    items: &[(&str, &str, bool)],
) -> anyhow::Result<Option<Vec<usize>>> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::crossterm::terminal;
    use std::io::Write;

    println!(
        "{}  {}",
        heading(title),
        dim("(↑/↓ move, space toggle, enter confirm, esc cancel)")
    );

    struct Raw;
    impl Drop for Raw {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    terminal::enable_raw_mode()?;
    let _raw = Raw;

    let mut checked: Vec<bool> = items.iter().map(|(_, _, on)| *on).collect();
    let mut sel = 0usize;
    let mut first = true;
    loop {
        let mut out = String::new();
        if !first {
            out.push_str(&format!("\x1b[{}A", items.len()));
        }
        first = false;
        for (i, (name, desc, _)) in items.iter().enumerate() {
            let box_ = if checked[i] { "[x]" } else { "[ ]" };
            let line = if i == sel {
                format!("{} {}  {}", box_, heading(name), dim(desc))
            } else {
                format!("{box_}   {name}  {}", dim(desc))
            };
            out.push_str(&format!("\r\x1b[2K{line}\r\n"));
        }
        let mut stdout = std::io::stdout();
        stdout.write_all(out.as_bytes())?;
        stdout.flush()?;

        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => sel = (sel + items.len() - 1) % items.len(),
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => sel = (sel + 1) % items.len(),
                KeyCode::Char(' ') => checked[sel] = !checked[sel],
                KeyCode::Enter => {
                    return Ok(Some(
                        checked
                            .iter()
                            .enumerate()
                            .filter_map(|(i, &on)| on.then_some(i))
                            .collect(),
                    ));
                }
                KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // paint() branches on the cached terminal detection — in tests stdout is
    // piped, so styling is off and strings pass through unchanged.
    #[test]
    fn plain_when_not_a_terminal() {
        assert_eq!(super::bold("x"), "x");
        assert_eq!(super::ok("done"), "✓ done");
    }
}
