//! The colon-separated fields of one element (GR-40).
//!
//! An element parser takes the named fields it knows first (`step=60`,
//! `STACK`), the last one of each name winning, and then the remaining
//! fields in order as positional values. Whatever is left over is an error.

/// One field: its text with `\:` turned into `:`, and the text as given,
/// which error messages repeat.
#[derive(Debug)]
struct Field {
    text: String,
    raw: String,
    used: bool,
}

#[derive(Debug)]
pub(super) struct Fields<'a> {
    element: &'a str,
    fields: Vec<Field>,
}

impl<'a> Fields<'a> {
    /// Splits `rest`, the element text after its name and colon, at the
    /// colons that are not escaped. `element` is the whole element text.
    pub(super) fn new(element: &'a str, rest: Option<&str>) -> Self {
        let mut fields = Vec::new();
        if let Some(rest) = rest {
            let (mut text, mut raw) = (String::new(), String::new());
            let mut chars = rest.chars().peekable();
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' if chars.peek() == Some(&':') => {
                        chars.next();
                        text.push(':');
                        raw.push_str("\\:");
                    }
                    ':' => fields.push(Field {
                        text: std::mem::take(&mut text),
                        raw: std::mem::take(&mut raw),
                        used: false,
                    }),
                    _ => {
                        text.push(ch);
                        raw.push(ch);
                    }
                }
            }
            fields.push(Field {
                text,
                raw,
                used: false,
            });
        }
        Self { element, fields }
    }

    /// The whole element text.
    pub(super) fn element(&self) -> &'a str {
        self.element
    }

    /// The value of the last unused `key=value` field.
    pub(super) fn value(&mut self, key: &str) -> Option<String> {
        let field = self.fields.iter_mut().rev().find(|field| {
            !field.used
                && field
                    .text
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.starts_with('='))
        })?;
        field.used = true;
        Some(field.text[key.len() + 1..].to_owned())
    }

    /// Whether an unused field is exactly `flag`; the last one is taken.
    pub(super) fn flag(&mut self, flag: &str) -> bool {
        match self
            .fields
            .iter_mut()
            .rev()
            .find(|field| !field.used && field.text == flag)
        {
            Some(field) => {
                field.used = true;
                true
            }
            None => false,
        }
    }

    /// A field that is either `key` alone or `key=value`, the last one of
    /// either form: `Some(None)` for the bare key.
    pub(super) fn key(&mut self, key: &str) -> Option<Option<String>> {
        let field = self.fields.iter_mut().rev().find(|field| {
            !field.used
                && field
                    .text
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
        })?;
        field.used = true;
        Some(field.text.get(key.len() + 1..).map(str::to_owned))
    }

    /// The next unused field.
    pub(super) fn next(&mut self) -> Option<String> {
        let field = self.fields.iter_mut().find(|field| !field.used)?;
        field.used = true;
        Some(field.text.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_fields_first_then_positions() {
        let mut fields = Fields::new("E", Some("x=f:a\\:b:step=1:step=2:STACK"));
        assert_eq!(fields.value("step").as_deref(), Some("2"));
        assert!(fields.flag("STACK"));
        assert_eq!(fields.next().as_deref(), Some("x=f"));
        assert_eq!(fields.next().as_deref(), Some("a:b"));
        assert_eq!(
            fields.finish().unwrap_err(),
            "Unused Arguments \"step=1\" in command : E"
        );
        assert!(fields.next().is_some());
        assert!(fields.next().is_none());
        assert!(fields.finish().is_ok());
    }

    #[test]
    fn bare_or_valued_keys() {
        let mut fields = Fields::new("E", Some("dashes=1,2:dashes"));
        assert_eq!(fields.key("dashes"), Some(None));
        assert_eq!(fields.key("dashes"), Some(Some("1,2".into())));
        assert_eq!(fields.key("dashes"), None);
        assert!(Fields::new("E", None).finish().is_ok());
        let empty = Fields::new("E", Some("::"));
        assert_eq!(
            empty.finish().unwrap_err(),
            "Unused Arguments \"::\" in command : E"
        );
    }
}
