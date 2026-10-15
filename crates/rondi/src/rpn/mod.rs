//! The RPN expression language used by `CDEF` in `graph`, `graphv` and
//! `xport`.
//!
//! An expression is parsed once into an [`Expression`], a list of typed
//! tokens whose variable references are already resolved to graph element
//! indices. An [`Evaluator`] binds those references to data and then
//! computes one value per output row, reusing its stack between rows.
//!
//! The observable behavior, including the error texts below, is specified in
//! `docs/compatibility/rpn.md` and pinned by
//! `crates/rondi-cli/tests/data/rpn_vectors.json`.

mod calendar;
mod eval;
mod ordering;
mod parse;

pub(crate) use eval::{Evaluator, Row, Source};

/// A failure while parsing or evaluating an expression. `Display` gives the
/// text RRDtool prints after `ERROR: `; scripts match on these, so they are
/// part of the compatibility contract.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(crate) enum RpnError {
    #[error("can not parse an empty rpn expression")]
    Empty,
    /// The unparsed remainder of the expression, from the failing token on.
    #[error("don't understand '{0}'")]
    Unrecognized(String),
    #[error("garbage in RPN: '{0}'")]
    TrailingGarbage(String),
    #[error("variable '{0}' not found")]
    UnknownVariable(String),
    #[error("rpn expressions without DEF or CDEF variables are not supported")]
    NoVariables,
    #[error("RPN stack underflow")]
    StackUnderflow,
    #[error("RPN final stack size != 1")]
    FinalStackSize,
    #[error("RPN out of memory (allocating {0} objects)")]
    OutOfMemory(i64),
    #[error("malformed trend arguments")]
    MalformedTrend,
    /// Rondi's own text: RRDtool's result is undefined when the PREDICT
    /// family's last operand is not a variable reference.
    #[error("malformed predict arguments")]
    MalformedPredict,
    #[error("percentile argument must be between 0 and 100")]
    PercentileRange,
    #[error("unsupported percentile: {0:.6}")]
    UnsupportedPercentile(f64),
    #[error("negative shift step not allowed: {0}")]
    NegativeShift(i64),
}

/// A parsed expression.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Expression {
    tokens: Vec<Token>,
}

impl Expression {
    /// Parses `text`, resolving names with `lookup`, which returns the graph
    /// element index of a variable defined before this expression.
    pub(crate) fn parse(
        text: &str,
        lookup: impl Fn(&str) -> Option<usize>,
    ) -> Result<Self, RpnError> {
        parse::parse(text, &lookup).map(|tokens| Self { tokens })
    }

    /// Element indices referenced by the expression, in token order, either
    /// directly or through `PREV(name)`.
    pub(crate) fn variables(&self) -> impl Iterator<Item = usize> + '_ {
        self.tokens.iter().filter_map(|token| match *token {
            Token::Variable(element) | Token::PreviousOf(element) => Some(element),
            Token::Number(_) | Token::Operator(_) => None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Number(f64),
    /// A DEF, CDEF or VDEF, by graph element index.
    Variable(usize),
    /// `PREV(name)`.
    PreviousOf(usize),
    Operator(Operator),
}

/// Every operator of the language. Names follow `rrdgraph_rpn(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    AddNan,
    Power,
    Sin,
    Cos,
    Log,
    Exp,
    Sqrt,
    Atan,
    Atan2,
    Floor,
    Ceil,
    Round,
    DegreesToRadians,
    RadiansToDegrees,
    Abs,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Equal,
    NotEqual,
    IsUnknown,
    IsInfinite,
    If,
    Min,
    Max,
    MinNan,
    MaxNan,
    Limit,
    Unknown,
    Infinity,
    NegativeInfinity,
    Previous,
    Count,
    Now,
    Time,
    LocalTime,
    StepWidth,
    NewDay,
    NewWeek,
    NewMonth,
    NewYear,
    Dup,
    Pop,
    Exchange,
    Depth,
    Copy,
    Index,
    Roll,
    Sort,
    Reverse,
    Average,
    SetMin,
    SetMax,
    Median,
    StandardDeviation,
    Percentile,
    Trend,
    TrendNan,
    Predict,
    PredictSigma,
    PredictPercentile,
}

impl Operator {
    const NAMES: &[(&'static str, Self)] = &[
        ("+", Self::Add),
        ("-", Self::Subtract),
        ("*", Self::Multiply),
        ("/", Self::Divide),
        ("%", Self::Remainder),
        ("ADDNAN", Self::AddNan),
        ("POW", Self::Power),
        ("SIN", Self::Sin),
        ("COS", Self::Cos),
        ("LOG", Self::Log),
        ("EXP", Self::Exp),
        ("SQRT", Self::Sqrt),
        ("ATAN", Self::Atan),
        ("ATAN2", Self::Atan2),
        ("FLOOR", Self::Floor),
        ("CEIL", Self::Ceil),
        ("ROUND", Self::Round),
        ("DEG2RAD", Self::DegreesToRadians),
        ("RAD2DEG", Self::RadiansToDegrees),
        ("ABS", Self::Abs),
        ("LT", Self::Less),
        ("LE", Self::LessEqual),
        ("GT", Self::Greater),
        ("GE", Self::GreaterEqual),
        ("EQ", Self::Equal),
        ("NE", Self::NotEqual),
        ("UN", Self::IsUnknown),
        ("ISINF", Self::IsInfinite),
        ("IF", Self::If),
        ("MIN", Self::Min),
        ("MAX", Self::Max),
        ("MINNAN", Self::MinNan),
        ("MAXNAN", Self::MaxNan),
        ("LIMIT", Self::Limit),
        ("UNKN", Self::Unknown),
        ("INF", Self::Infinity),
        ("NEGINF", Self::NegativeInfinity),
        ("PREV", Self::Previous),
        ("COUNT", Self::Count),
        ("NOW", Self::Now),
        ("TIME", Self::Time),
        ("LTIME", Self::LocalTime),
        ("STEPWIDTH", Self::StepWidth),
        ("NEWDAY", Self::NewDay),
        ("NEWWEEK", Self::NewWeek),
        ("NEWMONTH", Self::NewMonth),
        ("NEWYEAR", Self::NewYear),
        ("DUP", Self::Dup),
        ("POP", Self::Pop),
        ("EXC", Self::Exchange),
        ("DEPTH", Self::Depth),
        ("COPY", Self::Copy),
        ("INDEX", Self::Index),
        ("ROLL", Self::Roll),
        ("SORT", Self::Sort),
        ("REV", Self::Reverse),
        ("AVG", Self::Average),
        ("SMIN", Self::SetMin),
        ("SMAX", Self::SetMax),
        ("MEDIAN", Self::Median),
        ("STDEV", Self::StandardDeviation),
        ("PERCENT", Self::Percentile),
        ("TREND", Self::Trend),
        ("TRENDNAN", Self::TrendNan),
        ("PREDICT", Self::Predict),
        ("PREDICTSIGMA", Self::PredictSigma),
        ("PREDICTPERC", Self::PredictPercentile),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::NAMES
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|&(_, operator)| operator)
    }
}

#[cfg(test)]
mod tests;
