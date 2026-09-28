//! Query and resample existing RRD archives for RRDtool-style exports.
use crate::{RrdFetchResult, StoreError, fetch_rrd_file};
use std::{collections::HashMap, path::PathBuf};

#[inline]
fn rrd_nan() -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        f64::from_bits(0xfff8_0000_0000_0000)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        f64::NAN
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrdXportDefinition {
    pub name: String,
    pub file: PathBuf,
    pub data_source: String,
    pub consolidation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrdXportColumn {
    pub variable: String,
    pub legend: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrdXportCdef {
    pub name: String,
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RrdXportResult {
    pub start: i64,
    pub end: i64,
    pub step: u64,
    pub legends: Vec<String>,
    pub rows: Vec<Vec<Option<f64>>>,
}

struct FetchedDefinition {
    name: String,
    fetched: RrdFetchResult,
    source_index: usize,
}

/// Fetch raw DEF sources, align them to a shared step, and select XPORT columns.
/// CDEF/RPN evaluation and output formatting are deliberately separate.
pub fn fetch_xport(
    definitions: &[RrdXportDefinition],
    columns: &[RrdXportColumn],
    start: i64,
    end: i64,
    requested_step: u64,
    max_rows: u64,
) -> Result<RrdXportResult, StoreError> {
    fetch_xport_with_cdefs(
        definitions,
        &[],
        columns,
        start,
        end,
        requested_step,
        max_rows,
    )
}

/// Fetch DEF sources and evaluate the row-wise RPN subset used by CDEFs.
/// Definitions are evaluated in order and may refer to earlier DEFs/CDEFs.
pub fn fetch_xport_with_cdefs(
    definitions: &[RrdXportDefinition],
    cdefs: &[RrdXportCdef],
    columns: &[RrdXportColumn],
    start: i64,
    end: i64,
    requested_step: u64,
    max_rows: u64,
) -> Result<RrdXportResult, StoreError> {
    if definitions.is_empty() || columns.is_empty() {
        return Err(StoreError::RrdUnsupported(
            "xport requires at least one DEF and XPORT".into(),
        ));
    }
    if end < start {
        return Err(StoreError::RrdUnsupported(
            "xport end must not precede start".into(),
        ));
    }
    if max_rows < 10 {
        return Err(StoreError::RrdUnsupported("maxrows below 10 rows".into()));
    }
    let range = end
        .checked_sub(start)
        .and_then(|range| u64::try_from(range).ok())
        .ok_or_else(|| StoreError::RrdUnsupported("xport range overflows".into()))?;
    let automatic_step = range / max_rows;
    let fetch_resolution = requested_step.max(automatic_step).max(1);

    let mut fetched_definitions = Vec::with_capacity(definitions.len());
    let mut output_step = 0_u64;
    for definition in definitions {
        let fetched = fetch_rrd_file(
            &definition.file,
            &definition.consolidation,
            start,
            end,
            fetch_resolution,
        )?;
        let source_index = fetched
            .data_sources
            .iter()
            .position(|source| source == &definition.data_source)
            .ok_or_else(|| {
                StoreError::RrdUnsupported(format!(
                    "unknown data source '{}' in {}",
                    definition.data_source,
                    definition.file.display()
                ))
            })?;
        output_step = if output_step == 0 {
            fetched.step
        } else {
            gcd(output_step, fetched.step)
        };
        fetched_definitions.push(FetchedDefinition {
            name: definition.name.clone(),
            fetched,
            source_index,
        });
    }

    let step_i64 = i64::try_from(output_step)
        .map_err(|_| StoreError::RrdUnsupported("xport step overflows".into()))?;
    let aligned_start = start
        .checked_sub(start.rem_euclid(step_i64))
        .ok_or_else(|| StoreError::RrdUnsupported("xport start overflows".into()))?;
    let floor_end = end
        .checked_sub(end.rem_euclid(step_i64))
        .ok_or_else(|| StoreError::RrdUnsupported("xport end overflows".into()))?;
    let aligned_end = if end > floor_end {
        floor_end
            .checked_add(step_i64)
            .ok_or_else(|| StoreError::RrdUnsupported("xport end overflows".into()))?
    } else {
        floor_end
    };
    let row_count = aligned_end
        .checked_sub(aligned_start)
        .and_then(|range| usize::try_from(range / step_i64).ok())
        .ok_or_else(|| StoreError::RrdUnsupported("xport output is too large".into()))?;
    if row_count > 10_000_000 {
        return Err(StoreError::RrdUnsupported(
            "xport output exceeds the 10 million row safety limit".into(),
        ));
    }

    let mut variables: HashMap<String, Vec<f64>> = HashMap::new();
    let mut variable_steps: HashMap<String, u64> = HashMap::new();
    for definition in &fetched_definitions {
        let source_step = i64::try_from(definition.fetched.step)
            .map_err(|_| StoreError::RrdUnsupported("DEF step overflows".into()))?;
        let mut values = Vec::with_capacity(row_count);
        for row_index in 0..row_count {
            let row_offset = i64::try_from(row_index)
                .ok()
                .and_then(|row| row.checked_mul(step_i64))
                .ok_or_else(|| StoreError::RrdUnsupported("xport row time overflows".into()))?;
            let bucket_start = aligned_start
                .checked_add(row_offset)
                .ok_or_else(|| StoreError::RrdUnsupported("xport row time overflows".into()))?;
            let offset = bucket_start
                .checked_sub(definition.fetched.start)
                .ok_or_else(|| StoreError::RrdUnsupported("DEF time offset overflows".into()))?;
            let source_row = offset / source_step;
            values.push(if source_row < 0 {
                rrd_nan()
            } else {
                usize::try_from(source_row)
                    .ok()
                    .and_then(|idx| definition.fetched.rows.get(idx))
                    .and_then(|row| row.values[definition.source_index])
                    .unwrap_or(rrd_nan())
            });
        }
        variable_steps.insert(definition.name.clone(), definition.fetched.step);
        variables.insert(definition.name.clone(), values);
    }
    for cdef in cdefs {
        if variables.contains_key(&cdef.name) {
            return Err(StoreError::RrdUnsupported(format!(
                "duplicate xport variable '{}'",
                cdef.name
            )));
        }
        let mut values = Vec::with_capacity(row_count);
        for row_index in 0..row_count {
            let timestamp = i64::try_from(row_index)
                .ok()
                .and_then(|row| row.checked_add(1))
                .and_then(|row| row.checked_mul(step_i64))
                .and_then(|offset| aligned_start.checked_add(offset))
                .ok_or_else(|| StoreError::RrdUnsupported("CDEF timestamp overflows".into()))?;
            let previous = values.last().copied().unwrap_or(rrd_nan());
            values.push(evaluate_rpn(
                &cdef.expression,
                &variables,
                &variable_steps,
                row_index,
                timestamp,
                output_step,
                previous,
            )?);
        }
        variable_steps.insert(cdef.name.clone(), output_step);
        variables.insert(cdef.name.clone(), values);
    }
    let mut rows = vec![Vec::with_capacity(columns.len()); row_count];
    for (row_index, output_row) in rows.iter_mut().enumerate() {
        for column in columns {
            let values = variables.get(&column.variable).ok_or_else(|| {
                StoreError::RrdUnsupported(format!(
                    "unknown DEF or CDEF name '{}'",
                    column.variable
                ))
            })?;
            let value = values[row_index];
            output_row.push(if value.is_finite() { Some(value) } else { None });
        }
    }
    Ok(RrdXportResult {
        start: aligned_start,
        end: aligned_end,
        step: output_step,
        legends: columns.iter().map(|column| column.legend.clone()).collect(),
        rows,
    })
}

fn evaluate_rpn(
    expression: &str,
    variables: &HashMap<String, Vec<f64>>,
    variable_steps: &HashMap<String, u64>,
    row: usize,
    timestamp: i64,
    step_width: u64,
    previous: f64,
) -> Result<f64, StoreError> {
    let mut stack = Vec::<f64>::new();
    let tokens: Vec<_> = expression.split(',').collect();
    for (token_index, token) in tokens.iter().enumerate() {
        let token = *token;
        let unary: Option<fn(f64) -> f64> = match token {
            "UNKN" => {
                stack.push(rrd_nan());
                continue;
            }
            "INF" => {
                stack.push(f64::INFINITY);
                continue;
            }
            "NEGINF" => {
                stack.push(f64::NEG_INFINITY);
                continue;
            }
            "TIME" => {
                stack.push(timestamp as f64);
                continue;
            }
            "COUNT" => {
                stack.push((row + 1) as f64);
                continue;
            }
            "STEPWIDTH" => {
                stack.push(step_width as f64);
                continue;
            }
            "PREV" => {
                stack.push(previous);
                continue;
            }
            "NOW" => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    // RRDtool's OP_NOW uses time(NULL), which has whole-second
                    // precision. Do not leak subsecond clock precision into RPN.
                    .map(|duration| duration.as_secs() as f64)
                    .unwrap_or(rrd_nan());
                stack.push(now);
                continue;
            }
            "LTIME" => {
                stack.push(
                    local_time_offset(timestamp)
                        .map_or(rrd_nan(), |offset| timestamp as f64 + offset as f64),
                );
                continue;
            }
            "NEWDAY" | "NEWWEEK" | "NEWMONTH" | "NEWYEAR" => {
                let current = local_tm(timestamp);
                let prior = i64::try_from(step_width)
                    .ok()
                    .and_then(|step| timestamp.checked_sub(step))
                    .and_then(local_tm);
                let changed = current
                    .zip(prior)
                    .is_some_and(|(current, prior)| match token {
                        "NEWDAY" => current.tm_mday != prior.tm_mday,
                        "NEWWEEK" => {
                            current.tm_wday == rrd_first_weekday()
                                && current.tm_wday != prior.tm_wday
                        }
                        "NEWMONTH" => current.tm_mon != prior.tm_mon,
                        "NEWYEAR" => current.tm_year != prior.tm_year,
                        _ => unreachable!(),
                    });
                stack.push(if changed { 1.0 } else { 0.0 });
                continue;
            }
            "DUP" => {
                let a = *stack.last().ok_or_else(|| rpn_error("stack underflow"))?;
                stack.push(a);
                continue;
            }
            "POP" => {
                stack.pop().ok_or_else(|| rpn_error("stack underflow"))?;
                continue;
            }
            "EXC" => {
                if stack.len() < 2 {
                    return Err(rpn_error("stack underflow"));
                }
                let n = stack.len();
                stack.swap(n - 1, n - 2);
                continue;
            }
            "DEPTH" => {
                stack.push(stack.len() as f64);
                continue;
            }
            "SIN" => Some(f64::sin),
            "COS" => Some(f64::cos),
            "LOG" => Some(f64::ln),
            "EXP" => Some(f64::exp),
            "SQRT" => Some(f64::sqrt),
            "FLOOR" => Some(f64::floor),
            "CEIL" => Some(f64::ceil),
            "ROUND" => Some(f64::round),
            "ABS" => Some(f64::abs),
            "ATAN" => Some(f64::atan),
            "RAD2DEG" => Some(|value| 57.29577951 * value),
            "DEG2RAD" => Some(|value| 0.0174532952 * value),
            "UN" => {
                let a = pop(&mut stack)?;
                stack.push(if a.is_nan() { 1.0 } else { 0.0 });
                continue;
            }
            "ISINF" => {
                let a = pop(&mut stack)?;
                stack.push(if a.is_infinite() { 1.0 } else { 0.0 });
                continue;
            }
            _ => None,
        };
        if let Some(op) = unary {
            let a = pop(&mut stack)?;
            stack.push(op(a));
            continue;
        }
        let binary: Option<fn(f64, f64) -> f64> = match token {
            "+" => Some(|a: f64, b: f64| a + b),
            "-" => Some(|a, b| a - b),
            "*" => Some(|a, b| a * b),
            "/" => Some(|a, b| a / b),
            "%" => Some(|a, b| a % b),
            "POW" => Some(|a, b| a.powf(b)),
            "ATAN2" => Some(|a: f64, b: f64| a.atan2(b)),
            "MIN" => Some(|a: f64, b: f64| {
                if a.is_nan() || b.is_nan() {
                    rrd_nan()
                } else {
                    a.min(b)
                }
            }),
            "MAX" => Some(|a: f64, b: f64| {
                if a.is_nan() || b.is_nan() {
                    rrd_nan()
                } else {
                    a.max(b)
                }
            }),
            "ADDNAN" => Some(|a: f64, b: f64| {
                if a.is_nan() {
                    b
                } else if b.is_nan() {
                    a
                } else {
                    a + b
                }
            }),
            "MINNAN" => Some(|a: f64, b: f64| {
                if a.is_nan() {
                    b
                } else if b.is_nan() {
                    a
                } else {
                    a.min(b)
                }
            }),
            "MAXNAN" => Some(|a: f64, b: f64| {
                if a.is_nan() {
                    b
                } else if b.is_nan() {
                    a
                } else {
                    a.max(b)
                }
            }),
            "LT" => Some(|a: f64, b: f64| cmp(a, b, a < b)),
            "LE" => Some(|a, b| cmp(a, b, a <= b)),
            "GT" => Some(|a, b| cmp(a, b, a > b)),
            "GE" => Some(|a, b| cmp(a, b, a >= b)),
            "EQ" => Some(|a, b| cmp(a, b, a == b)),
            "NE" => Some(|a, b| cmp(a, b, a != b)),
            _ => None,
        };
        if let Some(op) = binary {
            let b = pop(&mut stack)?;
            let a = pop(&mut stack)?;
            stack.push(op(a, b));
            continue;
        }
        match token {
            token if token.starts_with("PREV(") && token.ends_with(')') => {
                let variable = &token[5..token.len() - 1];
                let values = variables
                    .get(variable)
                    .ok_or_else(|| rpn_error(&format!("unknown variable '{variable}'")))?;
                let prior = row
                    .checked_sub(1)
                    .and_then(|prior_row| values.get(prior_row))
                    .copied()
                    .unwrap_or(rrd_nan());
                stack.push(prior);
            }
            "AVG" | "MEDIAN" | "STDEV" | "SMIN" | "SMAX" | "SORT" | "REV" => {
                let count = pop_count(&mut stack)?;
                if stack.len() < count {
                    return Err(rpn_error("stack underflow"));
                }
                let split = stack.len() - count;
                let mut values = stack.split_off(split);
                match token {
                    "SORT" => {
                        values.sort_by(rrd_percent_cmp);
                        stack.extend(values);
                    }
                    "REV" => {
                        values.reverse();
                        stack.extend(values);
                    }
                    "AVG" => {
                        let known: Vec<f64> = values.into_iter().filter(|v| !v.is_nan()).collect();
                        let known_count = known.len();
                        stack.push(if known.is_empty() {
                            rrd_nan()
                        } else {
                            // RRDtool pops aggregate operands from the RPN stack, so it
                            // adds the rightmost operand first. Floating point addition
                            // is order-sensitive; preserve that operation order.
                            known.into_iter().rev().sum::<f64>() / known_count as f64
                        });
                    }
                    "MEDIAN" => {
                        values.retain(|v| !v.is_nan());
                        values.sort_by(f64::total_cmp);
                        let n = values.len();
                        stack.push(if n == 0 {
                            rrd_nan()
                        } else if n % 2 == 1 {
                            values[n / 2]
                        } else {
                            (values[n / 2 - 1] + values[n / 2]) * 0.5
                        });
                    }
                    "STDEV" => {
                        let mut n = 0.0;
                        let mut mean = 0.0;
                        let mut mean2 = 0.0;
                        // rrd_rpncalc.c consumes the aggregate operands by popping the
                        // stack, which visits them in reverse expression order.
                        for datum in values.into_iter().rev().filter(|v| !v.is_nan()) {
                            n += 1.0;
                            let delta = datum - mean;
                            mean += delta / n;
                            mean2 += delta * (datum - mean);
                        }
                        stack.push(if n < 2.0 {
                            rrd_nan()
                        } else {
                            (mean2 / (n - 1.0)).sqrt()
                        });
                    }
                    "SMIN" | "SMAX" => {
                        let result = values
                            .into_iter()
                            .filter(|v| !v.is_nan())
                            .reduce(|a, b| if token == "SMIN" { a.min(b) } else { a.max(b) });
                        stack.push(result.unwrap_or(rrd_nan()));
                    }
                    _ => unreachable!(),
                }
            }
            "COPY" => {
                let count = pop_count(&mut stack)?;
                if stack.len() < count {
                    return Err(rpn_error("stack underflow"));
                }
                let start = stack.len() - count;
                let copied = stack[start..].to_vec();
                stack.extend(copied);
            }
            "INDEX" => {
                let index = stack
                    .last()
                    .copied()
                    .ok_or_else(|| rpn_error("stack underflow"))?;
                // The upstream OP_INDEX implementation converts its RPN number
                // to C `int`, truncating fractional values toward zero.
                let index = index.trunc();
                if !index.is_finite() || index < 0.0 || index > i32::MAX as f64 {
                    return Err(rpn_error("invalid INDEX value"));
                }
                let index = index as usize;
                let target = stack
                    .len()
                    .checked_sub(index + 1)
                    .ok_or_else(|| rpn_error("INDEX out of range"))?;
                let top = stack.len() - 1;
                stack[top] = stack[target];
            }
            "ROLL" => {
                let shift = pop(&mut stack)?;
                let shift = if shift.is_finite() {
                    shift as isize
                } else {
                    return Err(rpn_error("invalid ROLL shift"));
                };
                let count = pop_count(&mut stack)?;
                if stack.len() < count {
                    return Err(rpn_error("stack underflow"));
                }
                if count == 0 {
                    continue;
                }
                let start = stack.len() - count;
                let amount = shift.rem_euclid(count as isize) as usize;
                stack[start..].rotate_right(amount);
            }
            "PERCENT" => {
                let count = pop_count(&mut stack)?;
                let percent = pop(&mut stack)?;
                if !(0.0..=100.0).contains(&percent) || !percent.is_finite() {
                    return Err(rpn_error("percentile argument must be between 0 and 100"));
                }
                if count == 0 || stack.len() < count {
                    return Err(rpn_error("stack underflow or invalid PERCENT count"));
                }
                let start = stack.len() - count;
                let mut values = stack.split_off(start);
                values.sort_by(rrd_percent_cmp);
                let rounded = (percent * count as f64 / 100.0).round() as usize;
                let index = rounded.saturating_sub(1).min(count - 1);
                let selected = values[index];
                stack.push(selected);
            }
            "TREND" | "TRENDNAN" => {
                if token_index < 2 {
                    return Err(rpn_error("malformed trend arguments"));
                }
                let variable = tokens[token_index - 2];
                let duration = tokens[token_index - 1]
                    .parse::<f64>()
                    .map_err(|_| rpn_error("trend duration must follow a variable"))?;
                let values = variables
                    .get(variable)
                    .ok_or_else(|| rpn_error("trend must immediately follow a variable"))?;
                let source_step = *variable_steps
                    .get(variable)
                    .ok_or_else(|| rpn_error("missing trend source step"))?;
                let duration_seconds = if duration.is_finite() {
                    duration as i64
                } else {
                    return Err(rpn_error("invalid trend duration"));
                };
                let source_step_i64 = i64::try_from(source_step)
                    .map_err(|_| rpn_error("trend source step overflows"))?;
                let window = if duration_seconds <= 0 {
                    1
                } else {
                    usize::try_from(
                        duration_seconds.saturating_add(source_step_i64 - 1) / source_step_i64,
                    )
                    .map_err(|_| rpn_error("trend window is too large"))?
                };
                let required = window;
                let value = if row + 1 < required {
                    rrd_nan()
                } else {
                    let stride = usize::try_from(source_step / step_width)
                        .map_err(|_| rpn_error("trend step ratio overflows"))?
                        .max(1);
                    let mut sum = 0.0;
                    let mut count = 0_usize;
                    let mut unknown = false;
                    for offset in 0..window {
                        let Some(index) = row.checked_sub(offset.saturating_mul(stride)) else {
                            unknown = true;
                            break;
                        };
                        let sample = values.get(index).copied().unwrap_or(f64::NAN);
                        if sample.is_nan() {
                            if token == "TREND" {
                                unknown = true;
                                break;
                            }
                        } else {
                            sum += sample;
                            count += 1;
                        }
                    }
                    if unknown || count == 0 {
                        rrd_nan()
                    } else {
                        sum / count as f64
                    }
                };
                let _duration_value = pop(&mut stack)?;
                let _source_value = pop(&mut stack)?;
                stack.push(value);
            }
            "PREDICT" | "PREDICTSIGMA" | "PREDICTPERC" => {
                // Match rrd_rpncalc.c's stack contract:
                // shifts..., shift_count, window_seconds, [percentile,] x, OP.
                let percentile = if token == "PREDICTPERC" {
                    Some(pop(&mut stack)?)
                } else {
                    None
                };
                let window_seconds = pop(&mut stack)?;
                let shift_count = pop(&mut stack)?;
                if !window_seconds.is_finite() || !shift_count.is_finite() {
                    return Err(rpn_error("invalid prediction arguments"));
                }
                if percentile.is_some_and(|value| !value.is_finite() || value.abs() > 100.0) {
                    return Err(rpn_error(
                        "prediction percentile must be between -100 and 100",
                    ));
                }
                let shifts = shift_count.trunc() as i64;
                let shift_values: Vec<f64> = if shifts < 0 {
                    vec![pop(&mut stack)?]
                } else {
                    if shifts > 100_000 || stack.len() < shifts as usize {
                        return Err(rpn_error("invalid prediction shift count"));
                    }
                    let first = stack.len().saturating_sub(shifts as usize);
                    let values = stack[first..].to_vec();
                    stack.truncate(first);
                    values
                };
                let x = tokens
                    .get(token_index.wrapping_sub(1))
                    .ok_or_else(|| rpn_error("prediction must immediately follow a variable"))?;
                let values = variables
                    .get(*x)
                    .ok_or_else(|| rpn_error("prediction must immediately follow a variable"))?;
                let source_step = *variable_steps
                    .get(*x)
                    .ok_or_else(|| rpn_error("missing prediction source step"))?;
                let stride = usize::try_from(source_step / step_width.max(1))
                    .map_err(|_| rpn_error("prediction step ratio overflows"))?
                    .max(1);
                let locstepsize = window_seconds.trunc() as i64;
                let locstep = if locstepsize <= 0 {
                    0
                } else {
                    ((locstepsize as f64 / source_step as f64).ceil() as usize).min(values.len())
                };
                let shift_iterations = shifts.unsigned_abs().min(100_000) as usize;
                let mut observations = Vec::new();
                for shift_index in 0..shift_iterations {
                    let shift = if shifts < 0 {
                        shift_index as f64 * shift_values[0]
                    } else if shift_index < shift_values.len() {
                        shift_values[shift_index]
                    } else {
                        break;
                    };
                    if !shift.is_finite() || shift < 0.0 {
                        return Err(rpn_error("prediction shift must be nonnegative"));
                    }
                    let shift_steps = (shift.trunc() / source_step as f64).ceil() as usize;
                    for local_offset in 0..=locstep {
                        let offset = shift_steps.saturating_add(local_offset);
                        if offset >= row {
                            continue;
                        }
                        let Some(index) = row
                            .saturating_add(1)
                            .checked_sub(offset.saturating_mul(stride))
                        else {
                            continue;
                        };
                        if let Some(value) = values.get(index).copied().filter(|v| !v.is_nan()) {
                            observations.push(value);
                        }
                    }
                }
                let prediction = if observations.is_empty() {
                    rrd_nan()
                } else if token == "PREDICT" {
                    observations.iter().sum::<f64>() / observations.len() as f64
                } else if token == "PREDICTSIGMA" {
                    let count = observations.len() as f64;
                    if count < 2.0 {
                        rrd_nan()
                    } else {
                        let sum = observations.iter().sum::<f64>();
                        let sum2 = observations.iter().map(|value| value * value).sum::<f64>();
                        ((count * sum2 - sum * sum) / (count * (count - 1.0))).sqrt()
                    }
                } else {
                    let percentile = percentile.unwrap();
                    observations.sort_by(f64::total_cmp);
                    let position = percentile.abs() / 100.0 * (observations.len() - 1) as f64;
                    if percentile < 0.0 {
                        observations[position.round() as usize]
                    } else {
                        let lower = position.floor() as usize;
                        let upper = position.ceil() as usize;
                        let fraction = position - lower as f64;
                        observations[lower] * (1.0 - fraction) + observations[upper] * fraction
                    }
                };
                stack.push(prediction);
            }
            "IF" => {
                let no = pop(&mut stack)?;
                let yes = pop(&mut stack)?;
                let condition = pop(&mut stack)?;
                stack.push(if condition.is_nan() || condition == 0.0 {
                    no
                } else {
                    yes
                });
            }
            "LIMIT" => {
                let max = pop(&mut stack)?;
                let min = pop(&mut stack)?;
                let value = pop(&mut stack)?;
                stack.push(
                    if value.is_nan() || min.is_nan() || max.is_nan() || value < min || value > max
                    {
                        rrd_nan()
                    } else {
                        value
                    },
                );
            }
            _ => {
                if let Ok(number) = token.parse::<f64>() {
                    stack.push(number);
                } else if let Some(values) = variables.get(token) {
                    if !tokens.get(token_index + 1).is_some_and(|next| {
                        matches!(*next, "PREDICT" | "PREDICTSIGMA" | "PREDICTPERC")
                    }) {
                        stack.push(values.get(row).copied().unwrap_or(rrd_nan()));
                    }
                } else {
                    return Err(rpn_error(&format!("unknown token or variable '{token}'")));
                }
            }
        }
    }
    if stack.len() != 1 {
        return Err(rpn_error("expression must leave exactly one value"));
    }
    Ok(stack[0])
}
fn pop(stack: &mut Vec<f64>) -> Result<f64, StoreError> {
    stack.pop().ok_or_else(|| rpn_error("stack underflow"))
}
fn pop_count(stack: &mut Vec<f64>) -> Result<usize, StoreError> {
    // RRDtool stores these RPN operands in C `int` variables, truncating
    // fractional values toward zero before using them as stack counts.
    let value = pop(stack)?.trunc();
    if !value.is_finite() || value < 0.0 || value > 1_000_000.0 {
        return Err(rpn_error("invalid stack count"));
    }
    Ok(value as usize)
}
fn rrd_percent_cmp(left: &f64, right: &f64) -> std::cmp::Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        // rpn_compare_double() in RRDtool 1.11.0 explicitly sorts unknowns
        // before numbers for the RPN PERCENT operator.
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal),
    }
}
fn cmp(a: f64, b: f64, result: bool) -> f64 {
    if a.is_nan() {
        a
    } else if b.is_nan() {
        b
    } else if result {
        1.0
    } else {
        0.0
    }
}
fn rpn_error(message: &str) -> StoreError {
    StoreError::RrdUnsupported(format!("invalid CDEF RPN: {message}"))
}

fn local_tm(timestamp: i64) -> Option<libc::tm> {
    let timestamp = libc::time_t::try_from(timestamp).ok()?;
    let mut result = std::mem::MaybeUninit::<libc::tm>::uninit();
    // libc initializes the output structure when localtime_r succeeds.
    let converted = unsafe { libc::localtime_r(&timestamp, result.as_mut_ptr()) };
    (!converted.is_null()).then(|| unsafe { result.assume_init() })
}

fn local_time_offset(timestamp: i64) -> Option<i64> {
    let timestamp = libc::time_t::try_from(timestamp).ok()?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    let mut utc = std::mem::MaybeUninit::<libc::tm>::uninit();
    // Both libc conversion functions initialize their output on success.
    let local_ptr = unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) };
    let utc_ptr = unsafe { libc::gmtime_r(&timestamp, utc.as_mut_ptr()) };
    if local_ptr.is_null() || utc_ptr.is_null() {
        return None;
    }
    let local = unsafe { local.assume_init() };
    let utc = unsafe { utc.assume_init() };
    let mut offset = (local.tm_sec - utc.tm_sec) as i64
        + (local.tm_min - utc.tm_min) as i64 * 60
        + (local.tm_hour - utc.tm_hour) as i64 * 3600;
    if local.tm_yday > utc.tm_yday || local.tm_year > utc.tm_year {
        offset += 86_400;
    } else if local.tm_yday < utc.tm_yday || local.tm_year < utc.tm_year {
        offset -= 86_400;
    }
    Some(offset)
}

/// Matches RRDtool's `find_first_weekday`: glibc uses the active LC_TIME
/// metadata, while platforms without its private langinfo items use Sunday.
#[cfg(all(unix, target_env = "gnu"))]
fn rrd_first_weekday() -> i32 {
    const NL_TIME_WEEK_1STDAY: libc::nl_item = 131_174;
    const NL_TIME_FIRST_WEEKDAY: libc::nl_item = 131_176;

    // These item values are glibc's private LC_TIME langinfo entries, also
    // used by RRDtool 1.11.0's find_first_weekday implementation.
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
    (week_start + first_weekday - 1).rem_euclid(7)
}

#[cfg(not(all(unix, target_env = "gnu")))]
fn rrd_first_weekday() -> i32 {
    0
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}
