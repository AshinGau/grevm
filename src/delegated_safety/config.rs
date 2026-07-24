/// Block-scoped runtime switches for the two EIP-7702 protections.
///
/// The switches are independent because delegated CREATE changes an EOA's nonce, while the
/// balance guard handles value movement that admission filtering cannot see without execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DelegatedSafetyConfig {
    /// Rejects CREATE and CREATE2 in an EIP-7702 delegated account's execution context.
    pub forbid_delegated_create: bool,
    /// Prevents delegated execution from consuming funds needed by later transactions.
    pub reserve_delegated_balance: bool,
}

impl DelegatedSafetyConfig {
    /// Disables both protections and preserves upstream revm semantics.
    pub const fn disabled() -> Self {
        Self { forbid_delegated_create: false, reserve_delegated_balance: false }
    }

    /// Enables only the delegated CREATE/CREATE2 guard.
    pub const fn create_only() -> Self {
        Self { forbid_delegated_create: true, reserve_delegated_balance: false }
    }

    /// Enables only delegated balance protection.
    pub const fn reserve_only() -> Self {
        Self { forbid_delegated_create: false, reserve_delegated_balance: true }
    }

    /// Enables both protections.
    pub const fn enabled() -> Self {
        Self { forbid_delegated_create: true, reserve_delegated_balance: true }
    }
}
