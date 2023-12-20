pub mod v1;
pub mod v2;

pub use self::v2::{
    BitwardenSecret, BitwardenSecretSpec, BitwardenSecretStatus, ApplyCondition, ConditionStatus, ConditionType, get_api_version
};

pub fn get_kind() -> &'static str {
    return "BitwardenSecret";
}
