#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use]
extern crate log;

use std::borrow::ToOwned;
use std::collections::btree_map::BTreeMap;
use std::env;
use std::io::ErrorKind::AlreadyExists;
use std::sync::Arc;

use config::Config;
use const_format::formatcp;
use futures::stream::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::{
    Api, api::ListParams, client::Client, Error as KubeError, runtime::Controller, runtime::controller::Action,
};
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::Resource;
use kube::ResourceExt;
use serde_json::{json, Value};
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::Duration;

use crate::bw::BitwardenClientWrapper;
use crate::crd::BitwardenSecret;

pub mod crd;
mod bw;
// mod bitwarden;

const BW_OPERATOR_ENV_PREFIX: &'static str = "BW_OPERATOR";
const ENV_CONFIG_PATH: &'static str = formatcp!("{}_CONFIG", BW_OPERATOR_ENV_PREFIX);
const DEFAULT_CONFIG_PATH: &'static str = "config/config";

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
            add_finalizer(client.clone(), &name, &namespace).await?;

            let mutex_guard_fut = context.bw_client.lock();
            let mut bw_client: MutexGuard<BitwardenClientWrapper> = mutex_guard_fut.await;

            let result = bw_client.fetch_item(bitwarden_secret.spec.item.to_owned());
            if result.is_ok() {
                let secret_keys: BTreeMap<String, String> = result.unwrap();

                let owner_ref = OwnerReference {
                    api_version: "tomjo.net/v1".to_string(),
                    kind: "BitwardenSecret".to_string(),
                    name: name.clone(),
                    uid: bitwarden_secret.uid().expect("uid should always be set by serve"),
                    block_owner_deletion: Some(true),
                    controller: None,
                };

                let labels: BTreeMap<String, String> = bitwarden_secret.metadata.labels.clone().unwrap_or(BTreeMap::new());
                let annotations: BTreeMap<String, String> = bitwarden_secret.metadata.annotations.clone().unwrap_or(BTreeMap::new());

                let api: Api<Secret> = Api::namespaced(client.clone(), &namespace);
                let existing_secret = api.get_opt(&name).await?;
                if existing_secret.is_some() {
                    let secret = existing_secret.unwrap();
                    let mut owner_references: Vec<OwnerReference> = secret.metadata.owner_references.unwrap_or(vec![]);
                    owner_references.push(owner_ref);
                    merge_secret(client, owner_references, &name, &namespace, &bitwarden_secret.spec.type_, secret_keys, labels, annotations).await?;
                } else {
                    create_secret(client, owner_ref, &name, &namespace, &bitwarden_secret.spec.type_, secret_keys, labels, annotations).await?;
                }
            } else {
                error!("{}", result.err().unwrap().to_string());
                bw_client.reset();
            }
            Ok(Action::requeue(Duration::from_secs(10)))
        }
        BitwardenSecretAction::Delete => {
            delete_secret(client.clone(), &name, &namespace).await?;
            delete_finalizer(client, &name, &namespace).await?;
            Ok(Action::await_change())
        }
        BitwardenSecretAction::NoOp => Ok(Action::requeue(Duration::from_secs(120))),
    };
}

pub fn api_v_test<T: Resource<DynamicType=()>>(resource: &BitwardenSecret) -> String {
    return T::api_version(&()).to_string();
    // .kind(T::kind(&()))
    // .name(resource.name_any())
    // .uid_opt(resource.meta().uid.clone());
}

pub fn kind_test<T: Resource<DynamicType=()>>(resource: &BitwardenSecret) -> String {
    return T::kind(&()).to_string();
    // .kind(T::kind(&()))
    // .name(resource.name_any())
    // .uid_opt(resource.meta().uid.clone());
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
    client: Client,
    owner_references: Vec<OwnerReference>,
    name: &str,
    namespace: &str,
    type_: &str,
    secret_keys: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
) -> Result<Secret, KubeError> {
    let finalizer: Value = json!({
                "metadata": {
                    "finalizers": ["bitwardensecrets.tomjo.net/finalizer.secret"],
                    "ownerReferences": owner_references,
                },
                "stringData": secret_keys
            });
    info!("adding keys: {}", finalizer);
    let secret_api: Api<Secret> = Api::namespaced(client, namespace);

    let patch: Patch<&Value> = Patch::Merge(&finalizer);
    secret_api.patch(name, &PatchParams::default(), &patch)
        .await
}

/// TODO Note: It is assumed the resource does not already exists for simplicity. Returns an `Error` if it does.
pub async fn create_secret(
    client: Client,
    owner_ref: OwnerReference,
    name: &str,
    namespace: &str,
    type_: &str,
    secret_keys: BTreeMap<String, String>,
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
        string_data: Some(secret_keys.clone()),
        type_: Some(type_.to_owned()),
        ..Secret::default()
    };

    let secret_api: Api<Secret> = Api::namespaced(client, namespace);
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
    #[error("Invalid BitwardenSecret CRD: {0}")]
    UserInputError(String),
}
