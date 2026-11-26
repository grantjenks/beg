use crate::{backend::Message, unix};
use serde_json::{Value, json};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const VIEWER: &str = include_str!("../tools/trace-viewer.html");
const TRACE_TAG: &str = r#"<script type="application/json" id="initial-trace">"#;

pub struct Trace {
    file: Option<File>,
    debug: bool,
    temporary_path: Option<PathBuf>,
    viewer_path: Option<PathBuf>,
    error: Option<io::Error>,
    started: Instant,
}

impl Trace {
    pub fn open(
        path: Option<&Path>,
        debug: bool,
        open_viewer: bool,
        source: &Metadata,
        started: Instant,
    ) -> io::Result<Self> {
        let file = path
            .map(|path| {
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(path)?;
                let metadata = file.metadata()?;
                if !metadata.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "trace must be a regular file",
                    ));
                }
                let trace_identity = identity(&metadata);
                if trace_identity == identity(source) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "trace is the script file",
                    ));
                }
                for descriptor in 0..=2 {
                    let stream = unix::duplicate(descriptor)?.metadata()?;
                    if trace_identity == identity(&stream) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "trace is connected to a standard stream",
                        ));
                    }
                }
                Ok(file)
            })
            .transpose()?;
        let (file, temporary_path) = if (debug || open_viewer) && file.is_none() {
            let (file, path) = tempfile::Builder::new()
                .prefix(if debug { "beg-debug-" } else { "beg-trace-" })
                .suffix(".jsonl")
                .tempfile()?
                .keep()
                .map_err(|error| error.error)?;
            (Some(file), Some(path))
        } else {
            (file, None)
        };
        let viewer_path = if open_viewer {
            path.or(temporary_path.as_deref()).map(Path::to_path_buf)
        } else {
            None
        };
        Ok(Self {
            file,
            debug,
            temporary_path,
            viewer_path,
            error: None,
            started,
        })
    }

    pub fn temporary_path(&self) -> Option<&Path> {
        self.temporary_path.as_deref()
    }

    pub fn open_viewer(&mut self) -> io::Result<()> {
        let (Some(path), Some(file)) = (&self.viewer_path, &mut self.file) else {
            return Ok(());
        };
        file.flush()?;
        let page = write_viewer(path, file)?;
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        Command::new(opener)
            .arg(&page)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map(|_| ())
            .map_err(|error| io::Error::other(format!("{error}. HTML saved at {}", page.display())))
    }

    pub fn backend_event(&mut self, message: &Message) {
        if !self.debug || self.file.is_none() {
            return;
        }
        match message {
            Message::Frame(frame) if frame["method"].is_string() => {
                self.record("codex_event", frame.clone());
            }
            Message::Frame(frame) if frame["error"].is_object() => {
                self.record("codex_request_error", frame.clone());
            }
            Message::EventsSkipped(skipped) => {
                self.record("codex_events_skipped", json!({"count": skipped}));
            }
            _ => {}
        }
    }

    pub fn record(&mut self, event: &'static str, detail: Value) {
        let Some(file) = &mut self.file else {
            return;
        };
        let mut line = json!({
            "pid": std::process::id(),
            "elapsed_ms": self.started.elapsed().as_millis(),
            "event": event,
            "detail": detail,
        })
        .to_string();
        line.push('\n');
        if let Err(error) = file.write_all(line.as_bytes()) {
            self.file = None;
            self.error = Some(error);
        }
    }

    pub fn take_error(&mut self) -> Option<io::Error> {
        self.error.take()
    }

    pub fn error(&self) -> Option<&io::Error> {
        self.error.as_ref()
    }
}

fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn write_viewer(path: &Path, trace: &File) -> io::Result<PathBuf> {
    let reader = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = reader.metadata()?;
    if !metadata.is_file() || identity(&metadata) != identity(&trace.metadata()?) {
        return Err(io::Error::other(
            "trace file changed before the viewer opened",
        ));
    }
    let mut reader = BufReader::new(reader.take(metadata.len()));
    let (before, after) = VIEWER
        .split_once(TRACE_TAG)
        .and_then(|(before, rest)| rest.strip_prefix("null").map(|after| (before, after)))
        .ok_or_else(|| io::Error::other("trace viewer template is missing its data slot"))?;
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let mut html = tempfile::Builder::new()
        .prefix("beg-viewer-")
        .suffix(".html")
        .tempfile()?;
    {
        let mut output = BufWriter::new(html.as_file_mut());
        output.write_all(before.as_bytes())?;
        output.write_all(TRACE_TAG.as_bytes())?;
        output.write_all(b"{\"name\":")?;
        output.write_all(json!(name).to_string().replace('<', "\\u003c").as_bytes())?;
        write!(output, ",\"pid\":{},\"text\":\"", std::process::id())?;
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line)? != 0 {
            // Escape a tag opening even inside an inert JSON script element.
            let string = json!(String::from_utf8_lossy(&line)).to_string();
            output.write_all(
                string[1..string.len() - 1]
                    .replace('<', "\\u003c")
                    .as_bytes(),
            )?;
            line.clear();
        }
        output.write_all(b"\"}")?;
        output.write_all(after.as_bytes())?;
        output.flush()?;
    }
    let (_, page) = html.keep().map_err(|error| error.error)?;
    page.canonicalize()
}
