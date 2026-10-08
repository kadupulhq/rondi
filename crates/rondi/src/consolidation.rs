use crate::format::{ArchivePoint, DatabaseFile};
use crate::storage::StoreError;

pub(crate) fn finish_average_pdp(db: &mut DatabaseFile, timestamp: i64) {
    let value = if db.known_seconds.saturating_mul(2) >= db.config.step {
        Some(db.weighted_sum / db.known_seconds as f64)
    } else {
        None
    };
    push_bounded(db, ArchivePoint { timestamp, value });
    db.bucket_start = timestamp;
    db.known_seconds = 0;
    db.weighted_sum = 0.0;
}

pub(crate) fn append_full_average_pdps(
    db: &mut DatabaseFile,
    first_boundary: i64,
    count: u64,
    value: Option<f64>,
) -> Result<(), StoreError> {
    if count == 0 {
        return Ok(());
    }
    let rows = u64::try_from(db.config.rows).map_err(|_| StoreError::InvalidValue)?;
    let retained = count.min(rows);
    if count >= rows {
        db.points.clear();
    }
    // Trim once up front; push_bounded's remove(0) per point is O(rows^2).
    let excess = (db.points.len() + retained as usize).saturating_sub(db.config.rows);
    db.points.drain(..excess.min(db.points.len()));
    let step = i64::try_from(db.config.step).map_err(|_| StoreError::InvalidValue)?;
    let first_index = count - retained;
    for index in first_index..count {
        let offset = i64::try_from(index + 1)
            .ok()
            .and_then(|value| value.checked_mul(step))
            .ok_or(StoreError::InvalidValue)?;
        let timestamp = first_boundary
            .checked_add(offset)
            .ok_or(StoreError::InvalidValue)?;
        push_bounded(db, ArchivePoint { timestamp, value });
    }
    Ok(())
}

fn push_bounded(db: &mut DatabaseFile, point: ArchivePoint) {
    db.points.push(point);
    if db.points.len() > db.config.rows {
        db.points.remove(0);
    }
}
