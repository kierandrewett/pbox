//! Terminal snapshot encoding; deliberately avoids clearing the normal screen.
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
