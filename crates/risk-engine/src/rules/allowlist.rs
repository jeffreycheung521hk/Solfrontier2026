//! Allowlist and denylist rule helpers.

/// Returns `true` if all instruction programs in the summary are in the allowlist.
/// An empty allowlist means "allow everything".
pub fn all_programs_allowed(
    program_ids: &[&str],
    allowlist: &[String],
) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    program_ids.iter().all(|p| allowlist.iter().any(|a| a == p))
}

/// Returns `true` if any account pubkey is in the denylist.
pub fn any_account_denied(
    account_pubkeys: &[&str],
    denylist: &[String],
) -> bool {
    if denylist.is_empty() {
        return false;
    }
    account_pubkeys.iter().any(|p| denylist.iter().any(|d| d == p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allowlist_allows_all() {
        assert!(all_programs_allowed(&["SomeProgram111"], &[]));
    }

    #[test]
    fn allowlist_blocks_unknown() {
        let allowed = vec!["KnownProgram111".to_string()];
        assert!(!all_programs_allowed(&["UnknownProgram222"], &allowed));
    }

    #[test]
    fn denylist_blocks_known_bad() {
        let denied = vec!["BadActor111".to_string()];
        assert!(any_account_denied(&["BadActor111"], &denied));
    }
}
