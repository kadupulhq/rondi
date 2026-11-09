//! The expressions of `COMPUTE` data sources.
//!
//! A computed source stores its expression in the 80 parameter bytes of its
//! definition record as a short list of fixed-size nodes. This module turns
//! the definition text into that list, reads it back from a file, shows it
//! as text for `info` and `dump`, and evaluates it once per completed step.
//!
//! The behavior is specified in docs/compatibility/compute.md, whose rule
//! numbers (CP-n) are cited below; it was written from that specification
//! and the vectors only. Parsing and evaluation reuse the CDEF language of
//! this module's parent.

use std::fmt;

use super::eval::{Evaluator, Row, Source};
use super::{Expression, Operator, RpnError, Token, parse};
use crate::rrd_binary::rrd_nan;

/// Most nodes an expression may have, not counting the end marker (CP-3).
pub(crate) const MAX_NODES: usize = 19;
/// Bytes of a definition record that hold the nodes (CP-5).
pub(crate) const PROGRAM_LEN: usize = 80;
/// Bytes per stored node (CP-5).
const NODE_LEN: usize = 4;

/// Stored operator codes (CP-6). They are part of the file format.
const CONSTANT_CODE: u8 = 0;
const VARIABLE_CODE: u8 = 1;
const END_CODE: u8 = 32;
const PREVIOUS_OF_CODE: u8 = 36;
const OPERATOR_CODES: &[(Operator, u8)] = &[
    (Operator::Infinity, 2),
    (Operator::NegativeInfinity, 4),
    (Operator::Unknown, 5),
    (Operator::Now, 6),
    (Operator::Add, 8),
    (Operator::Remainder, 9),
    (Operator::Subtract, 10),
    (Operator::Multiply, 11),
    (Operator::Divide, 12),
    (Operator::Sin, 13),
    (Operator::Dup, 14),
    (Operator::Exchange, 15),
    (Operator::Pop, 16),
    (Operator::Cos, 17),
    (Operator::Log, 18),
    (Operator::Exp, 19),
    (Operator::Less, 20),
    (Operator::LessEqual, 21),
    (Operator::Greater, 22),
    (Operator::GreaterEqual, 23),
    (Operator::Equal, 24),
    (Operator::If, 25),
    (Operator::Min, 26),
    (Operator::Max, 27),
    (Operator::Limit, 28),
    (Operator::Floor, 29),
    (Operator::Ceil, 30),
    (Operator::IsUnknown, 31),
    (Operator::NotEqual, 34),
    (Operator::IsInfinite, 35),
    (Operator::Atan, 38),
    (Operator::Sqrt, 39),
    (Operator::Sort, 40),
    (Operator::Reverse, 41),
    (Operator::Atan2, 44),
    (Operator::RadiansToDegrees, 45),
    (Operator::DegreesToRadians, 46),
    (Operator::Average, 49),
    (Operator::Abs, 50),
    (Operator::AddNan, 51),
    (Operator::MinNan, 52),
    (Operator::MaxNan, 53),
    (Operator::Median, 54),
    (Operator::Depth, 56),
    (Operator::Copy, 57),
    (Operator::Roll, 58),
    (Operator::Index, 59),
    (Operator::SetMin, 65),
    (Operator::SetMax, 66),
    (Operator::StandardDeviation, 67),
    (Operator::Percentile, 68),
    (Operator::Power, 69),
    (Operator::Round, 70),
];

/// Why a definition's expression was refused. `Display` gives the exact
/// text RRDtool prints after `ERROR: ` (CP-1, CP-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompileError {
    Parse,
    UnsupportedOperator,
    TooManyNodes(usize),
    ConstantRange,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse => f.write_str("failed to parse computed data source"),
            Self::UnsupportedOperator => f.write_str(
                "operators TIME LTIME STEPWIDTH PREV NEW* COUNT TREND TRENDNAN PREDICT \
                 PREDICTSIGMA PREDICTPERC are not supported with DS COMPUTE",
            ),
            Self::TooManyNodes(count) => write!(
                f,
                "Maximum {MAX_NODES} RPN nodes permitted. Got {count} RPN nodes at present."
            ),
            // Compatibility: the bounds in the text are not the accepted
            // range, which includes -32768 and 32767.
            // Provenance: black-box compatibility test; see
            // docs/compatibility/compute.md (CP-1).
            Self::ConstantRange => {
                f.write_str("constants must be integers in the interval (-32768, 32767)")
            }
        }
    }
}

/// Why evaluation failed when a step completed (CP-16). `Display` gives the
/// text RRDtool prints after the file name.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EvalError {
    Rpn(RpnError),
    PreviousOf,
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpn(error) => error.fmt(f),
            // Compatibility: `PREV(name)` compiles but fails every
            // evaluation with this text.
            // Provenance: black-box compatibility test; see
            // docs/compatibility/compute.md (CP-16).
            Self::PreviousOf => f.write_str("VDEF made it into rpn_calc... aborting"),
        }
    }
}

/// One stored element of a compiled expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    Constant(i16),
    /// A data source by index.
    Variable(u16),
    /// `PREV(name)`, kept by index.
    PreviousOf(u16),
    Operator(Operator),
}

/// A compiled `COMPUTE` expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComputeProgram {
    nodes: Vec<Node>,
}

/// Operators that depend on time or history, which a computed source does
/// not have (CP-1 item 2).
fn is_time_dependent(operator: Operator) -> bool {
    use Operator as O;
    matches!(
        operator,
        O::Time
            | O::LocalTime
            | O::StepWidth
            | O::Previous
            | O::NewDay
            | O::NewWeek
            | O::NewMonth
            | O::NewYear
            | O::Count
            | O::Trend
            | O::TrendNan
            | O::Predict
            | O::PredictSigma
            | O::PredictPercentile
    )
}

fn operator_code(operator: Operator) -> Option<u8> {
    OPERATOR_CODES
        .iter()
        .find(|(candidate, _)| *candidate == operator)
        .map(|&(_, code)| code)
}

fn operator_from_code(code: u8) -> Option<Operator> {
    OPERATOR_CODES
        .iter()
        .find(|(_, candidate)| *candidate == code)
        .map(|&(operator, _)| operator)
}

fn operator_name(operator: Operator) -> &'static str {
    Operator::NAMES
        .iter()
        .find(|(_, candidate)| *candidate == operator)
        .map_or("", |&(name, _)| name)
}

/// The stored constant for a number token, if it is an integer in range.
fn constant(value: f64) -> Option<i16> {
    let in_range = (f64::from(i16::MIN)..=f64::from(i16::MAX)).contains(&value);
    // The range check rejects NaN, so the cast below is exact.
    (in_range && value.fract() == 0.0).then_some(value as i16)
}

impl ComputeProgram {
    /// Compiles the text after `DS:<name>:COMPUTE:`. `lookup` resolves a
    /// data source name to its index; it knows only the sources a
    /// reference may name (CP-1 item 1, CP-19).
    ///
    /// The checks run as whole-expression passes in the order of CP-4, so
    /// the first failing pass decides wherever its fault is.
    pub(crate) fn compile(
        text: &str,
        lookup: impl Fn(&str) -> Option<usize>,
    ) -> Result<Self, CompileError> {
        // Pass 1: syntax and names. Compatibility: a number in the last
        // position is not read as a number (CP-1 item 6); the CDEF
        // tokenizer already behaves that way.
        // Provenance: black-box compatibility test; see
        // docs/compatibility/compute.md (CP-1, CP-4).
        let index = |name: &str| lookup(name).filter(|index| *index <= i16::MAX as usize);
        let tokens = parse::parse(text, &index).map_err(|_| CompileError::Parse)?;
        // Pass 2: the operator restriction.
        if tokens
            .iter()
            .any(|token| matches!(token, Token::Operator(operator) if is_time_dependent(*operator)))
        {
            return Err(CompileError::UnsupportedOperator);
        }
        // Pass 3: the node limit.
        if tokens.len() > MAX_NODES {
            return Err(CompileError::TooManyNodes(tokens.len()));
        }
        // Pass 4: constants.
        let nodes = tokens
            .iter()
            .map(|token| {
                Ok(match *token {
                    Token::Number(value) => {
                        Node::Constant(constant(value).ok_or(CompileError::ConstantRange)?)
                    }
                    // The lookup above caps indices at i16::MAX.
                    Token::Variable(index) => Node::Variable(index as u16),
                    Token::PreviousOf(index) => Node::PreviousOf(index as u16),
                    Token::Operator(operator) => Node::Operator(operator),
                })
            })
            .collect::<Result<Vec<_>, CompileError>>()?;
        Ok(Self { nodes })
    }

    /// The 80 stored bytes: the nodes, the end marker, then zeros (CP-5).
    pub(crate) fn to_bytes(&self) -> [u8; PROGRAM_LEN] {
        let mut bytes = [0_u8; PROGRAM_LEN];
        let end = (END_CODE, 0_u16);
        let encoded = self.nodes.iter().map(|node| match *node {
            Node::Constant(value) => (CONSTANT_CODE, value as u16),
            Node::Variable(index) => (VARIABLE_CODE, index),
            Node::PreviousOf(index) => (PREVIOUS_OF_CODE, index),
            // Every operator that passes compilation has a code.
            Node::Operator(operator) => (operator_code(operator).unwrap_or(END_CODE), 0),
        });
        for (slot, (code, value)) in bytes
            .as_chunks_mut::<NODE_LEN>()
            .0
            .iter_mut()
            .zip(encoded.chain(std::iter::once(end)))
        {
            slot[0] = code;
            slot[2..4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    /// Reads stored nodes up to the end marker. `None` when a node has an
    /// unknown code or no end marker is found.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let mut nodes = Vec::new();
        for slot in bytes.as_chunks::<NODE_LEN>().0 {
            let value = u16::from_le_bytes([slot[2], slot[3]]);
            let node = match slot[0] {
                END_CODE => return Some(Self { nodes }),
                CONSTANT_CODE => Node::Constant(value as i16),
                VARIABLE_CODE => Node::Variable(value),
                PREVIOUS_OF_CODE => Node::PreviousOf(value),
                code => Node::Operator(operator_from_code(code)?),
            };
            nodes.push(node);
        }
        None
    }

    /// The expression as `info`, `dump` and restore show it (CP-8).
    /// `name_of` gives a data source's stored name by index.
    pub(crate) fn render<'n>(&self, name_of: impl Fn(usize) -> Option<&'n [u8]>) -> Vec<u8> {
        let mut text = Vec::new();
        for (position, node) in self.nodes.iter().enumerate() {
            if position > 0 {
                text.push(b',');
            }
            match *node {
                Node::Constant(value) => text.extend_from_slice(value.to_string().as_bytes()),
                Node::Variable(index) => {
                    text.extend_from_slice(name_of(usize::from(index)).unwrap_or_default());
                }
                // Compatibility: `PREV(name)` is shown as an empty token.
                // Provenance: black-box compatibility test; see
                // docs/compatibility/compute.md (CP-8).
                Node::PreviousOf(_) => {}
                Node::Operator(operator) => {
                    text.extend_from_slice(operator_name(operator).as_bytes());
                }
            }
        }
        text
    }

    /// Evaluates the expression with each variable bound to `values[index]`
    /// (CP-12, CP-13). A `NaN` result is the file's unknown.
    pub(crate) fn evaluate(&self, values: &[f64]) -> Result<f64, EvalError> {
        // Compatibility: evaluation runs up to the first `PREV(name)` and
        // fails there, unless an earlier operator has already failed.
        // Provenance: black-box compatibility test; see
        // docs/compatibility/compute.md (CP-16).
        let stop = self
            .nodes
            .iter()
            .position(|node| matches!(node, Node::PreviousOf(_)));
        let tokens = self.nodes[..stop.unwrap_or(self.nodes.len())]
            .iter()
            .map(|node| match *node {
                Node::Constant(value) => Token::Number(f64::from(value)),
                Node::Variable(index) | Node::PreviousOf(index) => {
                    Token::Variable(usize::from(index))
                }
                Node::Operator(operator) => Token::Operator(operator),
            })
            .collect();
        let expression = Expression { tokens };
        let mut evaluator = Evaluator::new(&expression, |index| {
            Source::Constant(values.get(index).copied().unwrap_or_else(rrd_nan))
        });
        // None of the operators a computed source may use reads the row.
        let row = Row {
            time: 0,
            index: 0,
            step: 1,
            previous: rrd_nan(),
        };
        match (evaluator.evaluate(&row), stop) {
            (Ok(value), None) => Ok(if value.is_nan() { rrd_nan() } else { value }),
            (Err(RpnError::FinalStackSize), Some(_)) | (Ok(_), Some(_)) => {
                Err(EvalError::PreviousOf)
            }
            (Err(error), _) => Err(EvalError::Rpn(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 3] = ["a", "b", "c"];

    /// Compiles `text` for the third source: `a` and `b` are earlier.
    fn compile(text: &str) -> Result<ComputeProgram, String> {
        ComputeProgram::compile(text, |name| {
            NAMES[..2].iter().position(|candidate| *candidate == name)
        })
        .map_err(|error| error.to_string())
    }

    fn render(program: &ComputeProgram) -> String {
        let text = program.render(|index| NAMES.get(index).map(|name| name.as_bytes()));
        String::from_utf8(text).unwrap()
    }

    const PARSE: &str = "failed to parse computed data source";
    const OPERATOR: &str = "operators TIME LTIME STEPWIDTH PREV NEW* COUNT TREND TRENDNAN \
                            PREDICT PREDICTSIGMA PREDICTPERC are not supported with DS COMPUTE";
    const RANGE: &str = "constants must be integers in the interval (-32768, 32767)";

    #[test]
    fn encodes_nodes_with_an_end_marker() {
        let bytes = compile("a,-2,*,PREV(b),+").unwrap().to_bytes();
        let expected: [[u8; 4]; 6] = [
            [1, 0, 0, 0],
            [0, 0, 0xfe, 0xff],
            [11, 0, 0, 0],
            [36, 0, 1, 0],
            [8, 0, 0, 0],
            [32, 0, 0, 0],
        ];
        assert_eq!(bytes[..24], *expected.as_flattened());
        assert!(bytes[24..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn every_permitted_operator_round_trips() {
        for &(operator, code) in OPERATOR_CODES {
            let name = operator_name(operator);
            let program = compile(&format!("a,a,a,{name}")).unwrap();
            let bytes = program.to_bytes();
            assert_eq!(bytes[12], code, "{name}");
            assert_eq!(bytes[16], END_CODE, "{name}");
            let decoded = ComputeProgram::from_bytes(&bytes).unwrap();
            assert_eq!(decoded, program, "{name}");
            assert_eq!(render(&decoded), format!("a,a,a,{name}"));
        }
    }

    #[test]
    fn nineteen_nodes_fill_the_parameter_bytes() {
        let text = vec!["a"; MAX_NODES].join(",");
        let bytes = compile(&text).unwrap().to_bytes();
        assert_eq!(&bytes[76..], &[END_CODE, 0, 0, 0]);
        let text = vec!["a"; MAX_NODES + 1].join(",");
        assert_eq!(
            compile(&text),
            Err("Maximum 19 RPN nodes permitted. Got 20 RPN nodes at present.".into())
        );
        // PREV(name) is one node.
        let text = format!("{},PREV(a)", vec!["a"; MAX_NODES].join(","));
        assert_eq!(
            compile(&text),
            Err("Maximum 19 RPN nodes permitted. Got 20 RPN nodes at present.".into())
        );
    }

    #[test]
    fn checks_run_in_four_passes() {
        let many = vec!["a"; 20].join(",");
        let cases = [
            // Pass 1 wins over everything later in the expression.
            ("TIME,z,+", PARSE),
            ("40000,z,+", PARSE),
            ("z,40000,+", PARSE),
            (&format!("z,{many},+"), PARSE),
            // Pass 2 before the node limit and the constants.
            ("40000,TIME,+", OPERATOR),
            (&format!("TIME,{many},+"), OPERATOR),
            ("a,1,TIME", OPERATOR),
            // Pass 3 before the constants.
            (
                &format!("40000,{many},+"),
                "Maximum 19 RPN nodes permitted. Got 22 RPN nodes at present.",
            ),
            (
                &format!("1.5,{many}"),
                "Maximum 19 RPN nodes permitted. Got 21 RPN nodes at present.",
            ),
            // Pass 4.
            ("a,40000,+,1.5,+", RANGE),
        ];
        for (text, message) in cases {
            assert_eq!(compile(text), Err(message.into()), "{text}");
        }
    }

    #[test]
    fn grammar_follows_the_specification() {
        for text in [
            "a,b",
            "+",
            "a,+",
            "a,POP",
            "7,a,+",
            "a,1,1,+,+",
            "a,-32768,+",
            "a,32767,+",
            "a,b,+,",
            "a,1,",
            "UNKN",
            "NOW",
        ] {
            assert!(compile(text).is_ok(), "{text}");
        }
        for text in [
            "1",
            "a,1",
            "a,1,b,1",
            "a,,+",
            "a, b,+",
            "a,b,+:x",
            "c,1,+",
            "A,b,+",
            "a,0x10,+",
            "a,PREV(z),+",
            "",
            "a,b,+,,",
        ] {
            assert_eq!(compile(text), Err(PARSE.into()), "{text}");
        }
        for text in [
            "a,PREV,+",
            "a,COUNT,+",
            "a,NEWDAY,+",
            "a,PREDICTPERC,+",
            "a,LTIME,+",
        ] {
            assert_eq!(compile(text), Err(OPERATOR.into()), "{text}");
        }
        for text in ["a,32768,+", "a,-32769,+", "a,1.5,+", "a,1e10,+"] {
            assert_eq!(compile(text), Err(RANGE.into()), "{text}");
        }
    }

    #[test]
    fn rendering_rebuilds_the_text() {
        let cases = [
            ("a,+02,+", "a,2,+"),
            ("a,1e2,+", "a,100,+"),
            ("a,-0,+", "a,0,+"),
            ("a,1.0,+", "a,1,+"),
            ("a,-5,*", "a,-5,*"),
            ("a,PREV(b),+", "a,,+"),
            ("a,b,+,", "a,b,+"),
            ("a,UN,0,a,IF", "a,UN,0,a,IF"),
        ];
        for (text, shown) in cases {
            let program = compile(text).unwrap();
            assert_eq!(render(&program), shown, "{text}");
            let decoded = ComputeProgram::from_bytes(&program.to_bytes()).unwrap();
            assert_eq!(render(&decoded), shown, "{text}");
        }
    }

    #[test]
    fn decoding_refuses_unknown_codes_and_missing_ends() {
        assert_eq!(ComputeProgram::from_bytes(&[0; PROGRAM_LEN]), None);
        let mut bytes = compile("a,b,+").unwrap().to_bytes();
        bytes[8] = 3;
        assert_eq!(ComputeProgram::from_bytes(&bytes), None);
        let empty = ComputeProgram::from_bytes(&[END_CODE, 0, 0, 0]).unwrap();
        assert_eq!(render(&empty), "");
    }

    #[test]
    fn evaluation_binds_values_and_reports_stack_errors() {
        let eval = |text: &str, values: &[f64]| compile(text).unwrap().evaluate(values);
        assert_eq!(eval("a,b,+", &[1.0, 2.0]), Ok(3.0));
        assert_eq!(eval("a,b,-,7,*", &[1.0, 2.0]), Ok(-7.0));
        assert_eq!(eval("a,b,/", &[1.0, 0.0]), Ok(f64::INFINITY));
        let unknown = eval("a,b,/", &[0.0, 0.0]).unwrap();
        assert_eq!(unknown.to_bits(), rrd_nan().to_bits());
        assert!(eval("a,b,+", &[f64::NAN, 2.0]).unwrap().is_nan());
        assert_eq!(eval("a,UN,0,a,IF", &[f64::NAN]), Ok(0.0));
        assert_eq!(
            eval("a,b", &[1.0, 2.0]).map_err(|error| error.to_string()),
            Err("RPN final stack size != 1".into())
        );
        assert_eq!(
            eval("a,+", &[1.0]).map_err(|error| error.to_string()),
            Err("RPN stack underflow".into())
        );
        assert_eq!(
            eval("a,PREV(b),+", &[1.0, 2.0]).map_err(|error| error.to_string()),
            Err("VDEF made it into rpn_calc... aborting".into())
        );
        // An operator that fails before the first PREV(name) reports itself.
        assert_eq!(
            eval("+,PREV(b)", &[1.0, 2.0]).map_err(|error| error.to_string()),
            Err("RPN stack underflow".into())
        );
    }
}
