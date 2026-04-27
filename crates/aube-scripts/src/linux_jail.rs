use crate::ScriptJail;
use landlock::{
    ABI, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, RulesetStatus,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use std::collections::BTreeMap;
use std::path::Path;

fn add_rule(
    ruleset: RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<RulesetCreated, String> {
    let fd = PathFd::new(path)
        .map_err(|e| format!("failed to open jail allow path {}: {e}", path.display()))?;
    ruleset
        .add_rule(PathBeneath::new(fd, access))
        .map_err(|e| format!("failed to add jail allow path {}: {e}", path.display()))
}

fn add_rule_with_canonical(
    mut ruleset: RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<RulesetCreated, String> {
    ruleset = add_rule(ruleset, path, access)?;
    if let Ok(canonical) = path.canonicalize()
        && canonical != path
    {
        ruleset = add_rule(ruleset, &canonical, access)?;
    }
    Ok(ruleset)
}

pub(crate) fn apply_landlock(jail: &ScriptJail, home: &Path) -> Result<(), String> {
    // Must run before restrict_self() so a setuid exec inside the jail
    // cannot pick up privileges that would shadow the Landlock domain.
    // Also needed on the network: true path, where the seccomp filter
    // (which used to set this) is skipped.
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(format!(
            "failed to set PR_SET_NO_NEW_PRIVS: {}",
            std::io::Error::last_os_error()
        ));
    }
    // ABI v2 (kernel >= 5.19) covers every write-restriction this policy
    // needs and unblocks the LTS kernels that ship 5.15-6.1 (Ubuntu 22.04,
    // Debian 12, RHEL 9). v3 only adds LANDLOCK_ACCESS_FS_TRUNCATE.
    let abi = ABI::V2;
    let read_access = AccessFs::from_read(abi);
    let full_access = read_access | AccessFs::from_write(abi);
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(full_access)
        .map_err(|e| format!("failed to create jail ruleset: {e}"))?
        .create()
        .map_err(|e| format!("failed to create jail ruleset: {e}"))?;

    ruleset = add_rule(ruleset, Path::new("/"), read_access)?;
    for path in [
        Path::new("/dev"),
        jail.package_dir.as_path(),
        home,
        std::env::temp_dir().as_path(),
    ] {
        ruleset = add_rule_with_canonical(ruleset, path, full_access)?;
    }
    for path in &jail.write_paths {
        ruleset = add_rule_with_canonical(ruleset, path, full_access)?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| format!("failed to apply jail filesystem rules: {e}"))?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!(
            "jail filesystem rules were not fully enforced: {:?}",
            status.landlock
        ));
    }
    Ok(())
}

pub(crate) fn apply_seccomp_net_filter() -> Result<(), String> {
    let target_arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| format!("unsupported architecture for jail network filter: {e}"))?;
    let socket_rule_inet = SeccompRule::new(vec![
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_INET as u64,
        )
        .map_err(|e| format!("failed to build jail network filter: {e}"))?,
    ])
    .map_err(|e| format!("failed to build jail network filter: {e}"))?;
    let socket_rule_inet6 = SeccompRule::new(vec![
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_INET6 as u64,
        )
        .map_err(|e| format!("failed to build jail network filter: {e}"))?,
    ])
    .map_err(|e| format!("failed to build jail network filter: {e}"))?;

    let mut rules = BTreeMap::new();
    #[allow(clippy::useless_conversion)]
    for syscall in [libc::SYS_socket, libc::SYS_socketpair].map(i64::from) {
        rules.insert(
            syscall,
            vec![socket_rule_inet.clone(), socket_rule_inet6.clone()],
        );
    }

    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch,
    )
    .map_err(|e| format!("failed to build jail network filter: {e}"))?
    .try_into()
    .map_err(|e| format!("failed to compile jail network filter: {e}"))?;
    seccompiler::apply_filter(&filter)
        .map_err(|e| format!("failed to apply jail network filter: {e}"))?;
    Ok(())
}
