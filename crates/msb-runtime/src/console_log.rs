//! The `console-log` subcommand: tails the guest's captured stdout/stderr and
//! prints one JSON object (`{source, ts, text}`) per line to stdout.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use microsandbox::MicrosandboxError;
use microsandbox::config::set_sdk_msb_path;
use microsandbox::logs::{LogEntry, LogSource, LogStreamOptions, log_stream};
use serde::Serialize;
use tracing::{info, warn};

#[derive(Parser)]
pub struct ConsoleLogArgs {
    /// msb's flat sandbox name (encoded namespace/name).
    #[arg(long, env = "MSB_SANDBOX_NAME")]
    sandbox_name: String,

    #[arg(long, default_value = "/usr/local/bin/msb", env = "MSB_PATH")]
    msb_path: PathBuf,
}

const LOG_DIR_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Serialize)]
struct ConsoleLogLine<'a> {
    source: &'a str,
    ts: chrono::DateTime<chrono::Utc>,
    text: String,
}

fn format_line(entry: &LogEntry) -> String {
    let line = ConsoleLogLine {
        source: source_str(entry.source),
        ts: entry.timestamp,
        text: String::from_utf8_lossy(&entry.data).trim_end_matches('\n').to_string(),
    };
    serde_json::to_string(&line).expect("console log line always serializes")
}

fn source_str(s: LogSource) -> &'static str {
    match s {
        LogSource::Stdout => "stdout",
        LogSource::Stderr => "stderr",
        LogSource::Output => "output",
        LogSource::System => "system",
    }
}

pub async fn run(args: ConsoleLogArgs) -> Result<()> {
    // The SDK resolves the log dir under $MSB_HOME, read from the environment.
    set_sdk_msb_path(&args.msb_path);

    let opts = LogStreamOptions {
        follow: true,
        ..Default::default()
    };

    loop {
        match log_stream(&args.sandbox_name, &opts).await {
            Ok(stream) => {
                info!(sandbox = %args.sandbox_name, "log directory found; streaming");
                tail(stream).await;
                return Ok(());
            }
            // The log dir doesn't exist until the guest boots; wait rather than crash-loop.
            Err(MicrosandboxError::SandboxNotFound(_)) => {
                tokio::time::sleep(LOG_DIR_POLL_INTERVAL).await;
            }
            Err(e) => {
                warn!(error = %e, "console-log: unexpected error opening log stream");
                tokio::time::sleep(LOG_DIR_POLL_INTERVAL).await;
            }
        }
    }
}

async fn tail(stream: impl futures::Stream<Item = microsandbox::MicrosandboxResult<LogEntry>>) {
    let mut stream = std::pin::pin!(stream);
    while let Some(item) = stream.next().await {
        match item {
            Ok(entry) => println!("{}", format_line(&entry)),
            Err(e) => {
                warn!(error = %e, "console-log: error reading log entry; stopping this stream");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use chrono::{DateTime, Utc};
    use microsandbox::logs::LogCursor;

    fn entry(source: LogSource, text: &str) -> LogEntry {
        LogEntry {
            timestamp: "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap(),
            source,
            session_id: Some(1),
            data: Bytes::from(text.as_bytes().to_vec()),
            cursor: LogCursor::empty(),
        }
    }

    #[test]
    fn formats_stdout_line_as_json() {
        let line = format_line(&entry(LogSource::Stdout, "hello world\n"));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["source"], "stdout");
        assert_eq!(v["text"], "hello world");
        assert_eq!(v["ts"], "2026-01-01T00:00:00Z");
    }

    #[test]
    fn formats_stderr_line() {
        let line = format_line(&entry(LogSource::Stderr, "oops"));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["source"], "stderr");
        assert_eq!(v["text"], "oops");
    }

    #[test]
    fn strips_trailing_newline_but_keeps_interior_content() {
        let line = format_line(&entry(LogSource::Stdout, "a\nb\n"));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["text"], "a\nb");
    }

    #[test]
    fn lossily_decodes_non_utf8_bytes() {
        let mut e = entry(LogSource::Stdout, "");
        e.data = Bytes::from_static(&[0xff, 0xfe, b'h', b'i']);
        let line = format_line(&e);
        let _: serde_json::Value = serde_json::from_str(&line).unwrap();
    }

    #[test]
    fn one_json_object_per_line_no_embedded_newlines() {
        let line = format_line(&entry(LogSource::System, "x"));
        assert_eq!(line.matches('\n').count(), 0);
    }
}
