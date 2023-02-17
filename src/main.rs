#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use]
extern crate log;

use std::{env};
use std::borrow::Cow;
use std::collections::BTreeMap;

use std::sync::Arc;

use futures::stream::StreamExt;
use k8s_openapi::api::core::v1::{Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Resource;
use kube::ResourceExt;
use kube::{
    api::ListParams, client::Client, runtime::controller::Action, runtime::Controller, Api, Error as KubeError,
};
use tokio::time::Duration;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::core::DynamicObject;
use serde_json::{json, Value};

use crate::crd::BitwardenSecret;

pub mod crd;

#[tokio::main]
async fn main() {
    env::set_var("RUST_LOG", "info");
    env_logger::init();

    let kubernetes_client: Client = Client::try_default()
        .await
        .expect("Expected a valid KUBECONFIG environment variable.");

    let crd_api: Api<BitwardenSecret> = Api::all(kubernetes_client.clone());
    let context: Arc<ContextData> = Arc::new(ContextData::new(kubernetes_client.clone()));

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
}

impl ContextData {
    pub fn new(client: Client) -> Self {
        ContextData { client }
    }
}

enum BitwardenSecretAction {
    Create,
    Delete,
    NoOp,
}

async fn reconcile(bitwarden_secret: Arc<BitwardenSecret>, context: Arc<ContextData>) -> Result<Action, Error> {
    let client: Client = context.client.clone(); // The `Client` is shared -> a clone from the reference is obtained

    // The resource of `BitwardenSecret` kind is required to have a namespace set. However, it is not guaranteed
    // the resource will have a `namespace` set. Therefore, the `namespace` field on object's metadata
    // is optional and Rust forces the programmer to check for it's existence first.
    let namespace: String = match bitwarden_secret.namespace() {
        // None => {
        //     return Err(Error::UserInputError(
        //         "Expected BitwardenSecret resource to be namespaced. Can't deploy to an unknown namespace."
        //             .to_owned(),
        //     ));
        // }
        None => "default".to_string(),
        Some(namespace) => namespace,
    };

    let name = bitwarden_secret.name_any();

    return match determine_action(&bitwarden_secret) {
        BitwardenSecretAction::Create => {
            add_finalizer(client.clone(), &name, &namespace).await?;

            let mut labels: BTreeMap<String, String> = BTreeMap::new();
            labels.insert("app".to_owned(), name.to_owned());
            // TODO copy labels (all but?)

            let mut secret_keys: BTreeMap<String, String> = BTreeMap::new();
            // TODO retrieve fields/attachments from bitwarden item spec.item
            secret_keys.insert("foo".to_owned(), "bar".to_owned());



            let owner_ref = OwnerReference {
                // api_version: api_v_test(bitwarden_secret.as_ref()),
                // kind: kind_test(bitwarden_secret.as_ref()),
                api_version: "".to_string(),
                kind: "".to_string(),
                name: name.clone(),
                uid: bitwarden_secret.uid().expect(&format!("Bitwarden secret without uid: {}/{}", namespace, &name)),
                block_owner_deletion: Some(true),
                controller: None,
            };



            create_secret(client, owner_ref, &name, &namespace, &bitwarden_secret.spec.type_, secret_keys, labels).await?;
            Ok(Action::requeue(Duration::from_secs(10)))
        }
        BitwardenSecretAction::Delete => {
            delete_secret(client.clone(), &name, &namespace).await?;
            delete_finalizer(client, &name, &namespace).await?;
            Ok(Action::await_change())
        }
        BitwardenSecretAction::NoOp => Ok(Action::requeue(Duration::from_secs(10))),
    };
}

pub fn api_v_test<T: Resource<DynamicType = ()>>(resource: &BitwardenSecret) -> String {
    return T::api_version(&()).to_string();
        // .kind(T::kind(&()))
        // .name(resource.name_any())
        // .uid_opt(resource.meta().uid.clone());
}

pub fn kind_test<T: Resource<DynamicType = ()>>(resource: &BitwardenSecret) -> String {
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

/// TODO Note: Does not check for resource's existence for simplicity.
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

/// TODO Note: It is assumed the resource does not already exists for simplicity. Returns an `Error` if it does.
pub async fn create_secret(
    client: Client,
    owner_ref: OwnerReference,
    name: &str,
    namespace: &str,
    type_: &str,
    secret_keys: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
) -> Result<Secret, KubeError> {
    let secret: Secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(labels.clone()),
            // owner_references: Some(vec![owner_ref]),
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