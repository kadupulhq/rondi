use super::*;

const HEAD: &str = "<rrd><version>0003</version><step>60</step><lastupdate>0</lastupdate>";
const DS: &str = "<ds><name>a</name><type>GAUGE</type></ds>";
const RRA: &str = "<rra><cf>AVERAGE</cf><pdp_per_row>1</pdp_per_row><params><xff>0.5</xff></params>\
    <cdp_prep><ds><value>1</value></ds></cdp_prep>\
    <database><row><v>1</v></row><row><v>2</v></row></database></rra>";

fn doc(head: &str, ds: &str, rra: &str) -> String {
    format!("{head}{ds}{rra}</rrd>")
}

fn run(xml: &str) -> Restored {
    run_with(xml.as_bytes(), false, false)
}

fn run_with(xml: &[u8], stdin: bool, range_check: bool) -> Restored {
    let input = if stdin {
        RestoreInput::Stdin(xml)
    } else {
        RestoreInput::File {
            name: "in.xml",
            xml,
        }
    };
    restore_bytes(input, range_check, &mut std::io::sink())
}

fn message(xml: &str) -> String {
    run(xml)
        .message
        .map(|error| error.to_string())
        .unwrap_or_default()
}

fn word(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn float(bytes: &[u8], offset: usize) -> f64 {
    f64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// Offset of the first row value for a file with `ds` sources and `rra`
/// archives.
fn rows_at(ds: usize, rra: usize) -> usize {
    128 + 120 * ds + 120 * rra + 16 + 112 * ds + 80 * ds * rra + 8 * rra
}

#[test]
fn tokens_keep_the_first_word() {
    assert_eq!(token(" 7 "), "7");
    assert_eq!(token("2 3"), "2");
    assert_eq!(token("\n\t\r "), "");
    assert_eq!(token("\u{a0}1"), "\u{a0}1");
}

#[test]
fn doubles_follow_the_decimal_rules() {
    let mut errno = 0;
    for (text, value) in [
        ("1e", 1.0),
        ("1e+", 1.0),
        ("5.", 5.0),
        (".5", 0.5),
        ("-2.5e1", -25.0),
        ("+.5e1", 5.0),
        ("00012", 12.0),
    ] {
        assert_eq!(convert_double(text, &mut errno), Ok(value), "{text}");
    }
    assert_eq!(errno, 0);
    for text in ["-", "e5", ".", "", "U"] {
        errno = 0;
        assert_eq!(convert_double(text, &mut errno), Err(BadDouble), "{text}");
        assert_eq!(errno, libc::ERANGE, "{text}");
    }
    for text in ["0x10", "2junk", "1,5", "1_0"] {
        errno = 0;
        assert_eq!(convert_double(text, &mut errno), Err(BadDouble), "{text}");
        assert_eq!(errno, 0, "{text} keeps the error state");
    }
    for text in ["1e1025", "1e-1022", "1.5e-1021"] {
        errno = 0;
        assert_eq!(convert_double(text, &mut errno), Err(BadDouble), "{text}");
        assert_eq!(errno, libc::ERANGE);
    }
    errno = 0;
    assert_eq!(convert_double("1e309", &mut errno), Ok(f64::INFINITY));
    assert_eq!(errno, libc::ERANGE);
    // Only an exponent whose highest power of ten overflows multiplies
    // zero by infinity; `0e400` stays 0 in the reference.
    assert_eq!(convert_double("0e400", &mut errno), Ok(0.0));
    assert!(convert_double("0e600", &mut errno).unwrap().is_nan());
    assert!(convert_double("1e1024", &mut errno).unwrap().is_infinite());
    assert_eq!(convert_double("1e-1021", &mut errno), Ok(0.0));
}

#[test]
fn doubles_recognise_special_spellings_anywhere() {
    let mut errno = 0;
    for text in ["NaN", "nan", "banana", "nanx", "-nan"] {
        assert!(convert_double(text, &mut errno).unwrap().is_nan(), "{text}");
    }
    for text in ["-inf", "-INF", "-Infinity"] {
        assert_eq!(convert_double(text, &mut errno), Ok(f64::NEG_INFINITY));
    }
    for text in ["inf", "Inf", "xinfx", "information"] {
        assert_eq!(convert_double(text, &mut errno), Ok(f64::INFINITY));
    }
}

#[test]
fn version_numbers_use_the_leading_digits() {
    assert_eq!(version_number(b"0003"), 3);
    assert_eq!(version_number(b"3"), 3);
    assert_eq!(version_number(b"abc"), 0);
    assert_eq!(version_number(b""), 0);
    assert_eq!(version_number(b"12x"), 12);
}

#[test]
fn library_messages_are_flattened_and_cut() {
    assert_eq!(library_text("Extra content"), "Extra content ");
    assert_eq!(library_text("a\nb\n\tc"), "a b ");
    assert_eq!(library_text("\u{1}x"), "\u{1}x ");
}

#[test]
fn messages_have_the_reference_wording() {
    let cases = [
        (
            RestoreError::ExpectedElement {
                line: 3,
                found: "x".into(),
                expected: "rrd".into(),
            },
            "line 3: expected <x> element but found <rrd>",
        ),
        (
            RestoreError::ExpectedEndElement {
                line: 4,
                expected: "step".into(),
                found: "x".into(),
            },
            "line 4: expected </step> end element but found <x>",
        ),
        (
            RestoreError::UnknownTag {
                container: RestoreScope::Rra,
                line: 9,
                name: "/params".into(),
            },
            "line 9: parse_tag_rra: Unknown tag: /params",
        ),
        (
            RestoreError::UnknownTag {
                container: RestoreScope::Ds,
                line: 9,
                name: "bogus".into(),
            },
            "parse_tag_ds: Unknown tag: bogus",
        ),
        (
            RestoreError::TooLong {
                token: "00031".into(),
                limit: 5,
            },
            "'00031' is longer than 5",
        ),
        (
            RestoreError::Integer {
                line: 2,
                kind: RestoreInteger::Time,
                token: "x".into(),
                errno: libc::ERANGE,
            },
            "ling 2: get_xml_time_t from 'x' ",
        ),
        (
            RestoreError::Xml("Document is empty".into()),
            "error reading/parsing XML: Document is empty ",
        ),
        (
            RestoreError::Create {
                name: "out.rrd".into(),
                reason: "File exists".into(),
            },
            "creating 'out.rrd': File exists",
        ),
    ];
    for (error, text) in cases {
        assert!(error.to_string().starts_with(text), "{error}");
    }
}

#[test]
fn a_small_dump_lays_out_like_create() {
    let restored = run(&doc(HEAD, DS, RRA));
    assert!(restored.message.is_none(), "{:?}", restored.message);
    let bytes = restored.bytes.unwrap();
    assert_eq!(&bytes[..9], b"RRD\x000003\0");
    assert_eq!(float(&bytes, 16), FLOAT_COOKIE);
    assert_eq!(
        (word(&bytes, 24), word(&bytes, 32), word(&bytes, 40)),
        (1, 1, 60)
    );
    assert_eq!(&bytes[128..129], b"a");
    assert_eq!(&bytes[148..153], b"GAUGE");
    let rra = 128 + 120;
    assert_eq!(&bytes[rra..rra + 7], b"AVERAGE");
    assert_eq!((word(&bytes, rra + 24), word(&bytes, rra + 32)), (2, 1));
    assert_eq!(float(&bytes, rra + 40), 0.5);
    let cdp = rra + 120 + 16 + 112;
    assert_eq!(float(&bytes, cdp), 1.0);
    // The newest row sits at the pointer, the oldest just after it.
    let pointer = cdp + 80;
    assert_eq!(word(&bytes, pointer), 1);
    let rows = rows_at(1, 1);
    assert_eq!((float(&bytes, rows), float(&bytes, rows + 8)), (1.0, 2.0));
    assert_eq!(bytes.len(), rows + 16);
}

#[test]
fn versions_below_three_are_stored_as_0003() {
    for (version, stored) in [
        ("0001", &b"0003\0"[..]),
        ("0002", b"0003\0"),
        ("3", b"3\0\0\0\0"),
        ("0006", b"0006\0"),
    ] {
        let head = HEAD.replace("0003", version);
        let rra = if version == "0001" {
            RRA.replace("<params><xff>0.5</xff></params>", "<xff>0.5</xff>")
        } else {
            RRA.to_owned()
        };
        let bytes = run(&doc(&head, DS, &rra)).bytes.unwrap();
        assert_eq!(&bytes[4..9], stored, "{version}");
    }
}

#[test]
fn walk_failures_report_their_messages() {
    let cases = [
        (
            "<x></x>".to_owned(),
            "line 1: expected <x> element but found <rrd>",
        ),
        (
            doc(HEAD, "<ds><name>a</name><bogus/></ds>", ""),
            "parse_tag_ds: Unknown tag: bogus",
        ),
        (
            doc(HEAD, DS, "<rra><bogus/></rra>"),
            "line 1: parse_tag_rra: Unknown tag: bogus",
        ),
        (
            doc(HEAD, "", &format!("{RRA}{DS}")),
            "line 1: expected <ds> element but found </cdp_prep>",
        ),
        (
            format!("{HEAD}<rra><cf>AVERAGE</cf></rra>{DS}</rrd>"),
            "parse_tag_ds: All data source definitions MUST precede the RRA definitions!",
        ),
        (
            doc(HEAD, "<ds><name>abcdefghijklmnopqrst</name></ds>", ""),
            "'abcdefghijklmnopqrst' is longer than 20",
        ),
        (
            doc(HEAD, "<ds><type>gauge</type></ds>", ""),
            "parse_tag_ds_type: Unknown data source type: gauge",
        ),
        (
            doc(HEAD, DS, &RRA.replace("AVERAGE", "AVG")),
            "parse_tag_rra_cf: Unknown consolidation function: AVG",
        ),
        (
            doc(
                HEAD,
                DS,
                &RRA.replace(
                    "<database><row><v>1</v></row><row><v>2</v></row>",
                    "<database>",
                ),
            ),
            if cfg!(target_arch = "x86_64") {
                "restore of an RRA without rows is not supported"
            } else {
                "parse_tag_rra: malloc failed."
            },
        ),
        (
            doc(HEAD, DS, &RRA.replace("</row><row>", "</row><x/><row>")),
            "line 1: found unexpected tag: x",
        ),
        (
            doc(HEAD, DS, &RRA.replace("<v>2</v>", "")),
            "line 1: expected </row> element but found <v>",
        ),
        (
            doc(
                HEAD,
                DS,
                &RRA.replace("<value>1</value>", "<value>1</value><bogus/>"),
            ),
            "parse_tag_rra_cdp_prep: Unknown tag: bogus",
        ),
        (
            doc(&HEAD.replace("<step>60", "<step>60<x/>"), DS, RRA),
            "line 1: expected </step> end element but found <x>",
        ),
        (
            doc(
                &HEAD.replace("<step>60</step>", "<step><x/></step>"),
                DS,
                RRA,
            ),
            "line 1: expected a value but found a <x> element",
        ),
        (
            doc(HEAD, "junk", ""),
            "line 1: expected element but found text 'junk'",
        ),
        (
            "<rrd/>".to_owned(),
            "the xml ended while we were looking for an element",
        ),
    ];
    for (xml, expected) in cases {
        assert_eq!(message(&xml), expected, "{xml}");
    }
}

#[test]
fn fatal_failures_write_nothing_but_recoverable_ones_do() {
    let fatal = run(&doc(HEAD, "<ds><bogus/></ds>", ""));
    assert!(fatal.bytes.is_none());
    let recoverable = run(&doc(
        HEAD,
        DS,
        &RRA.replace("<xff>0.5</xff>", "<xff>U</xff>"),
    ));
    let bytes = recoverable.bytes.unwrap();
    assert_eq!(float(&bytes, 128 + 120 + 40), 0.0);
    assert!(
        recoverable
            .message
            .unwrap()
            .to_string()
            .starts_with("ling 1: get_xml_double from 'U' ")
    );
    let unknown = run(&doc(
        HEAD,
        DS,
        &RRA.replace("<xff>0.5</xff>", "<xff>0.5</xff><bogus></bogus>"),
    ));
    assert!(unknown.bytes.is_some());
    assert_eq!(
        unknown.message.unwrap().to_string(),
        "line 1: parse_tag_rra_params: Unknown tag: bogus"
    );
    let text_in_root = run(&doc(HEAD, DS, &format!("{RRA}junk")));
    assert!(text_in_root.bytes.is_some());
    assert!(text_in_root.message.is_some());
}

#[test]
fn empty_params_or_cdp_records_stop_silently() {
    for rra in [
        RRA.replace("<xff>0.5</xff>", ""),
        RRA.replace("<xff>0.5</xff>", "<!-- c --> "),
        RRA.replace("<value>1</value>", ""),
    ] {
        let restored = run(&doc(HEAD, DS, &rra));
        assert!(
            restored.bytes.is_none() && restored.message.is_none(),
            "{rra}"
        );
    }
    // An earlier recoverable message survives the quiet stop.
    let bad_xff = RRA.replace("<xff>0.5</xff>", "<xff>U</xff>");
    let empty = RRA.replace("<xff>0.5</xff>", "");
    let restored = run(&doc(HEAD, DS, &format!("{bad_xff}{empty}")));
    assert!(restored.bytes.is_none());
    assert!(restored.message.is_some());
}

#[test]
fn legacy_params_values_use_slot_zero() {
    let restored = run(&doc(
        HEAD,
        DS,
        &RRA.replace("<xff>0.5</xff>", "<value>0.25</value><value>0.75</value>"),
    ));
    let bytes = restored.bytes.unwrap();
    assert_eq!(float(&bytes, 128 + 120 + 40), 0.75);
    assert!(
        restored
            .message
            .unwrap()
            .to_string()
            .starts_with("ling 1: get_xml_double from '' ")
    );
}

#[test]
fn version_gates_flat_xff_and_params() {
    let flat = RRA.replace("<params><xff>0.5</xff></params>", "<xff>0.5</xff>");
    assert_eq!(
        message(&doc(HEAD, DS, &flat)),
        "line 1: parse_tag_rra: Unknown tag: xff"
    );
    let v1 = HEAD.replace("0003", "0001");
    assert!(run(&doc(&v1, DS, &flat)).message.is_none());
    assert_eq!(
        message(&doc(&v1, DS, RRA)),
        "line 1: parse_tag_rra: Unknown tag: params"
    );
}

#[test]
fn holt_winters_slots_and_history_are_stored() {
    let params = "<params><hw_alpha>0.1</hw_alpha><hw_beta>0.2</hw_beta>\
        <dependent_rra_idx>3</dependent_rra_idx><window_len>9</window_len>\
        <failure_threshold>7</failure_threshold></params>";
    let cdp = "<cdp_prep><ds><history>1011x111111111</history><last_nan_count>5</last_nan_count>\
        <primary_value>2.5</primary_value></ds></cdp_prep>";
    let rra = RRA
        .replace("<params><xff>0.5</xff></params>", params)
        .replace("<cdp_prep><ds><value>1</value></ds></cdp_prep>", cdp)
        .replace("AVERAGE", "FAILURES");
    let bytes = run(&doc(HEAD, DS, &rra)).bytes.unwrap();
    let slots = 128 + 120 + 40;
    assert_eq!(float(&bytes, slots + 8), 0.1);
    assert_eq!(float(&bytes, slots + 16), 0.2);
    assert_eq!(word(&bytes, slots + 24), 3);
    assert_eq!(word(&bytes, slots + 32), 9);
    assert_eq!(word(&bytes, slots + 40), 7);
    let record = 128 + 120 + 120 + 16 + 112;
    assert_eq!(&bytes[record..record + 10], &[1, 0, 1, 1, 0, 1, 1, 1, 1, 1]);
    assert_eq!(word(&bytes, record + 56), 5);
    assert_eq!(float(&bytes, record + 64), 2.5);
}

#[test]
fn range_check_turns_out_of_range_rows_unknown() {
    let ds = "<ds><name>a</name><type>GAUGE</type><min>0</min><max>10</max></ds>";
    let rra = RRA
        .replace("<v>1</v>", "<v>-1</v>")
        .replace("<v>2</v>", "<v>10</v>");
    let xml = doc(HEAD, ds, &rra);
    let rows = rows_at(1, 1);
    let checked = run_with(xml.as_bytes(), false, true).bytes.unwrap();
    assert!(float(&checked, rows).is_nan());
    assert_eq!(float(&checked, rows + 8), 10.0);
    let unchecked = run(&xml).bytes.unwrap();
    assert_eq!(float(&unchecked, rows), -1.0);
}

#[test]
fn integer_fields_follow_strtoul() {
    for (step, expected) in [("0x1F", 31), ("012", 10), ("12abc", 12), ("-1", u64::MAX)] {
        let head = HEAD.replace("<step>60", &format!("<step>{step}"));
        let bytes = run(&doc(&head, DS, RRA)).bytes.unwrap();
        assert_eq!(word(&bytes, 40), expected, "{step}");
    }
    let head = HEAD.replace("<step>60", "<step>18446744073709551616");
    assert!(
        message(&doc(&head, DS, RRA))
            .starts_with("ling 1: get_xml_ulong from '18446744073709551616' ")
    );
}

#[test]
fn errno_carries_from_the_previous_conversion() {
    let ds = |min: &str| {
        format!("<ds><name>a</name><type>GAUGE</type><min>{min}</min><max>2junk</max></ds>")
    };
    let after_overflow = message(&doc(HEAD, &ds("1e309"), RRA));
    assert!(
        after_overflow.ends_with(&strerror(libc::ERANGE)),
        "{after_overflow}"
    );
    let after_plain = message(&doc(HEAD, &ds("1"), RRA));
    assert!(after_plain.ends_with(&strerror(0)), "{after_plain}");
}

#[test]
fn computed_sources_are_refused() {
    let ds = "<ds><name>a</name><type>COMPUTE</type></ds>";
    assert!(message(&doc(HEAD, ds, "")).contains("not supported by Rondi"));
    let cdef = "<ds><name>a</name><cdef>a,2,*</cdef></ds>";
    assert!(message(&doc(HEAD, cdef, "")).contains("not supported by Rondi"));
}

#[test]
fn stdin_input_ends_at_ctrl_z() {
    let mut xml = doc(HEAD, DS, RRA).into_bytes();
    xml.extend_from_slice(b"\n\x1a\nnot xml at all");
    let restored = run_with(&xml, true, false);
    assert!(restored.message.is_none(), "{:?}", restored.message);
    assert!(run_with(&xml, false, false).message.is_some());
}

#[test]
fn oversized_layouts_are_refused_before_allocation() {
    let dump = Dump {
        sources: vec![DataSource::default(); 4],
        archives: vec![Archive {
            cf: b"AVERAGE".to_vec(),
            pdp_per_row: 1,
            params: [0; 80],
            cdp: vec![[0; 80]; 4],
            values: Vec::new(),
            rows: u64::MAX / 2,
        }],
        ..Dump::default()
    };
    assert_eq!(serialize(&dump), Err(RestoreError::TooLarge));
}

#[test]
fn many_rows_restore_in_proportion() {
    let rows: String = (0..20_000)
        .map(|i| format!("<row><v>{i}</v></row>"))
        .collect();
    let rra = RRA.replace("<row><v>1</v></row><row><v>2</v></row>", &rows);
    let bytes = run(&doc(HEAD, DS, &rra)).bytes.unwrap();
    assert_eq!(bytes.len(), rows_at(1, 1) + 20_000 * 8);
    assert_eq!(float(&bytes, rows_at(1, 1) + 19_999 * 8), 19_999.0);
}

/// A small deterministic generator, so failures reproduce.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

const PIECES: &[&str] = &[
    "<",
    ">",
    "</",
    "/>",
    "<!--",
    "-->",
    "<![CDATA[",
    "]]>",
    "<?pi",
    "?>",
    "&amp;",
    "&#1;",
    "&nope;",
    "\n",
    "\t",
    " ",
    "\0",
    "\x1a",
    "<row>",
    "</row>",
    "<v>",
    "</v>",
    "<ds>",
    "</ds>",
    "<rra>",
    "</rra>",
    "<params>",
    "</params>",
    "<cdp_prep>",
    "</cdp_prep>",
    "<database>",
    "</database>",
    "NaN",
    "1e309",
    "-1",
    "0x10",
    "\u{e9}",
    "<!DOCTYPE rrd [<!ENTITY e \"x\">]>",
    "<?xml version=\"1.0\"?>",
];

fn mutate(rng: &mut XorShift, base: &[u8]) -> Vec<u8> {
    let mut xml = base.to_vec();
    for _ in 0..=rng.below(4) {
        let at = rng.below(xml.len() + 1);
        match rng.below(5) {
            0 => {
                let end = (at + rng.below(40)).min(xml.len());
                xml.drain(at..end);
            }
            1 => {
                let piece = PIECES[rng.below(PIECES.len())].as_bytes();
                xml.splice(at..at, piece.iter().copied());
            }
            2 => xml.truncate(at),
            3 => {
                let end = (at + rng.below(200)).min(xml.len());
                let copy = xml[at..end].to_vec();
                let to = rng.below(xml.len() + 1);
                xml.splice(to..to, copy);
            }
            _ => {
                if at < xml.len() {
                    xml[at] = rng.next() as u8;
                }
            }
        }
    }
    xml
}

/// Mutated dumps never panic or hang, whatever the input source.
#[test]
fn mutated_dumps_neither_panic_nor_hang() {
    let bases = [
        doc(HEAD, DS, RRA),
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE rrd SYSTEM \"x.dtd\">\n{}",
            doc(HEAD, DS, RRA).replace("><", ">\n\t<")
        ),
    ];
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    let started = std::time::Instant::now();
    let (mut written, mut refused) = (0, 0);
    for round in 0..3_000 {
        let base = bases[round % bases.len()].as_bytes();
        let xml = mutate(&mut rng, base);
        let begun = std::time::Instant::now();
        for (stdin, range_check) in [(false, false), (true, true)] {
            let restored = run_with(&xml, stdin, range_check);
            match &restored.bytes {
                Some(bytes) => {
                    assert!(bytes.len() >= 128 && &bytes[..4] == b"RRD\0");
                    written += 1;
                }
                None => refused += 1,
            }
        }
        assert!(
            begun.elapsed() < std::time::Duration::from_secs(10),
            "slow input: {:?}",
            String::from_utf8_lossy(&xml)
        );
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(300));
    // Both outcomes must be common, or the mutations test nothing.
    assert!(
        written > 300 && refused > 300,
        "{written} written, {refused} refused"
    );
}
