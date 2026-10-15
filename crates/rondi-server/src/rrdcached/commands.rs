//! The command words of the request protocol, their usage lines and help
//! text, and where each one may be used.
//!
//! The usage lines and help texts are the daemon's observable `HELP`
//! output, recorded from rrdcached 1.11.0 (docs/compatibility/rrdcached.md
//! RC-20, RC-21; vectors `help-*`).

/// A recognized command word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Update,
    Tune,
    Dump,
    Flush,
    FlushAll,
    Pending,
    Forget,
    Queue,
    Stats,
    Help,
    Ping,
    Batch,
    Fetch,
    FetchBin,
    Info,
    First,
    Last,
    Create,
    List,
    Suspend,
    Resume,
    SuspendAll,
    ResumeAll,
    Quit,
    /// Only meaningful in the journal.
    Wrote,
    /// Ends a batch.
    Dot,
}

struct Entry {
    command: Command,
    name: &'static str,
    usage: &'static str,
    /// The help text after the usage line, without the final empty line.
    description: &'static str,
}

// Compatibility: the overview lists the commands in this order and each
// help reply is `Usage: <usage>` followed by this description.
// Provenance: black-box compatibility test (help-001 to help-028).
const TABLE: &[Entry] = &[
    Entry {
        command: Command::Update,
        name: "UPDATE",
        usage: "UPDATE <filename> <values> [<values> ...]",
        description: "\nAdds the given file to the internal cache if it is not yet known and\nappends the given value(s) to the entry. See the rrdcached(1) manpage\nfor details.\n\nEach <values> has the following form:\n  <values> = <time>:<value>[:<value>[...]]\nSee the rrdupdate(1) manpage for details.\n",
    },
    Entry {
        command: Command::Tune,
        name: "TUNE",
        usage: "TUNE <filename> [options]",
        description: "\nTunes the given file, takes the parameters as defined in rrdtool.\n",
    },
    Entry {
        command: Command::Dump,
        name: "DUMP",
        usage: "DUMP <filename> [-h none|xsd|dtd]",
        description: "\nDumps the specified RRD to XML.\n",
    },
    Entry {
        command: Command::Flush,
        name: "FLUSH",
        usage: "FLUSH <filename>",
        description: "\nAdds the given filename to the head of the update queue and returns\nafter it has been dequeued.\n",
    },
    Entry {
        command: Command::FlushAll,
        name: "FLUSHALL",
        usage: "FLUSHALL",
        description: "\nTriggers writing of all pending updates.  Returns immediately.\n",
    },
    Entry {
        command: Command::Pending,
        name: "PENDING",
        usage: "PENDING <filename>",
        description: "\nShows any 'pending' updates for a file, in order.\nThe updates shown have not yet been written to the underlying RRD file.\n",
    },
    Entry {
        command: Command::Forget,
        name: "FORGET",
        usage: "FORGET <filename>",
        description: "\nRemoves the file completely from the cache.\nAny pending updates for the file will be lost.\n",
    },
    Entry {
        command: Command::Queue,
        name: "QUEUE",
        usage: "QUEUE",
        description: "\nShows all files in the output queue.\nThe output is zero or more lines in the following format:\n(where <num_vals> is the number of values to be written)\n\n<num_vals> <filename>\n",
    },
    Entry {
        command: Command::Stats,
        name: "STATS",
        usage: "STATS",
        description: "\nReturns some performance counters, see the rrdcached(1) manpage for\na description of the values.\n",
    },
    Entry {
        command: Command::Help,
        name: "HELP",
        usage: "HELP [<command>]",
        description: "",
    },
    Entry {
        command: Command::Ping,
        name: "PING",
        usage: "PING",
        description: "\nPING given, PONG returned\n",
    },
    Entry {
        command: Command::Batch,
        name: "BATCH",
        usage: "BATCH",
        description: "\nThe 'BATCH' command permits the client to initiate a bulk load\n   of commands to rrdcached.\n\nUsage:\n\n    client: BATCH\n    server: 0 Go ahead.  End with dot '.' on its own line.\n    client: command #1\n    client: command #2\n    client: ... and so on\n    client: .\n    server: 2 errors\n    server: 7 message for command #7\n    server: 9 message for command #9\n\nFor more information, consult the rrdcached(1) documentation.\n",
    },
    Entry {
        command: Command::Fetch,
        name: "FETCH",
        usage: "FETCH <file> <CF> [<start> [<end>] [<column>...]]",
        description: "\nThe 'FETCH' can be used by the client to retrieve values from an RRD file.\n",
    },
    Entry {
        command: Command::FetchBin,
        name: "FETCHBIN",
        usage: "FETCHBIN <file> <CF> [<start> [<end>] [<column>...]]",
        description: "\nThe 'FETCHBIN' can be used by the client to retrieve values from an RRD file.\n",
    },
    Entry {
        command: Command::Info,
        name: "INFO",
        usage: "INFO <filename>",
        description: "\nThe INFO command retrieves information about a specified RRD file.\nThis is returned in standard rrdinfo format, a sequence of lines\nwith the format <keyname> = <value>\nNote that this is the data as of the last update of the RRD file itself,\nnot the last time data was received via rrdcached, so there may be pending\nupdates in the queue.  If this bothers you, then first run a FLUSH.\n",
    },
    Entry {
        command: Command::First,
        name: "FIRST",
        usage: "FIRST <filename> <rra index>",
        description: "\nThe FIRST command retrieves the first data time for a specified RRA in\nan RRD file.\n",
    },
    Entry {
        command: Command::Last,
        name: "LAST",
        usage: "LAST <filename>",
        description: "\nThe LAST command retrieves the last update time for a specified RRD file.\nNote that this is the time of the last update of the RRD file itself, not\nthe last time data was received via rrdcached, so there may be pending\nupdates in the queue.  If this bothers you, then first run a FLUSH.\n",
    },
    Entry {
        command: Command::Create,
        name: "CREATE",
        usage: "CREATE <filename> [-b start] [-s step] [-O] <DS definitions> <RRA definitions>",
        description: "\nThe CREATE command will create an RRD file, overwriting any existing file\nunless the -O option is given or rrdcached was started with the -O option.\nThe start parameter needs to be in seconds since 1/1/70 (AT-style syntax is\nnot acceptable) and the step is in seconds (default is 300).\nThe DS and RRA definitions are as for the 'rrdtool create' command.\n",
    },
    Entry {
        command: Command::List,
        name: "LIST",
        usage: "LIST [RECURSIVE] /[<path>]",
        description: "\nThis command lists the RRD files in the storage base directory (/).\nNote that this is the list of RRD files on storage as of the last update.\nThere may be pending updates in the queue, so a FLUSH may have to be run\nbeforehand.\nWhen invoked with 'LIST RECURSIVE /<path>' it will behave similarly to\n'ls -R' but limited to rrd files (listing all the rrd bases in the subtree\n of <path>, skipping empty directories).\n",
    },
    Entry {
        command: Command::Suspend,
        name: "SUSPEND",
        usage: "SUSPEND <filename>",
        description: "\nThe SUSPEND command will suspend writing to an RRD file. While a file is\nsuspended, all metrics for it are cached in memory until RESUME is called\nfor that file or RESUMEALL is called.\n",
    },
    Entry {
        command: Command::Resume,
        name: "RESUME",
        usage: "RESUME <filename>",
        description: "\nThe RESUME command will resume writing to an RRD file previously suspended\nby SUSPEND or SUSPENDALL.\n",
    },
    Entry {
        command: Command::SuspendAll,
        name: "SUSPENDALL",
        usage: "SUSPENDALL",
        description: "\nThe SUSPENDALL command will suspend writing to all RRD files. While a file\nis suspended, all metrics for it are cached in memory until RESUME is called\nfor that file or RESUMEALL is called.\n",
    },
    Entry {
        command: Command::ResumeAll,
        name: "RESUMEALL",
        usage: "RESUMEALL",
        description: "\nThe RESUMEALL command will resume writing to all RRD files previously suspended.\n",
    },
    Entry {
        command: Command::Quit,
        name: "QUIT",
        usage: "QUIT",
        description: "\nDisconnect from rrdcached.\n",
    },
];

impl Command {
    /// The command a word names, compared without regard to ASCII case
    /// (RC-5, RC-10).
    pub(crate) fn find(word: &[u8]) -> Option<Self> {
        if word == b"." {
            return Some(Self::Dot);
        }
        if word.eq_ignore_ascii_case(b"WROTE") {
            return Some(Self::Wrote);
        }
        TABLE
            .iter()
            .find(|entry| word.eq_ignore_ascii_case(entry.name.as_bytes()))
            .map(|entry| entry.command)
    }

    fn entry(self) -> Option<&'static Entry> {
        TABLE.iter().find(|entry| entry.command == self)
    }

    /// The upper-case name, as `-P` lists compare it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Wrote => "WROTE",
            Self::Dot => ".",
            other => other.entry().map_or("", |entry| entry.name),
        }
    }

    /// `Usage: <usage line>`, the text of a usage error.
    pub(crate) fn usage(self) -> String {
        format!("Usage: {}", self.entry().map_or("", |entry| entry.usage))
    }

    /// Whether a `BATCH` executes this command (RC-121).
    pub(crate) fn runs_in_batch(self) -> bool {
        matches!(
            self,
            Self::Update
                | Self::Create
                | Self::Flush
                | Self::Forget
                | Self::Suspend
                | Self::Resume
                | Self::SuspendAll
                | Self::ResumeAll
                | Self::Quit
        )
    }

    /// Whether a listener restricted to `allowed` accepts this command
    /// (RC-130): `HELP` and `QUIT` always, `.` when `BATCH` is allowed.
    pub(crate) fn permitted(self, allowed: Option<&[String]>) -> bool {
        let Some(allowed) = allowed else {
            return true;
        };
        let listed = |command: Self| {
            allowed
                .iter()
                .any(|name| name.eq_ignore_ascii_case(command.name()))
        };
        match self {
            Self::Help | Self::Quit => true,
            Self::Dot => listed(Self::Batch),
            other => listed(other),
        }
    }
}

/// The `HELP` reply for its first argument, if any (RC-20, RC-21).
pub(crate) fn help(topic: Option<&[u8]>) -> Vec<u8> {
    let entry = topic.and_then(Command::find).and_then(Command::entry);
    let mut reply = Vec::new();
    match entry {
        Some(entry) => {
            let body = format!("Usage: {}\n{}\n", entry.usage, entry.description);
            let lines = body.bytes().filter(|&byte| byte == b'\n').count();
            reply.extend_from_slice(format!("{lines} Help for {}\n", entry.name).as_bytes());
            reply.extend_from_slice(body.as_bytes());
        }
        None => {
            reply.extend_from_slice(format!("{} Command overview\n", TABLE.len()).as_bytes());
            for entry in TABLE {
                reply.extend_from_slice(entry.usage.as_bytes());
                reply.push(b'\n');
            }
        }
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_match_without_regard_to_case() {
        assert_eq!(Command::find(b"pInG"), Some(Command::Ping));
        assert_eq!(Command::find(b"wrote"), Some(Command::Wrote));
        assert_eq!(Command::find(b"."), Some(Command::Dot));
        assert_eq!(Command::find(b".x"), None);
        assert_eq!(Command::find(b""), None);
        assert_eq!(Command::find(b"PINGX"), None);
    }

    #[test]
    fn help_counts_its_lines() {
        assert_eq!(
            help(Some(b"ping")),
            b"4 Help for PING\nUsage: PING\n\nPING given, PONG returned\n\n"
        );
        assert_eq!(
            help(Some(b"HELP")),
            b"2 Help for HELP\nUsage: HELP [<command>]\n\n"
        );
        let overview = help(Some(b"WROTE"));
        assert!(overview.starts_with(b"24 Command overview\nUPDATE "));
        assert!(overview.ends_with(b"RESUMEALL\nQUIT\n"));
        assert_eq!(help(None), overview);
        assert_eq!(help(Some(b".")), overview);
    }

    #[test]
    fn permissions_always_allow_help_and_quit() {
        let allowed = ["batch".to_owned(), "Ping".to_owned()];
        assert!(Command::Ping.permitted(Some(&allowed)));
        assert!(Command::Dot.permitted(Some(&allowed)));
        assert!(Command::Help.permitted(Some(&allowed)));
        assert!(Command::Quit.permitted(Some(&allowed)));
        assert!(!Command::Stats.permitted(Some(&allowed)));
        assert!(!Command::Dot.permitted(Some(&["PING".to_owned()])));
        assert!(Command::Stats.permitted(None));
    }
}
