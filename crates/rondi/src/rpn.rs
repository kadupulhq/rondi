//! Port of `rpn_parse` and `rpn_calc` from RRDtool 1.11.0 `rrd_rpncalc.c`.
//!
//! Graph CDEFs are evaluated one output row at a time. Each variable node
//! keeps its own cursor into the referenced series and advances it only when
//! the evaluation time is a multiple of that series' step, exactly like the C
//! data pointers, so mixed-resolution expressions see the same rows upstream
//! sees.

use crate::{StoreError, rrd_binary::rrd_nan, rrd_number::parse_rrd_number};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Number,
    Variable,
    Inf,
    Prev,
    NegInf,
    Unkn,
    Now,
    Time,
    Add,
    Mod,
    Sub,
    Mul,
    Div,
    Sin,
    Dup,
    Exc,
    Pop,
    Cos,
    Log,
    Exp,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    If,
    Min,
    Max,
    Limit,
    Floor,
    Ceil,
    Un,
    Ltime,
    Ne,
    IsInf,
    PrevOther,
    Count,
    Atan,
    Sqrt,
    Sort,
    Rev,
    Trend,
    TrendNan,
    Atan2,
    Rad2Deg,
    Deg2Rad,
    Predict,
    PredictSigma,
    Avg,
    Abs,
    AddNan,
    MinNan,
    MaxNan,
    Median,
    PredictPerc,
    Depth,
    Copy,
    Roll,
    Index,
    StepWidth,
    NewDay,
    NewWeek,
    NewMonth,
    NewYear,
    Smin,
    Smax,
    Stdev,
    Percent,
    Pow,
    Round,
}

/// The `match_op` table of `rpn_parse`, in source order.
const OPERATORS: &[(&str, Op)] = &[
    ("+", Op::Add),
    ("-", Op::Sub),
    ("*", Op::Mul),
    ("/", Op::Div),
    ("%", Op::Mod),
    ("SIN", Op::Sin),
    ("COS", Op::Cos),
    ("LOG", Op::Log),
    ("FLOOR", Op::Floor),
    ("CEIL", Op::Ceil),
    ("EXP", Op::Exp),
    ("DUP", Op::Dup),
    ("EXC", Op::Exc),
    ("POP", Op::Pop),
    ("LTIME", Op::Ltime),
    ("NEWDAY", Op::NewDay),
    ("NEWWEEK", Op::NewWeek),
    ("NEWMONTH", Op::NewMonth),
    ("NEWYEAR", Op::NewYear),
    ("STEPWIDTH", Op::StepWidth),
    ("LT", Op::Lt),
    ("LE", Op::Le),
    ("GT", Op::Gt),
    ("GE", Op::Ge),
    ("EQ", Op::Eq),
    ("IF", Op::If),
    ("MIN", Op::Min),
    ("MAX", Op::Max),
    ("LIMIT", Op::Limit),
    ("UNKN", Op::Unkn),
    ("UN", Op::Un),
    ("NEGINF", Op::NegInf),
    ("NE", Op::Ne),
    ("COUNT", Op::Count),
    ("PREV", Op::Prev),
    ("INF", Op::Inf),
    ("ISINF", Op::IsInf),
    ("NOW", Op::Now),
    ("TIME", Op::Time),
    ("ATAN2", Op::Atan2),
    ("ATAN", Op::Atan),
    ("SQRT", Op::Sqrt),
    ("SORT", Op::Sort),
    ("REV", Op::Rev),
    ("TREND", Op::Trend),
    ("TRENDNAN", Op::TrendNan),
    ("PREDICT", Op::Predict),
    ("PREDICTSIGMA", Op::PredictSigma),
    ("PREDICTPERC", Op::PredictPerc),
    ("RAD2DEG", Op::Rad2Deg),
    ("DEG2RAD", Op::Deg2Rad),
    ("AVG", Op::Avg),
    ("ABS", Op::Abs),
    ("ADDNAN", Op::AddNan),
    ("MINNAN", Op::MinNan),
    ("MAXNAN", Op::MaxNan),
    ("MEDIAN", Op::Median),
    ("DEPTH", Op::Depth),
    ("COPY", Op::Copy),
    ("ROLL", Op::Roll),
    ("INDEX", Op::Index),
    ("SMAX", Op::Smax),
    ("SMIN", Op::Smin),
    ("STDEV", Op::Stdev),
    ("PERCENT", Op::Percent),
    ("POW", Op::Pow),
    ("ROUND", Op::Round),
];

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub op: Op,
    pub val: f64,
    /// Graph element index of an `Op::Variable`/`Op::PrevOther` operand.
    pub ptr: usize,
    /// Cursor into the referenced series, `rpnp_t.data` in C.
    pub data: isize,
    pub step: i64,
}

fn error(message: impl Into<String>) -> StoreError {
    StoreError::RrdExpression(message.into())
}

/// `DEF_NAM_FMT` (`%255[_A-Za-z0-9-]`): the length of the leading name.
pub(crate) fn vname_len(expr: &[u8]) -> usize {
    expr.iter()
        .take(255)
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        .count()
}

fn at(expr: &[u8], index: usize) -> u8 {
    expr.get(index).copied().unwrap_or(0)
}

/// Port of `rpn_parse`. `lookup` resolves a variable name to its graph
/// element index.
pub(crate) fn rpn_parse(
    expression: &str,
    lookup: &dyn Fn(&str) -> Option<usize>,
) -> Result<Vec<Node>, StoreError> {
    let mut expr = expression.as_bytes();
    if expr.is_empty() {
        return Err(error("can not parse an empty rpn expression"));
    }
    let mut nodes = Vec::new();
    while !expr.is_empty() {
        let mut node = Node {
            op: Op::Number,
            val: 0.0,
            ptr: 0,
            data: 0,
            step: 0,
        };
        let number_len = expr
            .iter()
            .take(40)
            .take_while(|byte| matches!(byte, b'0'..=b'9' | b'.' | b'e' | b'+' | b'-'))
            .count();
        let number = (number_len > 0 && at(expr, number_len) == b',')
            .then(|| std::str::from_utf8(&expr[..number_len]).ok())
            .flatten()
            .and_then(parse_rrd_number);
        if let Some(value) = number {
            node.val = value;
            expr = &expr[number_len..];
        } else if let Some((name, op)) = OPERATORS.iter().find(|(name, _)| {
            expr.starts_with(name.as_bytes()) && matches!(at(expr, name.len()), b',' | 0)
        }) {
            node.op = *op;
            expr = &expr[name.len()..];
        } else if expr.starts_with(b"PREV(") && vname_len(&expr[5..]) > 0 && {
            // sscanf("PREV(%255[...])") succeeds on the name alone; the
            // closing parenthesis is only implied by the length check.
            let length = 4 + vname_len(&expr[5..]) + 2;
            matches!(at(expr, length), b',' | 0)
        } {
            let name_len = vname_len(&expr[5..]);
            let name = String::from_utf8_lossy(&expr[5..5 + name_len]).into_owned();
            node.op = Op::PrevOther;
            node.ptr =
                lookup(&name).ok_or_else(|| error(format!("variable '{name}' not found")))?;
            expr = &expr[(4 + name_len + 2).min(expr.len())..];
        } else if let Some(ptr) = Some(vname_len(expr))
            .filter(|len| *len > 0 && matches!(at(expr, *len), b',' | 0))
            .and_then(|len| lookup(&String::from_utf8_lossy(&expr[..len])).map(|ptr| (ptr, len)))
            .map(|(ptr, len)| {
                expr = &expr[len..];
                ptr
            })
        {
            node.op = Op::Variable;
            node.ptr = ptr;
        } else {
            return Err(error(format!(
                "don't understand '{}'",
                String::from_utf8_lossy(expr)
            )));
        }
        nodes.push(node);
        match expr.first() {
            None => break,
            Some(b',') => expr = &expr[1..],
            Some(_) => {
                return Err(error(format!(
                    "garbage in RPN: '{}'",
                    String::from_utf8_lossy(expr)
                )));
            }
        }
    }
    Ok(nodes)
}

/// The RPN stack, kept for the whole `data_calc` run like `rpnstack_t`.
/// Slots above the stack pointer keep their previous values, which some
/// operators read; slots never written read as zero.
#[derive(Default)]
pub(crate) struct RpnStack {
    s: Vec<f64>,
}

impl RpnStack {
    fn get(&self, index: i64) -> f64 {
        // Negative indices are reads before RRDtool's stack buffer, which
        // have no defined value.
        usize::try_from(index)
            .ok()
            .map_or_else(rrd_nan, |index| self.s.get(index).copied().unwrap_or(0.0))
    }

    fn set(&mut self, index: i64, value: f64) {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        if index >= self.s.len() {
            self.s.resize(index + 1, 0.0);
        }
        self.s[index] = value;
    }

    fn slice_mut(&mut self, start: i64, len: i64) -> &mut [f64] {
        let (Ok(start), Ok(len)) = (usize::try_from(start), usize::try_from(len)) else {
            return &mut [];
        };
        if start + len > self.s.len() {
            self.s.resize(start + len, 0.0);
        }
        &mut self.s[start..start + len]
    }
}

/// `(int)` conversion of a double as the pinned builds' hardware performs it:
/// x86_64 yields `INT_MIN` for NaN and out-of-range values, aarch64
/// saturates and maps NaN to zero.
pub(crate) fn c_int(value: f64) -> i32 {
    #[cfg(target_arch = "x86_64")]
    {
        if value.is_nan() || value >= 2_147_483_648.0 || value <= -2_147_483_649.0 {
            return i32::MIN;
        }
    }
    value as i32
}

/// `(long)`/`(time_t)` conversion of a double; see [`c_int`].
pub(crate) fn c_long(value: f64) -> i64 {
    #[cfg(target_arch = "x86_64")]
    {
        if value.is_nan()
            || value >= 9_223_372_036_854_775_808.0
            || value < -9.223_372_036_854_776e18
        {
            return i64::MIN;
        }
    }
    value as i64
}

/// `isinf()` as the host libc reports it: glibc returns the sign, Apple and
/// the BSDs return 1 for both infinities.
fn c_isinf(value: f64) -> libc::c_int {
    if !value.is_infinite() {
        0
    } else if cfg!(target_env = "gnu") && value.is_sign_negative() {
        -1
    } else {
        1
    }
}

unsafe extern "C" fn rpn_compare_double(
    x: *const libc::c_void,
    y: *const libc::c_void,
) -> libc::c_int {
    let x = unsafe { *x.cast::<f64>() };
    let y = unsafe { *y.cast::<f64>() };
    if x.is_nan() && y.is_nan() {
        return 0;
    }
    if x.is_nan() {
        return -1;
    }
    if y.is_nan() {
        return 1;
    }
    if x.is_infinite() {
        return c_isinf(x);
    }
    if y.is_infinite() {
        return c_isinf(y);
    }
    let diff = x - y;
    if diff < 0.0 {
        -1
    } else if diff > 0.0 {
        1
    } else {
        0
    }
}

/// `rpn_compare_double` is not a total order for infinities, so the result
/// depends on the host qsort; call it rather than a Rust sort.
fn qsort(values: &mut [f64]) {
    if values.is_empty() {
        return;
    }
    unsafe {
        libc::qsort(
            values.as_mut_ptr().cast(),
            values.len(),
            std::mem::size_of::<f64>(),
            Some(rpn_compare_double),
        );
    }
}

/// Reads `data[index]` of a variable. Reads before the series start are out
/// of bounds upstream; the pinned builds observed on aarch64 and x86_64
/// return zero or a denormal there, so use zero. Reads past the end are
/// unknown.
fn read(data: &[f64], index: isize) -> f64 {
    if index < 0 {
        0.0
    } else {
        data.get(index as usize).copied().unwrap_or_else(rrd_nan)
    }
}

fn local_tm(timestamp: i64) -> libc::tm {
    let timestamp = timestamp as libc::time_t;
    let mut result = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&timestamp, &mut result) };
    result
}

fn tzoffset(now: i64) -> i32 {
    let timestamp = now as libc::time_t;
    let mut gm = unsafe { std::mem::zeroed::<libc::tm>() };
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe {
        libc::gmtime_r(&timestamp, &mut gm);
        libc::localtime_r(&timestamp, &mut local);
    }
    let mut off = (local.tm_sec - gm.tm_sec)
        + (local.tm_min - gm.tm_min) * 60
        + (local.tm_hour - gm.tm_hour) * 3600;
    if local.tm_yday > gm.tm_yday || local.tm_year > gm.tm_year {
        off += 24 * 3600;
    } else if local.tm_yday < gm.tm_yday || local.tm_year < gm.tm_year {
        off -= 24 * 3600;
    }
    off
}

/// Matches RRDtool's `find_first_weekday`: glibc uses the active LC_TIME
/// metadata, while platforms without its private langinfo items use Sunday.
#[cfg(all(unix, target_env = "gnu"))]
fn find_first_weekday() -> i32 {
    const NL_TIME_WEEK_1STDAY: libc::nl_item = 131_174;
    const NL_TIME_FIRST_WEEKDAY: libc::nl_item = 131_176;

    let (first_weekday, week_start) = unsafe {
        let first_weekday = libc::nl_langinfo(NL_TIME_FIRST_WEEKDAY);
        let week_start = libc::nl_langinfo(NL_TIME_WEEK_1STDAY);
        if first_weekday.is_null() || week_start.is_null() {
            return 1;
        }
        (*first_weekday as i32, week_start as usize as u64)
    };
    let week_start = if week_start == 19_971_130 || week_start >> 32 == 19_971_130 {
        0
    } else if week_start == 19_971_201 || week_start >> 32 == 19_971_201 {
        1
    } else {
        return 1;
    };
    (week_start + first_weekday - 1) % 7
}

#[cfg(not(all(unix, target_env = "gnu")))]
fn find_first_weekday() -> i32 {
    0
}

/// Port of `rpn_calc`. `series` returns the data of a graph element by
/// index; `output[output_idx]` receives the result.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rpn_calc<'a>(
    rpnp: &mut [Node],
    stack: &mut RpnStack,
    data_idx: i64,
    output: &mut [f64],
    output_idx: i32,
    step_width: i32,
    series: &dyn Fn(usize) -> &'a [f64],
) -> Result<(), StoreError> {
    let mut stptr: i64 = -1;
    macro_rules! underflow {
        ($min:expr) => {
            if stptr < i64::from($min) {
                return Err(error("RPN stack underflow"));
            }
        };
    }
    macro_rules! s {
        ($index:expr) => {
            stack.get($index)
        };
    }
    for rpi in 0..rpnp.len() {
        match rpnp[rpi].op {
            Op::Number => {
                stptr += 1;
                stack.set(stptr, rpnp[rpi].val);
            }
            Op::Variable | Op::PrevOther => {
                let node = &mut rpnp[rpi];
                let data = series(node.ptr);
                let value = if node.op == Op::Variable {
                    read(data, node.data)
                } else if output_idx <= 0 {
                    rrd_nan()
                } else {
                    read(data, node.data - 1)
                };
                stptr += 1;
                stack.set(stptr, value);
                if node.step != 0 && data_idx % node.step == 0 {
                    node.data += 1;
                }
            }
            Op::StepWidth => {
                stptr += 1;
                stack.set(stptr, f64::from(step_width));
            }
            Op::Count => {
                stptr += 1;
                stack.set(stptr, f64::from(output_idx + 1));
            }
            Op::Prev => {
                let value = if output_idx <= 0 {
                    rrd_nan()
                } else {
                    output[output_idx as usize - 1]
                };
                stptr += 1;
                stack.set(stptr, value);
            }
            Op::Unkn => {
                stptr += 1;
                stack.set(stptr, rrd_nan());
            }
            Op::Inf => {
                stptr += 1;
                stack.set(stptr, f64::INFINITY);
            }
            Op::NegInf => {
                stptr += 1;
                stack.set(stptr, f64::NEG_INFINITY);
            }
            Op::Now => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or_else(|_| rrd_nan(), |duration| duration.as_secs() as f64);
                stptr += 1;
                stack.set(stptr, now);
            }
            Op::Time => {
                stptr += 1;
                stack.set(stptr, data_idx as f64);
            }
            Op::Ltime => {
                stptr += 1;
                stack.set(stptr, f64::from(tzoffset(data_idx)) + data_idx as f64);
            }
            Op::NewDay | Op::NewWeek | Op::NewMonth | Op::NewYear => {
                let current = local_tm(data_idx);
                let prior = local_tm(data_idx - i64::from(step_width));
                let changed = match rpnp[rpi].op {
                    Op::NewDay => current.tm_mday != prior.tm_mday,
                    Op::NewWeek => {
                        current.tm_wday == find_first_weekday() && current.tm_wday != prior.tm_wday
                    }
                    Op::NewMonth => current.tm_mon != prior.tm_mon,
                    _ => current.tm_year != prior.tm_year,
                };
                stptr += 1;
                stack.set(stptr, if changed { 1.0 } else { 0.0 });
            }
            op @ (Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Pow | Op::Atan2) => {
                underflow!(1);
                let (a, b) = (s!(stptr - 1), s!(stptr));
                let value = match op {
                    Op::Add => a + b,
                    Op::Sub => a - b,
                    Op::Mul => a * b,
                    Op::Div => a / b,
                    Op::Mod => a % b,
                    Op::Pow => a.powf(b),
                    _ => a.atan2(b),
                };
                stack.set(stptr - 1, value);
                stptr -= 1;
            }
            Op::AddNan => {
                underflow!(1);
                let (a, b) = (s!(stptr - 1), s!(stptr));
                if a.is_nan() {
                    stack.set(stptr - 1, b);
                } else if !b.is_nan() {
                    stack.set(stptr - 1, a + b);
                }
                stptr -= 1;
            }
            op @ (Op::Sin
            | Op::Atan
            | Op::Rad2Deg
            | Op::Deg2Rad
            | Op::Cos
            | Op::Ceil
            | Op::Round
            | Op::Floor
            | Op::Log
            | Op::Exp
            | Op::Un
            | Op::IsInf
            | Op::Sqrt
            | Op::Abs) => {
                underflow!(0);
                let a = s!(stptr);
                let value = match op {
                    Op::Sin => a.sin(),
                    Op::Atan => a.atan(),
                    Op::Rad2Deg => 57.29577951 * a,
                    Op::Deg2Rad => 0.0174532952 * a,
                    Op::Cos => a.cos(),
                    Op::Ceil => a.ceil(),
                    Op::Round => a.round(),
                    Op::Floor => a.floor(),
                    Op::Log => a.ln(),
                    Op::Exp => a.exp(),
                    Op::Un => f64::from(u8::from(a.is_nan())),
                    Op::IsInf => f64::from(u8::from(a.is_infinite())),
                    Op::Sqrt => a.sqrt(),
                    _ => a.abs(),
                };
                stack.set(stptr, value);
            }
            Op::Dup => {
                underflow!(0);
                let a = s!(stptr);
                stack.set(stptr + 1, a);
                stptr += 1;
            }
            Op::Pop => {
                underflow!(0);
                stptr -= 1;
            }
            Op::Exc => {
                underflow!(1);
                let (a, b) = (s!(stptr - 1), s!(stptr));
                stack.set(stptr, a);
                stack.set(stptr - 1, b);
            }
            op @ (Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Ne | Op::Eq) => {
                underflow!(1);
                let (a, b) = (s!(stptr - 1), s!(stptr));
                if a.is_nan() {
                } else if b.is_nan() {
                    stack.set(stptr - 1, b);
                } else {
                    let truth = match op {
                        Op::Lt => a < b,
                        Op::Le => a <= b,
                        Op::Gt => a > b,
                        Op::Ge => a >= b,
                        Op::Ne => a != b,
                        _ => a == b,
                    };
                    stack.set(stptr - 1, if truth { 1.0 } else { 0.0 });
                }
                stptr -= 1;
            }
            Op::If => {
                underflow!(2);
                let condition = s!(stptr - 2);
                let value = if condition.is_nan() || condition == 0.0 {
                    s!(stptr)
                } else {
                    s!(stptr - 1)
                };
                stack.set(stptr - 2, value);
                stptr -= 2;
            }
            Op::Min | Op::Max => {
                underflow!(1);
                let (a, b) = (s!(stptr - 1), s!(stptr));
                if a.is_nan() {
                } else if b.is_nan()
                    || (rpnp[rpi].op == Op::Min && a > b)
                    || (rpnp[rpi].op == Op::Max && a < b)
                {
                    stack.set(stptr - 1, b);
                }
                stptr -= 1;
            }
            Op::MinNan | Op::MaxNan => {
                underflow!(1);
                let (a, b) = (s!(stptr - 1), s!(stptr));
                if a.is_nan() {
                    stack.set(stptr - 1, b);
                } else if b.is_nan() {
                } else if (rpnp[rpi].op == Op::MinNan && a > b)
                    || (rpnp[rpi].op == Op::MaxNan && a < b)
                {
                    stack.set(stptr - 1, b);
                }
                stptr -= 1;
            }
            Op::Limit => {
                underflow!(2);
                let (value, min, max) = (s!(stptr - 2), s!(stptr - 1), s!(stptr));
                if value.is_nan() {
                } else if min.is_nan() {
                    stack.set(stptr - 2, min);
                } else if max.is_nan() {
                    stack.set(stptr - 2, max);
                } else if value < min || value > max {
                    stack.set(stptr - 2, rrd_nan());
                }
                stptr -= 2;
            }
            Op::Sort => {
                underflow!(0);
                let spn = c_int(s!(stptr));
                stptr -= 1;
                underflow!(spn.wrapping_sub(1));
                qsort(stack.slice_mut(stptr - i64::from(spn) + 1, i64::from(spn)));
            }
            Op::Rev => {
                underflow!(0);
                let spn = c_int(s!(stptr));
                stptr -= 1;
                underflow!(spn.wrapping_sub(1));
                let (mut p, mut q) = (stptr - i64::from(spn) + 1, stptr);
                while p < q {
                    let x = s!(q);
                    let y = s!(p);
                    stack.set(q, y);
                    stack.set(p, x);
                    p += 1;
                    q -= 1;
                }
            }
            op @ (Op::Predict | Op::PredictSigma | Op::PredictPerc) => {
                let mut percentile = rrd_nan();
                if op == Op::PredictPerc {
                    underflow!(1);
                    stptr -= 1;
                    percentile = s!(stptr);
                    if percentile.abs() > 100.0 {
                        return Err(error(format!("unsupported percentile: {percentile:.6}")));
                    }
                    percentile /= 100.0;
                }
                underflow!(2);
                stptr -= 1;
                let locstepsize = c_int(s!(stptr));
                stptr -= 1;
                let shifts = c_int(s!(stptr));
                underflow!(shifts);
                if shifts < 0 {
                    stptr -= 1;
                } else {
                    stptr -= i64::from(shifts);
                }
                // RRDtool reads the operand node before the operator as the
                // data source; anything else has no data pointer there.
                let source = rpi
                    .checked_sub(1)
                    .map(|index| rpnp[index].clone())
                    .filter(|node| matches!(node.op, Op::Variable | Op::PrevOther))
                    .ok_or_else(|| error("PREDICT requires a variable operand"))?;
                let data = series(source.ptr);
                let dsstep = source.step;
                let locstep = c_int(f64::from((locstepsize as f32 / dsstep as f32).ceil()));
                let (mut sum, mut sum2, mut count) = (0.0_f64, 0.0_f64, 0_i32);
                let doshifts = shifts.unsigned_abs();
                let mut extra = Vec::new();
                for loop_index in 0..doshifts {
                    let mut shiftstep = if shifts < 0 {
                        c_int(f64::from(loop_index as i32) * s!(stptr))
                    } else {
                        c_int(s!(stptr + i64::from(loop_index)))
                    };
                    if shiftstep < 0 {
                        return Err(error(format!(
                            "negative shift step not allowed: {shiftstep}"
                        )));
                    }
                    shiftstep = c_int(f64::from((shiftstep as f32 / dsstep as f32).ceil()));
                    let mut i = 0;
                    while i <= locstep {
                        let offset = shiftstep.wrapping_add(i);
                        if offset >= 0 && offset < output_idx {
                            let value = read(data, source.data - offset as isize);
                            if !value.is_nan() {
                                sum += value;
                                sum2 += value * value;
                                if op == Op::PredictPerc {
                                    extra.push(value);
                                }
                                count += 1;
                            }
                        }
                        i += 1;
                    }
                }
                let mut value = rrd_nan();
                match op {
                    Op::Predict => {
                        if count > 0 {
                            value = sum / f64::from(count);
                        }
                    }
                    Op::PredictSigma => {
                        if count > 1 {
                            value = f64::from(count) * sum2 - sum * sum;
                            value = if value < 0.0 {
                                rrd_nan()
                            } else {
                                let count = f64::from(count as f32);
                                (value / (count * (count - 1.0))).sqrt()
                            };
                        }
                    }
                    _ => {
                        if count > 0 {
                            qsort(&mut extra);
                            let idxf = percentile * (f64::from(count as f32) - 1.0);
                            if percentile < 0.0 {
                                value = extra[c_int(idxf.abs().round()) as usize];
                            } else {
                                let idx = c_int(idxf.floor());
                                let deltax = idxf - f64::from(idx);
                                value = extra[idx as usize];
                                if deltax != 0.0 {
                                    let deltay = extra[idx as usize + 1] - extra[idx as usize];
                                    value += deltay * deltax;
                                }
                            }
                        }
                    }
                }
                stack.set(stptr, value);
            }
            op @ (Op::Trend | Op::TrendNan) => {
                underflow!(1);
                let source = rpi
                    .checked_sub(2)
                    .map(|index| rpnp[index].clone())
                    .filter(|node| node.op == Op::Variable)
                    .ok_or_else(|| error("malformed trend arguments"))?;
                let mut dur = c_long(s!(stptr));
                let step = source.step;
                let data = series(source.ptr);
                stptr -= 1;
                if output_idx + 1 >= c_int(f64::from((dur as f32 / step as f32).ceil())) {
                    let ignorenan = op == Op::Trend;
                    let mut accum = 0.0;
                    let mut i: isize = -1;
                    let mut count = 0;
                    loop {
                        let value = read(data, source.data + i);
                        i -= 1;
                        if ignorenan || !value.is_nan() {
                            accum += value;
                            count += 1;
                        }
                        dur -= step;
                        if dur <= 0 {
                            break;
                        }
                    }
                    stack.set(
                        stptr,
                        if count == 0 {
                            rrd_nan()
                        } else {
                            accum / f64::from(count)
                        },
                    );
                } else {
                    stack.set(stptr, rrd_nan());
                }
            }
            Op::Avg => {
                underflow!(0);
                let mut i = c_int(s!(stptr));
                stptr -= 1;
                let (mut sum, mut count) = (0.0, 0);
                underflow!(i.wrapping_sub(1));
                while i > 0 {
                    let value = s!(stptr);
                    stptr -= 1;
                    i -= 1;
                    if value.is_nan() {
                        continue;
                    }
                    count += 1;
                    sum += value;
                }
                stptr += 1;
                stack.set(
                    stptr,
                    if count > 0 {
                        sum / f64::from(count)
                    } else {
                        rrd_nan()
                    },
                );
            }
            Op::Median => {
                underflow!(0);
                let elements = c_int(s!(stptr));
                stptr -= 1;
                let mut final_elements = elements;
                let element_ptr = stptr - i64::from(elements) + 1;
                let mut goodvals = element_ptr;
                let mut badvals = element_ptr + i64::from(elements) - 1;
                underflow!(elements.wrapping_sub(1));
                while goodvals < badvals {
                    if s!(goodvals).is_nan() {
                        let value = s!(badvals);
                        stack.set(goodvals, value);
                        badvals -= 1;
                        final_elements -= 1;
                    } else {
                        goodvals += 1;
                    }
                }
                if s!(goodvals).is_nan() {
                    final_elements -= 1;
                }
                stptr -= i64::from(elements);
                if final_elements == 0 {
                    stptr += 1;
                    stack.set(stptr, rrd_nan());
                } else {
                    qsort(stack.slice_mut(element_ptr, i64::from(final_elements)));
                    let half = i64::from(final_elements / 2);
                    let value = if final_elements % 2 == 1 {
                        s!(element_ptr + half)
                    } else {
                        0.5 * (s!(element_ptr + half) + s!(element_ptr + half - 1))
                    };
                    stptr += 1;
                    stack.set(stptr, value);
                }
            }
            Op::Stdev => {
                underflow!(0);
                let mut elements = c_int(s!(stptr));
                stptr -= 1;
                underflow!(elements.wrapping_sub(1));
                let (mut n, mut mean, mut mean2) = (0, 0.0_f64, 0.0_f64);
                while elements > 0 {
                    elements -= 1;
                    let datum = s!(stptr);
                    stptr -= 1;
                    if datum.is_nan() {
                        continue;
                    }
                    n += 1;
                    let delta = datum - mean;
                    mean += delta / f64::from(n);
                    mean2 += delta * (datum - mean);
                }
                stptr += 1;
                stack.set(
                    stptr,
                    if n < 2 {
                        rrd_nan()
                    } else {
                        (mean2 / f64::from(n - 1)).sqrt()
                    },
                );
            }
            Op::Percent => {
                underflow!(2);
                let elements = c_int(s!(stptr));
                stptr -= 1;
                let percent = s!(stptr);
                stptr -= 1;
                if !(0.0..=100.0).contains(&percent) {
                    return Err(error("percentile argument must be between 0 and 100"));
                }
                underflow!(elements.wrapping_sub(1));
                qsort(stack.slice_mut(stptr - i64::from(elements) + 1, i64::from(elements)));
                stptr -= i64::from(elements);
                // Rank zero reads the slot below the sorted window, which
                // is outside the buffer when the window is the whole stack.
                let value =
                    s!(stptr + i64::from(c_int((percent * f64::from(elements) / 100.0).round())));
                stack.set(stptr + 1, value);
                stptr += 1;
            }
            op @ (Op::Smax | Op::Smin) => {
                underflow!(0);
                let mut ximum = rrd_nan();
                let mut elements = c_int(s!(stptr));
                stptr -= 1;
                underflow!(elements.wrapping_sub(1));
                while elements > 0 {
                    elements -= 1;
                    let element = s!(stptr);
                    stptr -= 1;
                    if ximum.is_nan()
                        || (op == Op::Smax && element > ximum)
                        || (op == Op::Smin && element < ximum)
                    {
                        ximum = element;
                    }
                }
                stptr += 1;
                stack.set(stptr, ximum);
            }
            Op::Roll => {
                underflow!(1);
                let step = c_int(s!(stptr));
                stptr -= 1;
                let base = c_int(s!(stptr));
                stptr -= 1;
                let mut i = base;
                let mut j = i.wrapping_add(step);
                underflow!(base.wrapping_sub(1));
                if base < 0 {
                    return Err(error(format!(
                        "RPN out of memory (allocating {base} objects)"
                    )));
                }
                // memcpy from s + stptr copies the top value and the two
                // popped operands above it; larger counts read stale slots.
                let tmp: Vec<f64> = (0..i64::from(base)).map(|k| s!(stptr + k)).collect();
                while i != 0 {
                    i -= 1;
                    j -= 1;
                    while j < 0 {
                        j += base;
                    }
                    while j >= base {
                        j -= base;
                    }
                    stack.set(stptr - i64::from(i), tmp[j as usize]);
                }
            }
            Op::Index => {
                underflow!(0);
                let i = c_int(s!(stptr));
                underflow!(i);
                let value = s!(stptr - i64::from(i));
                stack.set(stptr, value);
            }
            Op::Copy => {
                let base = c_int(s!(stptr));
                stptr -= 1;
                let mut i = base;
                underflow!(base.wrapping_sub(1));
                while i > 0 {
                    i -= 1;
                    stptr += 1;
                    let value = s!(stptr - i64::from(base));
                    stack.set(stptr, value);
                }
            }
            Op::Depth => {
                stptr += 1;
                stack.set(stptr, stptr as f64);
            }
        }
    }
    if stptr != 0 {
        return Err(error("RPN final stack size != 1"));
    }
    output[output_idx as usize] = stack.get(0);
    Ok(())
}
