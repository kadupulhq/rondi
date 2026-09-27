use crate::format::{DatabaseFile, FetchResult};

pub(crate) fn fetch_retained(name: &str, db: DatabaseFile) -> FetchResult {
    FetchResult {
        database: name.into(),
        step: db.config.step,
        points: db.points,
    }
}
