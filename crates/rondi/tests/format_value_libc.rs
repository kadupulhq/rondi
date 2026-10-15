#![cfg(unix)]
//! Differential and robustness tests for `rondi::format_value`.
//!
//! The host printer must agree with this machine's C library `snprintf`
//! for every plain floating-point conversion, since graph PRINT text is
//! specified as "what the C library prints" (format-strings.md FS-13,
//! FS-14). The other entry points must never panic on any input.

use std::ffi::CString;

use rondi::format_value::{
    DurationError, NumberSpec, PrintFormat, Printer, UnitScale, format_duration, host_number,
    missing_time_text, scaled_number,
};

/// A small deterministic generator, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    fn value(&mut self) -> f64 {
        match self.below(6) {
            0 => f64::from_bits(self.next()),
            1 => *self.pick(&[
                0.0,
                -0.0,
                0.5,
                1.5,
                2.5,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NAN,
                -f64::NAN,
                f64::MIN_POSITIVE,
                5e-324,
                f64::MAX,
                999.9995,
                0.125,
            ]),
            2 => {
                let digits = self.below(17) + 1;
                let mantissa = self.below(10_u64.pow(digits as u32));
                let exponent = self.below(60) as i32 - 30;
                format!("{mantissa}5e{exponent}").parse().unwrap()
            }
            3 => (self.below(2_000_001) as f64 - 1_000_000.0) / 8.0,
            _ => {
                let exponent = self.below(600) as i32 - 300;
                (self.below(1 << 52) as f64 + 1.0) * 10_f64.powi(exponent) / (1u64 << 52) as f64
            }
        }
    }

    fn conversion(&mut self) -> String {
        let mut text = String::from("%");
        for flag in ['-', '+', ' ', '0', '#'] {
            if self.below(4) == 0 {
                text.push(flag);
            }
        }
        match self.below(4) {
            0 => {}
            1 => text.push_str(&self.below(12).to_string()),
            2 => text.push_str(&self.below(40).to_string()),
            _ => text.push_str(&(self.below(400) + 100).to_string()),
        }
        match self.below(5) {
            0 => {}
            1 => text.push('.'),
            2 => text.push_str(&format!(".{}", self.below(8))),
            3 => text.push_str(&format!(".{}", self.below(40))),
            _ => text.push_str(&format!(".{}", self.below(1500))),
        }
        text.push(*self.pick(&['e', 'E', 'f', 'F', 'g', 'G']));
        text
    }
}

fn c_snprintf(format: &str, value: f64) -> String {
    let format = CString::new(format).unwrap();
    // SAFETY: a null buffer of size 0 only asks for the length; the second
    // call writes at most `length + 1` bytes into a buffer of that size.
    // The format holds exactly one double conversion, matching the one
    // variadic argument.
    unsafe {
        let length = libc::snprintf(std::ptr::null_mut(), 0, format.as_ptr(), value);
        assert!(length >= 0);
        let mut buffer = vec![0_u8; length as usize + 1];
        libc::snprintf(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            format.as_ptr(),
            value,
        );
        buffer.truncate(length as usize);
        String::from_utf8(buffer).unwrap()
    }
}

#[test]
fn host_printer_matches_the_c_library() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut failures = Vec::new();
    for _ in 0..20_000 {
        let format = rng.conversion();
        let value = rng.value();
        let spec = NumberSpec::parse(&format).unwrap();
        let ours = host_number(&spec, value, ".")
            .to_string_within(1 << 20)
            .unwrap();
        let theirs = c_snprintf(&format, value);
        if ours != theirs {
            failures.push(format!(
                "{format} {value:e}: rondi {ours:?}, libc {theirs:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches, first: {:#?}",
        failures.len(),
        &failures[..failures.len().min(10)]
    );
}

/// Arbitrary bytes from a small alphabet that hits the format grammars.
fn random_format(rng: &mut Rng) -> String {
    const ALPHABET: &[u8] = b"%%%%lleEfFgGsSdDhHmMWfY0123456789.-+ #:\\xq";
    let length = rng.below(24);
    (0..length)
        .map(|_| char::from(*rng.pick(ALPHABET)))
        .collect()
}

#[test]
fn formatters_never_panic() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut scale = UnitScale::new(1000);
    for _ in 0..50_000 {
        let format = random_format(&mut rng);
        let value = rng.value();
        if let Ok(parsed) = PrintFormat::parse(&format) {
            for printer in [Printer::Host { decimal_point: "," }, Printer::Scaled] {
                let text = parsed.render(value, &mut scale, printer);
                let mut out = Vec::new();
                text.field.write_prefix(&mut out, 4096);
            }
        }
        let _ = format_duration(&format, value, ".");
        let _ = missing_time_text(&format);
        if let Some(spec) = NumberSpec::parse(&format) {
            let _ = scaled_number(&spec, value);
        }
    }
}

#[test]
fn huge_widths_and_precisions_stay_measured() {
    for format in [
        "%99999999999999999999lf",
        "%.99999999999999999999lf",
        "%18446744073709551615.18446744073709551615lg",
    ] {
        let parsed = PrintFormat::parse(format).unwrap();
        let mut scale = UnitScale::new(1024);
        let text = parsed.render(1.5, &mut scale, Printer::Host { decimal_point: "." });
        assert_eq!(text.field.to_string_within(1 << 20), None, "{format}");
        let mut out = Vec::new();
        text.field.write_prefix(&mut out, 16);
        assert_eq!(out.len(), 16);
    }
    assert_eq!(
        format_duration("%99999999999999999999.99999999999999999999D", 1.0, "."),
        Err(DurationError::WrongFormat)
    );
}
