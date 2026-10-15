//! Formatted text whose padding is kept as counted runs.
//!
//! A printf width or precision can ask for billions of identical bytes.
//! Keeping those as `(byte, count)` runs lets callers measure the result,
//! apply length limits and cut it without allocating the whole text.

/// Text made of literal pieces and runs of one repeated byte.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Field {
    parts: Vec<Part>,
    len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Run(u8, u64),
}

impl Field {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.len = self.len.saturating_add(text.len() as u64);
        match self.parts.last_mut() {
            Some(Part::Text(last)) => last.push_str(text),
            _ => self.parts.push(Part::Text(text.to_owned())),
        }
    }

    /// Appends `count` copies of the ASCII byte `byte`.
    pub fn push_run(&mut self, byte: u8, count: u64) {
        debug_assert!(byte.is_ascii());
        if count == 0 {
            return;
        }
        self.len = self.len.saturating_add(count);
        match self.parts.last_mut() {
            Some(Part::Run(last, total)) if *last == byte => *total = total.saturating_add(count),
            _ => self.parts.push(Part::Run(byte, count)),
        }
    }

    pub fn append(&mut self, other: Field) {
        for part in other.parts {
            match part {
                Part::Text(text) => self.push_str(&text),
                Part::Run(byte, count) => self.push_run(byte, count),
            }
        }
    }

    /// Pads to `width` bytes: before the text, or after it when `left`.
    pub fn pad(self, width: u64, left: bool) -> Field {
        let missing = width.saturating_sub(self.len);
        if missing == 0 {
            return self;
        }
        if left {
            let mut field = self;
            field.push_run(b' ', missing);
            field
        } else {
            let mut field = Field::new();
            field.push_run(b' ', missing);
            field.append(self);
            field
        }
    }

    /// Writes at most `limit` bytes of the text to `out`. A cut can split a
    /// multibyte character, so the output is bytes, not a `String`.
    pub fn write_prefix(&self, out: &mut Vec<u8>, limit: u64) {
        let mut room = limit;
        for part in &self.parts {
            if room == 0 {
                return;
            }
            match part {
                Part::Text(text) => {
                    let take = text.len().min(usize::try_from(room).unwrap_or(usize::MAX));
                    out.extend_from_slice(&text.as_bytes()[..take]);
                    room -= take as u64;
                }
                Part::Run(byte, count) => {
                    let take = (*count).min(room);
                    out.resize(
                        out.len() + usize::try_from(take).unwrap_or(usize::MAX),
                        *byte,
                    );
                    room -= take;
                }
            }
        }
    }

    /// The whole text, if it is no longer than `limit` bytes.
    pub fn to_string_within(&self, limit: u64) -> Option<String> {
        if self.len > limit {
            return None;
        }
        let mut bytes = Vec::with_capacity(usize::try_from(self.len).ok()?);
        self.write_prefix(&mut bytes, limit);
        // Every part is either a String or an ASCII run, so the whole
        // text is valid UTF-8.
        String::from_utf8(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::Field;

    #[test]
    fn runs_are_measured_without_allocation() {
        let mut field = Field::new();
        field.push_str("ab");
        field.push_run(b' ', u64::MAX - 1);
        field.push_run(b' ', 5);
        assert_eq!(field.len(), u64::MAX);
        let mut out = Vec::new();
        field.write_prefix(&mut out, 4);
        assert_eq!(out, b"ab  ");
        assert_eq!(field.to_string_within(1000), None);
    }

    #[test]
    fn padding_goes_before_or_after() {
        let mut field = Field::new();
        field.push_str("x");
        assert_eq!(
            field.clone().pad(3, false).to_string_within(10).unwrap(),
            "  x"
        );
        assert_eq!(
            field.clone().pad(3, true).to_string_within(10).unwrap(),
            "x  "
        );
        assert_eq!(field.pad(0, true).to_string_within(10).unwrap(), "x");
    }

    #[test]
    fn cut_can_split_a_character() {
        let mut field = Field::new();
        field.push_str("é");
        let mut out = Vec::new();
        field.write_prefix(&mut out, 1);
        assert_eq!(out, [0xc3]);
    }
}
