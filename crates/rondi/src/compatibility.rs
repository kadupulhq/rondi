//! Pinned upstream compatibility baselines. These are targets, not parity claims.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamTarget {
    pub component: &'static str,
    pub revision: &'static str,
}

pub const TARGETS: [UpstreamTarget; 3] = [
    UpstreamTarget {
        component: "RRDtool/rrdcached",
        revision: "v1.11.0",
    },
    UpstreamTarget {
        component: "Cacti RRDProxy",
        revision: "54aad579803f2b9cf1e9dd979881247656853230",
    },
    UpstreamTarget {
        component: "Kadupul Cacti checkout",
        revision: "release/1.2.31-97-g97538de5b",
    },
];
