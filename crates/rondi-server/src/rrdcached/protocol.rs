//! Request lines and the fields they split into.

/// The longest request line, in bytes before its LF (RC-15).
pub(crate) const LINE_MAX: usize = 8191;

/// A request line split into fields.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request<'a> {
    /// Each field with its escapes removed. A field that runs into an
    /// escaped line end is not included.
    pub(crate) fields: Vec<Vec<u8>>,
    /// The bytes of the line after the separator that ends the first field,
    /// as sent; empty when the first field ends the line.
    pub(crate) rest: &'a [u8],
}

impl Request<'_> {
    /// The field at `index`, if the line has one.
    pub(crate) fn field(&self, index: usize) -> Option<&[u8]> {
        self.fields.get(index).map(Vec::as_slice)
    }
}

/// Splits `line` (without its LF) into fields.
// Compatibility: fields end at a space or a NUL byte; a backslash makes
// the next byte ordinary, except a NUL, which still ends the field; quotes
// have no meaning; a separator before the end of the line leaves an empty
// last field. A field whose backslash is the last byte of the line is
// dropped, so a command reading it finds nothing there (RC-7, RC-11,
// RC-12, RC-12a). Provenance: black-box compatibility test.
pub(crate) fn split(line: &[u8]) -> Request<'_> {
    let mut fields = Vec::new();
    let mut field = Vec::new();
    let mut rest: Option<&[u8]> = None;
    let mut index = 0;
    loop {
        let Some(&byte) = line.get(index) else {
            fields.push(field);
            break;
        };
        match byte {
            b' ' | 0 => {
                fields.push(std::mem::take(&mut field));
                index += 1;
            }
            b'\\' => match line.get(index + 1) {
                None => break,
                Some(0) => {
                    fields.push(std::mem::take(&mut field));
                    index += 2;
                }
                Some(&escaped) => {
                    field.push(escaped);
                    index += 2;
                }
            },
            other => {
                field.push(other);
                index += 1;
            }
        }
        if fields.len() == 1 && rest.is_none() {
            rest = Some(&line[index.min(line.len())..]);
        }
    }
    Request {
        fields,
        rest: rest.unwrap_or_default(),
    }
}

/// Removes one carriage return before the line end (RC-8).
pub(crate) fn strip_carriage_return(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// `field` written so that [`split`] reads it back as one field.
pub(crate) fn escape(field: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(field.len());
    for &byte in field {
        if matches!(byte, b' ' | b'\\') {
            escaped.push(b'\\');
        }
        escaped.push(byte);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(line: &[u8]) -> Vec<Vec<u8>> {
        split(line).fields
    }

    #[test]
    fn fields_split_on_single_spaces_and_nul() {
        assert_eq!(fields(b"PING"), [b"PING".to_vec()]);
        assert_eq!(fields(b""), [b"".to_vec()]);
        assert_eq!(fields(b"PING "), [b"PING".to_vec(), Vec::new()]);
        assert_eq!(
            fields(b"HELP  PING"),
            [b"HELP".to_vec(), Vec::new(), b"PING".to_vec()]
        );
        assert_eq!(fields(b"HELP\0PING"), [b"HELP".to_vec(), b"PING".to_vec()]);
        assert_eq!(fields(b"\0PING"), [Vec::new(), b"PING".to_vec()]);
        assert_eq!(fields(b"PING\t"), [b"PING\t".to_vec()]);
    }

    #[test]
    fn backslash_escapes_one_byte() {
        assert_eq!(fields(b"PI\\NG"), [b"PING".to_vec()]);
        assert_eq!(fields(b"s\\ p.rrd"), [b"s p.rrd".to_vec()]);
        assert_eq!(fields(b"back\\\\slash"), [b"back\\slash".to_vec()]);
        assert_eq!(fields(b"PI\\\0NG"), [b"PI".to_vec(), b"NG".to_vec()]);
        assert_eq!(fields(b"\"s p\""), [b"\"s".to_vec(), b"p\"".to_vec()]);
    }

    #[test]
    fn a_trailing_backslash_drops_its_field() {
        assert!(fields(b"PING\\").is_empty());
        assert_eq!(fields(b"PING \\"), [b"PING".to_vec()]);
        assert_eq!(fields(b"LAST a.rrd\\"), [b"LAST".to_vec()]);
    }

    #[test]
    fn rest_is_the_text_after_the_command_word() {
        assert_eq!(split(b"UPDATE a b\\ c").rest, b"a b\\ c");
        assert_eq!(split(b"UPDATE").rest, b"");
        assert_eq!(split(b"UPDATE ").rest, b"");
        assert_eq!(split(b"UPDATE\0a").rest, b"a");
        assert_eq!(split(b"UP\\ DATE x").rest, b"x");
        assert_eq!(split(b"x\\\0y").rest, b"y");
    }

    #[test]
    fn escaped_fields_read_back_unchanged() {
        for field in [&b"s p.rrd"[..], b"back\\slash", b"plain", b""] {
            let line = [b"X ".as_slice(), &escape(field)].concat();
            assert_eq!(fields(&line)[1], field);
        }
    }

    #[test]
    fn one_carriage_return_is_removed() {
        assert_eq!(strip_carriage_return(b"PING\r"), b"PING");
        assert_eq!(strip_carriage_return(b"PING\r\r"), b"PING\r");
        assert_eq!(strip_carriage_return(b"PING"), b"PING");
    }
}
