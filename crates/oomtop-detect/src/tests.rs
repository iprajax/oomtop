use super::*;
use oomtop_core::attribution::MatchSpec;
use oomtop_core::redact::default_allowlist;
use oomtop_core::{AttributionSignal, Markers, Measured, MemBreakdown};

fn p(pid: u32, ppid: u32, exe: &str, argv: &[&str]) -> Process {
    Process {
        id: ProcId::new(pid, 1_000 + pid as u64),
        ppid: Some(ppid),
        name: basename(exe).chars().take(32).collect(),
        exe: exe.into(),
        cmdline: argv.iter().map(|s| s.to_string()).collect(),
        uid: Some(501),
        mem: MemBreakdown {
            footprint_or_pss: Measured::exact(1 << 30, "t"),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn named(mut pr: Process, name: &str) -> Process {
    pr.name = name.into();
    pr
}

fn protect() -> ProtectContext {
    ProtectContext {
        self_uid: Some(501),
        ..Default::default()
    }
}

#[test]
fn builtin_rules_parse_compile_and_lint_clean() {
    let r = builtin_rules();
    assert!(r.rules.len() >= 60, "{} rules", r.rules.len());
    CompiledRules::compile(&r).expect("builtin rules compile");
    for rule in &r.rules {
        let id = rule
            .id
            .as_deref()
            .expect("every built-in rule has an explicit id");
        assert!(id.starts_with("builtin:"), "{id}");
    }
    let warnings = lint_rules(&r, &default_allowlist());
    assert!(warnings.is_empty(), "{warnings:#?}");
    assert_eq!(Detector::builtin().compiled().len(), r.rules.len());
}

/// (process, expected rule label or None). argv shapes are taken from real installs.
#[test]
fn rule_table() {
    let d = Detector::builtin();
    let cases: Vec<(Process, Option<&str>)> = vec![
        // --- agents
        (
            named(p(1, 0, "", &["claude", "--dangerously-skip-permissions"]), "claude.exe"),
            Some("Claude Code"),
        ),
        (
            p(2, 0, "/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe", &["claude"]),
            Some("Claude Code"),
        ),
        (
            p(3, 0, "/Users/u/.local/share/claude/versions/2.1.3", &["claude"]),
            Some("Claude Code"),
        ),
        (
            p(4, 0, "/usr/bin/node", &["node", "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"]),
            Some("Claude Code"),
        ),
        (p(5, 0, "/Applications/Claude.app/Contents/MacOS/Claude", &[]), None),
        (
            p(6, 0, "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex", &["codex", "app-server"]),
            Some("Codex"),
        ),
        (
            p(7, 0, "/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex-code-mode-host", &[]),
            None,
        ),
        (
            p(8, 0, "/usr/bin/node", &["node", "/usr/local/lib/node_modules/@openai/codex/bin/codex.js"]),
            Some("Codex"),
        ),
        (
            p(9, 0, "/usr/bin/node", &["node", "/opt/homebrew/lib/node_modules/@google/gemini-cli/dist/index.js"]),
            Some("Gemini CLI"),
        ),
        (
            p(10, 0, "/usr/bin/python3.12", &["python3", "/Users/u/.local/bin/aider", "--model", "x"]),
            Some("Aider"),
        ),
        (p(11, 0, "/usr/bin/python3", &["python3", "-m", "aider"]), Some("Aider")),
        (p(12, 0, "/usr/local/bin/goose", &["goose", "session"]), Some("Goose")),
        (p(13, 0, "/Users/u/.local/bin/cursor-agent", &["cursor-agent"]), Some("Cursor Agent")),
        // --- model servers
        (p(20, 0, "/usr/local/bin/ollama", &["/usr/local/bin/ollama", "serve"]), Some("Ollama")),
        (
            p(21, 0, "/Applications/Ollama.app/Contents/Resources/ollama", &["ollama", "runner", "--model", "/b"]),
            Some("Ollama"),
        ),
        (p(22, 0, "/usr/local/bin/ollama", &["ollama", "run", "llama3"]), None),
        (p(23, 0, "/usr/local/bin/ollama", &["ollama", "ps"]), None),
        (p(24, 0, "/opt/homebrew/bin/llama-server", &["llama-server", "-m", "x.gguf"]), Some("llama.cpp server")),
        (p(25, 0, "/Users/u/bin/mistral.llamafile", &["mistral.llamafile"]), Some("llama.cpp server")),
        (
            p(26, 0, "/x/qwen-image-studio/bin/sd-server", &["sd-server", "--listen-port", "7861"]),
            Some("sd-server"),
        ),
        (p(27, 0, "/venv/bin/python", &["python", "/venv/bin/vllm", "serve", "Qwen/Qwen3-8B"]), Some("vLLM")),
        (
            p(28, 0, "/venv/bin/python", &["python", "-m", "vllm.entrypoints.openai.api_server", "--model", "m"]),
            Some("vLLM"),
        ),
        (p(29, 0, "/venv/bin/vllm", &["vllm", "chat"]), None),
        (p(30, 0, "/venv/bin/python", &["python", "-m", "mlx_lm.server", "--port", "8080"]), Some("MLX server")),
        (p(31, 0, "/venv/bin/python", &["python", "-m", "mlx_lm", "server"]), Some("MLX server")),
        (p(32, 0, "/Applications/LM Studio.app/Contents/MacOS/LM Studio", &[]), Some("LM Studio")),
        (p(33, 0, "/venv/bin/python", &["python", "-m", "llama_cpp.server"]), Some("llama-cpp-python server")),
        (p(34, 0, "/venv/bin/python", &["python", "/src/ComfyUI/main.py", "--listen"]), Some("ComfyUI")),
        // --- sandboxes
        (p(40, 0, "/usr/bin/firecracker", &["firecracker", "--api-sock", "/tmp/fc.sock"]), Some("Firecracker microVM")),
        (p(41, 0, "/usr/bin/qemu-system-x86_64", &["qemu-system-x86_64", "-m", "4G"]), Some("QEMU VM")),
        (
            p(51, 0, "/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless", &["/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless", "-avd", "wifisonar", "-no-window"]),
            Some("Android emulator"),
        ),
        (
            p(42, 0, "/usr/bin/containerd-shim-runc-v2", &["containerd-shim-runc-v2", "-namespace", "moby"]),
            Some("container"),
        ),
        (p(43, 0, "/usr/bin/bwrap", &["bwrap", "--ro-bind", "/", "/"]), Some("bubblewrap sandbox")),
        (p(44, 0, "/usr/bin/sandbox-exec", &["sandbox-exec", "-p", "(version 1)"]), Some("Seatbelt sandbox")),
        (p(45, 0, "/usr/local/bin/runsc", &["runsc", "boot"]), Some("gVisor sandbox")),
        (p(46, 0, "/opt/homebrew/bin/limactl", &["limactl", "hostagent", "--pidfile", "x", "colima"]), Some("Lima VM")),
        (p(47, 0, "/opt/homebrew/bin/limactl", &["limactl", "list"]), None),
        (p(48, 0, "/opt/homebrew/bin/vfkit", &["vfkit", "--memory", "2048"]), Some("Podman machine")),
        (p(52, 0, "/usr/bin/firejail", &["firejail", "--private", "firefox"]), Some("firejail sandbox")),
        (p(53, 0, "/usr/bin/nsjail", &["nsjail", "-Mo", "--", "/bin/sh"]), Some("nsjail sandbox")),
        (p(54, 0, "/usr/bin/dockerd", &["/usr/bin/dockerd", "-H", "fd://"]), Some("Docker engine")),
        (p(55, 0, "/usr/bin/containerd", &["/usr/bin/containerd"]), Some("Docker engine")),
        (p(56, 0, "/usr/bin/podman", &["podman", "system", "service", "--time=0"]), Some("Podman service")),
        (p(57, 0, "/usr/bin/podman", &["podman", "ps"]), None),
        (p(58, 0, "/usr/bin/conmon", &["/usr/bin/conmon", "--api-version", "1", "-c", "abc"]), Some("container")),
        (p(59, 0, "/opt/homebrew/bin/colima", &["colima", "daemon", "start", "default"]), Some("Colima")),
        (p(69, 0, "/opt/homebrew/bin/colima", &["colima", "status"]), None),
        (p(70, 0, "/usr/bin/cloud-hypervisor", &["cloud-hypervisor", "--memory", "size=1G"]), Some("Cloud Hypervisor VM")),
        (p(71, 0, "/usr/bin/jailer", &["jailer", "--id", "vm1", "--exec-file", "/usr/bin/firecracker"]), Some("Firecracker microVM")),
        (p(72, 0, "/usr/local/bin/runsc-gofer", &["runsc-gofer"]), Some("gVisor sandbox")),
        // App-owned VMs stay in their app (SPEC §11): Docker Desktop / OrbStack are apps, not rule roots.
        (p(73, 0, "/Applications/Docker.app/Contents/MacOS/com.docker.backend", &["com.docker.backend"]), None),
        (p(74, 0, "/Applications/OrbStack.app/Contents/MacOS/OrbStack", &[]), None),
        (
            p(49, 0, "/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine", &[]),
            None,
        ),
        // --- build daemons & language servers
        (
            p(60, 0, "/usr/bin/java", &["java", "-cp", "x.jar", "org.gradle.launcher.daemon.bootstrap.GradleDaemon", "9.1"]),
            Some("GradleDaemon"),
        ),
        (
            p(61, 0, "/usr/bin/java", &["java", "-Xmx4g", "org.jetbrains.kotlin.daemon.KotlinCompileDaemon", "--daemon-runFilesPath", "x"]),
            Some("KotlinCompileDaemon"),
        ),
        (
            // real shape: comm is `java`, argv[0] is rewritten
            named(p(62, 0, "/home/u/.cache/bazel/install/embedded_tools/jdk/bin/java", &["bazel(ws)", "-Xverify:none", "-Djava.util.logging.config.file=x"]), "java"),
            Some("Bazel server"),
        ),
        (
            named(p(68, 0, "/usr/bin/java", &["java", "-jar", "/home/u/.cache/bazel/_bazel_u/install/abc/A-server.jar"]), "java"),
            Some("Bazel server"),
        ),
        (
            p(63, 0, "/usr/bin/node", &["node", "/p/node_modules/typescript/lib/tsserver.js", "--useInferredProjectPerProjectRoot"]),
            Some("tsserver"),
        ),
        (p(64, 0, "/Users/u/.cargo/bin/rust-analyzer", &["rust-analyzer"]), Some("rust-analyzer")),
        (p(65, 0, "/usr/bin/node", &["node", "/x/bin/yaml-language-server", "--stdio"]), Some("language server")),
        (p(66, 0, "/opt/homebrew/bin/sccache", &["sccache"]), Some("sccache")),
        (p(67, 0, "/usr/bin/node", &["node", "/x/pyright/langserver.index.js", "--stdio"]), Some("Pyright")),
        // --- apps & system
        (p(80, 0, "/usr/lib/firefox/firefox", &["/usr/lib/firefox/firefox"]), Some("Firefox")),
        (p(81, 0, "/Applications/Firefox.app/Contents/MacOS/firefox", &[]), None),
        (p(82, 0, "/opt/google/chrome/chrome", &["/opt/google/chrome/chrome"]), Some("Google Chrome")),
        (p(83, 0, "/usr/lib/systemd/systemd", &["/usr/lib/systemd/systemd", "--user"]), Some("systemd")),
        (p(84, 0, "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer", &["WindowServer"]), Some("WindowServer")),
        (p(85, 0, "/usr/bin/gnome-shell", &["/usr/bin/gnome-shell"]), Some("desktop shell")),
        (p(86, 0, "/bin/zsh", &["-zsh"]), None),
    ];
    let mut failures = Vec::new();
    for (proc_, want) in &cases {
        let got = d.matching_rule(proc_).map(|r| r.label.as_str());
        if got != *want {
            failures.push(format!(
                "{:?} {:?}: got {got:?}, want {want:?}",
                proc_.exe, proc_.cmdline
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn container_cgroup_rule_groups_container_processes() {
    let d = Detector::builtin();
    let shim = p(
        100,
        1,
        "/usr/bin/containerd-shim-runc-v2",
        &["containerd-shim-runc-v2"],
    );
    let mut init = p(101, 100, "/usr/local/bin/python", &["python", "app.py"]);
    init.cgroup = Some("/system.slice/docker-0123abcd.scope".into());
    let mut worker = p(102, 101, "/usr/local/bin/python", &["python", "worker.py"]);
    worker.cgroup = init.cgroup.clone();
    // rootless / podman shapes match by cgroup alone
    let mut podman = p(200, 1, "/usr/bin/nginx", &["nginx"]);
    podman.cgroup =
        Some("/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-9f9f.scope/container".into());
    let mut conmon_scope = p(201, 1, "/usr/bin/conmonx", &["conmonx"]);
    conmon_scope.cgroup = Some("/user.slice/libpod-conmon-9f9f.scope".into());
    for (pr, want) in [(&init, true), (&podman, true), (&conmon_scope, false)] {
        assert_eq!(
            d.matching_rule(pr).map(|r| r.label.as_str()) == Some("container"),
            want,
            "{:?}",
            pr.cgroup
        );
    }
    let s = Snapshot {
        processes: vec![shim, init, worker],
        ..Default::default()
    };
    let groups = d.attribute(&s, &BTreeMap::new(), &protect());
    assert_eq!(groups.len(), 1, "{groups:#?}");
    assert_eq!(groups[0].kind, GroupKind::Sandbox);
    assert_eq!(groups[0].members.len(), 3);
}

#[test]
fn detects_motivating_machine() {
    let d = Detector::builtin();
    let s = Snapshot {
        processes: vec![
            p(10, 1, "/opt/homebrew/bin/sd-server", &["sd-server", "--listen-port", "7861"]),
            p(20, 1, "/usr/bin/java", &["java", "-cp", "x.jar", "org.gradle.launcher.daemon.bootstrap.GradleDaemon", "9.8.0"]),
            p(21, 1, "/usr/bin/java", &["java", "org.jetbrains.kotlin.daemon.KotlinCompileDaemon"]),
            p(30, 1, "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", &[]),
            p(31, 30, "/Applications/Google Chrome.app/Contents/Frameworks/x/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper", &[]),
        ],
        ..Default::default()
    };
    let groups = d.attribute(&s, &BTreeMap::new(), &protect());
    let kind_of = |label: &str| {
        groups
            .iter()
            .find(|g| g.label == label)
            .map(|g| (g.kind, g.members.len()))
    };
    assert_eq!(kind_of("sd-server"), Some((GroupKind::ModelServer, 1)));
    assert_eq!(kind_of("GradleDaemon"), Some((GroupKind::BuildDaemon, 1)));
    assert_eq!(kind_of("KotlinCompileDaemon"), Some((GroupKind::BuildDaemon, 1)));
    assert_eq!(kind_of("Google Chrome"), Some((GroupKind::App, 2)));
}

/// An unchanged process set reuses the previous attribution and refreshes only the numbers; the result is
/// exactly what a full attribution gives. A new process (or a changed argv) re-attributes.
#[test]
fn unchanged_process_set_reuses_attribution_with_fresh_numbers() {
    let d = Detector::builtin();
    let mut s = Snapshot {
        processes: vec![
            p(10, 1, "/opt/homebrew/bin/sd-server", &["sd-server", "--listen-port", "7861"]),
            p(20, 1, "/usr/bin/java", &["java", "org.gradle.launcher.daemon.bootstrap.GradleDaemon", "9.8.0"]),
            p(30, 1, "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", &[]),
            p(31, 30, "/Applications/Google Chrome.app/Contents/Frameworks/x/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper", &[]),
        ],
        ..Default::default()
    };
    let first = d.attribute(&s, &BTreeMap::new(), &protect());
    // Same processes, new numbers: sd-server grows to 10 GiB, Chrome's helper shrinks.
    s.processes[0].mem.footprint_or_pss = Measured::exact(10 << 30, "t");
    s.processes[3].mem.footprint_or_pss = Measured::exact(1 << 20, "t");
    let cached = d.attribute(&s, &BTreeMap::new(), &protect());
    let fresh = Detector::builtin().attribute(&s, &BTreeMap::new(), &protect());
    assert_eq!(cached, fresh, "the reused attribution equals a full one");
    assert_ne!(cached, first);
    assert_eq!(cached[0].label, "sd-server");
    assert_eq!(cached[0].totals.footprint.value, Some(10 << 30));
    // A new process joins: attributed from scratch.
    s.processes.push(p(32, 30, "/Applications/Google Chrome.app/Contents/Frameworks/x/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper", &[]));
    let grown = d.attribute(&s, &BTreeMap::new(), &protect());
    let chrome = grown.iter().find(|g| g.label == "Google Chrome").unwrap();
    assert_eq!(chrome.members.len(), 3);
    assert_eq!(
        grown,
        Detector::builtin().attribute(&s, &BTreeMap::new(), &protect())
    );
}

#[test]
fn claude_sessions_group_by_marker_and_keep_children() {
    let d = Detector::builtin();
    let marker = |sid: &str| Markers {
        session_id: Some(sid.into()),
        agent: Some("claude-code".into()),
        keys: vec!["CLAUDECODE".into(), "CLAUDE_CODE_SESSION_ID".into()],
    };
    let term = p(
        50,
        1,
        "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
        &[],
    );
    let zsh_a = p(51, 50, "/bin/zsh", &["-zsh"]);
    let zsh_b = p(52, 50, "/bin/zsh", &["-zsh"]);
    let claude_a = named(p(60, 51, "", &["claude"]), "claude.exe");
    let claude_b = named(p(70, 52, "", &["claude"]), "claude.exe");
    let mut mcp_a = p(61, 60, "/usr/bin/node", &["node", "server.js"]);
    mcp_a.markers = marker("aaaaaaaaaaaaaaaa");
    let mut mcp_b = p(71, 70, "/usr/bin/node", &["node", "server.js"]);
    mcp_b.markers = marker("bbbbbbbbbbbbbbbb");
    // re-parented to launchd, still carries session A's marker
    let mut orphan = p(62, 1, "/usr/bin/python3", &["python3", "-m", "http.server"]);
    orphan.markers = marker("aaaaaaaaaaaaaaaa");
    let s = Snapshot {
        processes: vec![term, zsh_a, zsh_b, claude_a, claude_b, mcp_a, mcp_b, orphan],
        ..Default::default()
    };
    let groups = d.attribute(&s, &BTreeMap::new(), &protect());
    let agents: Vec<&Group> = groups
        .iter()
        .filter(|g| g.kind == GroupKind::AgentSession)
        .collect();
    assert_eq!(agents.len(), 2, "{groups:#?}");
    let a = groups.iter().find(|g| g.id == "agent:aaaaaaaaaaaa").unwrap();
    assert_eq!(a.label, "Claude Code");
    assert_eq!(a.members.len(), 3);
    assert!(a.members.iter().any(|m| m.via == AttributionSignal::Marker));
    assert_eq!(a.owner_group.as_deref(), Some("app:terminal"));
    let term_g = groups.iter().find(|g| g.id == "app:terminal").unwrap();
    assert_eq!(term_g.members.len(), 3, "terminal + two shells");
}

#[test]
fn linux_launchers_do_not_own_what_they_launch() {
    let d = Detector::builtin();
    let init = p(1, 0, "/usr/lib/systemd/systemd", &["/sbin/init"]);
    let user_mgr = named(
        p(
            900,
            1,
            "/usr/lib/systemd/systemd",
            &["/usr/lib/systemd/systemd", "--user"],
        ),
        "systemd",
    );
    let shell = p(950, 900, "/usr/bin/gnome-shell", &["/usr/bin/gnome-shell"]);
    let firefox = p(
        1000,
        950,
        "/usr/lib/firefox/firefox",
        &["/usr/lib/firefox/firefox"],
    );
    let content = p(
        1001,
        1000,
        "/usr/lib/firefox/firefox",
        &["/usr/lib/firefox/firefox", "-contentproc"],
    );
    let term = p(
        1100,
        900,
        "/usr/libexec/gnome-terminal-server",
        &["gnome-terminal-server"],
    );
    let bash = p(1101, 1100, "/usr/bin/bash", &["bash"]);
    let claude = p(
        1102,
        1101,
        "/home/u/.local/share/claude/versions/2.1.3",
        &["claude"],
    );
    // A real orphan: spawned by claude (lineage says ppid 1102), re-parented to the systemd --user subreaper.
    let orphan = p(1200, 900, "/usr/bin/node", &["node", "dev-server.js"]);
    // Always a direct child of systemd --user (lineage recorded ppid 900): not an orphan.
    let pipewire = p(1300, 900, "/usr/bin/pipewire", &["pipewire"]);
    let s = Snapshot {
        processes: vec![
            init,
            user_mgr,
            shell,
            firefox,
            content,
            term,
            bash,
            claude,
            orphan.clone(),
            pipewire.clone(),
        ],
        ..Default::default()
    };
    let mut lineage = BTreeMap::new();
    lineage.insert(
        orphan.id,
        LineageEntry {
            id: orphan.id,
            ppid: Some(1102),
            group_id: "agent:claude-x".into(),
            group_kind: GroupKind::AgentSession,
            group_label: "Claude Code".into(),
            spawned_by_agent: true,
            ..Default::default()
        },
    );
    lineage.insert(
        pipewire.id,
        LineageEntry {
            id: pipewire.id,
            ppid: Some(900),
            group_id: "system:systemd:900".into(),
            group_kind: GroupKind::System,
            group_label: "systemd".into(),
            ..Default::default()
        },
    );
    let groups = d.attribute(&s, &lineage, &protect());
    let by_label = |l: &str| groups.iter().find(|g| g.label == l).unwrap();
    // pid 1 and `systemd --user` share the protected systemd group (same rule) and own nothing else.
    let sysd: Vec<&Group> = groups.iter().filter(|g| g.label == "systemd").collect();
    assert_eq!(sysd.len(), 1, "{groups:#?}");
    assert_eq!(sysd[0].members.len(), 2);
    assert!(sysd[0].protected);
    assert!(groups
        .iter()
        .all(|g| g.owner_group.as_deref() != Some(sysd[0].id.as_str())));
    assert!(groups
        .iter()
        .all(|g| g.owner_group.as_deref() != Some("system:builtin:desktop-shell:950")));
    let ff = by_label("Firefox");
    assert_eq!((ff.kind, ff.members.len()), (GroupKind::App, 2));
    assert!(!ff.protected);
    let cc = by_label("Claude Code");
    assert_eq!(cc.members.len(), 1);
    // The journal still reaches the real orphan: it rejoins its recorded group, or (that group being gone)
    // becomes an orphan root owned by it — either way via lineage, not as a child of the launcher.
    let orphan_g = groups
        .iter()
        .find(|g| g.members.iter().any(|m| m.id == orphan.id))
        .unwrap();
    assert_eq!(
        orphan_g.members[0].via,
        AttributionSignal::Lineage,
        "{orphan_g:#?}"
    );
    assert!(orphan_g.id == "agent:claude-x" || orphan_g.owner_group.as_deref() == Some("agent:claude-x"));
    let pw = groups
        .iter()
        .find(|g| g.members.iter().any(|m| m.id == pipewire.id))
        .unwrap();
    assert_eq!(pw.members[0].via, AttributionSignal::Heuristic, "{pw:#?}");
    assert_eq!(pw.label, "pipewire");
    assert!(is_launcher(&s.processes[2]), "gnome-shell");
    assert!(
        !is_launcher(&s.processes[1]),
        "systemd --user is a subreaper, handled by core"
    );
    let shell_g = by_label("desktop shell");
    assert_eq!(shell_g.members.len(), 1, "gnome-shell owns nothing it launched");
    assert!(shell_g.protected);
}

#[test]
fn virtualization_vm_goes_to_app_or_becomes_sandbox() {
    let d = Detector::builtin();
    let vm_exe = "/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine";
    let app = p(88411, 1, "/Applications/Claude.app/Contents/MacOS/Claude", &[]);
    let mut vm = named(p(88489, 1, vm_exe, &[vm_exe]), "com.apple.Virtualization.Virtua");
    vm.responsible_pid = Some(88411);
    let s = Snapshot {
        processes: vec![app.clone(), vm.clone()],
        ..Default::default()
    };
    let groups = d.attribute(&s, &BTreeMap::new(), &protect());
    assert_eq!(groups.len(), 1, "{groups:#?}");
    assert_eq!(groups[0].label, "Claude");
    assert_eq!(groups[0].members.len(), 2);
    assert_eq!(groups[0].members[1].via, AttributionSignal::Responsible);

    // Owner unknown: sandbox, not a protected system group.
    vm.responsible_pid = None;
    let s = Snapshot {
        processes: vec![app, vm],
        ..Default::default()
    };
    let groups = d.attribute(&s, &BTreeMap::new(), &protect());
    let g = groups
        .iter()
        .find(|g| g.kind == GroupKind::Sandbox)
        .expect("vm sandbox");
    assert_eq!(g.label, ORPHAN_VM_LABEL);
    assert_eq!(g.id, "sandbox:virtualization-vm:88489");
    assert!(g.lower_bound);
    assert!(!g.protected);
    assert!(!g.fingerprint.is_empty());
}

#[test]
fn nearest_rule_root_beats_responsible_app() {
    // ChatGPT.app → codex app-server (agent rule root) → node tool; every process is "responsible" to the
    // app. The tool belongs to the Codex session, the app's own helper to the app.
    let d = Detector::builtin();
    let app = p(76193, 1, "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT", &[]);
    let mut codex = p(
        76248,
        76193,
        "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
        &["codex", "app-server"],
    );
    codex.responsible_pid = Some(76193);
    let mut tool = p(
        78995,
        76248,
        "/Applications/ChatGPT.app/Contents/Resources/cua_node/bin/node",
        &["node", "server.mjs"],
    );
    tool.responsible_pid = Some(76193);
    let mut helper = p(
        76208,
        76193,
        "/Applications/ChatGPT.app/Contents/Frameworks/Codex Framework.framework/Helpers/Codex (Service).app/Contents/MacOS/Codex (Service)",
        &[],
    );
    helper.responsible_pid = Some(76193);
    let s = Snapshot {
        processes: vec![app, codex.clone(), tool.clone(), helper.clone()],
        ..Default::default()
    };
    let groups = d.attribute(&s, &BTreeMap::new(), &protect());
    let of = |id: ProcId| {
        groups
            .iter()
            .find(|g| g.members.iter().any(|m| m.id == id))
            .unwrap()
    };
    assert_eq!(of(tool.id).id, of(codex.id).id, "{groups:#?}");
    assert_eq!(of(codex.id).kind, GroupKind::AgentSession);
    assert_eq!(of(helper.id).kind, GroupKind::App);
    assert_eq!(
        of(codex.id).owner_group.as_deref(),
        Some(of(helper.id).id.as_str())
    );
}

#[test]
fn system_rules_protect() {
    let d = Detector::builtin();
    let ws = p(
        400,
        1,
        "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer",
        &["WindowServer"],
    );
    let s = Snapshot {
        processes: vec![ws],
        ..Default::default()
    };
    let groups = d.attribute(&s, &BTreeMap::new(), &protect());
    assert_eq!(groups[0].label, "WindowServer");
    assert!(groups[0].protected);
}

#[test]
fn user_rules_dir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("10-studio.toml"),
        "[[group]]\nkind = \"model_server\"\nlabel = \"Qwen studio\"\nmatch.script = [\"**/studio.py\"]\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("20-bad.toml"), "[[group]]\nkind = \"nope\"\n").unwrap();
    std::fs::write(
        dir.path().join("30-badre.toml"),
        "[[group]]\nlabel=\"x\"\nmatch.cmdline=[\"(\"]\n",
    )
    .unwrap();
    std::fs::write(dir.path().join(".hidden.toml"), "garbage = [").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "not a rules file").unwrap();
    // Replace a built-in by id: sd-server becomes the studio's label.
    std::fs::write(
        dir.path().join("40-override.toml"),
        "[[group]]\nid = \"builtin:sd-cpp\"\nkind = \"model\"\nlabel = \"Qwen engine\"\nmatch.exe = [\"sd-server\"]\n",
    )
    .unwrap();
    let (d, errs) = Detector::with_user_rules(Some(dir.path()));
    assert_eq!(errs.len(), 2, "{errs:#?}");
    let bad = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n");
    assert!(bad.contains("20-bad.toml") && bad.contains("line"), "{bad}");
    assert!(bad.contains("30-badre.toml") && bad.contains("regex"), "{bad}");
    let studio = d.rules().rules.iter().find(|r| r.label == "Qwen studio").unwrap();
    assert_eq!(studio.priority, USER_PRIORITY_BOOST);
    assert_eq!(
        d.rules()
            .rules
            .iter()
            .filter(|r| r.rule_id() == "builtin:sd-cpp")
            .count(),
        1
    );
    let sd = p(10, 1, "/x/sd-server", &["sd-server"]);
    assert_eq!(d.matching_rule(&sd).unwrap().label, "Qwen engine");
    let studio_py = p(
        11,
        1,
        "/usr/bin/python3",
        &["python3", "/x/qwen-image-studio/studio.py"],
    );
    assert_eq!(d.matching_rule(&studio_py).unwrap().label, "Qwen studio");

    let (_, errs) = Detector::with_user_rules(Some(Path::new("/nonexistent/rules.d")));
    assert!(errs.is_empty());
    let (d0, errs) = Detector::with_user_rules(None);
    assert!(errs.is_empty());
    assert_eq!(d0.rules().rules.len(), builtin_rules().rules.len());
}

#[test]
fn oversized_rule_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let big = "# pad\n".repeat((MAX_RULE_FILE_BYTES as usize / 6) + 10);
    std::fs::write(dir.path().join("big.toml"), big).unwrap();
    let (set, errs) = load_rules_dir(dir.path());
    assert!(set.rules.is_empty());
    assert!(matches!(&errs[0], RuleLoadError::Io { msg, .. } if msg.contains("too large")));
}

#[test]
fn user_rule_wins_ties_against_builtin() {
    let user = parse_rules(
        "[[group]]\nkind = \"daemon\"\nlabel = \"My claude wrapper\"\nmatch.exe = [\"claude\"]\n",
        "t",
    )
    .unwrap();
    let d = Detector::new(merge_user_rules(builtin_rules(), user)).unwrap();
    let c = p(1, 1, "/usr/local/bin/claude", &["claude"]);
    assert_eq!(d.matching_rule(&c).unwrap().label, "My claude wrapper");
}

#[test]
fn lint_catches_unreadable_markers() {
    let set = RuleSet {
        rules: vec![
            Rule {
                id: Some("a".into()),
                kind: GroupKind::AgentSession,
                label: "A".into(),
                matcher: MatchSpec {
                    exe: vec!["a".into()],
                    env: vec!["MY_SECRET_TOKEN".into()],
                    ..Default::default()
                },
                session_key: Some("MY_SESSION".into()),
                ..Default::default()
            },
            Rule {
                id: Some("a".into()),
                label: "empty".into(),
                ..Default::default()
            },
            Rule {
                id: Some("b".into()),
                label: "B".into(),
                matcher: MatchSpec {
                    env: vec!["OOMTOP_*".into()],
                    ..Default::default()
                },
                session_key: Some("env:NOT_ALLOWED".into()),
                ..Default::default()
            },
        ],
    };
    let mut allow = default_allowlist();
    allow.push("OOMTOP_*".into());
    let w: Vec<String> = lint_rules(&set, &allow).iter().map(|w| w.to_string()).collect();
    let all = w.join("\n");
    assert!(all.contains("MY_SECRET_TOKEN"), "{all}");
    assert!(all.contains("must look like"), "{all}");
    assert!(all.contains("matches nothing"), "{all}");
    assert!(all.contains("2 rules share this id"), "{all}");
    assert!(all.contains("NOT_ALLOWED"), "{all}");
    assert_eq!(w.len(), 5, "{all}");
}

#[test]
fn every_process_lands_in_exactly_one_group() {
    let d = Detector::builtin();
    let mut procs = Vec::new();
    for i in 0..200u32 {
        let ppid = if i % 7 == 0 { 1 } else { 1000 + i / 2 };
        procs.push(p(1000 + i, ppid, "/usr/bin/node", &["node", "x.js"]));
    }
    // a cycle must not hang or drop processes
    procs.push(p(5000, 5001, "/bin/a", &["a"]));
    procs.push(p(5001, 5000, "/bin/b", &["b"]));
    let s = Snapshot {
        processes: procs,
        ..Default::default()
    };
    let groups = d.attribute(&s, &BTreeMap::new(), &protect());
    let mut seen = std::collections::HashSet::new();
    for g in &groups {
        for m in &g.members {
            assert!(seen.insert(m.id), "{:?} in two groups", m.id);
        }
    }
    assert_eq!(seen.len(), s.processes.len());
}

/// The cached attribution (same membership signature) equals a fresh one, also when `prepare` rewrites the
/// snapshot (children of a launcher are re-parented) — the cache check runs on the raw inputs and skips
/// `prepare` (SPEC §14 CPU budget).
#[test]
fn cached_attribution_matches_a_fresh_one_across_launcher_rewrites() {
    let term = p(
        50,
        1,
        "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
        &[],
    );
    let zsh = p(51, 50, "/bin/zsh", &["-zsh"]);
    let claude = named(p(60, 51, "", &["claude"]), "claude.exe");
    let tool = p(61, 60, "/usr/bin/node", &["node", "server.js"]);
    let gradle = p(
        70,
        1,
        "/usr/bin/java",
        &[
            "java",
            "org.gradle.launcher.daemon.bootstrap.GradleDaemon",
            "9.8.0",
        ],
    );
    let first = Snapshot {
        processes: vec![term, zsh, claude, tool, gradle],
        ..Default::default()
    };
    // Same membership, different numbers: footprints move, as they do every sample.
    let mut second = first.clone();
    for (i, pr) in second.processes.iter_mut().enumerate() {
        pr.mem.footprint_or_pss = Measured::exact((i as u64 + 2) << 28, "t");
    }
    let cached = Detector::builtin();
    let _ = cached.attribute(&first, &BTreeMap::new(), &protect());
    let from_cache = cached.attribute(&second, &BTreeMap::new(), &protect());
    let fresh = Detector::builtin().attribute(&second, &BTreeMap::new(), &protect());
    assert_eq!(from_cache, fresh);
    // A membership change (a new process) is noticed.
    let mut third = second.clone();
    third
        .processes
        .push(p(62, 60, "/usr/bin/python3", &["python3", "x.py"]));
    let after = cached.attribute(&third, &BTreeMap::new(), &protect());
    assert_eq!(
        after,
        Detector::builtin().attribute(&third, &BTreeMap::new(), &protect())
    );
    let agent = after.iter().find(|g| g.kind == GroupKind::AgentSession).unwrap();
    assert!(agent.members.iter().any(|m| m.id.pid == 62), "{agent:#?}");
}
