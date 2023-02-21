extern crate secstr;

use std::collections::BTreeMap;
use std::io;
use std::process::{Command, Output};
use std::str::{FromStr, Split};
use std::string::FromUtf8Error;

use config::Config;
use k8s_openapi::DeepMerge;
use secstr::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// use crate::bw::BitwardenCommandError::BitwardenCommandError;

static DEFAULT_INSTANCE_URL: &str = "https://vault.bitwarden.com";

#[derive(Clone)]
pub struct BitwardenClientWrapper {
    bw_path: String,
    url: String,
    user: SecStr,
    password: SecStr,
    organization: Option<String>,
    session_token: Option<SecStr>,
}

impl BitwardenClientWrapper {
    #[must_use]
    pub fn new(config: Config) -> Self {
        let bw = BitwardenClientWrapper {
            bw_path: config.get_string("bw_path").unwrap_or("/usr/bin/bw".to_owned()),
            url: config.get_string("url").unwrap_or(DEFAULT_INSTANCE_URL.to_owned()),
            user: SecStr::from(config.get_string("user").expect("Bitwarden user not configured.").as_str()),
            password: SecStr::from(config.get_string("pass").expect("Bitwarden password not configured.").as_str()),
            organization: config.get_string("organization").ok(),
            session_token: None,
        };
        bw.bw_command_with_env(vec!["config".to_string(), "server".to_string(), bw.url], BTreeMap::new()).expect("Could not configure bitwarden server");
        return bw;
    }

    fn find_collection_id(&self, collection: String) -> Result<String, BitwardenCommandError> {
        return if self.organization.is_some() {
            let org = self.organization.as_ref().unwrap().clone();
            self.command_with_env(format!("bw list org-collections --organizationid {org} --search \"{collection}\"  | jq -r -c '.[] | select( .name == \"{collection}\") | .id'"), self.create_session_env())
        } else {
            self.command_with_env(format!("bw list collections --search \"{collection}\"  | jq -r -c '.[] | select( .name == \"{collection}\") | .id'"), self.create_session_env())
        };
    }

    pub fn fetch_item(&mut self, item: String) -> Result<BTreeMap<String, String>, BitwardenCommandError> {
        let mut secrets: BTreeMap<String, String> = BTreeMap::new();
        if self.session_token.is_none() {
            self.session_token = Some(self.login()?);
        }
        info!("Finding item");
        let item_id: String = self.find_item_id(&item)?;
        let mut fields: BTreeMap<String, String> = self.get_item_fields(&item_id)?;
        secrets.append(&mut fields);
        //TODO attachments
        return Ok(secrets);
    }

    pub fn reset(&mut self) {
        let _ = self.bw_command(vec!["logout".to_string()]);
        self.session_token = None;
    }

    fn get_item_fields(&self, item_id: &str) -> Result<BTreeMap<String, String>, BitwardenCommandError> {
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        let json_fields: String = self.command_with_env(format!("bw get item {item_id} | jq .fields"), self.create_session_env())?;
        let result: Vec<ItemField> = serde_json::from_str(&json_fields)?;
        for x in result {
            fields.insert(x.name, x.value);
        }
        return Ok(fields);
    }

    fn find_item_id(&self, item: &str) -> Result<String, BitwardenCommandError> {
        let split_item = item.split("/").collect::<Vec<_>>();
        let item_name = split_item[1];
        if split_item[0].len() > 0 {
            let collection_id: String = self.find_collection_id(split_item[0].to_string())?;
            return self.command_with_env(format!("bw list items --search {item_name} --collectionid {collection_id} | jq -r -c '.[] | select( .name == \"{item_name}\") | .id'"), self.create_session_env());
        }
        return self.command_with_env(format!("bw list items --search {item_name} | jq -r -c '.[] | select( .name == \"{item_name}\") | .id'"), self.create_session_env());
    }


    // TODO add api key support, current homelab vaultwarden version does not support it yet: upgrade blocked until operator works instead of bitwardenfetch.sh
    fn login(&self) -> Result<SecStr, BitwardenCommandError> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert("BW_USER".to_string(), String::from_utf8(self.user.unsecure().to_vec())?);
        env.insert("BW_PASS".to_string(), String::from_utf8(self.password.unsecure().to_vec())?);
        info!("Bitwarden: Logging in {} : {}", String::from_utf8(self.user.unsecure().to_vec())?, String::from_utf8(self.password.unsecure().to_vec())?);
        let login_result: Result<String, BitwardenCommandError> = self.bw_command_with_env(vec!["login".to_owned(), "$BW_USER".to_owned(), "$BW_PASS".to_owned(), "--raw".to_owned()], env);
        // let login_result: Result<String, BitwardenCommandError> = self.command_with_env("echo $BW_USER $BW_PASS".to_string(), env);
        if login_result.is_ok() {
            info!("Bitwarden: Logged in");
            return Ok(SecStr::from(login_result.unwrap()));
        }
        let err: BitwardenCommandError = login_result.unwrap_err();
        info!("Unwrapped err str {}", err.to_string().as_str());
        return Err(match err.to_string().as_str() {
            "Email address is invalid." | "Username or password is incorrect. Try again" => BitwardenCommandError::InvalidCredentials(err.to_string()),
            _ => BitwardenCommandError::Other(err.to_string())
        });
    }

    fn bw_command(&self, args: Vec<String>) -> Result<String, BitwardenCommandError> {
        if args[0] == "login" || args[0] == "logout" {
            return self.bw_command_with_env(args, BTreeMap::new());
        }
        return self.bw_command_with_env(args, self.create_session_env());
    }

    fn create_session_env(&self) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        let s = self.session_token.as_ref().expect("Expected session token to be set");
        let vec = s.unsecure().to_vec();
        env.insert("BW_SESSION".to_string(), String::from_utf8(vec).expect("Expected session token to be set in valid UTF8"));
        env
    }

    fn bw_command_with_env(&self, args: Vec<String>, env: BTreeMap<String, String>) -> Result<String, BitwardenCommandError> {
        return self.command_with_env(format!("{} {}", self.bw_path, args.join(" ")), env);
    }

    fn command_with_env(&self, command: String, env: BTreeMap<String, String>) -> Result<String, BitwardenCommandError> {
        #[cfg(not(target_os = "windows"))]
            let shell: &str = "/bin/sh";
        #[cfg(not(target_os = "windows"))]
            let shell_command_param: &str = "-c";

        #[cfg(target_os = "windows")]
            let shell: &str = "cmd";
        #[cfg(target_os = "windows")]
            let shell_command_param: &str = "/C";

        info!("Executing {}", command);

        let output: Output = Command::new(shell)
            .args(&[shell_command_param, command.as_str()])
            .envs(env)
            .output()?;
        let status = output.status;
        info!("Status: {status}");
        let out = String::from_utf8(output.stdout)?;
        let err = String::from_utf8(output.stderr)?;
        info!("Out: {out}");
        info!("Err: {err}");
        if output.status.success() {
            return Ok(out);
        }
        return Err(BitwardenCommandError::BitwardenCommandError(err));
    }
}


#[derive(Serialize, Deserialize, Debug)]
pub struct ItemField {
    pub name: String,
    pub value: String,
    #[serde(alias = "type")]
    pub type_: u64,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BitwardenCommandError {
    #[error("Bitwarden CLI error {0}")]
    BitwardenCommandError(String),
    #[error("Session expired: {0}")]
    SessionExpired(String),
    #[error("Locked: {0}")]
    Locked(String),
    #[error("InvalidCredentials: {0}")]
    InvalidCredentials(String),
    #[error("IO error: {0}")]
    IO(String),
    #[error("Other error: {0}")]
    Other(String),
}

impl From<FromUtf8Error> for BitwardenCommandError {
    fn from(err: FromUtf8Error) -> Self {
        BitwardenCommandError::IO(err.to_string())
    }
}

impl From<io::Error> for BitwardenCommandError {
    fn from(err: io::Error) -> Self {
        BitwardenCommandError::IO(err.to_string())
    }
}

impl From<serde_json::Error> for BitwardenCommandError {
    fn from(err: serde_json::Error) -> Self {
        BitwardenCommandError::IO(err.to_string())
    }
}
