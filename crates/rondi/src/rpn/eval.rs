//! Row-by-row evaluation of a parsed expression.

use super::calendar::{self, Period};
use super::{Expression, Operator, RpnError, Token, ordering};
use crate::numeric_cast::{c_int, c_long};
use crate::rrd_binary::{rrd_mul_add, rrd_nan};

/// What a variable reference reads.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Source<'a> {
    /// A VDEF: the same value in every row, also through `PREV(name)`.
    Constant(f64),
    /// A DEF or CDEF series at its own step. `first` is the sample read in
    /// the first evaluated row.
    Series {
        values: &'a [f64],
        step: u64,
        first: usize,
    },
}

/// The context of one evaluated row.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Row {
    /// The row's timestamp.
    pub time: i64,
    /// Zero-based position in the evaluation.
    pub index: usize,
    /// The expression's step in seconds.
    pub step: u64,
    /// This expression's value in the previous row (unknown in the first).
    pub previous: f64,
}

/// Evaluates one expression over consecutive rows.
#[derive(Debug)]
pub(crate) struct Evaluator<'a> {
    program: Vec<Step>,
    cursors: Vec<Cursor<'a>>,
    stack: Stack,
    /// Sample buffer for the PREDICT family, reused across rows.
    scratch: Vec<f64>,
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Push(f64),
    /// Push the current sample of `cursors[n]`.
    Sample(usize),
    /// Push the sample before the current one of `cursors[n]`.
    PreviousSample(usize),
    Apply(Operator),
}

/// A variable's position in its own series.
#[derive(Debug)]
struct Cursor<'a> {
    values: &'a [f64],
    step: u64,
    position: i64,
}

impl Cursor<'_> {
    /// The sample `offset` positions after the current one.
    fn sample(&self, offset: i64) -> f64 {
        let index = self.position.saturating_add(offset);
        // Compatibility: a coarser variable lags the expression's row
        // count, so history windows in early rows reach before its first
        // sample; RRDtool reads zeros there. Past the end, Rondi reads
        // unknown.
        // Provenance: black-box compatibility test (mixedstep-06, -07) and
        // probes of `z,60,TREND` in a 10 s CDEF over a 30 s DEF; see
        // "Variable cursors" in docs/compatibility/rpn.md.
        if index < 0 {
            return 0.0;
        }
        usize::try_from(index)
            .ok()
            .and_then(|index| self.values.get(index).copied())
            .unwrap_or_else(rrd_nan)
    }

    /// Whether the cursor moves on after a row at `time`.
    fn advances_at(&self, time: i64) -> bool {
        time.rem_euclid(i64::try_from(self.step).unwrap_or(i64::MAX)) == 0
    }

    /// The offset of the sample that history windows (TREND and the
    /// PREDICT family) count back from at `time`.
    ///
    /// Compatibility: these windows end at the latest sample whose interval
    /// has begun by `time`: the current one when the cursor moves on after
    /// this row, otherwise the one before it. At equal steps that is always
    /// the current sample; a coarser variable switches one row earlier than
    /// its plain reference does.
    /// Provenance: black-box compatibility test (mixedstep-05 to -07).
    fn window_end(&self, time: i64) -> i64 {
        if self.advances_at(time) { 0 } else { -1 }
    }
}

/// Upper bound on the stack depth. RRDtool grows its stack without a fixed
/// limit (3000 values evaluate normally); Rondi stops at a million values,
/// which repeated COPY could otherwise double without end, and reports it
/// with RRDtool's allocation-failure text.
const MAX_STACK_DEPTH: usize = 1 << 20;

/// Upper bound on the samples one PREDICT-family operator gathers per row.
/// The reference has no such limit; it only matters for a negative shift
/// count of enormous magnitude with a zero multiplier.
const MAX_PREDICT_SAMPLES: usize = 1 << 22;

impl<'a> Evaluator<'a> {
    /// Binds every variable reference with `bind`, which receives the
    /// element index from the expression.
    pub(crate) fn new(expression: &Expression, mut bind: impl FnMut(usize) -> Source<'a>) -> Self {
        let mut cursors = Vec::new();
        let program = expression
            .tokens
            .iter()
            .map(|token| match *token {
                Token::Number(value) => Step::Push(value),
                Token::Operator(operator) => Step::Apply(operator),
                Token::Variable(element) | Token::PreviousOf(element) => match bind(element) {
                    Source::Constant(value) => Step::Push(value),
                    Source::Series {
                        values,
                        step,
                        first,
                    } => {
                        cursors.push(Cursor {
                            values,
                            step: step.max(1),
                            position: i64::try_from(first).unwrap_or(i64::MAX),
                        });
                        let cursor = cursors.len() - 1;
                        if matches!(token, Token::Variable(_)) {
                            Step::Sample(cursor)
                        } else {
                            Step::PreviousSample(cursor)
                        }
                    }
                },
            })
            .collect();
        Self {
            program,
            cursors,
            stack: Stack::default(),
            scratch: Vec::new(),
        }
    }

    /// Computes the value of `row`. Rows must be evaluated in order: each
    /// call moves the variable cursors forward.
    pub(crate) fn evaluate(&mut self, row: &Row) -> Result<f64, RpnError> {
        self.stack.clear();
        let mut now = None;
        for position in 0..self.program.len() {
            match self.program[position] {
                Step::Push(value) => self.stack.push(value),
                Step::Sample(cursor) => self.stack.push(self.cursors[cursor].sample(0)),
                Step::PreviousSample(cursor) => {
                    let value = if row.index == 0 {
                        rrd_nan()
                    } else {
                        self.cursors[cursor].sample(-1)
                    };
                    self.stack.push(value);
                }
                Step::Apply(operator) => self.apply(operator, position, row, &mut now)?,
            }
            if self.stack.len > MAX_STACK_DEPTH {
                return Err(RpnError::OutOfMemory(self.stack.len as i64));
            }
        }
        // Compatibility: a variable moves to its next sample after each row
        // whose time is a multiple of its own step.
        // Provenance: black-box compatibility test (mixedstep-01 to -10).
        for cursor in &mut self.cursors {
            if cursor.advances_at(row.time) {
                cursor.position = cursor.position.saturating_add(1);
            }
        }
        match self.stack.values() {
            [value] => Ok(*value),
            _ => Err(RpnError::FinalStackSize),
        }
    }

    /// The cursor read by the step `distance` positions before `position`,
    /// when that step is a plain variable reference.
    fn series_before(&self, position: usize, distance: usize) -> Option<usize> {
        match position
            .checked_sub(distance)
            .map(|index| self.program[index])
        {
            Some(Step::Sample(cursor)) => Some(cursor),
            _ => None,
        }
    }

    fn apply(
        &mut self,
        operator: Operator,
        position: usize,
        row: &Row,
        now: &mut Option<f64>,
    ) -> Result<(), RpnError> {
        use Operator as O;
        let stack = &mut self.stack;
        match operator {
            O::Add => stack.binary(|x, y| x + y)?,
            O::Subtract => stack.binary(|x, y| x - y)?,
            O::Multiply => stack.binary(|x, y| x * y)?,
            O::Divide => stack.binary(|x, y| x / y)?,
            // Rust's `%` on f64 is the C `fmod`: the sign follows `x`.
            O::Remainder => stack.binary(|x, y| x % y)?,
            O::AddNan => stack.binary(|x, y| match (x.is_nan(), y.is_nan()) {
                (true, _) => y,
                (false, true) => x,
                (false, false) => x + y,
            })?,
            O::Power => stack.binary(f64::powf)?,
            O::Atan2 => stack.binary(f64::atan2)?,
            O::Sin => stack.unary(f64::sin)?,
            O::Cos => stack.unary(f64::cos)?,
            O::Log => stack.unary(f64::ln)?,
            O::Exp => stack.unary(f64::exp)?,
            O::Sqrt => stack.unary(f64::sqrt)?,
            O::Atan => stack.unary(f64::atan)?,
            O::Floor => stack.unary(f64::floor)?,
            O::Ceil => stack.unary(f64::ceil)?,
            O::Round => stack.unary(f64::round)?,
            O::Abs => stack.unary(f64::abs)?,
            // Compatibility: the conversion factors are these truncated
            // constants, not pi/180 and 180/pi.
            // Provenance: black-box compatibility test (unaryedge-17 to -20).
            O::DegreesToRadians => stack.unary(|x| x * 0.017_453_295_2)?,
            O::RadiansToDegrees => stack.unary(|x| x * 57.295_779_51)?,
            O::IsUnknown => stack.unary(|x| truth(x.is_nan()))?,
            O::IsInfinite => stack.unary(|x| truth(x.is_infinite()))?,
            // Compatibility: comparisons are unknown only for unknown
            // operands; infinities compare as numbers, unlike the manual says.
            // Provenance: black-box compatibility test (special-08, -09).
            O::Less => stack.compare(|x, y| x < y)?,
            O::LessEqual => stack.compare(|x, y| x <= y)?,
            O::Greater => stack.compare(|x, y| x > y)?,
            O::GreaterEqual => stack.compare(|x, y| x >= y)?,
            O::Equal => stack.compare(|x, y| x == y)?,
            O::NotEqual => stack.compare(|x, y| x != y)?,
            O::Min => stack.binary(|x, y| {
                if x.is_nan() || y.is_nan() {
                    rrd_nan()
                } else if y < x {
                    y
                } else {
                    x
                }
            })?,
            O::Max => stack.binary(|x, y| {
                if x.is_nan() || y.is_nan() {
                    rrd_nan()
                } else if y > x {
                    y
                } else {
                    x
                }
            })?,
            O::MinNan => stack.binary(|x, y| match (x.is_nan(), y.is_nan()) {
                (true, _) => y,
                (false, true) => x,
                (false, false) if y < x => y,
                (false, false) => x,
            })?,
            O::MaxNan => stack.binary(|x, y| match (x.is_nan(), y.is_nan()) {
                (true, _) => y,
                (false, true) => x,
                (false, false) if y > x => y,
                (false, false) => x,
            })?,
            O::If => {
                stack.require(3)?;
                let otherwise = stack.pop()?;
                let then = stack.pop()?;
                let condition = stack.pop()?;
                let chosen = if condition == 0.0 || condition.is_nan() {
                    otherwise
                } else {
                    then
                };
                stack.push(chosen);
            }
            O::Limit => {
                stack.require(3)?;
                let high = stack.pop()?;
                let low = stack.pop()?;
                let value = stack.pop()?;
                let outside =
                    value.is_nan() || low.is_nan() || high.is_nan() || value < low || value > high;
                stack.push(if outside { rrd_nan() } else { value });
            }
            O::Unknown => stack.push(rrd_nan()),
            O::Infinity => stack.push(f64::INFINITY),
            O::NegativeInfinity => stack.push(f64::NEG_INFINITY),
            O::Previous => stack.push(row.previous),
            O::Count => stack.push(row.index as f64 + 1.0),
            O::Now => {
                let value = *now.get_or_insert_with(wall_clock);
                stack.push(value);
            }
            O::Time => stack.push(row.time as f64),
            O::LocalTime => stack.push(calendar::local_time(row.time)),
            O::StepWidth => stack.push(row.step as f64),
            O::NewDay => stack.push(truth(calendar::starts_new(Period::Day, row.time, row.step))),
            O::NewWeek => stack.push(truth(calendar::starts_new(
                Period::Week,
                row.time,
                row.step,
            ))),
            O::NewMonth => {
                stack.push(truth(calendar::starts_new(
                    Period::Month,
                    row.time,
                    row.step,
                )));
            }
            O::NewYear => stack.push(truth(calendar::starts_new(
                Period::Year,
                row.time,
                row.step,
            ))),
            O::Dup => {
                let top = stack.peek()?;
                stack.push(top);
            }
            O::Pop => {
                stack.pop()?;
            }
            O::Exchange => {
                stack.require(2)?;
                let len = stack.len;
                stack.buffer.swap(len - 1, len - 2);
            }
            O::Depth => stack.push(stack.len as f64),
            O::Copy => {
                let count = stack.pop_count()?;
                let count = stack.checked_count(count)?;
                if stack.len + count > MAX_STACK_DEPTH {
                    return Err(RpnError::OutOfMemory((stack.len + count) as i64));
                }
                let start = stack.len - count;
                for index in start..start + count {
                    let value = stack.buffer[index];
                    stack.push(value);
                }
            }
            O::Index => {
                let raw = stack.pop()?;
                match c_long(raw) {
                    // Compatibility: 0,INDEX leaves its operand in place.
                    // Provenance: black-box compatibility test (index-04).
                    0 => stack.push(raw),
                    // Compatibility: a negative index reads the slot that
                    // far above the top, so the result is pushed and the
                    // final depth check usually fails.
                    // Provenance: black-box compatibility test
                    // (graph_data_differential.rs, `x,-1,INDEX`).
                    depth if depth < 0 => {
                        let above = usize::try_from(depth.unsigned_abs()).unwrap_or(usize::MAX);
                        let value = stack.slot(stack.len.saturating_add(above));
                        stack.push(value);
                    }
                    depth => {
                        let depth = stack.checked_count(depth)?;
                        let value = stack.buffer[stack.len - depth];
                        stack.push(value);
                    }
                }
            }
            O::Roll => stack.roll()?,
            O::Sort => {
                let count = stack.pop_count()?;
                // Compatibility: a negative count sorts nothing, as on macOS.
                // Provenance: black-box compatibility test (sort-06).
                let count = stack.checked_count(count.max(0))?;
                ordering::sort(stack.top_mut(count));
            }
            O::Reverse => {
                let count = stack.pop_count()?;
                let count = stack.checked_count(count.max(0))?;
                stack.top_mut(count).reverse();
            }
            O::Average => {
                let count = stack.pop_count()?;
                // Compatibility: a negative count consumes nothing and
                // yields unknown.
                // Provenance: black-box compatibility test (avg-16).
                let count = stack.checked_count(count.max(0))?;
                let value = average(stack.top_mut(count).iter().rev().copied());
                stack.replace_top(count, value);
            }
            O::SetMin | O::SetMax => {
                let count = stack.pop_count()?;
                let count = stack.checked_count(count)?;
                // Compatibility: values are taken top first and only a
                // strictly smaller (larger) one replaces the candidate, which
                // decides between signed zeros.
                // Provenance: black-box compatibility test
                // (graph_data_differential.rs, signed zero cases).
                let known = stack
                    .top_mut(count)
                    .iter()
                    .rev()
                    .copied()
                    .filter(|v| !v.is_nan());
                let value = if operator == O::SetMin {
                    known.reduce(|best, v| if v < best { v } else { best })
                } else {
                    known.reduce(|best, v| if v > best { v } else { best })
                };
                stack.replace_top(count, value.unwrap_or_else(rrd_nan));
            }
            O::Median => {
                let count = stack.pop_count()?;
                let count = stack.checked_count(count)?;
                let value = median(stack.top_mut(count));
                stack.replace_top(count, value);
            }
            O::StandardDeviation => {
                let count = stack.pop_count()?;
                let count = stack.checked_count(count)?;
                let value = standard_deviation(stack.top_mut(count).iter().rev().copied());
                stack.replace_top(count, value);
            }
            O::Percentile => stack.percentile()?,
            O::Trend | O::TrendNan => {
                stack.require(2)?;
                let cursor = self
                    .series_before(position, 2)
                    .ok_or(RpnError::MalformedTrend)?;
                let stack = &mut self.stack;
                let window = c_long(stack.pop()?);
                stack.pop()?;
                let value = trend(&self.cursors[cursor], window, row, operator == O::TrendNan);
                stack.push(value);
            }
            O::Predict | O::PredictSigma | O::PredictPercentile => {
                let percentile = operator == O::PredictPercentile;
                stack.require(if percentile { 4 } else { 3 })?;
                let cursor = self
                    .series_before(position, 1)
                    .ok_or(RpnError::MalformedPredict)?;
                let value = predict(
                    &mut self.stack,
                    &self.cursors[cursor],
                    operator,
                    row,
                    &mut self.scratch,
                )?;
                self.stack.push(value);
            }
        }
        Ok(())
    }
}

fn truth(condition: bool) -> f64 {
    if condition { 1.0 } else { 0.0 }
}

fn wall_clock() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs() as f64)
}

/// The mean of the known values; unknown when there are none.
fn average(values: impl Iterator<Item = f64>) -> f64 {
    // Compatibility: the sum starts at +0, so averaging negative zeros
    // gives +0.
    // Provenance: black-box compatibility test (rpn_differential.rs).
    let (sum, count) = values
        .filter(|v| !v.is_nan())
        .fold((0.0, 0_u64), |(sum, count), v| (sum + v, count + 1));
    if count == 0 {
        rrd_nan()
    } else {
        sum / count as f64
    }
}

/// The median of the known values, reordering `values` in place.
fn median(values: &mut [f64]) -> f64 {
    // Compatibility: each unknown value, scanning from the deepest, is
    // replaced by the value in the highest unscanned position before the
    // remaining values are sorted. Only the order of infinities depends on
    // this.
    // Provenance: reference-source-inspection; see "Set operators" in
    // docs/compatibility/rpn.md.
    let mut known = values.len();
    let mut index = 0;
    while index < known {
        if values[index].is_nan() {
            known -= 1;
            values[index] = values[known];
        } else {
            index += 1;
        }
    }
    let known_values = &mut values[..known];
    ordering::sort(known_values);
    match known {
        0 => rrd_nan(),
        _ if known % 2 == 1 => known_values[known / 2],
        _ => (known_values[known / 2 - 1] + known_values[known / 2]) / 2.0,
    }
}

/// The sample standard deviation of the known values, taken in `values`
/// order with a running mean.
fn standard_deviation(values: impl Iterator<Item = f64>) -> f64 {
    // Compatibility: one-pass running mean; the squared-deviation update is
    // fused on aarch64.
    // Provenance: reference-source-inspection; see "Set operators" in
    // docs/compatibility/rpn.md.
    let mut count = 0.0;
    let mut mean = 0.0;
    let mut squares = 0.0;
    let mut infinite = false;
    for value in values.filter(|v| !v.is_nan()) {
        infinite |= value.is_infinite();
        count += 1.0;
        let delta = value - mean;
        mean += delta / count;
        squares = rrd_mul_add(delta, value - mean, squares);
    }
    if count < 2.0 || infinite {
        rrd_nan()
    } else {
        (squares / (count - 1.0)).sqrt()
    }
}

/// `ceil(value / step)` for a positive step, without overflow.
fn ceil_div(value: i64, step: u64) -> i64 {
    let quotient = i128::from(value).div_euclid(i128::from(step));
    let rounded = if i128::from(value).rem_euclid(i128::from(step)) == 0 {
        quotient
    } else {
        quotient + 1
    };
    i64::try_from(rounded).unwrap_or(if rounded > 0 { i64::MAX } else { i64::MIN })
}

/// TREND and TRENDNAN: the mean of the variable's latest samples covering
/// `window` seconds.
fn trend(cursor: &Cursor<'_>, window: i64, row: &Row, skip_unknown: bool) -> f64 {
    let samples = ceil_div(window, cursor.step).max(1);
    // Compatibility: the warm-up rows that yield unknown are counted with
    // the window and step rounded to single precision, while the average
    // spans the exact count. They differ only for steps beyond 2^24 s, where
    // a window slightly over one step averages two samples from the first
    // row.
    // Provenance: black-box compatibility test
    // (alias_modes::rrdtool_xport_trend_duration_rounding_matches_rrd_strtod)
    // and probes; see "TREND and TRENDNAN" in docs/compatibility/rpn.md.
    let warm_up = f64::from(((window as f32) / (cursor.step as f32)).ceil());
    let end = cursor.window_end(row.time);
    if (row.index as f64) < warm_up - 1.0 {
        return rrd_nan();
    }
    // Only the samples inside the series are visited, newest first, so a
    // huge window costs at most the series length per row. Positions past
    // the end are unknown: TREND is then unknown and TRENDNAN skips them.
    // Positions before the start read as zero, which adds nothing to the
    // sum but counts.
    let newest = cursor.position.saturating_add(end);
    let oldest = newest.saturating_sub(samples - 1);
    let last_index = i64::try_from(cursor.values.len()).unwrap_or(i64::MAX) - 1;
    if newest > last_index && !skip_unknown {
        return rrd_nan();
    }
    let mut sum = 0.0;
    let mut count = 0_u64;
    for index in (oldest.max(0)..=newest.min(last_index)).rev() {
        let value = cursor.values[index as usize];
        if skip_unknown && value.is_nan() {
            continue;
        }
        sum += value;
        count += 1;
    }
    let before_start = 0_i64.saturating_sub(oldest).min(samples).max(0);
    count += before_start as u64;
    if count == 0 {
        rrd_nan()
    } else {
        sum / count as f64
    }
}

/// PREDICT, PREDICTSIGMA and PREDICTPERC. The variable's value is still on
/// the stack and is popped here with the other operands.
fn predict(
    stack: &mut Stack,
    cursor: &Cursor<'_>,
    operator: Operator,
    row: &Row,
    scratch: &mut Vec<f64>,
) -> Result<f64, RpnError> {
    stack.pop()?;
    let percentile = if operator == Operator::PredictPercentile {
        let percentile = stack.pop()?;
        if percentile.abs() > 100.0 {
            return Err(RpnError::UnsupportedPercentile(percentile));
        }
        Some(percentile)
    } else {
        None
    };
    let window = c_long(stack.pop()?);
    let shift_count = c_long(stack.pop()?);

    // Offset o reads the sample o - 1 before the window end.
    let next = cursor.window_end(row.time) + 1;
    let row = i64::try_from(row.index).unwrap_or(i64::MAX);
    let span = ceil_div(window, cursor.step);
    let mut samples = std::mem::take(scratch);
    samples.clear();
    let mut visited = 0_usize;
    // Gathers the samples for one shift: offsets D..=D+span that are below
    // the row index.
    let mut gather = |shift: i64, samples: &mut Vec<f64>| -> Result<bool, RpnError> {
        if shift < 0 {
            return Err(RpnError::NegativeShift(shift));
        }
        let first = ceil_div(shift, cursor.step);
        if span < 0 || first >= row {
            return Ok(false);
        }
        let last = first.saturating_add(span).min(row - 1);
        visited = visited.saturating_add(usize::try_from(last - first + 1).unwrap_or(usize::MAX));
        if visited > MAX_PREDICT_SAMPLES {
            return Err(RpnError::OutOfMemory(shift_count));
        }
        for offset in first..=last {
            let value = cursor.sample(next - offset);
            if !value.is_nan() {
                samples.push(value);
            }
        }
        Ok(true)
    };

    if shift_count > 0 {
        let count = stack.checked_count(shift_count)?;
        let shifts: Vec<i64> = stack.top_mut(count).iter().map(|&v| c_long(v)).collect();
        stack.truncate(stack.len - count);
        for shift in shifts {
            gather(shift, &mut samples)?;
        }
    } else if shift_count < 0 {
        // Compatibility: a negative count -k takes one multiplier m and
        // uses the shifts 0, m, ..., (k-1)m.
        // Provenance: black-box compatibility test (predict-05, -06, -10).
        let multiplier = c_long(stack.pop()?);
        let shifts = shift_count.unsigned_abs();
        if multiplier < 0 && shifts > 1 {
            return Err(RpnError::NegativeShift(multiplier));
        }
        for index in 0..shifts {
            let shift =
                i64::try_from(i128::from(index) * i128::from(multiplier)).unwrap_or(i64::MAX);
            let gathered = gather(shift, &mut samples)?;
            // Later shifts are larger (or, with a zero multiplier, equal),
            // so once one yields nothing the rest do too.
            if !gathered {
                break;
            }
        }
    }

    let value = match (operator, percentile) {
        (Operator::PredictSigma, _) => sigma(&samples),
        (_, Some(percentile)) => interpolated_percentile(&mut samples, percentile),
        _ => average(samples.iter().copied()),
    };
    *scratch = samples;
    Ok(value)
}

/// PREDICTSIGMA's spread: `sqrt((n*q - s*s) / (n*(n-1)))` over count,
/// sum and sum of squares.
fn sigma(samples: &[f64]) -> f64 {
    // Compatibility: on aarch64 the sum of squares is accumulated with fused
    // multiply-add, and `n*q - s*s` is fused too, with `s*s` rounded first.
    // Samples are summed shift by shift, deepest shift first.
    // Provenance: reference-source-inspection for the accumulation (see
    // "PREDICT, PREDICTSIGMA, PREDICTPERC" in docs/compatibility/rpn.md);
    // black-box compatibility test for the numerator and the order
    // (graph_engine_parity.rs).
    let count = samples.len() as f64;
    let (sum, squares) = samples.iter().fold((0.0, 0.0), |(sum, squares), &v| {
        (sum + v, rrd_mul_add(v, v, squares))
    });
    if samples.len() < 2 {
        return rrd_nan();
    }
    let radicand = rrd_mul_add(count, squares, -(sum * sum)) / (count * (count - 1.0));
    if radicand < 0.0 {
        rrd_nan()
    } else {
        radicand.sqrt()
    }
}

/// PREDICTPERC's pick from the sorted samples.
fn interpolated_percentile(samples: &mut [f64], percentile: f64) -> f64 {
    if samples.is_empty() || percentile.is_nan() {
        return rrd_nan();
    }
    ordering::sort(samples);
    let position = percentile / 100.0 * (samples.len() - 1) as f64;
    // Compatibility: a negative percentile picks the nearest rank of its
    // magnitude; a positive one interpolates between neighbours, fused on
    // aarch64.
    // Provenance: reference-source-inspection (see "PREDICT, PREDICTSIGMA,
    // PREDICTPERC" in docs/compatibility/rpn.md); black-box compatibility
    // test for the fused step (graph_engine_parity.rs).
    if percentile < 0.0 {
        return samples[(position.abs().round() as usize).min(samples.len() - 1)];
    }
    let base = position.floor();
    let lower = samples[(base as usize).min(samples.len() - 1)];
    let fraction = position - base;
    if fraction == 0.0 {
        lower
    } else {
        let upper = samples[(base as usize + 1).min(samples.len() - 1)];
        rrd_mul_add(fraction, upper - lower, lower)
    }
}

/// The evaluation stack. Popping lowers `len` but keeps the slot's value,
/// because ROLL reads the slots just above the top (see "ROLL" in
/// docs/compatibility/rpn.md). The buffer is reused across rows.
#[derive(Debug, Default)]
struct Stack {
    buffer: Vec<f64>,
    len: usize,
}

impl Stack {
    fn clear(&mut self) {
        self.len = 0;
    }

    fn values(&self) -> &[f64] {
        &self.buffer[..self.len]
    }

    fn push(&mut self, value: f64) {
        if self.len == self.buffer.len() {
            self.buffer.push(value);
        } else {
            self.buffer[self.len] = value;
        }
        self.len += 1;
    }

    fn require(&self, depth: usize) -> Result<(), RpnError> {
        if self.len < depth {
            Err(RpnError::StackUnderflow)
        } else {
            Ok(())
        }
    }

    fn pop(&mut self) -> Result<f64, RpnError> {
        self.require(1)?;
        self.len -= 1;
        Ok(self.buffer[self.len])
    }

    fn peek(&self) -> Result<f64, RpnError> {
        self.require(1)?;
        Ok(self.buffer[self.len - 1])
    }

    fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }

    /// The value at `index`, live or stale; unknown past anything written.
    fn slot(&self, index: usize) -> f64 {
        self.buffer.get(index).copied().unwrap_or_else(rrd_nan)
    }

    fn top_mut(&mut self, count: usize) -> &mut [f64] {
        let len = self.len;
        &mut self.buffer[len - count..len]
    }

    /// Replaces the top `count` values with `value`.
    fn replace_top(&mut self, count: usize, value: f64) {
        self.len -= count;
        self.push(value);
    }

    /// Pops a count operand, truncated toward zero.
    fn pop_count(&mut self) -> Result<i64, RpnError> {
        Ok(c_long(self.pop()?))
    }

    /// Checks that `count` values are available. A negative count is
    /// undefined in the reference (it crashes or reads outside the stack);
    /// Rondi reports it as an underflow.
    fn checked_count(&self, count: i64) -> Result<usize, RpnError> {
        usize::try_from(count)
            .ok()
            .filter(|&count| count <= self.len)
            .ok_or(RpnError::StackUnderflow)
    }

    fn unary(&mut self, operation: impl FnOnce(f64) -> f64) -> Result<(), RpnError> {
        let value = self.pop()?;
        self.push(operation(value));
        Ok(())
    }

    fn binary(&mut self, operation: impl FnOnce(f64, f64) -> f64) -> Result<(), RpnError> {
        self.require(2)?;
        let y = self.pop()?;
        let x = self.pop()?;
        self.push(operation(x, y));
        Ok(())
    }

    fn compare(&mut self, holds: impl FnOnce(f64, f64) -> bool) -> Result<(), RpnError> {
        self.binary(|x, y| {
            if x.is_nan() || y.is_nan() {
                rrd_nan()
            } else {
                truth(holds(x, y))
            }
        })
    }

    /// `n,m,ROLL`.
    fn roll(&mut self) -> Result<(), RpnError> {
        self.require(2)?;
        let shift = c_int(self.pop()?);
        let count = c_int(self.pop()?);
        if i64::from(count) > self.len as i64 {
            return Err(RpnError::StackUnderflow);
        }
        if count < 0 {
            return Err(RpnError::OutOfMemory(i64::from(count)));
        }
        let count = count as usize;
        if count <= 1 {
            return Ok(());
        }
        // Compatibility: the rotation reads a window made of the top value
        // and the slots above it, which still hold the popped count and
        // shift as written; slot j of the top `count` (lowest first) takes
        // window entry ((count - 1 + shift, in wrapping 32-bit arithmetic)
        // - j) mod count.
        // Provenance: reference-source-inspection and black-box probes; see
        // "ROLL" in docs/compatibility/rpn.md.
        let bottom = self.len - count;
        let window: Vec<f64> = (0..count).map(|k| self.slot(self.len - 1 + k)).collect();
        let base = i64::from((count as i32 - 1).wrapping_add(shift));
        for slot in 0..count {
            let source = (base - slot as i64).rem_euclid(count as i64) as usize;
            self.buffer[bottom + slot] = window[source];
        }
        Ok(())
    }

    /// `p,n,PERCENT`.
    fn percentile(&mut self) -> Result<(), RpnError> {
        self.require(2)?;
        let count = self.pop_count()?;
        let percentile = self.pop()?;
        if !(0.0..=100.0).contains(&percentile) {
            return Err(RpnError::PercentileRange);
        }
        let count = self.checked_count(count)?;
        ordering::sort(self.top_mut(count));
        let bottom = self.len - count;
        let rank = (percentile * count as f64 / 100.0).round() as usize;
        // Compatibility: rank 0 copies the value just below the counted
        // values, which stays on the stack; with nothing below it is an
        // underflow.
        // Provenance: black-box compatibility test (percent-26, -28) and
        // `50,0,PERCENT,x,+` in rpn_differential.rs.
        let value = match rank {
            0 => self.buffer[bottom.checked_sub(1).ok_or(RpnError::StackUnderflow)?],
            _ => self.buffer[bottom + rank.min(count) - 1],
        };
        self.replace_top(count, value);
        Ok(())
    }
}
