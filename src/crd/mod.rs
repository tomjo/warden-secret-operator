pub mod v1;
pub mod v2;

pub use self::v2::{
    get_api_version, ApplyCondition, WardenSecret, WardenSecretStatus,
    ConditionStatus, ConditionType,
};

pub fn get_kind() -> &'static str {
    return "WardenSecret";
}
