use crate::consolidation::{append_full_average_pdps, finish_average_pdp};
use crate::format::{DatabaseFile, Update};
use crate::storage::StoreError;

pub(crate) fn apply_gauge_update(db: &mut DatabaseFile, update: &Update) -> Result<(), StoreError> {
    if update.timestamp <= db.last_update {
        return Err(StoreError::OutOfOrder {
            incoming: update.timestamp,
            last: db.last_update,
        });
    }
    if update.value.is_some_and(|value| !value.is_finite()) {
        return Err(StoreError::InvalidValue);
    }
    let elapsed = u64::try_from(
        update
            .timestamp
            .checked_sub(db.last_update)
            .ok_or(StoreError::InvalidValue)?,
    )
    .map_err(|_| StoreError::InvalidValue)?;
    // RRDtool GAUGE updates assign the arriving sample's value over the
    // interval since the preceding update. A gap beyond heartbeat is unknown.
    let known = elapsed <= db.config.heartbeat && update.value.is_some();
    let step = i64::try_from(db.config.step)
        .map_err(|_| StoreError::InvalidConfig("step exceeds supported timestamp range".into()))?;
    let first_boundary = db
        .bucket_start
        .checked_add(step)
        .ok_or(StoreError::InvalidValue)?;
    if update.timestamp < first_boundary {
        let seconds = checked_span(db.last_update, update.timestamp)?;
        add_to_bucket(db, seconds, known, update.value)?;
        db.last_update = update.timestamp;
        return Ok(());
    }

    add_to_bucket(
        db,
        checked_span(db.last_update, first_boundary)?,
        known,
        update.value,
    )?;
    if elapsed > db.config.heartbeat {
        // rrd_update.c process_pdp_st discards the whole closing PDP, including
        // seconds that earlier updates made known, when interval > heartbeat.
        db.known_seconds = 0;
        db.weighted_sum = 0.0;
    }
    finish_average_pdp(db, first_boundary);

    // Full buckets in a long update interval have a constant GAUGE value (or
    // are unknown). Consolidation writes only the newest retained generations.
    let after_first = update
        .timestamp
        .checked_sub(first_boundary)
        .ok_or(StoreError::InvalidValue)?;
    let full_buckets = u64::try_from(after_first / step).map_err(|_| StoreError::InvalidValue)?;
    append_full_average_pdps(
        db,
        first_boundary,
        full_buckets,
        if known { update.value } else { None },
    )
    .map_err(|_| StoreError::InvalidValue)?;
    let covered = i64::try_from(full_buckets)
        .ok()
        .and_then(|count| count.checked_mul(step))
        .and_then(|seconds| first_boundary.checked_add(seconds))
        .ok_or(StoreError::InvalidValue)?;
    db.bucket_start = covered;
    db.known_seconds = 0;
    db.weighted_sum = 0.0;

    let remainder = update
        .timestamp
        .checked_sub(covered)
        .ok_or(StoreError::InvalidValue)?;
    if remainder > 0 {
        add_to_bucket(
            db,
            u64::try_from(remainder).map_err(|_| StoreError::InvalidValue)?,
            known,
            update.value,
        )?;
    }
    db.last_update = update.timestamp;
    Ok(())
}

fn checked_span(start: i64, end: i64) -> Result<u64, StoreError> {
    u64::try_from(end.checked_sub(start).ok_or(StoreError::InvalidValue)?)
        .map_err(|_| StoreError::InvalidValue)
}

fn add_to_bucket(
    db: &mut DatabaseFile,
    seconds: u64,
    known: bool,
    value: Option<f64>,
) -> Result<(), StoreError> {
    if !known {
        return Ok(());
    }
    let known_seconds = db
        .known_seconds
        .checked_add(seconds)
        .ok_or(StoreError::InvalidValue)?;
    let weighted_sum = db.weighted_sum + value.ok_or(StoreError::InvalidValue)? * seconds as f64;
    if !weighted_sum.is_finite() {
        return Err(StoreError::InvalidValue);
    }
    db.known_seconds = known_seconds;
    db.weighted_sum = weighted_sum;
    Ok(())
}
