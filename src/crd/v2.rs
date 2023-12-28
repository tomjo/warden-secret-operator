use kube::CustomResource;
use schemars::JsonSchema;
use serde::{de, Deserialize, Serialize};
use serde_json::Value;

/// Struct corresponding to the Specification (`spec`) part of the `WardenSecret` resource, directly
/// reflects context of the `wardensecrets.tomjo.net.yaml` file to be found in this repository.
/// The `WardenSecret` struct will be generated by the `CustomResource` derive macro.
#[derive(CustomResource, Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[kube(
    group = "tomjo.net",
    version = "v2",
    kind = "WardenSecret",
    plural = "wardensecrets",
    derive = "PartialEq",
    status = "WardenSecretStatus",
    namespaced
)]
pub struct WardenSecretSpec {
    #[serde(alias = "type")]
    pub type_: String,
    pub item: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct WardenSecretStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ApplyCondition>,
    #[serde(rename = "startTime")]
    pub start_time: Option<String>,
    #[serde(rename = "observedGeneration")]
    pub observed_generation: Option<i64>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct ApplyCondition {
    /// This must default to true when in a good state
    #[serde(deserialize_with = "deserialize_status_bool")]
    pub status: ConditionStatus,
    /// Error reason type if not in a good state
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// One sentence error message if not in a good state
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// This is an extensible enum. Conditions we don't know about get thrown away.
    #[serde(rename = "type")]
    pub type_: ConditionType,
    /// When the condition was last written in a RFC 3339 format in UTC (e.g. 1996-12-19T16:39:57-08:00)
    #[serde(rename = "lastTransitionTime")]
    pub last_transition: String,
}

/// Absence of condition should be interpreted as `Unknown`.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub enum ConditionStatus {
    True,
    False,
    Unknown,
}

fn deserialize_status_bool<'de, D>(deserializer: D) -> Result<ConditionStatus, D::Error>
where
    D: de::Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::Bool(b) => ConditionStatus::from_bool(b),
        Value::String(s) => ConditionStatus::from_str(&s),
        _ => ConditionStatus::Unknown,
    })
}

impl ConditionStatus {
    pub fn from_str(s: &str) -> Self {
        match s {
            "True" => ConditionStatus::True,
            "False" => ConditionStatus::False,
            _ => ConditionStatus::Unknown,
        }
    }

    pub fn to_str(&self) -> String {
        match self {
            ConditionStatus::True => "True",
            ConditionStatus::False => "False",
            ConditionStatus::Unknown => "Unknown",
        }
        .to_string()
    }

    pub fn from_bool(b: bool) -> Self {
        if b {
            ConditionStatus::True
        } else {
            ConditionStatus::False
        }
    }
}

impl Default for ConditionStatus {
    fn default() -> Self {
        ConditionStatus::Unknown
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub enum ConditionType {
    Ready,

    /// Catch all event that we don't emit (forwards compatible on api side)
    #[serde(other, skip_serializing)]
    Unknown,
}

impl Default for ConditionType {
    fn default() -> Self {
        ConditionType::Unknown
    }
}

impl WardenSecretStatus {
    pub fn is_ready(&self) -> bool {
        for i in 0..self.conditions.len() {
            if self.conditions[i].type_ == ConditionType::Ready {
                return self.conditions[i].status == ConditionStatus::True;
            }
        }
        false
    }
}

impl WardenSecret {
    pub fn get_observed_generation(&self) -> Option<i64> {
        return self
            .status
            .as_ref()
            .map(|x| x.observed_generation)
            .flatten();
    }
}

pub fn get_api_version() -> &'static str {
    return "tomjo.net/v2";
}
