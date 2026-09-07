//! Terminal snapshot encoding; deliberately avoids clearing the normal screen.
pub const HISTORY_LINES: usize = 10_000;

/// Equivalent screen controls missing from vt100's built-in CSI handling.
pub fn terminal_extension(
    screen: &mut vt100::Screen,
    prefix: Option<u8>,
    params: &[&[u16]],
    action: char,
) -> bool {
    let sequence: &[u8] = match (prefix, params, action) {
        (None, [], 's') | (None, [[0]], 's') | (Some(b'?'), [[1048]], 'h') => b"\x1b7",
        (None, [], 'u') | (None, [[0]], 'u') | (Some(b'?'), [[1048]], 'l') => b"\x1b8",
        (Some(b'?'), [[1047]], 'h') => b"\x1b[?47h\x1b[2J",
        (Some(b'?'), [[1047]], 'l') => b"\x1b[?47l",
        _ => return false,
    };
    let (rows, cols) = screen.size();
    let mut parser = vt100::Parser::new(rows, cols, 0);
    // Move the existing grid, including scrollback, instead of copying it.
    std::mem::swap(parser.screen_mut(), screen);
    parser.process(sequence);
    std::mem::swap(parser.screen_mut(), screen);
    true
}

/// Read normal-screen history even while an application uses the alternate grid.
/// Responses are bounded below gRPC's default message limit.
pub fn terminal_history(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let mut copy = vt100::Parser::new(rows, cols, 0);
    *copy.screen_mut() = screen.clone();
    if screen.alternate_screen() {
        copy.process(b"\x1b[?1049l");
    }
    copy.screen_mut().set_scrollback(usize::MAX);
    let count = copy.screen().scrollback();
    let mut lines = Vec::new();
    let mut remaining = count;
    while remaining > 0 {
        copy.screen_mut().set_scrollback(remaining);
        let take = remaining.min(usize::from(rows));
        lines.extend(copy.screen().rows(0, cols).take(take));
        remaining -= take;
    }
    let mut bytes = 0;
    let keep = lines
        .iter()
        .rev()
        .take_while(|line| {
            bytes += line.len() + 8;
            bytes <= 1024 * 1024
        })
        .count();
    lines.drain(..lines.len() - keep);
    lines
}

pub fn append_terminal_screen(screen: &vt100::Screen) -> Vec<u8> {
    let (_, cols) = screen.size();
    // Shell reattachment is appended at the current local cursor. The
    // library's state_formatted starts with an unconditional screen clear.
    let (cursor_row, cursor_col) = screen.cursor_position();
    let last_row = screen
        .rows(0, cols)
        .enumerate()
        .filter(|(_, row)| !row.is_empty())
        .map(|(row, _)| row as u16)
        .max()
        .unwrap_or(0)
        .max(cursor_row);
    let mut bytes = b"\r\n".to_vec();
    for (row, contents) in screen
        .rows_formatted(0, cols)
        .take(usize::from(last_row) + 1)
        .enumerate()
    {
        if row > 0 && !screen.row_wrapped(row as u16 - 1) {
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"\x1b[0m");
        bytes.extend(contents);
    }
    bytes.push(b'\r');
    if last_row > cursor_row {
        bytes.extend(format!("\x1b[{}A", last_row - cursor_row).bytes());
    }
    if cursor_col > 0 {
        bytes.extend(format!("\x1b[{cursor_col}C").bytes());
    }
    bytes.extend(screen.attributes_formatted());
    bytes.extend(screen.input_mode_formatted());
    bytes.extend_from_slice(if screen.hide_cursor() {
        b"\x1b[?25l"
    } else {
        b"\x1b[?25h"
    });
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_extensions_preserve_cursor_attributes_and_full_history() {
        let mut parser = vt100::Parser::new(24, 80, HISTORY_LINES);
        for n in 0..10_050 {
            parser.process(format!("history-{n}\r\n").as_bytes());
        }
        parser.process(b"\x1b[3;7H\x1b[31m");
        let history = terminal_history(parser.screen());
        let before = parser.screen().state_formatted();
        for prefix in [None, Some(b'?')] {
            let param = [1048];
            let params: &[&[u16]] = if prefix.is_some() { &[&param] } else { &[] };
            assert!(terminal_extension(
                parser.screen_mut(),
                prefix,
                params,
                if prefix.is_some() { 'h' } else { 's' }
            ));
            parser.process(b"\x1b[10;20H\x1b[32m");
            assert!(terminal_extension(
                parser.screen_mut(),
                prefix,
                params,
                if prefix.is_some() { 'l' } else { 'u' }
            ));
            assert_eq!(parser.screen().state_formatted(), before);
        }
        assert!(terminal_extension(
            parser.screen_mut(),
            Some(b'?'),
            &[&[1047]],
            'h'
        ));
        assert!(parser.screen().alternate_screen());
        assert!(parser.screen().contents().is_empty());
        parser.process(b"alternate content");
        assert!(terminal_extension(
            parser.screen_mut(),
            Some(b'?'),
            &[&[1047]],
            'l'
        ));
        assert!(!parser.screen().alternate_screen());
        assert_eq!(terminal_history(parser.screen()), history);
    }

    #[test]
    fn history_is_bounded_and_remains_available_inside_an_alternate_app() {
        let mut parser = vt100::Parser::new(5, 30, HISTORY_LINES);
        for n in 0..10_050 {
            parser.process(format!("history-{n}\r\n").as_bytes());
        }
        let history = terminal_history(parser.screen());
        assert_eq!(history.len(), HISTORY_LINES);
        assert_eq!(history.first().unwrap(), "history-46");
        assert_eq!(history.last().unwrap(), "history-10045");
        parser.process(b"\x1b[?1049h\x1b[Heditor");
        assert_eq!(terminal_history(parser.screen()), history);
    }
}
