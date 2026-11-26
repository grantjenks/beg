use crate::process::Event;
use crate::unix;
use crate::{End, Failure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Duration;

pub const MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdin,
    Stdout,
    Stderr,
}

impl Stream {
    const ALL: [Self; 3] = [Self::Stdin, Self::Stdout, Self::Stderr];

    pub fn name(self) -> &'static str {
        match self {
            Self::Stdin => "stdin",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

struct State {
    file: Option<File>,
    terminal: bool,
    busy: bool,
    child: Option<u64>,
}

impl State {
    fn new(stream: Stream) -> io::Result<Self> {
        let file = unix::duplicate(stream as i32)?;
        Ok(Self {
            terminal: unix::is_terminal(&file),
            file: Some(file),
            busy: false,
            child: None,
        })
    }
}

pub struct Streams {
    states: [State; 3],
    utf8_tail: Vec<u8>,
}

pub enum Completion {
    Read(io::Result<Option<Vec<u8>>>),
    Write(Stream, WriteResult),
}

pub struct WriteResult {
    pub written: usize,
    pub error: Option<io::Error>,
}

impl Streams {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            states: [
                State::new(Stream::Stdin)?,
                State::new(Stream::Stdout)?,
                State::new(Stream::Stderr)?,
            ],
            utf8_tail: Vec::new(),
        })
    }

    pub fn context(&self) -> Value {
        let streams = Stream::ALL.map(|stream| {
            let state = &self.states[stream as usize];
            json!({
                "terminal": state.terminal,
                "open": state.file.is_some(),
                "busy": state.busy,
                "child": state.child,
            })
        });
        json!({ "stdin": streams[0], "stdout": streams[1], "stderr": streams[2] })
    }

    fn available(&self, stream: Stream) -> Result<&File, String> {
        let state = &self.states[stream as usize];
        if let Some(child) = state.child {
            return Err(format!("{} is connected to child {child}", stream.name()));
        }
        if state.busy {
            return Err(format!("{} already has a pending operation", stream.name()));
        }
        state
            .file
            .as_ref()
            .ok_or_else(|| format!("{} is closed", stream.name()))
    }

    pub fn child_stdio(&self, stream: Option<Stream>) -> Result<Stdio, String> {
        let Some(stream) = stream else {
            return Ok(Stdio::null());
        };
        self.duplicate(stream).map(Stdio::from)
    }

    pub fn duplicate(&self, stream: Stream) -> Result<File, String> {
        self.available(stream)?
            .try_clone()
            .map_err(|error| error.to_string())
    }

    pub fn attach(&mut self, streams: [Option<Stream>; 3], child: u64) {
        for stream in streams.into_iter().flatten() {
            self.states[stream as usize].child = Some(child);
            if stream == Stream::Stdin {
                // The child reads the remaining OS bytes, so decoding restarts
                // at the next byte the agent reads.
                self.utf8_tail.clear();
            }
        }
    }

    pub fn release(&mut self, child: u64) {
        for state in &mut self.states {
            if state.child == Some(child) {
                state.child = None;
            }
        }
    }

    pub fn flush(&self, stream: Stream) -> Result<(), String> {
        self.available(stream).map(|_| ())
    }

    pub fn close(&mut self, stream: Stream) -> Result<(), String> {
        self.available(stream)?;
        self.states[stream as usize].file = None;
        Ok(())
    }

    pub fn finish(&self, end: End) -> End {
        if let End::Failed(Failure { code, message }) = end {
            self.diagnostic(message);
            End::Exit(code)
        } else {
            end
        }
    }

    pub fn diagnostic(&self, message: String) {
        if let Some(stderr) = &self.states[Stream::Stderr as usize].file
            && let Ok(stderr) = stderr.try_clone()
        {
            write_diagnostic(stderr, message);
        }
    }

    pub fn begin_read(
        &mut self,
        max_bytes: usize,
        events: SyncSender<Event>,
    ) -> Result<UnixStream, String> {
        if max_bytes == 0 {
            return Err("max_bytes must be positive".into());
        }
        let max_bytes = max_bytes.min(MAX_BYTES);
        let mut file = self
            .available(Stream::Stdin)?
            .try_clone()
            .map_err(|error| error.to_string())?;
        let (cancel, listener) = UnixStream::pair().map_err(|error| error.to_string())?;
        thread::Builder::new()
            .name("beg-stdin".into())
            .spawn(move || {
                let result = read(&mut file, &listener, max_bytes);
                drop(file);
                let _ = events.send(Event::Stream(Completion::Read(result)));
            })
            .map_err(|error| error.to_string())?;
        self.states[Stream::Stdin as usize].busy = true;
        Ok(cancel)
    }

    pub fn finish_read(
        &mut self,
        result: io::Result<Option<Vec<u8>>>,
    ) -> Result<(Value, usize), String> {
        self.states[Stream::Stdin as usize].busy = false;
        let bytes = result
            .map_err(|error| error.to_string())?
            .ok_or("stdin read interrupted")?;
        let eof = bytes.is_empty();
        let mut decoded = std::mem::take(&mut self.utf8_tail);
        decoded.extend_from_slice(&bytes);
        if !eof {
            let prefix = incomplete_utf8_start(&decoded);
            self.utf8_tail = decoded.split_off(prefix);
        }
        let text = String::from_utf8_lossy(&decoded);
        let lossy = matches!(text, Cow::Owned(_));
        Ok((
            json!({
                "base64": STANDARD.encode(&bytes),
                "text": text,
                "text_lossy": lossy,
                "byte_count": bytes.len(),
                "eof": eof,
            }),
            bytes.len(),
        ))
    }

    pub fn begin_write(
        &mut self,
        stream: Stream,
        bytes: Vec<u8>,
        events: SyncSender<Event>,
    ) -> Result<(), String> {
        if stream == Stream::Stdin {
            return Err("cannot write to stdin".into());
        }
        if bytes.len() > MAX_BYTES {
            return Err(format!("write must be at most {MAX_BYTES} bytes"));
        }
        let mut file = self
            .available(stream)?
            .try_clone()
            .map_err(|error| error.to_string())?;
        thread::Builder::new()
            .name(format!("beg-{}", stream.name()))
            .spawn(move || {
                let result = write(&mut file, &bytes);
                drop(file);
                let _ = events.send(Event::Stream(Completion::Write(stream, result)));
            })
            .map_err(|error| error.to_string())?;
        self.states[stream as usize].busy = true;
        Ok(())
    }

    pub fn finish_write(&mut self, stream: Stream) {
        self.states[stream as usize].busy = false;
    }
}

pub fn cancel_read(cancel: &UnixStream) {
    let _ = cancel.shutdown(Shutdown::Write);
}

pub fn read(file: &mut File, cancel: &UnixStream, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = vec![0; max];
    loop {
        if !unix::wait_readable(file.as_raw_fd(), cancel.as_raw_fd())? {
            return Ok(None);
        }
        match file.read(&mut bytes) {
            Ok(count) => {
                bytes.truncate(count);
                return Ok(Some(bytes));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

fn incomplete_utf8_start(mut bytes: &[u8]) -> usize {
    let len = bytes.len();
    while let Err(error) = std::str::from_utf8(bytes) {
        let valid = error.valid_up_to();
        match error.error_len() {
            Some(invalid) => bytes = &bytes[valid + invalid..],
            None => return len - bytes.len() + valid,
        }
    }
    len
}

pub fn write(file: &mut File, bytes: &[u8]) -> WriteResult {
    let mut written = 0;
    let error = loop {
        if written == bytes.len() {
            break None;
        }
        match file.write(&bytes[written..]) {
            Ok(0) => break Some(io::ErrorKind::WriteZero.into()),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if let Err(error) = unix::wait_writable(file.as_raw_fd()) {
                    break Some(error);
                }
            }
            Err(error) => break Some(error),
        }
    };
    WriteResult { written, error }
}

pub fn diagnostic(message: String) {
    if let Ok(stderr) = unix::duplicate(2) {
        write_diagnostic(stderr, message);
    }
}

fn write_diagnostic(mut stderr: File, message: String) {
    let (done, wait) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("beg-diagnostic".into())
        .spawn(move || {
            let _ = write(&mut stderr, format!("beg: {message}\n").as_bytes());
            let _ = done.send(());
        });
    let _ = wait.recv_timeout(Duration::from_millis(100));
}
