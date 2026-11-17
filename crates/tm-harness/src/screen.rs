//! Terminal output as plain text, the way a person would read it on the screen.
//!
//! Enough of a terminal to follow installers, progress lines and the sign-in screens of
//! TUI programs: printable text, carriage return, backspace, cursor moves, erase in line
//! and in display, and the alternate screen. Colours and every other control sequence are
//! dropped. Lines are kept up to a limit, oldest first out.

const KEEP_LINES: usize = 2000;

#[derive(Debug, Clone)]
pub struct Screen {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// Rows of the visible screen, for absolute cursor positions.
    height: usize,
    width: usize,
    /// Bytes of a control sequence or a UTF-8 character not yet complete.
    pending: Vec<u8>,
    /// The main screen's lines while a program uses the alternate one.
    saved: Option<(Vec<Vec<char>>, usize, usize)>,
}

impl Screen {
    pub fn new(height: u16, width: u16) -> Screen {
        Screen {
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            height: height.max(1) as usize,
            width: width.max(1) as usize,
            pending: Vec::new(),
            saved: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            match b {
                0x1b => match self.escape(&data[i..]) {
                    Some(used) => i += used,
                    None => {
                        self.pending = data[i..].to_vec();
                        return;
                    }
                },
                b'\n' => {
                    self.newline();
                    i += 1;
                }
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                0x08 => {
                    self.col = self.col.saturating_sub(1);
                    i += 1;
                }
                b'\t' => {
                    let next = (self.col / 8 + 1) * 8;
                    while self.col < next {
                        self.put(' ');
                    }
                    i += 1;
                }
                0x00..=0x1f | 0x7f => i += 1,
                _ => {
                    let len = utf8_len(b);
                    if i + len > data.len() {
                        self.pending = data[i..].to_vec();
                        return;
                    }
                    let ch = std::str::from_utf8(&data[i..i + len]).ok().and_then(|s| s.chars().next()).unwrap_or('\u{fffd}');
                    self.put(ch);
                    i += len;
                }
            }
        }
    }

    /// Every line, trailing spaces and trailing empty lines removed.
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = self.lines.iter().map(|line| line.iter().collect::<String>().trim_end().to_string()).collect();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// The last `n` lines of [`Screen::text`].
    pub fn tail(&self, n: usize) -> String {
        let text = self.text();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }

    fn put(&mut self, ch: char) {
        if self.col >= self.width {
            self.newline();
        }
        let line = &mut self.lines[self.row];
        while line.len() < self.col {
            line.push(' ');
        }
        if self.col < line.len() {
            line[self.col] = ch;
        } else {
            line.push(ch);
        }
        self.col += 1;
    }

    fn newline(&mut self) {
        self.row += 1;
        self.col = 0;
        if self.row >= self.lines.len() {
            self.lines.push(Vec::new());
        }
        if self.lines.len() > KEEP_LINES {
            let drop = self.lines.len() - KEEP_LINES;
            self.lines.drain(..drop);
            self.row -= drop;
        }
    }

    /// The first row of the visible screen.
    fn top(&self) -> usize {
        self.lines.len().saturating_sub(self.height)
    }

    fn goto_row(&mut self, row: usize) {
        while self.lines.len() <= row {
            self.lines.push(Vec::new());
        }
        self.row = row;
    }

    /// Handle an escape at the start of `data`. `None` when it is not complete yet.
    fn escape(&mut self, data: &[u8]) -> Option<usize> {
        let kind = *data.get(1)?;
        match kind {
            b'[' => {
                let end = data[2..].iter().position(|b| (0x40..=0x7e).contains(b))? + 2;
                let params = std::str::from_utf8(&data[2..end]).unwrap_or("");
                self.csi(params, data[end]);
                Some(end + 1)
            }
            b']' | b'P' | b'_' | b'^' => {
                // A string ended by BEL or ST (ESC \).
                let mut j = 2;
                while j < data.len() {
                    if data[j] == 0x07 {
                        return Some(j + 1);
                    }
                    if data[j] == 0x1b && data.get(j + 1) == Some(&b'\\') {
                        return Some(j + 2);
                    }
                    if data[j] == 0x1b && j + 1 >= data.len() {
                        return None;
                    }
                    j += 1;
                }
                None
            }
            b'(' | b')' | b'*' | b'+' | b'#' => data.get(2).map(|_| 3),
            _ => Some(2),
        }
    }

    fn csi(&mut self, params: &str, op: u8) {
        let private = params.starts_with('?');
        let nums: Vec<usize> = params.trim_start_matches(['?', '>', '=']).split(';').map(|n| n.parse().unwrap_or(0)).collect();
        let n = |i: usize, default: usize| nums.get(i).copied().filter(|v| *v > 0).unwrap_or(default);
        match op {
            b'A' => {
                let up = n(0, 1);
                self.row = self.row.saturating_sub(up).max(self.top().min(self.row));
            }
            b'B' | b'E' => {
                let row = self.row + n(0, 1);
                self.goto_row(row);
                if op == b'E' {
                    self.col = 0;
                }
            }
            b'F' => {
                self.row = self.row.saturating_sub(n(0, 1));
                self.col = 0;
            }
            b'C' => self.col = (self.col + n(0, 1)).min(self.width.saturating_sub(1)),
            b'D' => self.col = self.col.saturating_sub(n(0, 1)),
            b'G' => self.col = n(0, 1) - 1,
            b'H' | b'f' => {
                let row = self.top() + n(0, 1) - 1;
                self.goto_row(row);
                self.col = n(1, 1) - 1;
            }
            b'K' => {
                let line = &mut self.lines[self.row];
                match nums.first().copied().unwrap_or(0) {
                    0 => line.truncate(self.col),
                    1 => {
                        for cell in line.iter_mut().take(self.col + 1) {
                            *cell = ' ';
                        }
                    }
                    _ => line.clear(),
                }
            }
            b'J' => match nums.first().copied().unwrap_or(0) {
                0 => {
                    self.lines[self.row].truncate(self.col);
                    self.lines.truncate(self.row + 1);
                }
                1 => {
                    let (top, row) = (self.top(), self.row);
                    for line in &mut self.lines[top..row] {
                        line.clear();
                    }
                }
                _ => {
                    let top = self.top();
                    self.lines.truncate(top);
                    self.lines.push(Vec::new());
                    self.row = self.lines.len() - 1;
                    self.col = 0;
                }
            },
            b'h' | b'l' if private && nums.iter().any(|v| matches!(v, 1049 | 1047 | 47)) => {
                if op == b'h' && self.saved.is_none() {
                    let saved = std::mem::replace(&mut self.lines, vec![Vec::new()]);
                    self.saved = Some((saved, self.row, self.col));
                    self.row = 0;
                    self.col = 0;
                } else if op == b'l'
                    && let Some((lines, row, col)) = self.saved.take()
                {
                    // What the program last showed stays readable after it leaves.
                    let shown = std::mem::replace(&mut self.lines, lines);
                    self.row = row;
                    self.col = col;
                    for line in shown.into_iter().filter(|line| line.iter().any(|c| !c.is_whitespace())) {
                        self.newline();
                        self.lines[self.row] = line;
                    }
                    self.newline();
                }
            }
            _ => {}
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// The address and code a sign-in screen asks a person to use, when it shows them.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SignInPrompt {
    pub url: Option<String>,
    pub code: Option<String>,
}

/// Read the last sign-in address and device code from a screen's text.
pub fn sign_in_prompt(text: &str) -> SignInPrompt {
    let mut prompt = SignInPrompt::default();
    for word in text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '<' || c == '>' || c == '(' || c == ')') {
        let word = word.trim_end_matches(['.', ',', ';']);
        if (word.starts_with("https://") || word.starts_with("http://localhost") || word.starts_with("http://127.0.0.1")) && word.len() > 10 {
            prompt.url = Some(word.to_string());
        } else if is_device_code(word) {
            prompt.code = Some(word.to_string());
        }
    }
    prompt
}

/// `ABCD-EFGH` or `ABCD-1234`: two to three groups of 4 to 5 capitals or digits, at least
/// one letter, joined by `-`.
fn is_device_code(word: &str) -> bool {
    let groups: Vec<&str> = word.split('-').collect();
    if !(2..=3).contains(&groups.len()) {
        return false;
    }
    let shaped = groups.iter().all(|g| (4..=5).contains(&g.len()) && g.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    shaped && word.chars().any(|c| c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(bytes: &[u8]) -> String {
        let mut screen = Screen::new(24, 80);
        screen.feed(bytes);
        screen.text()
    }

    #[test]
    fn colours_are_dropped_and_lines_kept() {
        assert_eq!(render(b"\x1b[1;32mok\x1b[0m done\r\nnext\r\n"), "ok done\nnext");
    }

    #[test]
    fn a_progress_line_overwrites_itself() {
        assert_eq!(render(b"downloading 10%\rdownloading 100%\r\nfinished\n"), "downloading 100%\nfinished");
        assert_eq!(render(b"50%\x1b[2K\rall done"), "all done");
    }

    #[test]
    fn a_redrawn_block_replaces_the_old_one() {
        // An Ink-style redraw: move up two lines, erase each, write again.
        let bytes = b"Waiting\r\n  .\r\n\x1b[1A\x1b[2K\x1b[1A\x1b[2K\rSigned in\r\n";
        assert_eq!(render(bytes), "Signed in");
    }

    #[test]
    fn a_sequence_split_across_reads_still_parses() {
        let mut screen = Screen::new(24, 80);
        screen.feed(b"a\x1b[3");
        screen.feed(b"1mb\xe2\x9c");
        screen.feed(b"\x93");
        assert_eq!(screen.text(), "ab\u{2713}");
    }

    #[test]
    fn the_alternate_screen_leaves_its_last_frame_behind() {
        let text = render(b"before\r\n\x1b[?1049h\x1b[2J\x1b[1;1HPick a provider\x1b[3;3H> OpenAI\x1b[?1049l");
        assert!(text.starts_with("before"), "{text}");
        assert!(text.contains("Pick a provider") && text.contains("> OpenAI"), "{text}");
    }

    #[test]
    fn osc_titles_and_links_are_not_text() {
        assert_eq!(render(b"\x1b]0;title\x07x\x1b]8;;https://a.b\x1b\\link\x1b]8;;\x1b\\"), "xlink");
    }

    #[test]
    fn a_device_code_and_its_page_are_found() {
        let text = "Open this page:\n  https://auth.example.com/device?x=1\nand enter the code ABCD-12EF.\n";
        let prompt = sign_in_prompt(text);
        assert_eq!(prompt.url.as_deref(), Some("https://auth.example.com/device?x=1"));
        assert_eq!(prompt.code.as_deref(), Some("ABCD-12EF"));
        // Version numbers, dates and lowercase ids are not codes.
        let none = sign_in_prompt("installed 2026-11-17 v1.2-3 abcd-efgh 1234-5678");
        assert_eq!(none, SignInPrompt::default());
    }
}
