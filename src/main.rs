#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use]
extern crate log;

use std::borrow::ToOwned;
use std::collections::btree_map::BTreeMap;
use std::env;
use std::sync::Arc;

use config::Config;
use const_format::formatcp;
use futures::stream::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::ByteString;
use k8s_openapi::chrono::Utc;
use kube::{
    Api, api::ListParams, client::Client, Error as KubeError, runtime::Controller, runtime::controller::Action,
};
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::Resource;
use kube::ResourceExt;
use serde_json::{json, Value};
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::Duration;

use crate::bw::{BitwardenClientWrapper, BitwardenCommandError};
use crate::crd::{ApplyCondition, ApplyPhase, BitwardenSecret, BitwardenSecretStatus, ConditionType};

pub mod crd;
mod bw;
// mod bitwarden;

const BW_OPERATOR_ENV_PREFIX: &'static str = "BW_OPERATOR";
const ENV_CONFIG_PATH: &'static str = formatcp!("{}_CONFIG", BW_OPERATOR_ENV_PREFIX);
const ENV_NOOP_REQUEUE_DELAY: &'static str = formatcp!("{}_NOOP_REQUEUE_DELAY", BW_OPERATOR_ENV_PREFIX);
const DEFAULT_CONFIG_PATH: &'static str = "config/config";
const DEFAULT_NOOP_REQUEUE_DELAY: u64 = 300;

// TODO add status
// TODO Watch secret deletion, if owner refs contains a bitwardensecret, recreate
#[tokio::main]
async fn main() {
    env::set_var("RUST_LOG", "info");
    env_logger::init();

    let config: Config = Config::builder()
        .add_source(config::File::with_name(&env::var(ENV_CONFIG_PATH).unwrap_or(DEFAULT_CONFIG_PATH.to_owned())))
        .add_source(config::Environment::with_prefix(BW_OPERATOR_ENV_PREFIX))
        .build()
        .expect("Could not initialize config");

    let bw_client = BitwardenClientWrapper::new(config);

    let kubernetes_client: Client = Client::try_default()
        .await
        .expect("Expected a valid KUBECONFIG environment variable.");

    let crd_api: Api<BitwardenSecret> = Api::all(kubernetes_client.clone());
    let bw_mutex = Mutex::new(bw_client);
    let context: Arc<ContextData> = Arc::new(ContextData::new(kubernetes_client.clone(), bw_mutex));

    let c = context.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300));

        loop {
            interval.tick().await;
            let mutex_guard_fut = c.bw_client.lock();
            let mut bw_client: MutexGuard<BitwardenClientWrapper> = mutex_guard_fut.await;
            info!("Syncing BitWarden vault");
            let result = bw_client.sync();
            if result.is_err() {
                error!("Could not sync BitWarden vault: {}", result.err().unwrap());
                bw_client.reset();
            }
            drop(bw_client);
        }
    });

    // The controller comes from the `kube_runtime` crate and manages the reconciliation process.
    // It requires the following information:
    // - `kube::Api<T>` this controller "owns". In this case, `T = BitwardenSecret`, as this controller owns the `BitwardenSecret` resource,
    // - `kube::api::ListParams` to select the `BitwardenSecret` resources with. Can be used for BitwardenSecret filtering `BitwardenSecret` resources before reconciliation,
    // - `reconcile` function with reconciliation logic to be called each time a resource of `BitwardenSecret` kind is created/updated/deleted,
    // - `on_error` function to call whenever reconciliation fails.
    Controller::new(crd_api.clone(), ListParams::default())
        .run(reconcile, on_error, context)
        .for_each(|reconciliation_result| async move {
            match reconciliation_result {
                Ok(bitwarden_secret_resource) => {
                    println!("Reconciliation successful. Resource: {:?}", bitwarden_secret_resource);
                }
                Err(reconciliation_err) => {
                    eprintln!("Reconciliation error: {:?}", reconciliation_err)
                }
            }
        })
        .await;
}

struct ContextData {
    client: Client,
    bw_client: Mutex<BitwardenClientWrapper>,
}

impl ContextData {
    pub fn new(client: Client, bw_client: Mutex<BitwardenClientWrapper>) -> Self {
        ContextData { client, bw_client }
    }
}

enum BitwardenSecretAction {
    Create,
    Delete,
    NoOp,
}

async fn reconcile(bitwarden_secret: Arc<BitwardenSecret>, context: Arc<ContextData>) -> Result<Action, Error> {
    let client: Client = context.client.clone();

    let namespace: String = match bitwarden_secret.namespace() {
        None => "default".to_string(),
        Some(namespace) => namespace,
    };

    let name = bitwarden_secret.name_any();

    return match determine_action(&bitwarden_secret) {
        BitwardenSecretAction::Create => {
            let bitwarden_secret_api: Api<BitwardenSecret> = Api::namespaced(client.clone(), &namespace);
            let mut initial_status = BitwardenSecretStatus {
                conditions: vec![],
                phase: Some(ApplyPhase::Pending),
                start_time: Some(Utc::now().to_rfc3339()),
            };
            patch_status(&bitwarden_secret_api, &name, &initial_status).await?;
            add_finalizer(client.clone(), &name, &namespace).await?;
            initial_status.phase = Some(ApplyPhase::Running);
            patch_status(&bitwarden_secret_api, &name, &initial_status).await?;
            let mutex_guard_fut = context.bw_client.lock();
            let mut bw_client: MutexGuard<BitwardenClientWrapper> = mutex_guard_fut.await;

            let fields_result = bw_client.fetch_item_fields(bitwarden_secret.spec.item.to_owned());
            let attachments_result = bw_client.fetch_item_attachments(bitwarden_secret.spec.item.to_owned());
            if fields_result.is_ok() && attachments_result.is_ok() {
                let secret_api: Api<Secret> = Api::namespaced(client.clone(), &namespace);
                let string_secrets: BTreeMap<String, String> = fields_result.unwrap();
                let secrets: BTreeMap<String, ByteString> = attachments_result.unwrap();

                let owner_ref = OwnerReference {
                    api_version: "tomjo.net/v1".to_string(),
                    kind: "BitwardenSecret".to_string(),
                    name: name.clone(),
                    uid: bitwarden_secret.uid().unwrap(),
                    block_owner_deletion: Some(true),
                    controller: None,
                };

                let labels: BTreeMap<String, String> = bitwarden_secret.metadata.labels.clone().unwrap_or(BTreeMap::new());
                let annotations: BTreeMap<String, String> = bitwarden_secret.metadata.annotations.clone().unwrap_or(BTreeMap::new());

                let existing_secret = secret_api.get_opt(&name).await?;
                if existing_secret.is_some() {
                    let secret = existing_secret.unwrap();
                    let mut owner_references: Vec<OwnerReference> = secret.metadata.owner_references.unwrap_or(vec![]);
                    owner_references.push(owner_ref);
                    merge_secret(secret_api, owner_references, &name, &bitwarden_secret.spec.type_, string_secrets, secrets, labels, annotations).await?;
                } else {
                    create_secret(secret_api, owner_ref, &name, &namespace, &bitwarden_secret.spec.type_, string_secrets, secrets, labels, annotations).await?;
                }
                initial_status.phase = Some(ApplyPhase::Succeeded);
                update_status(&bitwarden_secret_api, &name, initial_status, None).await?;
            } else {
                initial_status.phase = Some(ApplyPhase::Failed);
                patch_status(&bitwarden_secret_api, &name, &initial_status).await?;

                if fields_result.is_err() {
                    error(&bitwarden_secret_api, &name, initial_status, fields_result.err().unwrap()).await?;
                }else if attachments_result.is_err() {
                    error(&bitwarden_secret_api, &name, initial_status,attachments_result.err().unwrap()).await?;
                }
                bw_client.reset();
            }

            Ok(Action::requeue(Duration::from_secs(10)))
        }
        BitwardenSecretAction::Delete => {
            delete_secret(client.clone(), &name, &namespace).await?;
            delete_finalizer(client, &name, &namespace).await?;
            Ok(Action::await_change())
        }
        BitwardenSecretAction::NoOp => Ok(Action::requeue(Duration::from_secs(*&env::var(ENV_NOOP_REQUEUE_DELAY).map(|x| x.parse::<u64>().expect("NOOP_REQUEUE_DELAY should be u64")).unwrap_or(DEFAULT_NOOP_REQUEUE_DELAY)))),
    };
}

async fn error(bitwarden_secret_api: &Api<BitwardenSecret>, name: &String, status: BitwardenSecretStatus, err: BitwardenCommandError) -> Result<(), Error> {
    error!("{}", err.to_string());
    update_status(&bitwarden_secret_api, &name, status, Some(err)).await?;
    Ok(())
}

fn determine_action(bitwarden_secret: &BitwardenSecret) -> BitwardenSecretAction {
    return if bitwarden_secret.meta().deletion_timestamp.is_some() {
        BitwardenSecretAction::Delete
    } else if bitwarden_secret
        .meta()
        .finalizers
        .as_ref()
        .map_or(true, |finalizers| finalizers.is_empty()) {
        BitwardenSecretAction::Create
    } else {
        BitwardenSecretAction::NoOp
    };
}

pub async fn add_finalizer(client: Client, name: &str, namespace: &str) -> Result<BitwardenSecret, Error> {
    let api: Api<BitwardenSecret> = Api::namespaced(client, namespace);
    let finalizer: Value = json!({
        "metadata": {
            "finalizers": ["bitwardensecrets.tomjo.net/finalizer.secret"]
        }
    });

    let patch: Patch<&Value> = Patch::Merge(&finalizer);
    Ok(api.patch(name, &PatchParams::default(), &patch).await?)
}

pub async fn patch_status(bitwarden_secret_api: &Api<BitwardenSecret>, name: &str, status: &BitwardenSecretStatus) -> Result<BitwardenSecret, Error> {
    let status_json: Value = json!({
        "apiVersion": "tomjo.net/v1",
        "kind": "BitwardenSecret",
        "status": status
    });
    let patch: Patch<&Value> = Patch::Apply(&status_json);
    let pp = PatchParams::apply("bitwarden-operator").force();
    let o = bitwarden_secret_api.patch_status(name, &pp, &patch).await?;
    Ok(o)
}


pub async fn update_status(bitwarden_secret_api: &Api<BitwardenSecret>, name: &str, mut status: BitwardenSecretStatus, error: Option<BitwardenCommandError>) -> Result<BitwardenSecret, Error> {
    for (pos, condition) in status.conditions.iter().enumerate() {
        if condition.type_ == ConditionType::Ready {
            let mut copy = condition.clone();
            if error.is_some(){
                copy.status = false;
                copy.message = Some("Could not fetch secret".to_string());
                copy.reason = error.map(|e| e.to_string())
            }else{
                copy.status = true;
                copy.message = None;
                copy.reason = None;
            }
            if copy.status != condition.status {
                copy.last_transition = Utc::now().to_rfc3339();
            }
            status.conditions[pos] = copy;
            return patch_status(bitwarden_secret_api, name, &status).await;
        }
    }

    status.conditions.push(ApplyCondition {
        type_: ConditionType::Ready,
        status: true,
        last_transition: Utc::now().to_rfc3339(),
        message: None,
        reason: None,
    });
    return patch_status(bitwarden_secret_api, name, &status).await;
}

pub async fn delete_finalizer(client: Client, name: &str, namespace: &str) -> Result<(), Error> {
    let api: Api<BitwardenSecret> = Api::namespaced(client, namespace);
    let has_resource = api.get_opt(name).await?.is_some();
    if has_resource {
        let finalizer: Value = json!({
            "metadata": {
                "finalizers": null
            }
        });

        let patch: Patch<&Value> = Patch::Merge(&finalizer);
        api.patch(name, &PatchParams::default(), &patch).await?;
    }
    Ok(())
}

pub async fn merge_secret(
    secret_api: Api<Secret>,
    owner_references: Vec<OwnerReference>,
    name: &str,
    type_: &str,
    string_secrets: BTreeMap<String, String>,
    secrets: BTreeMap<String, ByteString>,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
) -> Result<Secret, KubeError> {
    let patch_json: Value = json!({
                "metadata": {
                    "finalizers": ["bitwardensecrets.tomjo.net/finalizer.secret"],
                    "ownerReferences": owner_references,
                    // "annotations": annotations,
                    // "labels" : labels,
                },
                "stringData": string_secrets,
                "data": secrets,
                "type": type_.to_owned(),
            });
    let patch: Patch<&Value> = Patch::Merge(&patch_json);
    secret_api.patch(name, &PatchParams::default(), &patch)
        .await
}

/// TODO Note: It is assumed the resource does not already exists for simplicity. Returns an `Error` if it does.
pub async fn create_secret(
    secret_api: Api<Secret>,
    owner_ref: OwnerReference,
    name: &str,
    namespace: &str,
    type_: &str,
    string_secrets: BTreeMap<String, String>,
    secrets: BTreeMap<String, ByteString>,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
) -> Result<Secret, KubeError> {
    let secret: Secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(labels.clone()),
            annotations: Some(annotations.clone()),
            owner_references: Some(vec![owner_ref]),
            ..ObjectMeta::default()
        },
        string_data: Some(string_secrets.clone()),
        data: Some(secrets.clone()),
        type_: Some(type_.to_owned()),
        ..Secret::default()
    };

    secret_api
        .create(&PostParams::default(), &secret)
        .await
}

pub async fn delete_secret(client: Client, name: &str, namespace: &str) -> Result<(), Error> {
    let api: Api<Secret> = Api::namespaced(client, namespace);
    let has_secret = api.get_opt(name).await?.is_some();
    if has_secret {
        api.delete(name, &DeleteParams::default()).await?;
    }
    Ok(())
}

fn on_error(bitwarden_secret: Arc<BitwardenSecret>, error: &Error, _context: Arc<ContextData>) -> Action {
    eprintln!("Reconciliation error:\n{:?}.\n{:?}", error, bitwarden_secret);
    Action::requeue(Duration::from_secs(5)) //TODO exponential backoff
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes reported error: {source}")]
    KubeError {
        #[from]
        source: kube::Error,
    },
    #[error("Serde error: {source}")]
    SerdeError {
        #[from]
        source: serde_json::Error,
    },
    #[error("Invalid BitwardenSecret CRD: {0}")]
    UserInputError(String),
}
