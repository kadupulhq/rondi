//! What `:strftime` prints when the VDEF has no time, such as the time of
//! an AVERAGE or of a series with no known value.

// Compatibility: each conversion becomes dashes of its usual width; `%%`,
// `% `, `%.` and `%@` stay as they are, `%n` becomes CR LF, `%t` a tab,
// `%E`/`%O` take the following byte with them, and any other conversion
// (or a final `%`) becomes one dash. The table is the same on macOS and
// glibc and does not depend on the locale.
// Provenance: black-box compatibility test of every printable byte after
// `%` (PRINT:v:<fmt>:strftime with VDEF:v=x,AVERAGE).
fn replacement(conversion: u8) -> &'static [u8] {
    match conversion {
        b'%' => b"%",
        b' ' => b"% ",
        b'.' => b"%.",
        b'@' => b"%@",
        b'n' => b"\r\n",
        b't' => b"\t",
        b'd' | b'g' | b'm' | b'y' | b'C' | b'H' | b'I' | b'M' | b'S' | b'U' | b'V' | b'W' => b"--",
        b'j' => b"---",
        b'G' | b'Y' => b"----",
        b'D' => b"--/--/--",
        b'F' => b"----------",
        b'R' => b"--:--",
        b'T' => b"--:--:--",
        _ => b"-",
    }
}

/// The text printed for `format` when the time is unknown. Works on bytes:
/// a conversion letter can be the first byte of a multibyte character.
pub fn missing_time_text(format: &str) -> Vec<u8> {
    let bytes = format.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'%' {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        match bytes.get(at + 1) {
            None => {
                out.push(b'-');
                at += 1;
            }
            Some(b'E' | b'O') => {
                out.push(b'-');
                at = (at + 3).min(bytes.len());
            }
            Some(&conversion) => {
                out.extend_from_slice(replacement(conversion));
                at += 2;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::missing_time_text;

    fn show(format: &str) -> String {
        String::from_utf8_lossy(&missing_time_text(format)).into_owned()
    }

    #[test]
    fn conversions_become_dashes() {
        assert_eq!(show("%Y-%m-%d %H:%M:%S"), "---------- --:--:--");
        assert_eq!(show("%Y-%m %H %% x %q %"), "------- -- % x - -");
        assert_eq!(show("abc%Y%%"), "abc----%");
        assert_eq!(
            show("%D|%F|%R|%T|%j"),
            "--/--/--|----------|--:--|--:--:--|---"
        );
        assert_eq!(show("%c %a %Z %s"), "- - - -");
    }

    #[test]
    fn modifiers_and_odd_bytes() {
        assert_eq!(show("%EY"), "-");
        assert_eq!(show("%E"), "-");
        assert_eq!(show("<%E>"), "<-");
        assert_eq!(show("%E%Y"), "-Y");
        assert_eq!(show("%Ex%Y"), "-----");
        assert_eq!(show("%5Y"), "-Y");
        assert_eq!(show("%-d %_d"), "-d -d");
        assert_eq!(show("<% ><%.><%@><%n><%t>"), "<% ><%.><%@><\r\n><\t>");
        assert_eq!(missing_time_text("%é"), b"-\xa9");
    }
}
