//! One client connection: reading request lines, `BATCH` mode, the
//! `-P` filter and writing replies (RC-1 to RC-18, RC-120 to RC-130).

use super::commands::Command;
use super::protocol::{self, LINE_MAX};
use super::service::{Answer, Shared};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;

/// Collected batch errors beyond this end the connection.
// Rondi policy (RC-122): bounds the memory one batch can hold.
const BATCH_ERRORS_MAX: usize = 16 * 1024 * 1024;

/// Bytes of a `FETCH` reply formatted before each write.
const FETCH_CHUNK: usize = 64 * 1024;

/// Reads the next request line into `line`, without its LF. Returns false
/// at the end of input, where an incomplete line gets no reply (RC-13).
async fn next_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(false);
        }
        let (taken, complete) = match available.iter().position(|&byte| byte == b'\n') {
            Some(at) => (at, true),
            None => (available.len(), false),
        };
        // Compatibility: a line of 8192 bytes or more before its LF closes
        // the connection without a reply (RC-15). Provenance: black-box
        // compatibility test.
        if line.len() + taken > LINE_MAX {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
        }
        line.extend_from_slice(&available[..taken]);
        reader.consume(taken + usize::from(complete));
        if complete {
            return Ok(true);
        }
    }
}

/// The batch in progress on a connection.
#[derive(Default)]
struct Batch {
    lines: usize,
    errors: Vec<u8>,
    count: usize,
}

impl Batch {
    /// Records an error for the current line. Returns false once the
    /// errors exceed their bound.
    fn record(&mut self, text: &[u8]) -> bool {
        // Compatibility: each error is `<line> <message>`; the lines of a
        // multi-line message follow as they are and each counts (RC-122).
        // Provenance: black-box compatibility test.
        self.errors
            .extend_from_slice(format!("{} ", self.lines).as_bytes());
        self.errors.extend_from_slice(text);
        self.errors.push(b'\n');
        self.count += 1 + text.iter().filter(|&&byte| byte == b'\n').count();
        self.errors.len() <= BATCH_ERRORS_MAX
    }

    fn summary(self) -> Vec<u8> {
        let mut reply = format!("{} errors\n", self.count).into_bytes();
        reply.extend_from_slice(&self.errors);
        reply
    }
}

/// What the connection does after a line.
enum Next {
    Continue,
    Close,
}

fn failure(text: &[u8]) -> Vec<u8> {
    let mut reply = b"-1 ".to_vec();
    reply.extend_from_slice(text);
    reply.push(b'\n');
    reply
}

fn cant_use(word: &[u8]) -> Vec<u8> {
    let mut text = b"Can't use '".to_vec();
    text.extend_from_slice(word);
    text.extend_from_slice(b"' here.");
    text
}

fn unknown(word: &[u8]) -> Vec<u8> {
    let mut text = b"Unknown command: ".to_vec();
    text.extend_from_slice(word);
    text
}

/// Serves one connection until it closes, or until `shutdown` turns true
/// while the client is idle.
pub(crate) async fn serve<S>(
    stream: S,
    shared: Arc<Shared>,
    permissions: Option<Arc<[String]>>,
    mut shutdown: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut batch: Option<Batch> = None;
    loop {
        let read = tokio::select! {
            biased;
            read = next_line(&mut reader, &mut line) => read,
            _ = shutdown.wait_for(|stop| *stop) => return,
        };
        if !matches!(read, Ok(true)) {
            return;
        }
        let next = handle(
            &shared,
            permissions.as_deref(),
            &mut batch,
            &line,
            &mut writer,
        )
        .await;
        if matches!(next, Ok(Next::Close) | Err(_)) {
            let _ = writer.shutdown().await;
            return;
        }
    }
}

async fn handle<W: AsyncWrite + Unpin>(
    shared: &Arc<Shared>,
    permissions: Option<&[String]>,
    batch: &mut Option<Batch>,
    line: &[u8],
    writer: &mut W,
) -> std::io::Result<Next> {
    // Rondi policy (RC-16): a line that is not UTF-8 closes the connection.
    if std::str::from_utf8(line).is_err() {
        return Ok(Next::Close);
    }
    let line = protocol::strip_carriage_return(line);
    let request = protocol::split(line);
    // Compatibility: a command word that runs into an escaped line end
    // closes the connection without a reply (RC-12a). Provenance:
    // black-box compatibility test.
    let Some(word) = request.field(0) else {
        return Ok(Next::Close);
    };
    let word = word.to_vec();
    if let Some(open) = batch.as_mut() {
        open.lines += 1;
    }
    // Compatibility: an unknown word is reported before the -P check, a
    // denied command before the check of where it may be used (RC-121,
    // RC-130). Provenance: black-box compatibility test.
    let command = match Command::find(&word) {
        None => return fail(batch, &unknown(&word), writer).await,
        Some(command) if !command.permitted(permissions) => {
            return fail(batch, b"Permission denied.", writer).await;
        }
        Some(command) => command,
    };
    match (command, batch.is_some()) {
        (Command::Quit, _) => return Ok(Next::Close),
        (Command::Dot, true) => {
            let summary = batch.take().unwrap_or_default().summary();
            writer.write_all(&summary).await?;
            return Ok(Next::Continue);
        }
        (Command::Batch, false) => {
            *batch = Some(Batch::default());
            writer
                .write_all(b"0 Go ahead.  End with dot '.' on its own line.\n")
                .await?;
            return Ok(Next::Continue);
        }
        (Command::Wrote | Command::Dot, _) => return fail(batch, &cant_use(&word), writer).await,
        (other, true) if !other.runs_in_batch() => {
            return fail(batch, &cant_use(&word), writer).await;
        }
        _ => {}
    }
    let worker = Arc::clone(shared);
    let owned = line.to_vec();
    let answer = tokio::task::spawn_blocking(move || worker.execute(command, &owned))
        .await
        .map_err(std::io::Error::other)?;
    match answer {
        Answer::Done(reply) => {
            if batch.is_none() {
                writer.write_all(&reply).await?;
            }
            Ok(Next::Continue)
        }
        Answer::Failed(text) => fail(batch, &text, writer).await,
        Answer::FailedThenClose(text) => {
            // Compatibility: inside a batch the connection closes without
            // the summary (RC-112). Provenance: black-box compatibility
            // test.
            if batch.is_none() {
                writer.write_all(&failure(&text)).await?;
            }
            Ok(Next::Close)
        }
        Answer::DoneThenClose(reply) => {
            writer.write_all(&reply).await?;
            Ok(Next::Close)
        }
        Answer::Fetch(reply) => {
            for chunk in reply.chunks(FETCH_CHUNK) {
                writer.write_all(&chunk).await?;
            }
            Ok(Next::Continue)
        }
    }
}

/// Sends an error, or records it in the batch.
async fn fail<W: AsyncWrite + Unpin>(
    batch: &mut Option<Batch>,
    text: &[u8],
    writer: &mut W,
) -> std::io::Result<Next> {
    match batch {
        Some(open) => Ok(if open.record(text) {
            Next::Continue
        } else {
            Next::Close
        }),
        None => {
            writer.write_all(&failure(text)).await?;
            Ok(Next::Continue)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lines_are_bounded_and_need_their_line_feed() {
        let input = [b"PING\nPI".as_slice(), &[b'x'; LINE_MAX], b"\n"].concat();
        let mut reader = BufReader::with_capacity(16, input.as_slice());
        let mut line = Vec::new();
        assert!(next_line(&mut reader, &mut line).await.unwrap());
        assert_eq!(line, b"PING");
        assert!(next_line(&mut reader, &mut line).await.is_err());
        let mut reader = BufReader::new(&b"PING"[..]);
        assert!(!next_line(&mut reader, &mut line).await.unwrap());
        let exact = [&[b'x'; LINE_MAX][..], b"\n"].concat();
        let mut reader = BufReader::with_capacity(7, exact.as_slice());
        assert!(next_line(&mut reader, &mut line).await.unwrap());
        assert_eq!(line.len(), LINE_MAX);
    }

    #[test]
    fn batch_errors_count_every_line() {
        let mut batch = Batch {
            lines: 1,
            ..Batch::default()
        };
        assert!(batch.record(b"No permission to recursively create: x\nDid you pass -R?"));
        batch.lines = 2;
        assert!(batch.record(b"Unknown command: NOPE"));
        assert_eq!(
            batch.summary(),
            b"3 errors\n1 No permission to recursively create: x\nDid you pass -R?\n2 Unknown command: NOPE\n"
        );
    }
}
