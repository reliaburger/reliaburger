use super::*;

fn knobs(toml_source: &str) -> RuntimeKnobs {
    toml::from_str(toml_source).unwrap()
}

fn parse_error(toml_source: &str) -> String {
    toml::from_str::<RuntimeKnobs>(toml_source)
        .unwrap_err()
        .to_string()
}

fn allowlist(toml_source: &str) -> RuntimeAllowlist {
    crate::config::NodeConfig::parse(toml_source)
        .unwrap()
        .runtime
        .knob_allowlist()
}

// --- Parsing --------------------------------------------------------------

#[test]
fn a_full_runtime_table_parses_into_canonical_values() {
    let parsed = knobs(
        "capabilities = [\"CHECKPOINT_RESTORE\", \"cap_ipc_owner\"]\n\
         sysctls = { \"net.core.somaxconn\" = \"1024\", \"kernel.shmmax\" = \"68719476736\" }\n\
         devices = [{ path = \"/dev/fuse\" }, { path = \"/dev/net/tun\", permissions = \"rw\" }]\n",
    );
    let names: Vec<&str> = parsed.capabilities.iter().map(|c| c.name()).collect();
    assert_eq!(names, ["CAP_CHECKPOINT_RESTORE", "CAP_IPC_OWNER"]);
    assert_eq!(parsed.sysctls.len(), 2);
    assert_eq!(parsed.devices[0].permissions.to_string(), "rw");
    assert_eq!(parsed.devices[1].permissions.to_string(), "rw");
    assert!(!parsed.is_empty());
}

#[test]
fn knobs_round_trip_through_json_and_postcard() {
    let parsed = knobs(
        "capabilities = [\"CAP_IPC_OWNER\"]\nsysctls = { \"fs.mqueue.msg_max\" = \"64\" }\n\
         devices = [{ path = \"/dev/fuse\", permissions = \"r\" }]\n",
    );
    let json = serde_json::to_string(&parsed).unwrap();
    assert!(json.contains("\"CAP_IPC_OWNER\""), "{json}");
    assert_eq!(serde_json::from_str::<RuntimeKnobs>(&json).unwrap(), parsed);
    let bytes = postcard::to_allocvec(&parsed).unwrap();
    assert_eq!(
        postcard::from_bytes::<RuntimeKnobs>(&bytes).unwrap(),
        parsed
    );
}

/// #1427: the container's network namespace belongs to the node's user
/// namespace, so a network capability granted in the container's own does
/// nothing. An app can't ask for one and a node can't allow one.
#[test]
fn a_network_capability_is_refused_in_an_app_and_in_a_node() {
    for name in [
        "NET_ADMIN",
        "CAP_NET_RAW",
        "net_bind_service",
        "NET_BROADCAST",
    ] {
        let error = parse_error(&format!("capabilities = [\"{name}\"]\n"));
        assert!(error.contains("can't be granted"), "{name}: {error}");
        assert!(
            error.contains("network namespace belongs to the node"),
            "{error}"
        );
        let node = crate::config::NodeConfig::parse(&format!(
            "[runtime]\nallowed_capabilities = [\"{name}\"]\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(node.contains("can't be granted"), "{name}: {node}");
    }
}

/// #1474: these capabilities are checked against the node's own user
/// namespace, whatever namespace the caller is in, so a container can hold
/// one and still be refused. `make test-linux` runs the operation each one
/// guards inside a container's user namespace with every capability there
/// (`runc_capability_knobs_work_only_where_the_container_owns_the_check`).
#[test]
fn a_capability_checked_against_the_node_is_refused_in_an_app_and_in_a_node() {
    let refused = [
        "AUDIT_CONTROL",
        "AUDIT_READ",
        "AUDIT_WRITE",
        "BLOCK_SUSPEND",
        "BPF",
        "DAC_READ_SEARCH",
        "IPC_LOCK",
        "LEASE",
        "LINUX_IMMUTABLE",
        "MAC_ADMIN",
        "MAC_OVERRIDE",
        "MKNOD",
        "PERFMON",
        "SYSLOG",
        "SYS_MODULE",
        "SYS_NICE",
        "SYS_PACCT",
        "SYS_RAWIO",
        "SYS_RESOURCE",
        "SYS_TIME",
        "SYS_TTY_CONFIG",
        "WAKE_ALARM",
    ];
    for name in refused {
        for spelling in [name.to_string(), format!("cap_{}", name.to_lowercase())] {
            let error = parse_error(&format!("capabilities = [\"{spelling}\"]\n"));
            assert!(
                error.contains(&format!("CAP_{name} can't be granted")),
                "{spelling}: {error}"
            );
            assert!(error.contains("node's own user namespace"), "{error}");
        }
        let node = crate::config::NodeConfig::parse(&format!(
            "[runtime]\nallowed_capabilities = [\"{name}\"]\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(node.contains("can't be granted"), "{name}: {node}");
    }
    let time = parse_error("capabilities = [\"SYS_TIME\"]\n");
    assert!(time.contains("setting the clock"), "{time}");

    // What's left works on what the container's own namespaces own.
    let accepted: Vec<&str> = KNOWN_CAPABILITIES
        .into_iter()
        .filter(|name| name.parse::<Capability>().is_ok())
        .collect();
    assert_eq!(
        accepted,
        [
            "CAP_CHECKPOINT_RESTORE",
            "CAP_CHOWN",
            "CAP_DAC_OVERRIDE",
            "CAP_FOWNER",
            "CAP_FSETID",
            "CAP_IPC_OWNER",
            "CAP_KILL",
            "CAP_SETFCAP",
            "CAP_SETGID",
            "CAP_SETPCAP",
            "CAP_SETUID",
            "CAP_SYS_ADMIN",
            "CAP_SYS_BOOT",
            "CAP_SYS_CHROOT",
            "CAP_SYS_PTRACE",
        ]
    );
}

#[test]
fn an_unknown_capability_is_a_parse_error() {
    let error = parse_error("capabilities = [\"CAP_TELEPORT\"]\n");
    assert!(
        error.contains("unknown capability \"CAP_TELEPORT\""),
        "{error}"
    );
}

#[test]
fn a_capability_wildcard_is_refused_with_its_reason() {
    for wildcard in ["ALL", "all", "CAP_*"] {
        let error = parse_error(&format!("capabilities = [\"{wildcard}\"]\n"));
        assert!(error.contains("name each capability"), "{error}");
        assert!(error.contains("CAP_SYS_ADMIN"), "{error}");
    }
}

#[test]
fn every_namespaced_sysctl_family_is_accepted() {
    for name in [
        "net.core.somaxconn",
        "net.ipv4.tcp_keepalive_time",
        "net.ipv4.conf.eth0.forwarding",
        "kernel.shmmax",
        "kernel.shm_rmid_forced",
        "kernel.msgmnb",
        "kernel.sem",
        "fs.mqueue.msg_max",
    ] {
        assert!(name.parse::<SysctlName>().is_ok(), "{name}");
    }
}

#[test]
fn a_host_wide_sysctl_is_refused_and_says_why() {
    for name in [
        "kernel.panic",
        "vm.overcommit_memory",
        "fs.file-max",
        "kernel.semx",
    ] {
        let error = name.parse::<SysctlName>().unwrap_err().to_string();
        assert!(error.contains("isn't namespaced"), "{error}");
        assert!(error.contains("whole node"), "{error}");
    }
}

#[test]
fn a_malformed_sysctl_name_or_value_is_refused() {
    for name in ["", "net..core", "net/core/somaxconn", "net.core.some conn"] {
        assert!(
            matches!(
                name.parse::<SysctlName>(),
                Err(KnobError::MalformedSysctl(_))
            ),
            "{name}"
        );
    }
    for value in ["", " ", "1\n2", &"9".repeat(257)] {
        assert!(value.parse::<SysctlValue>().is_err(), "{value:?}");
    }
    assert!("0 2147483647".parse::<SysctlValue>().is_ok());
}

#[test]
fn device_paths_must_be_absolute_under_dev() {
    for good in ["/dev/fuse", "/dev/net/tun", "/dev/dri/renderD128"] {
        assert!(good.parse::<DevicePath>().is_ok(), "{good}");
    }
    for bad in [
        "dev/fuse",
        "/dev",
        "/dev/",
        "/etc/shadow",
        "/dev/../etc/shadow",
        "/dev/./fuse",
        "/devices/fuse",
    ] {
        assert!(
            matches!(
                bad.parse::<DevicePath>(),
                Err(KnobError::MalformedDevice(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn device_permissions_are_read_write_or_both() {
    assert_eq!("wr".parse::<DevicePermissions>().unwrap().to_string(), "rw");
    assert_eq!("r".parse::<DevicePermissions>().unwrap().to_string(), "r");
    for bad in ["", "rr", "x", "rwx", "RW"] {
        assert!(bad.parse::<DevicePermissions>().is_err(), "{bad}");
    }
}

/// #1442: `m` would let the app make device nodes, if the kernel let a
/// container's user namespace do that at all. It doesn't, so the
/// permission is refused rather than granted for nothing.
#[test]
fn the_mknod_device_permission_is_refused() {
    for permissions in ["m", "rwm", "mr"] {
        let error = parse_error(&format!(
            "devices = [{{ path = \"/dev/fuse\", permissions = \"{permissions}\" }}]\n"
        ));
        assert!(error.contains("mknod"), "{permissions}: {error}");
    }
}

#[test]
fn an_unknown_runtime_field_is_refused() {
    let error = parse_error("privileged = true\n");
    assert!(error.contains("privileged"), "{error}");
}

// --- The node gate --------------------------------------------------------

#[test]
fn the_node_allowlist_parses_from_its_runtime_section() {
    let allowed = allowlist(
        "[runtime]\nallowed_capabilities = [\"IPC_OWNER\"]\n\
         allowed_sysctls = [\"net.ipv4.*\", \"kernel.shmmax\"]\n\
         allowed_devices = [\"/dev/fuse\"]\n",
    );
    assert_eq!(allowed.capabilities.len(), 1);
    assert_eq!(allowed.sysctls.len(), 2);
    assert_eq!(allowed.devices.len(), 1);
    assert!(allowlist("").is_empty());
}

#[test]
fn a_node_cannot_allowlist_a_host_wide_sysctl() {
    for entry in ["kernel.panic", "kernel.*", "vm.*", "*"] {
        let error = crate::config::NodeConfig::parse(&format!(
            "[runtime]\nallowed_sysctls = [\"{entry}\"]\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("isn't namespaced")
                || error.contains("aren't namespaced")
                || error.contains("malformed"),
            "{entry}: {error}"
        );
    }
}

#[test]
fn a_node_cannot_allow_every_capability_at_once() {
    let error = crate::config::NodeConfig::parse("[runtime]\nallowed_capabilities = [\"ALL\"]\n")
        .unwrap_err()
        .to_string();
    assert!(error.contains("name each capability"), "{error}");
}

#[test]
fn the_node_gate_names_the_field_the_value_and_the_gate() {
    let allowed = allowlist("[runtime]\nallowed_capabilities = [\"CAP_CHECKPOINT_RESTORE\"]\n");
    let refusal = allowed
        .check(&knobs(
            "capabilities = [\"CHECKPOINT_RESTORE\", \"IPC_OWNER\"]\n",
        ))
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("runtime.capabilities CAP_IPC_OWNER"),
        "{refusal}"
    );
    assert!(refusal.contains("node gate"), "{refusal}");
    assert!(refusal.contains("allowed_capabilities"), "{refusal}");

    let refusal = allowed
        .check(&knobs("sysctls = { \"net.core.somaxconn\" = \"1024\" }\n"))
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("runtime.sysctls net.core.somaxconn"),
        "{refusal}"
    );
    assert!(refusal.contains("allowed_sysctls"), "{refusal}");

    let refusal = allowed
        .check(&knobs("devices = [{ path = \"/dev/fuse\" }]\n"))
        .unwrap_err()
        .to_string();
    assert!(refusal.contains("runtime.devices /dev/fuse"), "{refusal}");
    assert!(refusal.contains("allowed_devices"), "{refusal}");
}

#[test]
fn the_node_gate_admits_exactly_what_it_lists() {
    let allowed = allowlist(
        "[runtime]\nallowed_capabilities = [\"CAP_IPC_OWNER\"]\n\
         allowed_sysctls = [\"net.ipv4.*\", \"kernel.shmmax\"]\n\
         allowed_devices = [\"/dev/fuse\"]\n",
    );
    assert!(allowed.admits(&knobs(
        "capabilities = [\"IPC_OWNER\"]\n\
         sysctls = { \"net.ipv4.tcp_syncookies\" = \"1\", \"kernel.shmmax\" = \"1\" }\n\
         devices = [{ path = \"/dev/fuse\", permissions = \"rw\" }]\n"
    )));
    assert!(allowed.admits(&RuntimeKnobs::default()));
    // A prefix covers whole parts only: net.ipv6 isn't under net.ipv4.
    assert!(!allowed.admits(&knobs(
        "sysctls = { \"net.ipv6.conf.all.forwarding\" = \"1\" }\n"
    )));
    assert!(!allowed.admits(&knobs("sysctls = { \"kernel.shmall\" = \"1\" }\n")));
    assert!(!allowed.admits(&knobs("devices = [{ path = \"/dev/net/tun\" }]\n")));
}

#[test]
fn an_escape_hatch_capability_needs_the_node_to_name_it() {
    let ordinary = allowlist("[runtime]\nallowed_capabilities = [\"CAP_IPC_OWNER\"]\n");
    let wants_admin = knobs("capabilities = [\"SYS_ADMIN\"]\n");
    assert!(!ordinary.admits(&wants_admin));
    assert!(ordinary.escape_capabilities().is_empty());

    let named = allowlist("[runtime]\nallowed_capabilities = [\"CAP_SYS_ADMIN\"]\n");
    assert!(named.admits(&wants_admin));
    let escapes: Vec<&str> = named
        .escape_capabilities()
        .into_iter()
        .map(Capability::name)
        .collect();
    assert_eq!(escapes, ["CAP_SYS_ADMIN"]);
}

#[test]
fn every_escape_hatch_is_a_known_capability() {
    for name in ESCAPE_CAPABILITIES {
        assert!(KNOWN_CAPABILITIES.contains(&name), "{name}");
        assert!(name.parse::<Capability>().unwrap().is_escape_hatch());
    }
    assert!(
        !"CAP_IPC_OWNER"
            .parse::<Capability>()
            .unwrap()
            .is_escape_hatch()
    );
}

// --- OCI generation -------------------------------------------------------

fn base_spec() -> OciSpec {
    let app: crate::config::app::AppSpec = toml::from_str("image = \"redis:8\"\n").unwrap();
    crate::grill::oci::generate_oci_spec(
        "redis",
        "default",
        &app,
        "default__redis-0",
        None,
        "/sys/fs/cgroup/reliaburger/default/redis-0",
        None,
        None,
    )
}

fn fuse(path: &Path) -> Result<HostDevice, DeviceLookupError> {
    match path.to_str() {
        Some("/dev/fuse") => Ok(HostDevice {
            kind: DeviceKind::Char,
            major: 10,
            minor: 229,
            file_mode: 0o666,
            uid: 0,
            gid: 0,
        }),
        Some("/dev/sdb") => Ok(HostDevice {
            kind: DeviceKind::Block,
            major: 8,
            minor: 16,
            file_mode: 0o660,
            uid: 0,
            gid: 6,
        }),
        _ => Err(DeviceLookupError::NotADevice {
            path: path.display().to_string(),
        }),
    }
}

#[test]
fn the_app_spec_carries_its_knobs_into_the_oci_spec() {
    let app: crate::config::app::AppSpec = toml::from_str(
        "image = \"redis:8\"\n[runtime]\ncapabilities = [\"CAP_CHECKPOINT_RESTORE\"]\n",
    )
    .unwrap();
    let spec = crate::grill::oci::generate_oci_spec(
        "redis",
        "default",
        &app,
        "default__redis-0",
        None,
        "/sys/fs/cgroup/reliaburger/default/redis-0",
        None,
        None,
    );
    assert_eq!(spec.runtime_knobs, app.runtime);
    // An empty table carries nothing, so it never trips a refusal.
    let empty: crate::config::app::AppSpec =
        toml::from_str("image = \"redis:8\"\n[runtime]\n").unwrap();
    let mut spec_for_empty = base_spec();
    spec_for_empty.runtime_knobs = empty.runtime.filter(|k| !k.is_empty());
    assert_eq!(spec_for_empty.runtime_knobs, None);
    assert_eq!(base_spec().runtime_knobs, None);
}

#[test]
fn added_capabilities_join_the_user_namespace_defaults() {
    let mut spec = base_spec();
    spec.process.user.uid = 0;
    spec.process.user.gid = 0;
    crate::grill::userns::apply(&mut spec).unwrap();
    apply_to_spec(
        &mut spec,
        &knobs("capabilities = [\"IPC_OWNER\", \"CHOWN\"]\n"),
        fuse,
    )
    .unwrap();
    let capabilities = spec.process.capabilities.unwrap();
    for set in [
        &capabilities.bounding,
        &capabilities.effective,
        &capabilities.permitted,
    ] {
        assert!(set.iter().any(|c| c == "CAP_IPC_OWNER"), "{set:?}");
        assert!(set.iter().any(|c| c == "CAP_SETUID"), "{set:?}");
        assert_eq!(
            set.iter().filter(|c| *c == "CAP_CHOWN").count(),
            1,
            "a default named again appears once: {set:?}"
        );
    }
}

#[test]
fn capabilities_on_a_spec_with_none_become_the_whole_set() {
    let mut spec = base_spec();
    apply_to_spec(&mut spec, &knobs("capabilities = [\"IPC_OWNER\"]\n"), fuse).unwrap();
    assert_eq!(
        spec.process.capabilities.unwrap().effective,
        ["CAP_IPC_OWNER"]
    );
}

#[test]
fn sysctls_go_into_linux_sysctl() {
    let mut spec = base_spec();
    apply_to_spec(
        &mut spec,
        &knobs("sysctls = { \"net.core.somaxconn\" = \"1024\", \"kernel.sem\" = \"250 32000 32 128\" }\n"),
        fuse,
    )
    .unwrap();
    assert_eq!(spec.linux.sysctl["net.core.somaxconn"], "1024");
    assert_eq!(spec.linux.sysctl["kernel.sem"], "250 32000 32 128");
    let json = serde_json::to_value(&spec).unwrap();
    assert_eq!(json["linux"]["sysctl"]["kernel.sem"], "250 32000 32 128");
}

#[test]
fn network_sysctls_are_taken_out_for_the_node_to_write() {
    let mut spec = base_spec();
    apply_to_spec(
        &mut spec,
        &knobs("sysctls = { \"net.core.somaxconn\" = \"1024\", \"kernel.shmmax\" = \"1\" }\n"),
        fuse,
    )
    .unwrap();
    let network = take_network_sysctls(&mut spec);
    assert_eq!(
        network,
        BTreeMap::from([("net.core.somaxconn".to_string(), "1024".to_string())])
    );
    assert_eq!(
        spec.linux.sysctl,
        BTreeMap::from([("kernel.shmmax".to_string(), "1".to_string())])
    );
}

#[test]
fn a_device_gets_its_node_and_a_matching_cgroup_allow_rule() {
    let mut spec = base_spec();
    apply_to_spec(
        &mut spec,
        &knobs(
            "devices = [{ path = \"/dev/fuse\", permissions = \"rw\" }, { path = \"/dev/sdb\", permissions = \"r\" }]\n",
        ),
        fuse,
    )
    .unwrap();
    let json = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        json["linux"]["devices"],
        serde_json::json!([
            {"path": "/dev/fuse", "type": "c", "major": 10, "minor": 229, "fileMode": 0o666, "uid": 0, "gid": 0},
            {"path": "/dev/sdb", "type": "b", "major": 8, "minor": 16, "fileMode": 0o660, "uid": 0, "gid": 6},
        ])
    );
    assert_eq!(
        json["linux"]["resources"]["devices"],
        serde_json::json!([
            {"allow": true, "type": "c", "major": 10, "minor": 229, "access": "rw"},
            {"allow": true, "type": "b", "major": 8, "minor": 16, "access": "r"},
        ])
    );
}

#[test]
fn a_device_rule_keeps_the_specs_resource_limits() {
    let app: crate::config::app::AppSpec =
        toml::from_str("image = \"redis:8\"\nmemory = \"64Mi-128Mi\"\n").unwrap();
    let mut spec = crate::grill::oci::generate_oci_spec(
        "redis",
        "default",
        &app,
        "default__redis-0",
        None,
        "/sys/fs/cgroup/reliaburger/default/redis-0",
        None,
        None,
    );
    apply_to_spec(
        &mut spec,
        &knobs("devices = [{ path = \"/dev/fuse\" }]\n"),
        fuse,
    )
    .unwrap();
    let resources = spec.linux.resources.unwrap();
    assert!(resources.memory.is_some());
    assert_eq!(resources.devices.len(), 1);
}

#[test]
fn a_missing_device_fails_the_launch_with_its_path() {
    let mut spec = base_spec();
    let error = apply_to_spec(
        &mut spec,
        &knobs("devices = [{ path = \"/dev/nvidia0\" }]\n"),
        fuse,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("/dev/nvidia0"), "{error}");
}

#[test]
fn a_spec_without_knobs_serialises_as_before() {
    let json = serde_json::to_value(base_spec()).unwrap();
    assert!(json["linux"].get("sysctl").is_none());
    assert!(json["linux"].get("devices").is_none());
    assert!(json.get("runtime_knobs").is_none());
}

#[test]
fn linux_device_numbers_split_like_glibc() {
    // glibc's makedev(), the inverse.
    fn makedev(major: u64, minor: u64) -> u64 {
        ((major & 0xfff) << 8) | ((major & !0xfff) << 32) | (minor & 0xff) | ((minor & !0xff) << 12)
    }
    for (major, minor) in [(10, 229), (8, 300), (4097, 1), (259, 1_048_575)] {
        assert_eq!(
            split_linux_device_number(makedev(major, minor)),
            (major as i64, minor as i64)
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_regular_file_is_not_a_device() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let error = host_device(file.path()).await.unwrap_err().to_string();
    assert!(error.contains("not a character or block device"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn dev_null_reads_as_a_character_device() {
    let device = host_device(Path::new("/dev/null")).await.unwrap();
    assert_eq!(device.kind, DeviceKind::Char);
}

#[cfg(unix)]
#[tokio::test]
async fn every_requested_device_is_read_by_its_path() {
    let devices = host_devices(&knobs("devices = [{ path = \"/dev/null\" }]\n"))
        .await
        .unwrap();
    assert_eq!(devices[Path::new("/dev/null")].kind, DeviceKind::Char);
    let missing = host_devices(&knobs("devices = [{ path = \"/dev/rb-missing\" }]\n"))
        .await
        .unwrap_err()
        .to_string();
    assert!(missing.contains("/dev/rb-missing"), "{missing}");
}
