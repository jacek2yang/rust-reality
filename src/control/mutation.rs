//! From one control operation to one complete candidate configuration.
//!
//! Every mutation here is a pure function of the live entry configuration:
//! it clones the user list, applies exactly one change, and then holds the
//! result to the same two gates a configuration file must pass — the
//! [`MAX_CONFIG_BYTES`] size bound and full semantic validation. A candidate
//! that passes is a [`ValidatedConfig`] indistinguishable from one loaded
//! from disk, so the generation store compiles and publishes it through the
//! one path every other update takes.
//!
//! Nothing here is incremental. There is no live user map to patch and no
//! side table that could disagree with the configuration the generation was
//! compiled from.

use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    config::{
        EntryConfig, MAX_CONFIG_BYTES, NodeConfig, SemanticError, ValidatedConfig,
        node::UserConfig, semantics,
    },
    crypto::{generate_short_id, generate_uuid},
};

use super::{
    handle::UserHandles,
    protocol::{
        CreateUserArgs, ErrorCode, ListShortIdsArgs, MAX_LABEL_BYTES, MAX_RETIRE, Operation,
        RotateArgs, SetEnabledArgs, ShortIdArgs, UserArgs,
    },
};

/// Width of a server-generated short ID, in bytes: the wire maximum.
const DEFAULT_SHORT_ID_BYTES: u8 = 8;

/// Attempts at drawing a short ID no other user owns before giving up. A
/// collision at 8 bytes is astronomically unlikely; at 1 byte it is not, and
/// the bound turns a crowded space into a clean `conflict`.
const SHORT_ID_DRAWS: usize = 8;

/// A control operation that could not be carried out. The live generation is
/// untouched whenever one of these is returned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlError {
    /// The failure category.
    pub code: ErrorCode,
    /// A human-oriented explanation. Never contains a UUID or key.
    pub message: String,
    /// The configuration path a validation failure names, when there is one.
    pub path: Option<String>,
}

impl ControlError {
    /// Creates an error without a configuration path.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: None,
        }
    }

    fn not_found_user() -> Self {
        Self::new(ErrorCode::NotFound, "no user has this handle")
    }
}

impl From<SemanticError> for ControlError {
    fn from(error: SemanticError) -> Self {
        Self {
            code: ErrorCode::ValidationFailed,
            message: error.message().to_owned(),
            path: Some(error.path().to_owned()),
        }
    }
}

/// A validated candidate and the operation's result payload.
#[derive(Debug)]
pub struct ControlOutcome {
    /// The complete configuration to publish.
    pub config: ValidatedConfig,
    /// The operation's result, sent once the candidate is published.
    pub result: Value,
}

/// One user as the control interface shows it: never its UUID.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserView {
    /// Stable non-secret handle.
    pub handle: String,
    /// Operator label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Routing policy name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// Whether the user may authenticate.
    pub enabled: bool,
    /// The short IDs this user owns.
    pub short_ids: Vec<String>,
}

fn view(handles: &UserHandles, user: &UserConfig) -> Result<UserView, ControlError> {
    let handle = handles
        .handle(&user.id)
        .ok_or_else(|| ControlError::new(ErrorCode::Internal, "a configured user has no handle"))?;
    Ok(UserView {
        handle,
        label: user.label.clone(),
        policy: user.policy.clone(),
        enabled: user.enabled(),
        short_ids: user.short_ids.clone(),
    })
}

fn to_value(value: &impl Serialize) -> Result<Value, ControlError> {
    serde_json::to_value(value)
        .map_err(|_| ControlError::new(ErrorCode::Internal, "result encoding failed"))
}

fn position(
    entry: &EntryConfig,
    handles: &UserHandles,
    handle: &str,
) -> Result<usize, ControlError> {
    if !UserHandles::is_handle(handle) {
        return Err(ControlError::new(
            ErrorCode::InvalidArgument,
            "`user` must be a user handle (`u_` followed by 32 lowercase hexadecimal characters)",
        ));
    }
    entry
        .users
        .iter()
        .position(|user| handles.handle(&user.id).as_deref() == Some(handle))
        .ok_or_else(ControlError::not_found_user)
}

fn short_id_owner(entry: &EntryConfig, short_id: &str) -> Option<usize> {
    entry.users.iter().position(|user| {
        user.short_ids
            .iter()
            .any(|owned| owned.eq_ignore_ascii_case(short_id))
    })
}

/// Answers a read-only operation from the live configuration.
///
/// # Errors
///
/// Returns `notFound` for an unknown handle, `invalidArgument` for a
/// malformed one, and `internal` for an operation that is not a read.
pub fn read(
    entry: &EntryConfig,
    handles: &UserHandles,
    operation: &Operation,
) -> Result<Value, ControlError> {
    match operation {
        Operation::UsersList => {
            let users = entry
                .users
                .iter()
                .map(|user| view(handles, user))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "users": to_value(&users)? }))
        }
        Operation::UsersGet(UserArgs { user }) => {
            let index = position(entry, handles, user)?;
            Ok(json!({ "user": to_value(&view(handles, &entry.users[index])?)? }))
        }
        Operation::ShortIdsList(ListShortIdsArgs { user }) => {
            let selected: Vec<&UserConfig> = match user {
                Some(handle) => vec![&entry.users[position(entry, handles, handle)?]],
                None => entry.users.iter().collect(),
            };
            let mut short_ids = Vec::new();
            for user in selected {
                let handle = view(handles, user)?.handle;
                for short_id in &user.short_ids {
                    short_ids.push(json!({
                        "shortId": short_id,
                        "user": handle,
                        "enabled": user.enabled(),
                    }));
                }
            }
            Ok(json!({ "shortIds": short_ids }))
        }
        _ => Err(ControlError::new(
            ErrorCode::Internal,
            "operation is not a read",
        )),
    }
}

/// Derives the candidate configuration one mutation describes.
///
/// # Errors
///
/// Returns `notFound`, `conflict`, `invalidArgument`, or `validationFailed`
/// for a change that cannot be made, and `internal` when the operation is not
/// a mutation or the operating system cannot supply randomness.
pub fn mutate(
    entry: &EntryConfig,
    handles: &UserHandles,
    operation: &Operation,
) -> Result<ControlOutcome, ControlError> {
    let mut candidate = entry.clone();
    let result = match operation {
        Operation::UsersCreate(args) => {
            let id = create(&mut candidate, args)?;
            // Validate before deriving the handle: an invalid requested UUID
            // must be reported by path, not as a missing handle.
            let config = finish(candidate)?;
            let created = config
                .node()
                .as_entry()
                .and_then(|entry| entry.users.last())
                .ok_or_else(|| ControlError::new(ErrorCode::Internal, "the new user vanished"))?;
            let result = json!({ "user": to_value(&view(handles, created)?)?, "id": id });
            return Ok(ControlOutcome { config, result });
        }
        Operation::UsersSetEnabled(SetEnabledArgs { user, enabled }) => {
            let index = position(&candidate, handles, user)?;
            // Enabled is the default, so enabling removes the field rather
            // than writing the default into the configuration.
            candidate.users[index].enabled = (!enabled).then_some(false);
            json!({ "user": to_value(&view(handles, &candidate.users[index])?)? })
        }
        Operation::UsersDelete(UserArgs { user }) => {
            let index = position(&candidate, handles, user)?;
            let removed = candidate.users.remove(index);
            json!({ "handle": view(handles, &removed)?.handle })
        }
        Operation::ShortIdsAdd(ShortIdArgs { user, short_id }) => {
            let index = position(&candidate, handles, user)?;
            if short_id_owner(&candidate, short_id).is_some() {
                return Err(ControlError::new(
                    ErrorCode::Conflict,
                    "this short ID is already owned by a user",
                ));
            }
            candidate.users[index]
                .short_ids
                .push(short_id.to_ascii_lowercase());
            json!({ "user": to_value(&view(handles, &candidate.users[index])?)? })
        }
        Operation::ShortIdsRemove(ShortIdArgs { user, short_id }) => {
            let index = position(&candidate, handles, user)?;
            remove_short_id(&mut candidate.users[index], short_id)?;
            json!({ "user": to_value(&view(handles, &candidate.users[index])?)? })
        }
        Operation::ShortIdsRotate(args) => rotate(&mut candidate, handles, args)?,
        _ => {
            return Err(ControlError::new(
                ErrorCode::Internal,
                "operation is not a mutation",
            ));
        }
    };
    Ok(ControlOutcome {
        config: finish(candidate)?,
        result,
    })
}

/// Appends the requested user and returns its UUID.
fn create(candidate: &mut EntryConfig, args: &CreateUserArgs) -> Result<String, ControlError> {
    if let Some(label) = &args.label
        && (label.len() > MAX_LABEL_BYTES || label.chars().any(char::is_control))
    {
        return Err(ControlError::new(
            ErrorCode::InvalidArgument,
            format!("`label` must be at most {MAX_LABEL_BYTES} bytes without control characters"),
        ));
    }
    let id = match &args.id {
        Some(id) => id.to_ascii_lowercase(),
        None => generate_uuid()
            .map_err(|_| ControlError::new(ErrorCode::Internal, "randomness is unavailable"))?
            .hyphenated()
            .to_string(),
    };
    if candidate
        .users
        .iter()
        .any(|user| user.id.eq_ignore_ascii_case(&id))
    {
        return Err(ControlError::new(
            ErrorCode::Conflict,
            "this identity is already configured",
        ));
    }
    let short_ids = match &args.short_ids {
        Some(short_ids) => {
            for short_id in short_ids {
                if short_id_owner(candidate, short_id).is_some() {
                    return Err(ControlError::new(
                        ErrorCode::Conflict,
                        "a requested short ID is already owned by a user",
                    ));
                }
            }
            short_ids.iter().map(|id| id.to_ascii_lowercase()).collect()
        }
        None => vec![fresh_short_id(candidate, DEFAULT_SHORT_ID_BYTES, &[])?],
    };
    let user = UserConfig {
        id: id.clone(),
        short_ids,
        label: args.label.clone(),
        policy: args.policy.clone(),
        enabled: (args.enabled == Some(false)).then_some(false),
    };
    candidate.users.push(user);
    // The UUID leaves the process exactly once, in this operation's result,
    // because the client that will use it has to receive it. Listings never
    // repeat it.
    Ok(id)
}

fn remove_short_id(user: &mut UserConfig, short_id: &str) -> Result<(), ControlError> {
    let before = user.short_ids.len();
    user.short_ids
        .retain(|owned| !owned.eq_ignore_ascii_case(short_id));
    if user.short_ids.len() == before {
        return Err(ControlError::new(
            ErrorCode::NotFound,
            "this user does not own this short ID",
        ));
    }
    Ok(())
}

fn rotate(
    candidate: &mut EntryConfig,
    handles: &UserHandles,
    args: &RotateArgs,
) -> Result<Value, ControlError> {
    let RotateArgs {
        user,
        retire,
        bytes,
    } = args;
    if retire.len() > MAX_RETIRE {
        return Err(ControlError::new(
            ErrorCode::InvalidArgument,
            format!("`retire` must name at most {MAX_RETIRE} short IDs"),
        ));
    }
    let width = bytes.unwrap_or(DEFAULT_SHORT_ID_BYTES);
    if !(1..=8).contains(&width) {
        return Err(ControlError::new(
            ErrorCode::InvalidArgument,
            "`bytes` must be between 1 and 8",
        ));
    }
    let index = position(candidate, handles, user)?;
    for short_id in retire {
        remove_short_id(&mut candidate.users[index], short_id)?;
    }
    // A retired value is never re-issued by the rotation that retires it:
    // clients of the previous generation may still be presenting it.
    let fresh = fresh_short_id(candidate, width, retire)?;
    candidate.users[index].short_ids.push(fresh.clone());
    Ok(json!({
        "shortId": fresh,
        "user": to_value(&view(handles, &candidate.users[index])?)?,
    }))
}

fn fresh_short_id(
    entry: &EntryConfig,
    width: u8,
    excluded: &[String],
) -> Result<String, ControlError> {
    for _ in 0..SHORT_ID_DRAWS {
        let drawn = generate_short_id(width)
            .map_err(|_| ControlError::new(ErrorCode::Internal, "randomness is unavailable"))?;
        if short_id_owner(entry, &drawn).is_none()
            && !excluded.iter().any(|old| old.eq_ignore_ascii_case(&drawn))
        {
            return Ok(drawn);
        }
    }
    Err(ControlError::new(
        ErrorCode::Conflict,
        "no unused short ID of this width could be drawn; use a wider one",
    ))
}

/// Holds a candidate to the gates a configuration file must pass.
fn finish(candidate: EntryConfig) -> Result<ValidatedConfig, ControlError> {
    let node = NodeConfig::Entry(Box::new(candidate));
    let encoded = serde_json::to_vec(&node)
        .map_err(|_| ControlError::new(ErrorCode::Internal, "candidate encoding failed"))?;
    if encoded.len() > MAX_CONFIG_BYTES {
        return Err(ControlError::new(
            ErrorCode::ValidationFailed,
            format!(
                "the resulting configuration would exceed {MAX_CONFIG_BYTES} bytes, the limit for any configuration"
            ),
        ));
    }
    Ok(semantics::validate(node)?)
}

#[cfg(test)]
mod tests {
    use base64::prelude::{BASE64_URL_SAFE_NO_PAD, Engine as _};
    use serde_json::Value;

    use super::{ControlError, mutate, read};
    use crate::{
        config::{EntryConfig, node::fixture},
        control::{
            UserHandles,
            protocol::{
                CreateUserArgs, ErrorCode, ListShortIdsArgs, Operation, RotateArgs, SetEnabledArgs,
                ShortIdArgs, UserArgs,
            },
        },
    };

    const FIRST: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND: &str = "22222222-2222-4222-8222-222222222222";

    fn entry() -> EntryConfig {
        let json = format!(
            r#"{{
  "role": "entry",
  "listeners": [{{ "port": 443 }}],
  "reality": {{ "cover": "www.example.com:443", "privateKey": "{}" }},
  "users": [
    {{ "id": "{FIRST}", "shortIds": ["aa", "ab"], "label": "first" }},
    {{ "id": "{SECOND}", "shortIds": ["bb"], "policy": "split" }}
  ],
  "routing": {{ "default": "direct", "policies": {{ "split": {{ "default": "block" }} }} }}
}}"#,
            BASE64_URL_SAFE_NO_PAD.encode([9_u8; 32])
        );
        fixture::parsed(&json)
            .as_entry()
            .expect("the fixture is an entry node")
            .clone()
    }

    fn handles(entry: &EntryConfig) -> UserHandles {
        UserHandles::from_entry(entry).expect("fixture key derives")
    }

    fn handle_of(entry: &EntryConfig, id: &str) -> String {
        handles(entry).handle(id).expect("UUID has a handle")
    }

    fn apply(entry: &EntryConfig, operation: &Operation) -> Result<EntryConfig, ControlError> {
        mutate(entry, &handles(entry), operation).map(|outcome| {
            outcome
                .config
                .node()
                .as_entry()
                .expect("an entry candidate")
                .clone()
        })
    }

    fn code(result: Result<EntryConfig, ControlError>) -> ErrorCode {
        result.expect_err("must be refused").code
    }

    #[test]
    fn listings_never_contain_a_uuid() {
        let entry = entry();
        let users = read(&entry, &handles(&entry), &Operation::UsersList).expect("list");
        let rendered = users.to_string();
        assert!(
            !rendered.contains(FIRST) && !rendered.contains(SECOND),
            "{rendered}"
        );
        assert_eq!(users["users"].as_array().map(Vec::len), Some(2));
        assert_eq!(users["users"][0]["handle"], handle_of(&entry, FIRST));
        assert_eq!(users["users"][0]["label"], "first");
        assert_eq!(users["users"][1]["policy"], "split");
        assert_eq!(users["users"][1]["enabled"], true);

        let short_ids = read(
            &entry,
            &handles(&entry),
            &Operation::ShortIdsList(ListShortIdsArgs::default()),
        )
        .expect("list");
        assert_eq!(short_ids["shortIds"].as_array().map(Vec::len), Some(3));
        assert!(!short_ids.to_string().contains(FIRST));

        let one = read(
            &entry,
            &handles(&entry),
            &Operation::ShortIdsList(ListShortIdsArgs {
                user: Some(handle_of(&entry, SECOND)),
            }),
        )
        .expect("list one");
        assert_eq!(one["shortIds"][0]["shortId"], "bb");
    }

    #[test]
    fn get_distinguishes_malformed_and_unknown_handles() {
        let entry = entry();
        let get = |user: &str| {
            read(
                &entry,
                &handles(&entry),
                &Operation::UsersGet(UserArgs {
                    user: user.to_owned(),
                }),
            )
        };
        assert_eq!(
            get(FIRST).expect_err("uuid").code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            get(&format!("u_{}", "0".repeat(32)))
                .expect_err("unknown")
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            get(&handle_of(&entry, FIRST)).expect("known")["user"]["shortIds"][1],
            "ab"
        );
    }

    #[test]
    fn create_generates_credentials_and_returns_the_uuid_once() {
        let entry = entry();
        let outcome = mutate(
            &entry,
            &handles(&entry),
            &Operation::UsersCreate(CreateUserArgs {
                label: Some("new".to_owned()),
                ..CreateUserArgs::default()
            }),
        )
        .expect("create");
        let id = outcome.result["id"]
            .as_str()
            .expect("the new UUID")
            .to_owned();
        let created = outcome.config.node().as_entry().expect("entry").clone();
        assert_eq!(created.users.len(), 3);
        let user = &created.users[2];
        assert_eq!(user.id, id);
        assert_eq!(user.short_ids.len(), 1);
        assert_eq!(user.short_ids[0].len(), 16);
        assert_eq!(outcome.result["user"]["handle"], handle_of(&created, &id));
        assert_eq!(outcome.result["user"]["label"], "new");
        assert!(user.enabled.is_none(), "the default is not written out");

        let listed = read(&created, &handles(&created), &super::Operation::UsersList)
            .expect("list")
            .to_string();
        assert!(!listed.contains(&id), "a listing never repeats the UUID");
    }

    #[test]
    fn create_refuses_duplicates_and_invalid_input() {
        let entry = entry();
        let create = |args: CreateUserArgs| apply(&entry, &Operation::UsersCreate(args));
        assert_eq!(
            code(create(CreateUserArgs {
                id: Some(FIRST.to_ascii_uppercase()),
                ..CreateUserArgs::default()
            })),
            ErrorCode::Conflict
        );
        assert_eq!(
            code(create(CreateUserArgs {
                short_ids: Some(vec!["AA".to_owned()]),
                ..CreateUserArgs::default()
            })),
            ErrorCode::Conflict
        );
        let invalid = create(CreateUserArgs {
            id: Some("not-a-uuid".to_owned()),
            ..CreateUserArgs::default()
        })
        .expect_err("invalid uuid");
        assert_eq!(invalid.code, ErrorCode::ValidationFailed);
        assert_eq!(invalid.path.as_deref(), Some("users[2].id"));
        assert!(!invalid.message.contains("not-a-uuid"));
        assert_eq!(
            code(create(CreateUserArgs {
                short_ids: Some(vec!["xyz".to_owned()]),
                ..CreateUserArgs::default()
            })),
            ErrorCode::ValidationFailed
        );
        assert_eq!(
            code(create(CreateUserArgs {
                short_ids: Some(Vec::new()),
                ..CreateUserArgs::default()
            })),
            ErrorCode::ValidationFailed
        );
        assert_eq!(
            code(create(CreateUserArgs {
                policy: Some("missing".to_owned()),
                ..CreateUserArgs::default()
            })),
            ErrorCode::ValidationFailed
        );
        assert_eq!(
            code(create(CreateUserArgs {
                label: Some("a\nb".to_owned()),
                ..CreateUserArgs::default()
            })),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            code(create(CreateUserArgs {
                label: Some("x".repeat(129)),
                ..CreateUserArgs::default()
            })),
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn lifecycle_toggles_without_writing_the_default() {
        let entry = entry();
        let first = handle_of(&entry, FIRST);
        let disabled = apply(
            &entry,
            &Operation::UsersSetEnabled(SetEnabledArgs {
                user: first.clone(),
                enabled: false,
            }),
        )
        .expect("disable");
        assert_eq!(disabled.users[0].enabled, Some(false));
        assert_eq!(
            disabled.users[0].short_ids,
            ["aa", "ab"],
            "short IDs stay reserved"
        );

        let enabled = apply(
            &disabled,
            &Operation::UsersSetEnabled(SetEnabledArgs {
                user: first,
                enabled: true,
            }),
        )
        .expect("enable");
        assert_eq!(enabled, entry);

        let second = handle_of(&disabled, SECOND);
        assert_eq!(
            code(apply(
                &disabled,
                &Operation::UsersSetEnabled(SetEnabledArgs {
                    user: second,
                    enabled: false,
                }),
            )),
            ErrorCode::ValidationFailed,
            "the last enabled user cannot be disabled"
        );
    }

    #[test]
    fn delete_removes_the_user_and_frees_its_short_ids() {
        let entry = entry();
        let deleted = apply(
            &entry,
            &Operation::UsersDelete(UserArgs {
                user: handle_of(&entry, FIRST),
            }),
        )
        .expect("delete");
        assert_eq!(deleted.users.len(), 1);
        let reused = apply(
            &deleted,
            &Operation::ShortIdsAdd(ShortIdArgs {
                user: handle_of(&deleted, SECOND),
                short_id: "aa".to_owned(),
            }),
        )
        .expect("a freed short ID may be reused");
        assert_eq!(reused.users[0].short_ids, ["bb", "aa"]);

        let last = handle_of(&deleted, SECOND);
        assert_eq!(
            code(apply(
                &deleted,
                &Operation::UsersDelete(UserArgs { user: last })
            )),
            ErrorCode::ValidationFailed,
            "a node keeps at least one user"
        );
    }

    #[test]
    fn short_id_add_and_remove_enforce_ownership() {
        let entry = entry();
        let first = handle_of(&entry, FIRST);
        let second = handle_of(&entry, SECOND);
        let add = |user: &str, short_id: &str| {
            apply(
                &entry,
                &Operation::ShortIdsAdd(ShortIdArgs {
                    user: user.to_owned(),
                    short_id: short_id.to_owned(),
                }),
            )
        };
        let remove = |user: &str, short_id: &str| {
            apply(
                &entry,
                &Operation::ShortIdsRemove(ShortIdArgs {
                    user: user.to_owned(),
                    short_id: short_id.to_owned(),
                }),
            )
        };
        assert_eq!(
            add(&first, "CAFE").expect("add").users[0].short_ids[2],
            "cafe"
        );
        assert_eq!(code(add(&first, "BB")), ErrorCode::Conflict);
        assert_eq!(code(add(&first, "ab")), ErrorCode::Conflict);
        assert_eq!(code(add(&first, "abc")), ErrorCode::ValidationFailed);
        assert_eq!(
            remove(&first, "AB").expect("remove").users[0].short_ids,
            ["aa"]
        );
        assert_eq!(code(remove(&first, "bb")), ErrorCode::NotFound);
        assert_eq!(
            code(remove(&second, "bb")),
            ErrorCode::ValidationFailed,
            "a user keeps at least one short ID"
        );
    }

    #[test]
    fn rotation_adds_a_fresh_short_id_and_optionally_retires_old_ones() {
        let entry = entry();
        let first = handle_of(&entry, FIRST);
        let rotate = |retire: Vec<String>, bytes: Option<u8>| {
            mutate(
                &entry,
                &handles(&entry),
                &Operation::ShortIdsRotate(RotateArgs {
                    user: first.clone(),
                    retire,
                    bytes,
                }),
            )
        };

        let staged = rotate(Vec::new(), None).expect("staged rotation");
        let fresh = staged.result["shortId"]
            .as_str()
            .expect("new id")
            .to_owned();
        assert_eq!(fresh.len(), 16);
        let staged_entry = staged.config.node().as_entry().expect("entry").clone();
        assert_eq!(
            staged_entry.users[0].short_ids,
            ["aa", "ab", fresh.as_str()]
        );

        let replaced = rotate(vec!["aa".to_owned(), "ab".to_owned()], Some(4)).expect("replace");
        let fresh = replaced.result["shortId"]
            .as_str()
            .expect("new id")
            .to_owned();
        assert_eq!(fresh.len(), 8);
        assert_eq!(
            replaced.config.node().as_entry().expect("entry").users[0].short_ids,
            [fresh.as_str()]
        );
        assert_eq!(
            replaced.result["user"]["shortIds"],
            Value::from(vec![fresh.clone()])
        );

        assert_eq!(
            rotate(vec!["bb".to_owned()], None)
                .expect_err("not owned")
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            rotate(Vec::new(), Some(9)).expect_err("too wide").code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            rotate(vec!["aa".to_owned(); 17], None)
                .expect_err("too many")
                .code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn reads_and_mutations_refuse_each_others_operations() {
        let entry = entry();
        assert_eq!(
            read(&entry, &handles(&entry), &Operation::ConfigReload)
                .expect_err("not a read")
                .code,
            ErrorCode::Internal
        );
        assert_eq!(
            mutate(&entry, &handles(&entry), &Operation::UsersList)
                .expect_err("not a mutation")
                .code,
            ErrorCode::Internal
        );
    }

    #[test]
    fn a_candidate_larger_than_any_loadable_file_is_refused() {
        let mut entry = entry();
        entry.users[0].label = Some("x".repeat(crate::config::MAX_CONFIG_BYTES));
        let error = apply(
            &entry,
            &Operation::UsersSetEnabled(SetEnabledArgs {
                user: handle_of(&entry, FIRST),
                enabled: true,
            }),
        )
        .expect_err("oversized");
        assert_eq!(error.code, ErrorCode::ValidationFailed);
        assert!(error.message.contains("exceed"));
    }
}
