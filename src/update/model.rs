use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SUBMITTED: &str = "submitted";
pub const INSTALLING: &str = "installing";
pub const RUNNING_EXTERNAL: &str = "running_external";
pub const INSTALLED_PENDING_REBOOT: &str = "installed_pending_reboot";
pub const BOOT_CONFIRMED: &str = "boot_confirmed";
pub const FAILED: &str = "failed";
pub const UNKNOWN: &str = "unknown";

pub const EMPTY_CHECK: &str =
    "EMPTY_CHECK: updater exited successfully and complete RAUC slot evidence is unchanged";
pub const BOOT_UNKNOWN: &str = "BOOT_RESULT_UNKNOWN: the saved installation cannot be confirmed from the current boot and slot evidence";
pub const BOOT_ROLLED_BACK: &str =
    "BOOT_ROLLED_BACK: target slot is bad; the previously running slot is booted, good and primary";
pub const SUPERSEDED: &str =
    "SUPERSEDED: a newer complete installation replaced the target slot pair";

pub type Images = BTreeMap<String, String>;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub percentage: i32,
    pub message: String,
    pub depth: i32,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub operation: String,
    pub progress: Progress,
    pub last_error: String,
    pub pending: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    pub result: Option<i32>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalIdentity {
    pub pid: i32,
    pub start_time: u64,
    pub boot_id: String,
    pub exe: String,
    pub config_path: String,
    pub config_sha256: String,
}

#[derive(Clone, Debug, Default)]
pub struct ExternalRequest {
    pub source: String,
    pub request_id: String,
    pub path: String,
    pub version: String,
    pub action_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlotEvidence {
    pub name: String,
    pub class: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bootname: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub boot_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub manifest_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transaction: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub installed_count: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn vec_from_nullable<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_id: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action_id: String,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub manifest_hash: String,
    #[serde(default, skip_serializing_if = "Images::is_empty")]
    pub images: Images,
    pub boot_id: String,
    pub target_slot: String,
    pub target_bootname: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub boot_slot: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub primary: String,
    pub progress: Progress,
    pub result: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updater_exit: Option<i32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_identity: Option<ExternalIdentity>,
    #[serde(deserialize_with = "vec_from_nullable")]
    pub before: Vec<SlotEvidence>,
    #[serde(
        default,
        deserialize_with = "vec_from_nullable",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub installed: Vec<SlotEvidence>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub observed_installing: bool,
}

pub(super) fn valid_id(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._:-".contains(&c))
}

pub(super) fn valid_state(s: &str) -> bool {
    matches!(
        s,
        SUBMITTED
            | INSTALLING
            | RUNNING_EXTERNAL
            | INSTALLED_PENDING_REBOOT
            | BOOT_CONFIRMED
            | FAILED
            | UNKNOWN
    )
}

pub(super) fn resolved_boot_outcome(t: &Task) -> bool {
    t.state == UNKNOWN && t.error.starts_with("BOOT_RESULT_UNKNOWN:")
}

pub(super) fn empty_check(t: &Task) -> bool {
    t.state == UNKNOWN && t.error.starts_with("EMPTY_CHECK:")
}

pub(super) fn staging(t: &Task) -> bool {
    matches!(t.state.as_str(), RUNNING_EXTERNAL | SUBMITTED | INSTALLING)
}

pub(super) fn terminal(t: &Task) -> bool {
    matches!(
        t.state.as_str(),
        INSTALLED_PENDING_REBOOT | BOOT_CONFIRMED | FAILED
    ) || resolved_boot_outcome(t)
        || empty_check(t)
}
