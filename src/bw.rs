extern crate secstr;

use std::collections::btree_map::BTreeMap;
use std::process::{Command, Output};
use std::string::FromUtf8Error;
use std::{fs, io};
use tempfile::NamedTempFile;

use config::Config;
use k8s_openapi::ByteString;
use secstr::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

static DEFAULT_INSTANCE_URL: &str = "https://vault.bitwarden.com";

#[derive(Clone)]
pub struct WardenClientWrapper {
    bw_path: String,
    url: String,
    user: SecStr,
    password: SecStr,
    organization: Option<String>,
    session_token: Option<SecStr>,
}

impl WardenClientWrapper {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let bw = WardenClientWrapper {
            bw_path: config
                .get_string("bw_path")
                .unwrap_or("/usr/bin/bw".to_owned()),
            url: config
                .get_string("url")
                .unwrap_or(DEFAULT_INSTANCE_URL.to_owned()),
            user: SecStr::from(
                config
                    .get_string("user")
                    .expect("User not configured.")
                    .as_str(),
            ),
            password: SecStr::from(
                config
                    .get_string("pass")
                    .expect("Password not configured.")
                    .as_str(),
            ),
            organization: config.get_string("organization").ok(),
            session_token: None,
        };
        bw.bw_command_with_env(
            vec![
                "config".to_string(),
                "server".to_string(),
                bw.url.to_string(),
            ],
            BTreeMap::new(),
        )
        .expect("Could not configure bitwarden/vaultwarden server");
        return bw;
    }

    fn find_collection_id(&self, collection: String) -> Result<String, WardenCommandError> {
        return if self.organization.is_some() {
            let org = self.organization.as_ref().unwrap().clone();
            self.command_with_env(format!("bw list org-collections --organizationid '{org}' --search \"{collection}\"  | jq -r -c '.[] | select( .name == \"{collection}\") | .id'"), self.create_session_env())
        } else {
            self.command_with_env(format!("bw list collections --search \"{collection}\" | jq -r -c '.[] | select( .name == \"{collection}\") | .id'"), self.create_session_env())
        };
    }

    pub fn fetch_item_fields(
        &mut self,
        item: String,
    ) -> Result<BTreeMap<String, String>, WardenCommandError> {
        self.verify_session_token()?;
        let item_id: String = self.find_item_id(&item)?;
        return self.get_item_fields(&item_id);
    }

    fn verify_session_token(&mut self) -> Result<(), WardenCommandError> {
        if self.session_token.is_none() {
            self.session_token = Some(self.login()?);
            self.sync()?;
        }
        Ok(())
    }

    pub fn fetch_item_attachments(
        &mut self,
        item: String,
    ) -> Result<BTreeMap<String, ByteString>, WardenCommandError> {
        self.verify_session_token()?;
        let item_id: String = self.find_item_id(&item)?;
        return self.get_item_attachments(&item_id);
    }

    pub fn sync(&mut self) -> Result<(), WardenCommandError> {
        self.verify_session_token()?;
        self.command_with_env(format!("bw sync"), self.create_session_env())?;
        Ok(())
    }

    pub fn reset(&mut self) {
        let _ = self.bw_command(vec!["logout".to_string()]);
        self.session_token = None;
    }

    fn get_item_fields(
        &self,
        item_id: &str,
    ) -> Result<BTreeMap<String, String>, WardenCommandError> {
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        let json_fields: String = self.command_with_env(
            format!("bw get item '{item_id}' | jq '[select(.fields != null) | .fields[]]'"),
            self.create_session_env(),
        )?;
        let result: Vec<ItemField> = serde_json::from_str(&json_fields)?;
        for x in result {
            fields.insert(x.name, x.value);
        }
        return Ok(fields);
    }

    fn get_item_attachments(
        &self,
        item_id: &str,
    ) -> Result<BTreeMap<String, ByteString>, WardenCommandError> {
        let mut attachments: BTreeMap<String, ByteString> = BTreeMap::new();
        let json_attachments: String = self.command_with_env(format!("bw get item '{item_id}' | jq '[select(.attachments != null) | .attachments[] | {{fileName: .fileName, id: .id}}]'"), self.create_session_env())?;
        let attachments_with_ids: Vec<Attachment> = serde_json::from_str(&json_attachments)?;
        for attachment in attachments_with_ids {
            let attachment_file: NamedTempFile = NamedTempFile::new()?;
            let attachment_file_path: &str = attachment_file.path().to_str().unwrap();
            let attachment_id = attachment.id;
            self.command_with_env(format!("bw get attachment '{attachment_id}' --itemid '{item_id}' --output {attachment_file_path} --quiet"), self.create_session_env())?;
            let attachment_value = fs::read(attachment_file.path()).map(|v| ByteString(v))?;
            attachments.insert(attachment.file_name, attachment_value);
            drop(attachment_file);
        }
        return Ok(attachments);
    }

    fn find_item_id(&self, item: &str) -> Result<String, WardenCommandError> {
        let split_item = item.split("/").collect::<Vec<_>>();
        let item_name = split_item[1];
        if split_item[0].len() > 0 {
            let collection_id: String = self.find_collection_id(split_item[0].to_string())?;
            return self.command_with_env(format!("bw list items --search '{item_name}' --collectionid '{collection_id}' | jq -r -c '.[] | select( .name == \"{item_name}\") | .id'"), self.create_session_env());
        }
        return self.command_with_env(format!("bw list items --search '{item_name}' | jq -r -c '.[] | select( .name == \"{item_name}\") | .id'"), self.create_session_env());
    }

    fn login(&self) -> Result<SecStr, WardenCommandError> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert(
            "BW_USER".to_string(),
            String::from_utf8(self.user.unsecure().to_vec())?,
        );
        env.insert(
            "BW_PASS".to_string(),
            String::from_utf8(self.password.unsecure().to_vec())?,
        );
        let login_result: Result<String, WardenCommandError> = self.bw_command_with_env(
            vec![
                "login".to_owned(),
                "$BW_USER".to_owned(),
                "$BW_PASS".to_owned(),
                "--raw".to_owned(),
            ],
            env,
        );
        if login_result.is_ok() {
            return Ok(SecStr::from(login_result.unwrap()));
        }
        let err: WardenCommandError = login_result.unwrap_err();
        return Err(match err.to_string().as_str() {
            "Email address is invalid." | "Username or password is incorrect. Try again" => {
                WardenCommandError::InvalidCredentials(err.to_string())
            }
            _ => WardenCommandError::Other(err.to_string()),
        });
    }

    fn bw_command(&self, args: Vec<String>) -> Result<String, WardenCommandError> {
        if args[0] == "login" || args[0] == "logout" {
            return self.bw_command_with_env(args, BTreeMap::new());
        }
        return self.bw_command_with_env(args, self.create_session_env());
    }

    fn create_session_env(&self) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        let s = self
            .session_token
            .as_ref()
            .expect("Expected session token to be set");
        let vec = s.unsecure().to_vec();
        env.insert(
            "BW_SESSION".to_string(),
            String::from_utf8(vec).expect("Expected session token to be set in valid UTF8"),
        );
        env
    }

    fn bw_command_with_env(
        &self,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Result<String, WardenCommandError> {
        return self.command_with_env(format!("{} {}", self.bw_path, args.join(" ")), env);
    }

    fn command_with_env(
        &self,
        command: String,
        env: BTreeMap<String, String>,
    ) -> Result<String, WardenCommandError> {
        #[cfg(not(target_os = "windows"))]
        let shell: &str = "/bin/bash";
        #[cfg(not(target_os = "windows"))]
        let shell_command_param: &str = "-c";

        #[cfg(target_os = "windows")]
        let shell: &str = "cmd";
        #[cfg(target_os = "windows")]
        let shell_command_param: &str = "/C";

        trace!("Executing {}", command);

        let output: Output = Command::new(shell)
            .args(&[shell_command_param, command.as_str()])
            .envs(env)
            .output()?;
        let status = output.status;
        trace!("Status: {status}");
        let mut out: String = String::from_utf8(output.stdout)?;
        let mut err = String::from_utf8(output.stderr)?;
        trim_newline(&mut out);
        trim_newline(&mut err);
        if output.status.success() {
            return Ok(out);
        }
        return Err(WardenCommandError::WardenCommandError(err));
    }
}

fn trim_newline(s: &mut String) {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Attachment {
    #[serde(alias = "fileName")]
    pub file_name: String,
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ItemField {
    pub name: String,
    pub value: String,
    #[serde(alias = "type")]
    pub type_: u64,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WardenCommandError {
    #[error("Bitwarden CLI error {0}")]
    WardenCommandError(String),
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

impl From<FromUtf8Error> for WardenCommandError {
    fn from(err: FromUtf8Error) -> Self {
        WardenCommandError::IO(err.to_string())
    }
}

impl From<io::Error> for WardenCommandError {
    fn from(err: io::Error) -> Self {
        WardenCommandError::IO(err.to_string())
    }
}

impl From<serde_json::Error> for WardenCommandError {
    fn from(err: serde_json::Error) -> Self {
        WardenCommandError::IO(err.to_string())
    }
}
