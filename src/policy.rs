use crate::Failure;
use crate::tools::{Native, decode_base64};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

pub struct Policy {
    pub spawn: bool,
    pub env_names: BTreeSet<String>,
}

impl Policy {
    pub fn load() -> Result<Self, Failure> {
        let spawn = match env::var("BEG_EXEC") {
            Ok(value) if value == "inherit" => true,
            Ok(value) if value == "deny" => false,
            Err(env::VarError::NotPresent) => false,
            _ => return Err(Failure::new(64, "BEG_EXEC must be deny or inherit")),
        };
        let env = match env::var("BEG_ENV") {
            Ok(value) => value,
            Err(env::VarError::NotPresent) => String::new(),
            Err(_) => {
                return Err(Failure::new(
                    64,
                    "BEG_ENV must be a UTF-8 list of variable names",
                ));
            }
        };
        let env_names = env
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if !env_names.iter().all(|name| valid_env_name(name)) {
            return Err(Failure::new(
                64,
                "BEG_ENV contains an invalid variable name",
            ));
        }
        Ok(Self { spawn, env_names })
    }

    pub fn get_env(&self, name: &str) -> Result<Option<OsString>, String> {
        if !self.env_names.contains(name) {
            return Err("environment lookup denied by BEG_ENV".into());
        }
        Ok(env::var_os(name))
    }

    pub fn resolve(
        &self,
        value: Native,
        argv: &[OsString],
        cwd: &Path,
    ) -> Result<OsString, String> {
        match value {
            Native::Text(text) => Ok(text.into()),
            Native::Bytes(bytes) => decode_base64(&bytes.base64).map(OsString::from_vec),
            Native::Handle(handle) if handle.handle == "cwd" => Ok(cwd.as_os_str().to_owned()),
            Native::Handle(handle) => {
                if let Some(index) = handle.handle.strip_prefix("argv:") {
                    return index
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| argv.get(index))
                        .cloned()
                        .ok_or_else(|| "unknown argv handle".into());
                }
                if let Some(name) = handle.handle.strip_prefix("env:") {
                    return self
                        .get_env(name)?
                        .ok_or_else(|| "environment value is unset".into());
                }
                Err("unknown native value handle".into())
            }
        }
    }
}

pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', '\0'])
}

pub fn native(value: &OsStr, handle: String) -> Value {
    json!({
        "text": value.to_str(),
        "base64": STANDARD.encode(value.as_bytes()),
        "handle": handle,
    })
}
