mod backend;
mod cache;
mod cli;
mod filter;
mod policy;
mod process;
mod streams;
mod tools;
mod trace;
mod unix;

use std::io::{self, Write};
use std::time::Instant;

#[derive(Debug)]
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn host(message: impl Into<String>) -> Self {
        Self::new(70, message)
    }
}

#[derive(Debug)]
enum End {
    Exit(u8),
    Failed(Failure),
    Signal(unix::Signal),
}

impl From<Failure> for End {
    fn from(failure: Failure) -> Self {
        Self::Failed(failure)
    }
}

fn main() {
    backend::dispatch_helpers();
    let started = Instant::now();
    let end = match unix::Signals::install() {
        Ok(signals) => {
            let invocation = cli::parse(std::env::args_os());
            if let Some(signal) = signals.first() {
                End::Signal(signal)
            } else {
                match invocation {
                    Ok(cli::Action::Run(invocation)) => process::run(invocation, started, signals),
                    Ok(cli::Action::Print(text)) => match io::stdout().write_all(text.as_bytes()) {
                        Ok(()) => End::Exit(0),
                        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                            End::Signal(unix::Signal::Pipe)
                        }
                        Err(error) => Failure::host(format!("cannot write stdout: {error}")).into(),
                    },
                    Err(failure) => failure.into(),
                }
            }
        }
        Err(error) => Failure::host(format!("cannot install signal handlers: {error}")).into(),
    };
    match end {
        End::Exit(code) => std::process::exit(i32::from(code)),
        End::Failed(Failure { code, message }) => {
            streams::diagnostic(message);
            std::process::exit(i32::from(code));
        }
        End::Signal(signal) => unix::terminate(signal),
    }
}
