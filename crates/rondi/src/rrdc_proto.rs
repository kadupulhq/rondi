//! Field encoding of the rrdcached line protocol, shared by the daemon (its
//! requests and journal) and the client so both split a line the same way.

/// Port of rrd_daemon.c `buffer_get_field`: a field ends at a single space or
/// at the end of the buffer, and a backslash takes the next character
/// literally. `None` is an exhausted buffer and `Some("")` one holding only
/// its terminating NUL, which still yields an empty field. A trailing lone
/// backslash fails and leaves the buffer unchanged.
pub fn next_field(buffer: &mut Option<&str>) -> Option<String> {
    let text = (*buffer)?;
    let mut field = String::new();
    let mut characters = text.char_indices();
    while let Some((index, character)) = characters.next() {
        match character {
            ' ' => {
                *buffer = Some(&text[index + 1..]);
                return Some(field);
            }
            '\\' => field.push(characters.next()?.1),
            _ => field.push(character),
        }
    }
    *buffer = None;
    Some(field)
}

/// Port of rrd_client.c `buffer_add_string` without the trailing separator.
/// A field holding a newline or NUL cannot cross the line protocol.
pub fn encode_field(field: &str) -> Option<String> {
    if field.contains(['\n', '\0']) {
        return None;
    }
    let mut encoded = String::with_capacity(field.len());
    for character in field.chars() {
        if matches!(character, ' ' | '\\') {
            encoded.push('\\');
        }
        encoded.push(character);
    }
    Some(encoded)
}

/// A request line without its `\n` and one `\r` before it, as `next_cmd`
/// terminates it.
pub fn strip_line_end(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_split_on_single_spaces_and_unescape() {
        let mut buffer = Some(r"update a\ b.rrd 1:2  x\\");
        assert_eq!(next_field(&mut buffer).as_deref(), Some("update"));
        assert_eq!(next_field(&mut buffer).as_deref(), Some("a b.rrd"));
        assert_eq!(next_field(&mut buffer).as_deref(), Some("1:2"));
        assert_eq!(next_field(&mut buffer).as_deref(), Some(""));
        assert_eq!(next_field(&mut buffer).as_deref(), Some(r"x\"));
        assert_eq!(next_field(&mut buffer), None);
        let mut trailing = Some(r"a\");
        assert_eq!(next_field(&mut trailing), None);
        assert_eq!(trailing, Some(r"a\"));
    }

    #[test]
    fn encoded_fields_decode_to_themselves() {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let alphabet = [' ', '\\', '\t', '\r', 'a', 'Z', ':', '.', '/', 'é', '0'];
        for _ in 0..2000 {
            let mut fields = Vec::new();
            for _ in 0..(state % 5) {
                let mut field = String::new();
                for _ in 0..(state % 7) {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    field.push(alphabet[(state % alphabet.len() as u64) as usize]);
                }
                fields.push(field);
                state = state.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1);
            }
            let line = fields
                .iter()
                .map(|field| encode_field(field).unwrap())
                .collect::<Vec<_>>()
                .join(" ");
            let mut buffer = (!fields.is_empty()).then_some(line.as_str());
            let mut decoded = Vec::new();
            while let Some(field) = next_field(&mut buffer) {
                decoded.push(field);
            }
            assert_eq!(decoded, fields, "line {line:?}");
            state = state.wrapping_add(0x632b_e59b_d9b4_e019);
        }
        assert_eq!(encode_field("a\nb"), None);
        assert_eq!(encode_field("a\0b"), None);
    }
}
