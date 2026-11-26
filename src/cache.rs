use crate::{backend, cli};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

const FORMAT_VERSION: u32 = 1;
const PREPARATION_VERSION: u32 = 2;
const RUNNER: &str = "javascript-v1";
const MAX_RESULT_METADATA: usize = 128 * 1024;
pub const MAX_CODE: usize = 128 * 1024;
pub const MAX_INPUT: usize = 1024 * 1024;
pub const MAX_OUTPUT: usize = 1024 * 1024;

pub struct Script {
    text: String,
    start: usize,
    end: usize,
    pub digest: String,
}

impl Script {
    pub fn new(text: String) -> Self {
        let start = text.len() - cli::instructions(&text).len();
        let end = storage_boundary(&text[start..]).map_or(text.len(), |(end, _)| start + end);
        let description = &text[start..end];
        let mut digest = Sha256::new();
        digest.update(description.as_bytes());
        // A file without a final line ending gains one when a storage block is appended.
        if !description.ends_with('\n') {
            digest.update(b"\n");
        }
        Self {
            text,
            start,
            end,
            digest: format!("{:x}", digest.finalize()),
        }
    }

    pub fn description(&self) -> &str {
        &self.text[self.start..self.end]
    }

    pub fn embedded(&self, model: Option<&str>) -> Option<Storage> {
        let (_, storage) = storage_boundary(&self.text[self.end..])?;
        let storage: Storage = serde_yaml::from_str(&self.text[self.end + storage..]).ok()?;
        storage.compatible(self, model).then_some(storage)
    }

    pub fn embed(&self, path: &Path, original: &Metadata, storage: &Storage) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut replacement = NamedTempFile::new_in(parent)?;
        let prefix = &self.text[..self.end];
        replacement.write_all(prefix.as_bytes())?;
        if !prefix.ends_with('\n') {
            replacement.write_all(b"\n")?;
        }
        replacement.write_all(b"\n---\n")?;
        serde_yaml::to_writer(&mut replacement, storage).map_err(io::Error::other)?;
        if replacement.as_file().metadata()?.len() > backend::MAX_FRAME as u64 {
            return Err(io::Error::other(
                "script and filter exceed the 8 MiB source limit",
            ));
        }
        replacement
            .as_file()
            .set_permissions(original.permissions())?;
        replacement.as_file().sync_all()?;

        let current = open_regular(path)?;
        let info = current.metadata()?;
        if (info.dev(), info.ino(), info.mode())
            != (original.dev(), original.ino(), original.mode())
            || read_bytes(current, backend::MAX_FRAME)? != self.text.as_bytes()
        {
            return Err(io::Error::other(
                "script changed while preparing the filter",
            ));
        }
        if info.nlink() != 1 || info.uid() != unsafe { libc::geteuid() } || info.mode() & 0o200 == 0
        {
            return Err(io::Error::other(
                "embedding requires an owned, writable script with one link",
            ));
        }
        replacement.persist(path)?;
        Ok(())
    }

    fn program_key(&self, model: Option<&str>) -> String {
        let mut hash = Sha256::new();
        hash_part(&mut hash, self.digest.as_bytes());
        hash_part(&mut hash, RUNNER.as_bytes());
        hash_part(&mut hash, &PREPARATION_VERSION.to_be_bytes());
        hash_part(&mut hash, backend::REVISION.as_bytes());
        hash_part(&mut hash, backend::FILTER_INSTRUCTIONS.as_bytes());
        hash_part(&mut hash, model.unwrap_or_default().as_bytes());
        format!("{:x}", hash.finalize())
    }
}

fn storage_boundary(body: &str) -> Option<(usize, usize)> {
    let mut start = 0;
    let mut blank = None;
    for line in body.split_inclusive('\n') {
        let end = start + line.len();
        let contents = line.strip_suffix('\n').unwrap_or(line);
        let contents = contents.strip_suffix('\r').unwrap_or(contents);
        if contents == "---"
            && let Some(blank) = blank
        {
            return Some((blank, end));
        }
        blank = contents.is_empty().then_some(start);
        start = end;
    }
    None
}

#[derive(Deserialize, Serialize)]
pub struct Storage {
    format_version: u32,
    source: Source,
    program: Program,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    build: Option<Build>,
}

#[derive(Deserialize, Serialize)]
struct Source {
    sha256: String,
}

#[derive(Deserialize, Serialize)]
struct Program {
    runner: String,
    preparation_version: u32,
    #[serde(default)]
    stdin: StdinMode,
    code: String,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StdinMode {
    #[default]
    Eof,
    Unused,
}

#[derive(Deserialize, Serialize)]
struct Build {
    model: String,
}

impl Storage {
    pub fn new(script: &Script, code: String, stdin: StdinMode, model: Option<String>) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            source: Source {
                sha256: script.digest.clone(),
            },
            program: Program {
                runner: RUNNER.into(),
                preparation_version: PREPARATION_VERSION,
                stdin,
                code,
            },
            build: model.map(|model| Build { model }),
        }
    }

    fn compatible(&self, script: &Script, model: Option<&str>) -> bool {
        let supports_io = self.program.preparation_version == PREPARATION_VERSION
            || (self.program.preparation_version == 1 && self.program.stdin == StdinMode::Eof);
        self.format_version == FORMAT_VERSION
            && self.source.sha256 == script.digest
            && self.program.runner == RUNNER
            && supports_io
            && self.program.code.len() <= MAX_CODE
            && !self.program.code.trim().is_empty()
            && model.is_none_or(|model| self.model() == Some(model))
    }

    pub fn code(&self) -> &str {
        &self.program.code
    }

    pub fn stdin(&self) -> StdinMode {
        self.program.stdin
    }

    pub fn model(&self) -> Option<&str> {
        self.build.as_ref().map(|build| build.model.as_str())
    }

    pub fn result_key(&self, argv: &[OsString], stdin: Option<&[u8; 32]>) -> String {
        let mut hash = Sha256::new();
        hash_part(&mut hash, b"result-v2");
        hash_part(&mut hash, self.source.sha256.as_bytes());
        hash_part(&mut hash, self.program.runner.as_bytes());
        hash_part(&mut hash, &self.program.preparation_version.to_be_bytes());
        hash_part(&mut hash, backend::REVISION.as_bytes());
        hash_part(&mut hash, self.program.code.as_bytes());
        hash_part(&mut hash, &(argv.len() as u64).to_be_bytes());
        for arg in argv {
            hash_part(&mut hash, arg.as_bytes());
        }
        hash_part(&mut hash, stdin.map_or(&[], |digest| digest.as_slice()));
        format!("{:x}", hash.finalize())
    }
}

fn hash_part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

pub struct ResultEntry {
    pub code: u8,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub environment: EnvironmentInputs,
}

pub type Environment = BTreeMap<OsString, OsString>;

#[derive(Deserialize, Serialize)]
pub struct EnvironmentInputs {
    // None means the program enumerated the whole environment.
    names: Option<BTreeSet<Vec<u8>>>,
    digest: String,
}

impl EnvironmentInputs {
    pub fn new(names: Option<BTreeSet<Vec<u8>>>, environment: &Environment) -> Self {
        let digest = Self::digest(names.as_ref(), environment);
        Self { names, digest }
    }

    fn matches(&self, environment: &Environment) -> bool {
        self.digest == Self::digest(self.names.as_ref(), environment)
    }

    fn digest(names: Option<&BTreeSet<Vec<u8>>>, environment: &Environment) -> String {
        let mut hash = Sha256::new();
        if let Some(names) = names {
            for name in names {
                hash_part(&mut hash, name);
                if let Some(value) = environment.get(OsStr::from_bytes(name)) {
                    hash.update([1]);
                    hash_part(&mut hash, value.as_bytes());
                } else {
                    hash.update([0]);
                }
            }
        } else {
            for (name, value) in environment {
                hash_part(&mut hash, name.as_bytes());
                hash_part(&mut hash, value.as_bytes());
            }
        }
        format!("{:x}", hash.finalize())
    }
}

pub struct Cache(PathBuf);

pub enum ProgramEntry {
    Prepared(Storage),
    Declined,
}

impl Cache {
    pub fn open() -> io::Result<Self> {
        let parent = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(dirs::cache_dir)
            .ok_or_else(|| io::Error::other("no user cache directory"))?;
        let beg = parent.join("beg");
        private_directory(&beg)?;
        let version = beg.join("v1");
        private_directory(&version)?;
        Ok(Self(version))
    }

    pub fn program(&self, script: &Script, model: Option<&str>) -> Option<ProgramEntry> {
        let key = script.program_key(model);
        let bytes = self.read("program", &key, MAX_CODE * 8)?;
        match serde_json::from_slice::<Option<Storage>>(&bytes).ok()? {
            Some(storage) if storage.compatible(script, model) => {
                Some(ProgramEntry::Prepared(storage))
            }
            Some(_) => None,
            None => Some(ProgramEntry::Declined),
        }
    }

    pub fn put_program(
        &self,
        script: &Script,
        model: Option<&str>,
        storage: &Storage,
    ) -> io::Result<()> {
        let bytes = serde_json::to_vec(storage).map_err(io::Error::other)?;
        self.write("program", &script.program_key(model), &bytes)
    }

    pub fn put_declined(&self, script: &Script, model: Option<&str>) -> io::Result<()> {
        self.write("program", &script.program_key(model), b"null")
    }

    pub fn result(&self, key: &str, environment: &Environment) -> Option<ResultEntry> {
        let bytes = self.read("result", key, MAX_RESULT_METADATA + 1 + MAX_OUTPUT + 5)?;
        let metadata_end = bytes.iter().position(|byte| *byte == b'\n')?;
        if metadata_end > MAX_RESULT_METADATA {
            return None;
        }
        let metadata: EnvironmentInputs = serde_json::from_slice(&bytes[..metadata_end]).ok()?;
        let bytes = &bytes[metadata_end + 1..];
        if bytes.len() > MAX_OUTPUT + 5 || !metadata.matches(environment) {
            return None;
        }
        let code = *bytes.first()?;
        let size = u32::from_be_bytes(bytes.get(1..5)?.try_into().ok()?) as usize;
        let (stdout, stderr) = bytes.get(5..)?.split_at_checked(size)?;
        Some(ResultEntry {
            code,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            environment: metadata,
        })
    }

    pub fn put_result(&self, key: &str, output: &ResultEntry) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(&output.environment).map_err(io::Error::other)?;
        if bytes.len() > MAX_RESULT_METADATA {
            return Ok(());
        }
        bytes.push(b'\n');
        bytes.push(output.code);
        bytes.extend_from_slice(&(output.stdout.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&output.stdout);
        bytes.extend_from_slice(&output.stderr);
        self.write("result", key, &bytes)
    }

    fn slot(&self, kind: &str, key: &str) -> PathBuf {
        // Bound disk usage independently for programs and results. The full key
        // in each slot distinguishes a hit from an unrelated replacement.
        self.0.join(format!("{kind}-{}", &key[..2]))
    }

    fn read(&self, kind: &str, key: &str, limit: usize) -> Option<Vec<u8>> {
        let mut file = open_regular(&self.slot(kind, key)).ok()?;
        let metadata = file.metadata().ok()?;
        if !private(&metadata) {
            return None;
        }
        let mut stored_key = [0; 64];
        file.read_exact(&mut stored_key).ok()?;
        if stored_key != key.as_bytes() {
            return None;
        }
        read_bytes(file, limit).ok()
    }

    fn write(&self, kind: &str, key: &str, bytes: &[u8]) -> io::Result<()> {
        let mut temp = NamedTempFile::new_in(&self.0)?;
        temp.write_all(key.as_bytes())?;
        temp.write_all(bytes)?;
        temp.as_file().sync_data()?;
        temp.persist(self.slot(kind, key))?;
        Ok(())
    }
}

fn read_bytes(file: File, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other("file exceeds its size limit"));
    }
    Ok(bytes)
}

fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("expected a regular file"));
    }
    Ok(file)
}

fn private_directory(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || !private(&metadata) {
        return Err(io::Error::other("cache directory is not private"));
    }
    Ok(())
}

fn private(metadata: &Metadata) -> bool {
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0
}
