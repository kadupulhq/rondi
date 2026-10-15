//! Process signals and the shutdown they request.

use tokio::sync::mpsc;

/// The signals the daemon handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaemonSignal {
    Terminate,
    Interrupt,
    User1,
    User2,
    Hangup,
    Alarm,
}

impl DaemonSignal {
    const ALL: [DaemonSignal; 6] = [
        DaemonSignal::Terminate,
        DaemonSignal::Interrupt,
        DaemonSignal::User1,
        DaemonSignal::User2,
        DaemonSignal::Hangup,
        DaemonSignal::Alarm,
    ];

    fn number(self) -> libc::c_int {
        match self {
            Self::Terminate => libc::SIGTERM,
            Self::Interrupt => libc::SIGINT,
            Self::User1 => libc::SIGUSR1,
            Self::User2 => libc::SIGUSR2,
            Self::Hangup => libc::SIGHUP,
            Self::Alarm => libc::SIGALRM,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Terminate => "SIGTERM",
            Self::Interrupt => "SIGINT",
            Self::User1 => "SIGUSR1",
            Self::User2 => "SIGUSR2",
            Self::Hangup => "SIGHUP",
            Self::Alarm => "SIGALRM",
        }
    }
}

/// How the daemon stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shutdown {
    /// SIGTERM or SIGINT: write pending values unless the journal keeps
    /// them for the next start (`-j` without `-F`).
    Normal,
    /// SIGUSR1: always write pending values.
    Flush,
    /// SIGUSR2: never write; values survive only in a journal.
    Abandon,
}

impl Shutdown {
    /// Whether pending values are written before exit.
    // Compatibility: the signal table of RCD-20.
    // Provenance: rrdcached(1) and black-box compatibility test.
    pub(crate) fn writes_pending(self, flushes_at_shutdown: bool) -> bool {
        match self {
            Self::Normal => flushes_at_shutdown,
            Self::Flush => true,
            Self::Abandon => false,
        }
    }
}

/// What a received signal does, given the daemon's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignalEffect {
    /// Logged and otherwise ignored.
    Ignore,
    /// Starts the shutdown.
    Stop(Shutdown),
}

/// The running/stopping state machine. Once a shutdown has started, later
/// signals change nothing.
#[derive(Debug, Default)]
pub(crate) struct SignalState {
    stopping: Option<Shutdown>,
}

impl SignalState {
    // Compatibility: SIGHUP and SIGALRM keep the daemon serving (RCD-21);
    // SIGPIPE stays ignored by the runtime.
    // Provenance: black-box compatibility test.
    pub(crate) fn receive(&mut self, signal: DaemonSignal) -> SignalEffect {
        if self.stopping.is_some() {
            return SignalEffect::Ignore;
        }
        let shutdown = match signal {
            DaemonSignal::Terminate | DaemonSignal::Interrupt => Shutdown::Normal,
            DaemonSignal::User1 => Shutdown::Flush,
            DaemonSignal::User2 => Shutdown::Abandon,
            DaemonSignal::Hangup | DaemonSignal::Alarm => return SignalEffect::Ignore,
        };
        self.stopping = Some(shutdown);
        SignalEffect::Stop(shutdown)
    }
}

/// Holds the handled signals blocked on the starting thread, so a signal
/// that arrives while the daemon sets up waits for its handler instead of
/// killing the process with the pid file and sockets in place.
pub(crate) struct BlockedSignals {
    previous: libc::sigset_t,
}

impl BlockedSignals {
    pub(crate) fn block() -> Self {
        // SAFETY: sigset_t is plain data; sigemptyset initializes it.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: both sets are valid for writing.
        unsafe {
            libc::sigemptyset(&mut set);
            for signal in DaemonSignal::ALL {
                libc::sigaddset(&mut set, signal.number());
            }
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut previous);
        }
        Self { previous }
    }
}

impl Drop for BlockedSignals {
    fn drop(&mut self) {
        // SAFETY: restores the mask saved by `block` on the same thread.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut()) };
    }
}

/// Installs a handler for every handled signal and forwards what arrives.
/// Call inside the runtime, then drop the [`BlockedSignals`] guard.
pub(crate) fn listen() -> std::io::Result<mpsc::Receiver<DaemonSignal>> {
    use tokio::signal::unix::{SignalKind, signal};
    let (sender, receiver) = mpsc::channel(16);
    for kind in DaemonSignal::ALL {
        let mut stream = signal(SignalKind::from_raw(kind.number()))?;
        let sender = sender.clone();
        tokio::spawn(async move {
            while stream.recv().await.is_some() {
                if sender.send(kind).await.is_err() {
                    break;
                }
            }
        });
    }
    Ok(receiver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_stopping_signal_wins_and_later_ones_are_ignored() {
        let mut state = SignalState::default();
        assert_eq!(state.receive(DaemonSignal::Hangup), SignalEffect::Ignore);
        assert_eq!(state.receive(DaemonSignal::Alarm), SignalEffect::Ignore);
        assert_eq!(
            state.receive(DaemonSignal::User2),
            SignalEffect::Stop(Shutdown::Abandon)
        );
        for signal in DaemonSignal::ALL {
            assert_eq!(state.receive(signal), SignalEffect::Ignore);
        }
    }

    #[test]
    fn each_signal_maps_to_its_shutdown() {
        for (signal, effect) in [
            (
                DaemonSignal::Terminate,
                SignalEffect::Stop(Shutdown::Normal),
            ),
            (
                DaemonSignal::Interrupt,
                SignalEffect::Stop(Shutdown::Normal),
            ),
            (DaemonSignal::User1, SignalEffect::Stop(Shutdown::Flush)),
            (DaemonSignal::User2, SignalEffect::Stop(Shutdown::Abandon)),
        ] {
            assert_eq!(
                SignalState::default().receive(signal),
                effect,
                "{}",
                signal.name()
            );
        }
    }

    #[test]
    fn signal_table_decides_whether_values_are_written() {
        // (shutdown, flushes at shutdown, written): the RCD-20 table, where
        // "flushes at shutdown" is false only for -j without -F.
        for (shutdown, flushes, written) in [
            (Shutdown::Normal, true, true),
            (Shutdown::Normal, false, false),
            (Shutdown::Flush, false, true),
            (Shutdown::Flush, true, true),
            (Shutdown::Abandon, true, false),
            (Shutdown::Abandon, false, false),
        ] {
            assert_eq!(
                shutdown.writes_pending(flushes),
                written,
                "{shutdown:?} {flushes}"
            );
        }
    }
}
