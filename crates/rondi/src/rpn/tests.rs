//! Evaluator tests independent of the compatibility vectors, and a
//! deterministic no-panic sweep over random token sequences.

use super::*;

const A: [f64; 8] = [1.25, 15.5, 6.75, -3.0, f64::NAN, 2.5, -6.25, 7.0];
const B: [f64; 8] = [2.0, 3.0, -3.0, -2.0, f64::NAN, 0.0, 1.0, 2.0];
const VDEF_VALUE: f64 = 42.0;

fn lookup(name: &str) -> Option<usize> {
    ["a", "b", "v"]
        .iter()
        .position(|candidate| *candidate == name)
}

fn bind(element: usize) -> Source<'static> {
    match element {
        0 => Source::Series {
            values: &A,
            step: 10,
            first: 0,
        },
        1 => Source::Series {
            values: &B,
            step: 10,
            first: 0,
        },
        _ => Source::Constant(VDEF_VALUE),
    }
}

/// Evaluates `text` over `rows` rows at a 10 s step starting at t = 10.
fn run(text: &str, rows: usize) -> Result<Vec<f64>, RpnError> {
    let expression = Expression::parse(text, lookup)?;
    let mut evaluator = Evaluator::new(&expression, bind);
    let mut output: Vec<f64> = Vec::new();
    for index in 0..rows {
        let row = Row {
            time: 10 * (index as i64 + 1),
            index,
            step: 10,
            previous: output.last().copied().unwrap_or(f64::NAN),
        };
        output.push(evaluator.evaluate(&row)?);
    }
    Ok(output)
}

fn first(text: &str) -> Result<f64, RpnError> {
    run(text, 1).map(|values| values[0])
}

fn same(left: f64, right: f64) -> bool {
    (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
}

#[track_caller]
fn assert_value(text: &str, expected: f64) {
    let got = first(text).unwrap_or_else(|error| panic!("{text}: {error}"));
    assert!(
        same(got, expected),
        "{text}: got {got}, expected {expected}"
    );
}

#[test]
fn arithmetic_follows_ieee_and_c_conventions() {
    assert_value("a,POP,5.5,2,%", 1.5);
    assert_value("a,POP,-5,3,%", -2.0);
    assert_value("a,POP,1,0,/", f64::INFINITY);
    assert_value("a,POP,0,0,/", f64::NAN);
    assert_value("a,POP,0,-1,*,1,EXC,/", f64::NEG_INFINITY);
    assert_value("a,POP,2.5,ROUND", 3.0);
    assert_value("a,POP,-0.5,ROUND", -1.0);
    assert_value("a,POP,UNKN,3,ADDNAN", 3.0);
    assert_value("a,POP,INF,NEGINF,ADDNAN", f64::NAN);
    assert_value("a,POP,1,DEG2RAD", 0.017_453_295_2);
    assert_value("a,POP,0,0,POW", 1.0);
}

#[test]
fn comparisons_and_selection() {
    assert_value("a,POP,INF,0,GT", 1.0);
    assert_value("a,POP,UNKN,0,LT", f64::NAN);
    assert_value("a,POP,0,-1,*,0,EQ", 1.0);
    assert_value("a,POP,UNKN,1,2,IF", 2.0);
    assert_value("a,POP,INF,1,2,IF", 1.0);
    assert_value("a,POP,0,-1,*,0,MIN,1,EXC,/", f64::NEG_INFINITY);
    assert_value("a,POP,UNKN,4,MINNAN", 4.0);
    assert_value("a,POP,3,1,2,LIMIT", f64::NAN);
    assert_value("a,POP,INF,NEGINF,INF,LIMIT", f64::INFINITY);
}

#[test]
fn stack_operators() {
    assert_value("a,POP,1,2,3,2,COPY,+,+,+,+", 11.0);
    assert_value("a,POP,1,2,3,4,3,INDEX,+,+,+,+", 12.0);
    assert_value("a,POP,7,0,INDEX,+", 7.0);
    assert_value("a,b,DEPTH,+,+", 5.25);
    // 1,2,3,4,5,6,3,1,ROLL leaves 1,2,3,6,1,3.
    assert_value(
        "a,POP,1,2,3,4,5,6,3,1,ROLL,EXC,10,*,+,EXC,100,*,+,EXC,1000,*,+,EXC,10000,*,+,EXC,100000,*,+",
        123_613.0,
    );
    assert_eq!(first("a,POP,1,2,-2,1,ROLL"), Err(RpnError::OutOfMemory(-2)));
    assert_eq!(first("a,POP,1,2,3,ROLL"), Err(RpnError::StackUnderflow));
}

#[test]
fn set_operators() {
    assert_value(
        "a,POP,4,3,22.1,1,4,SORT,EXC,10,*,+,EXC,100,*,+,EXC,1000,*,+",
        1362.1,
    );
    assert_value("a,POP,1,2,3,4,4,AVG", 2.5);
    assert_value("a,POP,1,2,-1,AVG,POP,+", 3.0);
    assert_value("a,POP,5,1,UNKN,9,UNKN,3,6,MEDIAN", 4.0);
    assert_value("a,POP,1,2,3,4,4,SMAX", 4.0);
    assert_value("a,POP,UNKN,UNKN,2,SMIN", f64::NAN);
    assert_value("a,POP,1,2,3,4,5,50,5,PERCENT", 3.0);
    assert_value("a,POP,7,1,2,3,0,3,PERCENT,EXC,10,*,+", 77.0);
    assert_eq!(
        first("a,POP,1,2,3,101,3,PERCENT"),
        Err(RpnError::PercentileRange)
    );
    assert_eq!(first("a,POP,1,2,-1,MEDIAN"), Err(RpnError::StackUnderflow));
    let deviation = first("a,POP,1,2,3,4,4,STDEV").unwrap();
    assert!((deviation - 1.290_994_448_7).abs() < 1e-9);
}

#[test]
fn row_context_and_history() {
    assert_eq!(run("a,POP,COUNT", 3), Ok(vec![1.0, 2.0, 3.0]));
    assert_eq!(run("a,POP,TIME", 2), Ok(vec![10.0, 20.0]));
    assert_eq!(run("a,POP,STEPWIDTH", 1), Ok(vec![10.0]));
    let previous = run("a,POP,PREV,UN,0,PREV,IF,1,+", 4).unwrap();
    assert_eq!(previous, vec![1.0, 2.0, 3.0, 4.0]);
    let prev_a = run("PREV(a)", 3).unwrap();
    assert!(prev_a[0].is_nan());
    assert_eq!(&prev_a[1..], &A[..2]);
    // A VDEF is a constant in every row, also through PREV(name).
    assert_eq!(run("a,POP,v,PREV(v),+", 2), Ok(vec![84.0, 84.0]));
}

#[test]
fn trend_and_predict_windows() {
    let trend = run("b,20,TREND", 5).unwrap();
    assert!(trend[0].is_nan());
    assert_eq!(&trend[1..4], &[2.5, 0.0, -2.5]);
    assert!(trend[4].is_nan(), "TREND keeps unknown samples");
    let trend_nan = run("b,20,TRENDNAN", 5).unwrap();
    assert_eq!(trend_nan[4], -2.0);
    assert_eq!(first("a,1,+,30,TREND"), Err(RpnError::MalformedTrend));
    assert_eq!(first("v,30,TREND"), Err(RpnError::MalformedTrend));
    // Offset 0 with no window reads the next row's sample.
    let next = run("0,1,0,a,PREDICT", 3).unwrap();
    assert!(next[0].is_nan());
    assert_eq!(&next[1..], &A[2..4]);
    assert_eq!(
        first("-60,1,30,a,PREDICT"),
        Err(RpnError::NegativeShift(-60))
    );
    assert_eq!(
        first("60,-4,30,101,a,PREDICTPERC"),
        Err(RpnError::UnsupportedPercentile(101.0))
    );
    assert!(
        run("60,-4,30,UNKN,a,PREDICTPERC", 8)
            .unwrap()
            .iter()
            .all(|v| v.is_nan())
    );
    assert_eq!(first("60,1,30,3,PREDICT"), Err(RpnError::MalformedPredict));
    assert_eq!(first("60,0,30,a,PREDICT"), Err(RpnError::FinalStackSize));
    // An enormous zero-multiplier shift list is bounded instead of looping.
    assert!(matches!(
        run("0,-1e18,30,a,PREDICT", 8),
        Err(RpnError::OutOfMemory(_))
    ));
}

#[test]
fn final_stack_must_hold_one_value() {
    assert_eq!(first("a,a"), Err(RpnError::FinalStackSize));
    assert_eq!(first("a,POP"), Err(RpnError::FinalStackSize));
    assert_eq!(first("a,POP,+"), Err(RpnError::StackUnderflow));
}

#[test]
fn error_texts_match_the_specification() {
    assert_eq!(
        RpnError::Unrecognized("x,+".into()).to_string(),
        "don't understand 'x,+'"
    );
    assert_eq!(
        RpnError::UnsupportedPercentile(101.0).to_string(),
        "unsupported percentile: 101.000000"
    );
    assert_eq!(
        RpnError::NegativeShift(-60).to_string(),
        "negative shift step not allowed: -60"
    );
    assert_eq!(
        RpnError::OutOfMemory(-2).to_string(),
        "RPN out of memory (allocating -2 objects)"
    );
}

/// A small deterministic generator (xorshift64*), so the sweep needs no
/// extra dependency and replays identically.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

#[test]
fn random_token_sequences_never_panic() {
    let mut vocabulary: Vec<&str> = Operator::NAMES.iter().map(|(name, _)| *name).collect();
    vocabulary.extend([
        "a",
        "b",
        "v",
        "PREV(a)",
        "PREV(v)",
        "PREV(zz)",
        "PREV(a",
        "0",
        "1",
        "2",
        "3",
        "-1",
        "-2",
        "2.9",
        "50",
        "101",
        "1e300",
        "-1e300",
        "1e20",
        "-1e20",
        "2147483647",
        "-2147483648",
        "3e9",
        "1e1100",
        "UNKN",
        "INF",
        "NEGINF",
        "0.5",
        "",
        " ",
        "x",
    ]);
    let mut random = Random(0x5eed_1234_abcd_0042);
    let mut evaluated = 0;
    for _ in 0..20_000 {
        let length = 1 + random.next() % 12;
        let tokens: Vec<&str> = (0..length).map(|_| random.pick(&vocabulary)).collect();
        let text = tokens.join(",");
        let Ok(expression) = Expression::parse(&text, lookup) else {
            continue;
        };
        let mut evaluator = Evaluator::new(&expression, bind);
        for index in 0..6 {
            let row = Row {
                time: 10 * (index as i64 + 1),
                index,
                step: 10,
                previous: f64::NAN,
            };
            if evaluator.evaluate(&row).is_err() {
                break;
            }
            evaluated += 1;
        }
    }
    assert!(evaluated > 1_000, "only {evaluated} rows evaluated");
}

#[test]
fn random_text_never_panics_the_parser() {
    let alphabet: Vec<char> = "ab,.e+-0123456789()PREVTNDUXA ".chars().collect();
    let mut random = Random(0x0dd_ba11_cafe_f00d);
    for _ in 0..20_000 {
        let length = random.next() % 24;
        let text: String = (0..length)
            .map(|_| alphabet[(random.next() % alphabet.len() as u64) as usize])
            .collect();
        let _ = Expression::parse(&text, lookup);
    }
}
