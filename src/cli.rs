use crate::Failure;
use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "\
Run English scripts as Unix processes.

Usage: beg [OPTIONS] SCRIPT [ARG...]

Options:
  --model MODEL_ID      Select a Codex model
  --timeout DURATION    Set an invocation deadline (e.g. 60s)
  --trace FILE          Append execution events to a JSON Lines file
  --debug               Include all Codex events in the trace
  --embed               Store a prepared filter in SCRIPT without running it
  --help                Show this help
  --version             Show the version

Use -- to end interpreter options. Arguments after SCRIPT go to the script.
When running a script directly, use BEG_DEBUG=1 and BEG_TRACE=FILE.
Set BEG_OPEN_TRACE=1 to open the trace viewer after the script finishes.
";

pub enum Action {
    Print(&'static str),
    Run(Invocation),
}

pub struct Invocation {
    pub launch: Vec<OsString>,
    pub argv: Vec<OsString>,
    pub model: Option<String>,
    pub timeout: Option<Duration>,
    pub trace: Option<PathBuf>,
    pub debug: bool,
    pub open_trace: bool,
    pub embed: bool,
}

fn usage(message: impl Into<String>) -> Failure {
    Failure::new(64, message)
}

fn env_flag(name: &str) -> Result<bool, Failure> {
    match env::var(name).as_deref() {
        Ok("1" | "true") => Ok(true),
        Ok("" | "0" | "false") | Err(env::VarError::NotPresent) => Ok(false),
        _ => Err(usage(format!("{name} must be 0, 1, false, or true"))),
    }
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Action, Failure> {
    let launch: Vec<_> = args.into_iter().collect();
    let mut model = None;
    let mut timeout = None;
    let mut trace = None;
    let mut debug = false;
    let mut embed = false;
    let mut cursor = 1;
    while let Some(arg) = launch.get(cursor) {
        let bytes = arg.as_bytes();
        if bytes == b"--" {
            cursor += 1;
            break;
        }
        if !bytes.starts_with(b"-") || bytes == b"-" {
            break;
        }
        if bytes == b"--help" {
            return Ok(Action::Print(HELP));
        }
        if bytes == b"--version" {
            return Ok(Action::Print(concat!(
                "beg ",
                env!("CARGO_PKG_VERSION"),
                "\n"
            )));
        }
        if bytes == b"--debug" {
            debug = true;
            cursor += 1;
            continue;
        }
        if bytes == b"--embed" {
            embed = true;
            cursor += 1;
            continue;
        }
        let split = bytes.iter().position(|byte| *byte == b'=');
        let (name, inline) = match split {
            Some(index) => (
                &bytes[..index],
                Some(OsStr::from_bytes(&bytes[index + 1..])),
            ),
            None => (bytes, None),
        };
        let option = match name {
            b"--model" => "--model",
            b"--timeout" => "--timeout",
            b"--trace" => "--trace",
            _ => return Err(usage(format!("unknown option {arg:?}; see beg --help"))),
        };
        cursor += 1;
        let value = match inline {
            Some(value) => value,
            None => {
                let value = launch
                    .get(cursor)
                    .ok_or_else(|| usage(format!("{option} needs a value")))?;
                if value.as_bytes().starts_with(b"--") {
                    return Err(usage(format!("{option} needs a value")));
                }
                cursor += 1;
                value.as_os_str()
            }
        };
        if value.is_empty() {
            return Err(usage(format!("{option} needs a value")));
        }
        match option {
            "--model" => {
                model = Some(
                    value
                        .to_str()
                        .ok_or_else(|| usage("model ID must be UTF-8"))?
                        .to_owned(),
                );
            }
            "--timeout" => {
                timeout = Some(
                    value
                        .to_str()
                        .and_then(|value| humantime::parse_duration(value).ok())
                        .ok_or_else(|| usage("invalid timeout; use a duration such as 60s"))?,
                );
            }
            "--trace" => trace = Some(PathBuf::from(value)),
            _ => unreachable!(),
        }
    }
    let argv = launch
        .get(cursor..)
        .filter(|args| !args.is_empty())
        .ok_or_else(|| usage("missing script path; see beg --help"))?
        .to_vec();
    if embed && argv.len() != 1 {
        return Err(usage(
            "--embed takes a script path without script arguments",
        ));
    }
    let debug = debug || env_flag("BEG_DEBUG")?;
    let trace = trace.or_else(|| {
        env::var_os("BEG_TRACE")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    });
    Ok(Action::Run(Invocation {
        launch,
        argv,
        model,
        timeout,
        trace,
        debug,
        open_trace: env_flag("BEG_OPEN_TRACE")?,
        embed,
    }))
}

pub fn instructions(source: &str) -> &str {
    if source.starts_with("#!") {
        source.split_once('\n').map_or("", |(_, text)| text)
    } else {
        source
    }
}
