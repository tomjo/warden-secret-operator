#![feature(proc_macro_hygiene, decl_macro)]
#![feature(unwrap_infallible)]

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
use k8s_openapi::chrono::Utc;
use k8s_openapi::ByteString;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::runtime::watcher::Config as WatcherConfig;
use kube::Resource;
use kube::ResourceExt;
use kube::{
    client::Client, runtime::controller::Action, runtime::Controller, Api, Error as KubeError,
};
use serde_json::{json, Value};
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::Duration;
use warp::Filter;

use crate::bw::{WardenClientWrapper, WardenCommandError};
use crate::crd::{
    get_api_version, get_kind, ApplyCondition, WardenSecret, WardenSecretStatus, ConditionStatus, ConditionType,
};

mod bw;
mod conversion;
mod crd;

const BW_OPERATOR_ENV_PREFIX: &'static str = "BW_OPERATOR";
const ENV_CONFIG_PATH: &'static str = formatcp!("{}_CONFIG", BW_OPERATOR_ENV_PREFIX);
const ENV_NOOP_REQUEUE_DELAY: &'static str =
    formatcp!("{}_NOOP_REQUEUE_DELAY", BW_OPERATOR_ENV_PREFIX);
const ENV_TLS_CERT_PATH: &'static str = formatcp!("{}_TLS_CERT_PATH", BW_OPERATOR_ENV_PREFIX);
const ENV_TLS_KEY_PATH: &'static str = formatcp!("{}_TLS_KEY_PATH", BW_OPERATOR_ENV_PREFIX);
const DEFAULT_CONFIG_PATH: &'static str = "config/config";
const DEFAULT_TLS_CERT_PATH: &'static str = "/certs/tls.crt";
const DEFAULT_TLS_KEY_PATH: &'static str = "/certs/tls.key";
const DEFAULT_NOOP_REQUEUE_DELAY: u64 = 300;

// TODO Watch secret deletion, if owner refs contains a wardensecret, recreate
// TODO refresh interval in spec (optional) -> default 5min?
// TODO config option vault sync interval
// TODO exponential backoff on error
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config: Config = Config::builder()
        .add_source(config::File::with_name(
            &env::var(ENV_CONFIG_PATH).unwrap_or(DEFAULT_CONFIG_PATH.to_owned()),
        ))
        .add_source(config::Environment::with_prefix(BW_OPERATOR_ENV_PREFIX))
        .build()
        .expect("Could not initialize config");

    setup_webserver(&config);

    let bw_client = WardenClientWrapper::new(&config);

    let kubernetes_client: Client = Client::try_default()
        .await
        .expect("Expected a valid KUBECONFIG environment variable.");

    let api: Api<WardenSecret> = Api::all(kubernetes_client.clone());
    let bw_mutex = Mutex::new(bw_client);
    let context: Arc<ContextData> = Arc::new(ContextData::new(kubernetes_client.clone(), bw_mutex));

    setup_periodic_vault_sync(context.clone(), Duration::from_secs(300));

    // The controller comes from the `kube_runtime` crate and manages the reconciliation process.
    // It requires the following information:
    // - `kube::Api<T>` this controller "owns". In this case, `T = WardenSecret`, as this controller owns the `WardenSecret` resource,
    // - `kube::api::ListParams` to select the `WardenSecret` resources with. Can be used for WardenSecret filtering `WardenSecret` resources before reconciliation,
    // - `reconcile` function with reconciliation logic to be called each time a resource of `WardenSecret` kind is created/updated/deleted,
    // - `on_error` function to call whenever reconciliation fails.
    Controller::new(api.clone(), WatcherConfig::default())
        .run(reconcile, on_error, context)
        .for_each(|reconciliation_result| async move {
            match reconciliation_result {
                Ok(warden_secret_resource) => {
                    trace!(
                        "Reconciliation successful. Resource: {:?}",
                        warden_secret_resource
                    );
                }
                Err(reconciliation_err) => {
                    error!("Reconciliation error: {:?}", reconciliation_err)
                }
            }
        })
        .await;
}

fn setup_periodic_vault_sync(context: Arc<ContextData>, period: Duration) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);

        loop {
            interval.tick().await;
            let mutex_guard_fut = context.bw_client.lock();
            let mut bw_client: MutexGuard<WardenClientWrapper> = mutex_guard_fut.await;
            debug!("Syncing vault");
            let result = bw_client.sync();
            if result.is_err() {
                error!("Could not sync vault: {}", result.err().unwrap());
                bw_client.reset();
            }
            drop(bw_client);
        }
    });
}

fn setup_webserver(config: &Config) {
    let routes = warp::path("crdconvert")
        .and(warp::body::json())
        .and_then(conversion::crdconvert_handler)
        .with(warp::trace::named("crdconvert"));

    let port: u16 = config.get_int("webserver_port").unwrap_or(8080) as u16;
    let ip: [u8; 4] = config
        .get_string("webserver_ip")
        .unwrap_or("0.0.0.0".to_string())
        .split(".")
        .map(|x| x.parse::<u8>().unwrap())
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap();

    let server_config = warp::serve(warp::post().and(routes));

    if config.get_bool("webserver_tls").unwrap_or(false) {
        tokio::spawn(
            server_config
                .tls()
                .cert_path(
                    config
                        .get_string("tls_cert_path")
                        .unwrap_or(DEFAULT_TLS_CERT_PATH.to_owned()),
                )
                .key_path(
                    config
                        .get_string("tls_key_path")
                        .unwrap_or(DEFAULT_TLS_KEY_PATH.to_owned()),
                )
                .run((ip, port)),
        );
    } else {
        tokio::spawn(server_config.run((ip, port)));
    }
}

struct ContextData {
    client: Client,
    bw_client: Mutex<WardenClientWrapper>,
}

impl ContextData {
    pub fn new(client: Client, bw_client: Mutex<WardenClientWrapper>) -> Self {
        ContextData { client, bw_client }
    }
}

enum WardenSecretAction {
    Create,
    Update,
    Delete,
    NoOp,
}

async fn reconcile(
    warden_secret: Arc<WardenSecret>,
    context: Arc<ContextData>,
) -> Result<Action, Error> {
    let client: Client = context.client.clone();

    let namespace: String = match warden_secret.namespace() {
        None => "default".to_string(),
        Some(namespace) => namespace,
    };

    let name = warden_secret.name_any();

    return match determine_action(&warden_secret) {
        WardenSecretAction::Create => {
            debug!("Creating WardenSecret: {}", name);
            let warden_secret_api: Api<WardenSecret> =
                Api::namespaced(client.clone(), &namespace);
            let mut status = WardenSecretStatus {
                start_time: Some(Utc::now().to_rfc3339()),
                conditions: vec![ApplyCondition {
                    type_: ConditionType::Ready,
                    status: ConditionStatus::Unknown,
                    last_transition: Utc::now().to_rfc3339(),
                    message: None,
                    reason: None,
                }],
                observed_generation: warden_secret.meta().generation,
            };
            patch_status(&warden_secret_api, &name, &status).await?;
            add_finalizer(&warden_secret_api, &name).await?;
            status = update_ready_condition(
                status,
                ApplyCondition {
                    type_: ConditionType::Ready,
                    status: ConditionStatus::True,
                    last_transition: Utc::now().to_rfc3339(),
                    message: None,
                    reason: None,
                },
            );
            patch_status(&warden_secret_api, &name, &status).await?;
            update_secret_with_vault_secrets(
                &warden_secret,
                &client,
                &namespace,
                &name,
                &warden_secret_api,
                status,
                context,
            )
            .await?;
            info!("Created WardenSecret {:?}", warden_secret);
            Ok(Action::requeue(Duration::from_secs(10)))
        }
        WardenSecretAction::Update => {
            let warden_secret_api: Api<WardenSecret> =
                Api::namespaced(client.clone(), &namespace);
            let status = warden_secret_api
                .get_status(&name)
                .await?
                .status
                .and_then(|mut x| {
                    x.observed_generation = warden_secret.meta().generation;
                    Some(x)
                })
                .unwrap_or(WardenSecretStatus {
                    conditions: vec![ApplyCondition {
                        type_: ConditionType::Ready,
                        status: ConditionStatus::Unknown,
                        last_transition: Utc::now().to_rfc3339(),
                        message: None,
                        reason: None,
                    }],
                    observed_generation: warden_secret.meta().generation,
                    start_time: Some(Utc::now().to_rfc3339()),
                });
            update_secret_with_vault_secrets(
                &warden_secret,
                &client,
                &namespace,
                &name,
                &warden_secret_api,
                status,
                context,
            )
            .await?;
            info!("Updated WardenSecret {:?}", warden_secret);
            Ok(Action::requeue(Duration::from_secs(10)))
        }
        WardenSecretAction::Delete => {
            delete_secret(client.clone(), &name, &namespace).await?;
            delete_finalizer(client, &name, &namespace).await?;
            Ok(Action::await_change())
        }
        WardenSecretAction::NoOp => Ok(Action::requeue(Duration::from_secs(
            env::var(ENV_NOOP_REQUEUE_DELAY)
                .map(|x| {
                    x.parse::<u64>()
                        .expect("NOOP_REQUEUE_DELAY should be u64 parseable")
                })
                .unwrap_or(DEFAULT_NOOP_REQUEUE_DELAY),
        ))),
    };
}

async fn update_secret_with_vault_secrets(
    warden_secret: &Arc<WardenSecret>,
    client: &Client,
    namespace: &String,
    name: &String,
    warden_secret_api: &Api<WardenSecret>,
    mut status: WardenSecretStatus,
    context: Arc<ContextData>,
) -> Result<(), Error> {
    let mutex_guard_fut = context.bw_client.lock();
    let mut bw_client: MutexGuard<WardenClientWrapper> = mutex_guard_fut.await;
    let fields_result = bw_client.fetch_item_fields(warden_secret.spec.item.to_owned());
    let attachments_result =
        bw_client.fetch_item_attachments(warden_secret.spec.item.to_owned());
    if fields_result.is_ok() && attachments_result.is_ok() {
        let secret_api: Api<Secret> = Api::namespaced(client.clone(), &namespace);
        let string_secrets: BTreeMap<String, String> = fields_result.unwrap();
        let secrets: BTreeMap<String, ByteString> = attachments_result.unwrap();

        let owner_ref = create_owner_ref_for(&warden_secret)?;

        let labels: BTreeMap<String, String> = warden_secret
            .metadata
            .labels
            .clone()
            .unwrap_or(BTreeMap::new());
        let annotations: BTreeMap<String, String> = warden_secret
            .metadata
            .annotations
            .clone()
            .unwrap_or(BTreeMap::new());

        let existing_secret = secret_api.get_opt(&name).await?;
        if existing_secret.is_some() {
            let secret = existing_secret.unwrap();
            let mut owner_references: Vec<OwnerReference> =
                secret.metadata.owner_references.unwrap_or(vec![]);
            add_or_update_owner_ref(&mut owner_references, owner_ref);
            merge_secret(
                secret_api,
                owner_references,
                &name,
                &warden_secret.spec.type_,
                string_secrets,
                secrets,
                labels,
                annotations,
            )
            .await?;
        } else {
            create_secret(
                secret_api,
                owner_ref,
                &name,
                &namespace,
                &warden_secret.spec.type_,
                string_secrets,
                secrets,
                labels,
                annotations,
            )
            .await?;
        }
        update_status(&warden_secret_api, &name, status, None).await?;
    } else {
        status = update_ready_condition(
            status,
            ApplyCondition {
                type_: ConditionType::Ready,
                status: ConditionStatus::False,
                last_transition: Utc::now().to_rfc3339(),
                message: Some("Failed to fetch item fields or attachments".to_string()),
                reason: Some("ObtainingSecretsFailed".to_string()),
            },
        );
        patch_status(&warden_secret_api, &name, &status).await?;

        if fields_result.is_err() {
            error(
                &warden_secret_api,
                &name,
                status,
                fields_result.err().unwrap(),
            )
            .await?;
        } else if attachments_result.is_err() {
            error(
                &warden_secret_api,
                &name,
                status,
                attachments_result.err().unwrap(),
            )
            .await?;
        }
        bw_client.reset();
    }
    Ok(())
}

fn add_or_update_owner_ref(owner_references: &mut Vec<OwnerReference>, owner_ref: OwnerReference) {
    for existing_owner_ref in &mut *owner_references {
        if existing_owner_ref.uid == owner_ref.uid {
            *existing_owner_ref = owner_ref;
            return;
        }
    }
    owner_references.push(owner_ref);
}

fn create_owner_ref_for(warden_secret: &Arc<WardenSecret>) -> Result<OwnerReference, Error> {
    let warden_secret_object_ref = warden_secret.object_ref(&());
    Ok(OwnerReference {
        api_version: warden_secret_object_ref.api_version.unwrap(),
        kind: warden_secret_object_ref.kind.unwrap(),
        name: warden_secret
            .metadata
            .name
            .clone()
            .ok_or(Error::UserInputError(
                format!(
                    "WardenSecret without name, uid: {}",
                    warden_secret.uid().unwrap()
                )
                .to_string(),
            ))?,
        uid: warden_secret.uid().unwrap(),
        block_owner_deletion: Some(true),
        controller: None,
    })
}

fn update_ready_condition(
    mut status: WardenSecretStatus,
    new_condition: ApplyCondition,
) -> WardenSecretStatus {
    if new_condition.type_ != ConditionType::Ready {
        panic!("Can only update Ready condition")
    }
    for i in 0..status.conditions.len() {
        if status.conditions[i].type_ == ConditionType::Ready {
            status.conditions[i] = new_condition;
            return status;
        }
    }
    status.conditions.push(new_condition);
    return status;
}

async fn error(
    warden_secret_api: &Api<WardenSecret>,
    name: &String,
    status: WardenSecretStatus,
    err: WardenCommandError,
) -> Result<(), Error> {
    error!("{}", err.to_string());
    update_status(&warden_secret_api, &name, status, Some(err)).await?;
    Ok(())
}

fn determine_action(warden_secret: &WardenSecret) -> WardenSecretAction {
    return if warden_secret.meta().deletion_timestamp.is_some() {
        WardenSecretAction::Delete
    } else if warden_secret
        .meta()
        .finalizers
        .as_ref()
        .map_or(true, |finalizers| finalizers.is_empty())
    {
        WardenSecretAction::Create
    } else if warden_secret.metadata.generation != warden_secret.get_observed_generation() {
        WardenSecretAction::Update
    } else {
        WardenSecretAction::NoOp
    };
}

pub async fn add_finalizer(
    api: &Api<WardenSecret>,
    name: &str,
) -> Result<WardenSecret, Error> {
    let finalizer: Value = json!({
        "metadata": {
            "finalizers": ["wardensecrets.tomjo.net/finalizer.secret"]
        }
    });

    let patch: Patch<&Value> = Patch::Merge(&finalizer);
    Ok(api.patch(name, &PatchParams::default(), &patch).await?)
}

pub async fn patch_status(
    warden_secret_api: &Api<WardenSecret>,
    name: &str,
    status: &WardenSecretStatus,
) -> Result<WardenSecret, Error> {
    let status_json: Value = json!({
        "apiVersion": get_api_version(),
        "kind": get_kind(),
        "status": status
    });
    let patch: Patch<&Value> = Patch::Apply(&status_json);
    let pp = PatchParams::apply("warden-secret-operator").force();
    let o = warden_secret_api.patch_status(name, &pp, &patch).await?;
    Ok(o)
}

pub async fn update_status(
    warden_secret_api: &Api<WardenSecret>,
    name: &str,
    mut status: WardenSecretStatus,
    error: Option<WardenCommandError>,
) -> Result<WardenSecret, Error> {
    for (pos, condition) in status.conditions.iter().enumerate() {
        if condition.type_ == ConditionType::Ready {
            let mut copy = condition.clone();
            if error.is_some() {
                copy.status = ConditionStatus::False;
                copy.message = Some("Could not fetch secret".to_string());
                copy.reason = error.map(|e| e.to_string())
            } else {
                copy.status = ConditionStatus::True;
                copy.message = None;
                copy.reason = None;
            }
            if copy.status != condition.status {
                copy.last_transition = Utc::now().to_rfc3339();
            }
            status.conditions[pos] = copy;
            return patch_status(warden_secret_api, name, &status).await;
        }
    }
    if error.is_some() {
        status.conditions.push(ApplyCondition {
            type_: ConditionType::Ready,
            status: ConditionStatus::False,
            last_transition: Utc::now().to_rfc3339(),
            message: Some("Could not fetch secret".to_string()),
            reason: error.map(|e| e.to_string()),
        });
        return patch_status(warden_secret_api, name, &status).await;
    }
    status.conditions.push(ApplyCondition {
        type_: ConditionType::Ready,
        status: ConditionStatus::True,
        last_transition: Utc::now().to_rfc3339(),
        message: None,
        reason: None,
    });
    return patch_status(warden_secret_api, name, &status).await;
}

pub async fn delete_finalizer(client: Client, name: &str, namespace: &str) -> Result<(), Error> {
    let api: Api<WardenSecret> = Api::namespaced(client, namespace);
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
    _labels: BTreeMap<String, String>,
    _annotations: BTreeMap<String, String>,
) -> Result<Secret, KubeError> {
    let patch_json: Value = json!({
        "metadata": {
            "ownerReferences": owner_references,
            // "annotations": annotations,
            // "labels" : labels,
        },
        "stringData": string_secrets,
        "data": secrets,
        "type": type_.to_owned(),
    });
    let patch: Patch<&Value> = Patch::Merge(&patch_json);
    secret_api
        .patch(name, &PatchParams::default(), &patch)
        .await
}

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

    secret_api.create(&PostParams::default(), &secret).await
}

pub async fn delete_secret(client: Client, name: &str, namespace: &str) -> Result<(), Error> {
    let api: Api<Secret> = Api::namespaced(client, namespace);
    let has_secret = api.get_opt(name).await?.is_some();
    if has_secret {
        api.delete(name, &DeleteParams::default()).await?;
    }
    Ok(())
}

fn on_error(
    warden_secret: Arc<WardenSecret>,
    error: &Error,
    _context: Arc<ContextData>,
) -> Action {
    error!(
        "Reconciliation error:\n{:?}.\n{:?}",
        error, warden_secret
    );
    Action::requeue(Duration::from_secs(5))
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
    #[error("Invalid WardenSecret CRD: {0}")]
    UserInputError(String),
}
