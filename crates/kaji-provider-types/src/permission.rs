use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    AlwaysAllow,
    AllowSession,
    AllowOnce,
    Cancel,
    DenyOnce,
    AlwaysDeny,
}

impl Permission {
    pub fn allows_execution(&self) -> bool {
        matches!(
            self,
            Permission::AlwaysAllow | Permission::AllowSession | Permission::AllowOnce
        )
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum PrincipalType {
    Extension,
    Tool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct PermissionConfirmation {
    pub principal_type: PrincipalType,
    pub permission: Permission,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_keep_their_wire_names() {
        for (permission, wire) in [
            (Permission::AlwaysAllow, "\"always_allow\""),
            (Permission::AllowSession, "\"allow_session\""),
            (Permission::AllowOnce, "\"allow_once\""),
            (Permission::Cancel, "\"cancel\""),
            (Permission::DenyOnce, "\"deny_once\""),
            (Permission::AlwaysDeny, "\"always_deny\""),
        ] {
            assert_eq!(serde_json::to_string(&permission).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<Permission>(wire).unwrap(),
                permission
            );
        }
    }

    #[test]
    fn only_the_allow_variants_execute() {
        assert!(Permission::AlwaysAllow.allows_execution());
        assert!(Permission::AllowSession.allows_execution());
        assert!(Permission::AllowOnce.allows_execution());
        assert!(!Permission::Cancel.allows_execution());
        assert!(!Permission::DenyOnce.allows_execution());
        assert!(!Permission::AlwaysDeny.allows_execution());
    }
}
