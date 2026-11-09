//! The colon-separated fields of one element (GR-40, GR-95).
//!
//! An element is split at the colons that are not escaped. A field of the
//! form `key=value` whose key the element knows is a named field; when a
//! key appears more than once, the last one is the named field and the
//! earlier ones are positional. Every other field is positional, in order.
//! An element reads each of its arguments from its named field when there
//! is one, and from the next positional field otherwise. Whatever is left
//! over is an error.

/// The most fields without `=` an element may have, its name included.
const MOST_PLAIN_FIELDS: usize = 10;

/// One field: its text with `\:` turned into `:`, and the text as given,
/// which error messages repeat.
#[derive(Debug)]
struct Field {
    text: String,
    raw: String,
    /// The length of the key when the text holds a `=`.
    key_len: Option<usize>,
    /// The named field of its key.
    named: bool,
    used: bool,
}

impl Field {
    fn key(&self) -> Option<&str> {
        self.key_len.map(|len| &self.text[..len])
    }

    /// The text after the first `=`, or the whole text.
    fn value(&self) -> &str {
        self.key_len.map_or(&self.text, |len| &self.text[len + 1..])
    }
}

/// A positional field split at its first `=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Pair {
    pub key: Option<String>,
    pub value: String,
}

#[derive(Debug)]
pub(super) struct Fields<'a> {
    element: &'a str,
    fields: Vec<Field>,
}

impl<'a> Fields<'a> {
    /// Splits `element` and takes out its name: the last `cmd=` field, or
    /// else the first field without `=`.
    pub(super) fn parse(element: &'a str) -> Result<(String, Self), String> {
        let mut fields = Vec::new();
        let (mut text, mut raw) = (String::new(), String::new());
        let mut chars = element.chars().peekable();
        let field = |text: String, raw: String| Field {
            key_len: text.find('='),
            text,
            raw,
            named: false,
            used: false,
        };
        while let Some(ch) = chars.next() {
            match ch {
                '\\' if chars.peek() == Some(&':') => {
                    chars.next();
                    text.push(':');
                    raw.push_str("\\:");
                }
                ':' => fields.push(field(std::mem::take(&mut text), std::mem::take(&mut raw))),
                _ => {
                    text.push(ch);
                    raw.push(ch);
                }
            }
        }
        fields.push(field(text, raw));
        let mut fields = Self { element, fields };
        // Compatibility: the limit counts the fields without `=`, the
        // element name among them, and is checked before the name.
        // Provenance: black-box compatibility test (graph_data_differential
        // element grammar, probes of 10 and 11 fields).
        let plain = fields
            .fields
            .iter()
            .filter(|field| field.key_len.is_none())
            .count();
        if plain > MOST_PLAIN_FIELDS {
            return Err("too many positional arguments".into());
        }
        let name = match fields.named("cmd") {
            Some(name) => name,
            None => {
                let field = fields
                    .fields
                    .iter_mut()
                    .find(|field| field.key_len.is_none())
                    .ok_or_else(|| format!("no command set in argument {element}"))?;
                field.used = true;
                field.text.clone()
            }
        };
        Ok((name, fields))
    }

    /// Marks the last field of each of `keys` as the named field.
    pub(super) fn bind(&mut self, keys: &[&str]) {
        for key in keys.iter().copied().chain(["debug"]) {
            if let Some(field) = self
                .fields
                .iter_mut()
                .rev()
                .find(|field| !field.used && field.key() == Some(key))
            {
                field.named = true;
            }
        }
    }

    /// The whole element text.
    pub(super) fn element(&self) -> &'a str {
        self.element
    }

    /// The value of the named field `key`. `cmd` is always known.
    pub(super) fn named(&mut self, key: &str) -> Option<String> {
        if key == "cmd" {
            let field = self
                .fields
                .iter_mut()
                .rev()
                .find(|field| !field.used && field.key() == Some("cmd"))?;
            field.used = true;
            return Some(field.value().to_owned());
        }
        let field = self
            .fields
            .iter_mut()
            .find(|field| field.named && !field.used && field.key() == Some(key))?;
        field.used = true;
        Some(field.value().to_owned())
    }

    /// Where the named field `key` stands, without taking it.
    fn named_position(&self, key: &str) -> Option<usize> {
        self.fields
            .iter()
            .position(|field| field.named && !field.used && field.key() == Some(key))
    }

    /// Where the last unused positional field that is exactly `flag`
    /// stands.
    fn flag_position(&self, flag: &str) -> Option<usize> {
        self.fields
            .iter()
            .rposition(|field| !field.used && !field.named && field.text == flag)
    }

    /// Whether a positional field is exactly `flag`; the last one is taken.
    pub(super) fn flag(&mut self, flag: &str) -> bool {
        match self.flag_position(flag) {
            Some(index) => {
                self.fields[index].used = true;
                true
            }
            None => false,
        }
    }

    /// A flag given bare or as a named field with any value.
    pub(super) fn flag_or_named(&mut self, flag: &str, key: &str) -> bool {
        let named = self.named(key).is_some();
        self.flag(flag) || named
    }

    /// Takes whichever of `choices` stands last, each a bare flag or a
    /// named field, and returns its index in `choices` and the named
    /// field's value. The others stay unused.
    pub(super) fn last_of(&mut self, choices: &[Choice<'_>]) -> Option<(usize, Option<String>)> {
        let (choice, position) = choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                let position = match choice {
                    Choice::Flag(flag) => self.flag_position(flag),
                    Choice::Named(key) => self.named_position(key),
                };
                position.map(|position| (index, position))
            })
            .max_by_key(|&(_, position)| position)?;
        let field = &mut self.fields[position];
        field.used = true;
        let value = matches!(choices[choice], Choice::Named(_)).then(|| field.value().to_owned());
        Some((choice, value))
    }

    fn next_field(&mut self) -> Option<&mut Field> {
        self.fields
            .iter_mut()
            .find(|field| !field.used && !field.named)
    }

    /// The next positional field, whole.
    pub(super) fn next(&mut self) -> Option<String> {
        let field = self.next_field()?;
        field.used = true;
        Some(field.text.clone())
    }

    /// The next positional field, split at its first `=`.
    pub(super) fn next_pair(&mut self) -> Option<Pair> {
        let field = self.next_field()?;
        field.used = true;
        Some(Pair {
            key: field.key().map(str::to_owned),
            value: field.value().to_owned(),
        })
    }

    /// The next positional field after its first `=`.
    pub(super) fn next_value(&mut self) -> Option<String> {
        self.next_pair().map(|pair| pair.value)
    }

    /// The named field `key`, or else the next positional field after its
    /// first `=`.
    pub(super) fn value_or_next(&mut self, key: &str) -> Option<String> {
        self.named(key).or_else(|| self.next_value())
    }

    /// The named field `key`, or else the next whole positional field.
    pub(super) fn text_or_next(&mut self, key: &str) -> Option<String> {
        self.named(key).or_else(|| self.next())
    }

    /// Fails when a field was not used.
    pub(super) fn finish(&self) -> Result<(), String> {
        let unused: Vec<&str> = self
            .fields
            .iter()
            .filter(|field| !field.used)
            .map(|field| field.raw.as_str())
            .collect();
        if unused.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "Unused Arguments \"{}\" in command : {}",
                unused.join(":"),
                self.element
            ))
        }
    }
}

/// One way of giving a choice to [`Fields::last_of`].
#[derive(Debug, Clone, Copy)]
pub(super) enum Choice<'k> {
    Flag(&'k str),
    Named(&'k str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (String, Fields<'_>) {
        Fields::parse(text).unwrap()
    }

    #[test]
    fn named_fields_then_positions() {
        let (name, mut fields) = parse("E:x=f:a\\:b:step=1:step=2:STACK");
        assert_eq!(name, "E");
        fields.bind(&["step"]);
        assert_eq!(fields.named("step").as_deref(), Some("2"));
        assert!(fields.flag("STACK"));
        assert_eq!(
            fields.next_pair(),
            Some(Pair {
                key: Some("x".into()),
                value: "f".into()
            })
        );
        assert_eq!(fields.next().as_deref(), Some("a:b"));
        assert_eq!(
            fields.finish().unwrap_err(),
            "Unused Arguments \"step=1\" in command : E:x=f:a\\:b:step=1:step=2:STACK"
        );
        assert_eq!(fields.next_value().as_deref(), Some("1"));
        assert!(fields.next().is_none());
        assert!(fields.finish().is_ok());
    }

    #[test]
    fn element_names_and_limits() {
        let (name, mut fields) = parse("LINE:cmd=LINE2:x");
        assert_eq!(name, "LINE2");
        assert_eq!(fields.next().as_deref(), Some("LINE"));
        assert_eq!(parse("a=b:x").0, "x");
        assert_eq!(
            Fields::parse("a=b").unwrap_err(),
            "no command set in argument a=b"
        );
        assert!(Fields::parse("A:B:C:D:E:F:G:H:I:J").is_ok());
        assert!(Fields::parse("A:B:C:D:E:F:G:H:I:J:k=K").is_ok());
        assert_eq!(
            Fields::parse("A:B:C:D:E:F:G:H:I:J:K").unwrap_err(),
            "too many positional arguments"
        );
        let (name, fields) = parse("COMMENT:");
        assert_eq!(name, "COMMENT");
        assert_eq!(
            fields.finish().unwrap_err(),
            "Unused Arguments \"\" in command : COMMENT:"
        );
        assert_eq!(parse("").0, "");
    }

    #[test]
    fn the_last_choice_wins() {
        let (_, mut fields) = parse("P:valstrftime:vformatter=duration:valstrfduration");
        fields.bind(&["vformatter"]);
        let choices = [
            Choice::Flag("valstrftime"),
            Choice::Flag("valstrfduration"),
            Choice::Named("vformatter"),
        ];
        assert_eq!(fields.last_of(&choices), Some((1, None)));
        assert_eq!(
            fields.finish().unwrap_err(),
            "Unused Arguments \"valstrftime:vformatter=duration\" in command : \
             P:valstrftime:vformatter=duration:valstrfduration"
        );
    }
}
