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
/// Top-level command banner, e.g. `cona install`. One per command, printed
/// once at the very top — gives every setup/install/upgrade/uninstall run the
/// same unmistakable start-of-output marker. Deliberately plainer than
/// `heading` (no `▸`) so the very first line of output reads as a title, not
/// a section.
pub fn banner(s: &str) -> String {
    format!("{}\n", bold(&cyan(s)))
}

/// Closing status line for a command that tracked a `count` of
/// warnings/issues: `✓ <ok_msg>` when zero, else `! <n> <noun>(s) <tail>`.
/// Centralizes the ok/warn tone pick + singular/plural noun so
/// install/upgrade/uninstall/setup/doctor don't each hand-roll it.
pub fn summary(count: usize, noun: &str, tail: &str, ok_msg: &str) -> String {
    if count == 0 {
        ok(ok_msg)
    } else {
        let plural = if count == 1 { "" } else { "s" };
        warn(&format!("{count} {noun}{plural} {tail}"))
    }
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

/// One row of a `multiselect`: either a non-selectable section header, or a
/// checkable item `(name, desc, preselected)`.
pub enum Row<'a> {
    /// A group heading — skipped by the cursor, rendered bold. Blank name = a
    /// spacer line.
    Header(&'a str),
    /// A toggleable choice: display name, dim description, initial checked state.
    Item(&'a str, &'a str, bool),
}

/// Raw-mode multi-select checklist over mixed header/item `rows`. Returns the
/// **item ordinals** (0-based over `Item` rows only, headers skipped) that ended
/// up checked, or `None` on cancel (esc/ctrl-c). Callers keep a parallel vector
/// of item payloads and index it by ordinal — no header offset to reconcile.
/// Space toggles the cursor row, `a` toggles every item at once, enter confirms.
/// Same drop-guard + redraw discipline as `select` — never re-roll the raw-mode
/// handling.
pub fn multiselect(title: &str, rows: &[Row<'_>]) -> anyhow::Result<Option<Vec<usize>>> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::crossterm::terminal;
    use std::io::Write;

    let is_item = |i: usize| matches!(rows[i], Row::Item(..));
    let item_idxs: Vec<usize> = (0..rows.len()).filter(|&i| is_item(i)).collect();
    if item_idxs.is_empty() {
        return Ok(Some(Vec::new()));
    }

    println!("{}", heading(title));
    println!(
        "{}",
        dim("  ↑/↓ move · space toggle · a all/none · enter confirm · esc cancel")
    );
    println!();

    struct Raw;
    impl Drop for Raw {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    terminal::enable_raw_mode()?;
    let _raw = Raw;

    let mut checked: Vec<bool> = rows
        .iter()
        .map(|r| matches!(r, Row::Item(_, _, true)))
        .collect();
    // name column = widest item name, so descriptions align and never collide
    let name_w = rows
        .iter()
        .filter_map(|r| match r {
            Row::Item(name, ..) => Some(name.len()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    // cursor starts on the first selectable row
    let mut sel = item_idxs[0];
    let lines = rows.len() + 1; // rows + trailing summary line
    let mut first = true;
    // move the cursor to the next/prev selectable row, skipping headers
    let step = |from: usize, dir: isize| -> usize {
        let n = rows.len() as isize;
        let mut i = from as isize;
        loop {
            i = (i + dir).rem_euclid(n);
            if is_item(i as usize) {
                return i as usize;
            }
        }
    };
    loop {
        let checked_n = item_idxs.iter().filter(|&&i| checked[i]).count();
        let mut out = String::new();
        if !first {
            out.push_str(&format!("\x1b[{lines}A"));
        }
        first = false;
        for (i, row) in rows.iter().enumerate() {
            let line = match row {
                Row::Header(h) if h.is_empty() => String::new(),
                Row::Header(h) => bold(h),
                Row::Item(name, desc, _) => {
                    let box_ = if checked[i] { green("◉") } else { dim("○") };
                    let cursor = if i == sel { cyan("›") } else { " ".into() };
                    // pad the RAW name to the column width first — styling it
                    // before padding would make the fill count the ANSI escape
                    // bytes and eat the gap, colliding name with description.
                    let name = format!("{name:<name_w$}");
                    let name = if i == sel { bold(&name) } else { name };
                    format!("  {cursor} {box_}  {name}  {}", dim(desc))
                }
            };
            out.push_str(&format!("\r\x1b[2K{line}\r\n"));
        }
        out.push_str(&format!(
            "\r\x1b[2K{}\r\n",
            dim(&format!("  {checked_n} of {} selected", item_idxs.len()))
        ));
        let mut stdout = std::io::stdout();
        stdout.write_all(out.as_bytes())?;
        stdout.flush()?;

        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => sel = step(sel, -1),
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => sel = step(sel, 1),
                KeyCode::Char(' ') => checked[sel] = !checked[sel],
                // `a` = select all, or clear all when everything is already on
                KeyCode::Char('a') => {
                    let all_on = checked_n == item_idxs.len();
                    for &i in &item_idxs {
                        checked[i] = !all_on;
                    }
                }
                KeyCode::Enter => {
                    // map the row-indexed `checked` back to item ordinals
                    let picked = item_idxs
                        .iter()
                        .enumerate()
                        .filter(|&(_, &row)| checked[row])
                        .map(|(ord, _)| ord)
                        .collect();
                    return Ok(Some(picked));
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
