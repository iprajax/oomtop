use super::*;
use crate::prober::Caches;
use oomtop_core::{Group, GroupTotals, Member};
use std::sync::{Arc, Mutex};

fn proc(pid: u32, ppid: u32, argv: &[&str]) -> Process {
    Process {
        id: ProcId::new(pid, pid as u64),
        ppid: Some(ppid),
        name: basename(argv[0]).to_string(),
        exe: argv[0].to_string(),
        cmdline: argv.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

type Handler = dyn Fn(&str, &str) -> (u16, String) + Send + Sync;

/// A loopback mock server answering by URL; records (url, body) of every request.
struct Mock {
    port: u16,
    log: Arc<Mutex<Vec<(String, String)>>>,
    _h: std::thread::JoinHandle<()>,
}

fn mock(handler: Box<Handler>) -> Mock {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    let h = std::thread::spawn(move || {
        while let Ok(Some(mut req)) = server.recv_timeout(Duration::from_secs(5)) {
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let url = req.url().to_string();
            let (code, resp) = handler(&url, &body);
            log2.lock().unwrap().push((url, body));
            let _ = req.respond(tiny_http::Response::from_string(resp).with_status_code(code));
        }
    });
    Mock { port, log, _h: h }
}

fn live() -> ProbeOptions {
    ProbeOptions {
        timeout: Duration::from_secs(2),
        ..Default::default()
    }
}

fn run(
    s: &Snapshot,
    opts: &ProbeOptions,
    env: &HostEnv,
) -> (Vec<ModelServer>, BTreeMap<String, SourceStatus>) {
    let mut st = BTreeMap::new();
    let ms = detect(s, opts, &BTreeMap::new(), &mut Caches::default(), env, &mut st);
    (ms, st)
}

#[test]
fn argv_detection_offline() {
    let s = Snapshot {
        processes: vec![
            proc(
                10,
                1,
                &[
                    "/bin/sd-server",
                    "--listen-port",
                    "7861",
                    "--diffusion-model",
                    "/m/q.gguf",
                    "--vae=/m/v.safetensors",
                    "--llm",
                    "rel/llm.gguf",
                ],
            ),
            proc(20, 1, &["/usr/local/bin/ollama", "serve"]),
            proc(
                21,
                20,
                &[
                    "/usr/local/bin/ollama",
                    "runner",
                    "--model",
                    "/b/sha256-x",
                    "--port",
                    "5555",
                ],
            ),
            proc(22, 1, &["/usr/local/bin/ollama", "run", "llama3"]),
            proc(
                30,
                1,
                &[
                    "python3",
                    "-m",
                    "mlx_lm.server",
                    "--port",
                    "9000",
                    "--model",
                    "mlx-community/Qwen3-4B-4bit",
                ],
            ),
            proc(40, 1, &["/venv/bin/vllm", "chat"]),
            proc(
                41,
                1,
                &[
                    "/venv/bin/python",
                    "/venv/bin/vllm",
                    "serve",
                    "Qwen/Qwen3-8B",
                    "--port",
                    "8001",
                ],
            ),
            proc(
                50,
                1,
                &[
                    "/opt/homebrew/bin/llama-server",
                    "-m",
                    "/m/a.gguf",
                    "--host",
                    "192.168.1.5",
                ],
            ),
        ],
        ..Default::default()
    };
    let opts = ProbeOptions {
        http: false,
        ..Default::default()
    };
    let (ms, st) = run(&s, &opts, &HostEnv::default());
    let by = |k: ModelServerKind| ms.iter().find(|m| m.kind == k).unwrap();
    assert_eq!(ms.len(), 5, "{ms:#?}");
    let sd = by(ModelServerKind::SdCpp);
    assert_eq!(sd.endpoint.as_deref(), Some("http://127.0.0.1:7861"));
    assert_eq!(sd.models.len(), 3);
    assert!(
        sd.models.iter().all(|m| m.weights_bytes.value.is_none()),
        "offline → unavailable, never zero"
    );
    let ol = by(ModelServerKind::Ollama);
    assert_eq!(ol.pids.len(), 2, "runner joins serve; `ollama run` is a client");
    assert_eq!(ol.endpoint.as_deref(), Some("http://127.0.0.1:11434"));
    assert_eq!(
        by(ModelServerKind::Mlx).endpoint.as_deref(),
        Some("http://127.0.0.1:9000")
    );
    let v = by(ModelServerKind::Vllm);
    assert_eq!(v.pids[0].pid, 41, "`vllm chat` is a client");
    assert_eq!(v.endpoint.as_deref(), Some("http://127.0.0.1:8001"));
    let l = by(ModelServerKind::LlamaCpp);
    assert_eq!(l.endpoint, None, "non-loopback bind is never probed");
    assert!(matches!(&l.status, SourceStatus::Partial(r) if r.contains("192.168.1.5")));
    assert!(matches!(st.get("adapter.sd_cpp"), Some(SourceStatus::Partial(_))));
}

#[test]
fn relative_argv_paths_resolve_against_cwd() {
    let mut p = proc(10, 1, &["/bin/llama-server", "-m", "models/x.gguf"]);
    p.cwd = Some("/srv/llm".into());
    let s = Snapshot {
        processes: vec![p],
        ..Default::default()
    };
    let (ms, _) = run(
        &s,
        &ProbeOptions {
            http: false,
            ..Default::default()
        },
        &HostEnv::default(),
    );
    assert_eq!(ms[0].models[0].file.as_deref(), Some("/srv/llm/models/x.gguf"));
}

#[test]
fn ollama_ps_blob_mapping_and_unload() {
    const HEX: &str = "6a0746a1ec1aef3e7ec53868f220ff6e389f6f8ef87a01d77c96807de94ca2aa";
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir_all(models.join("blobs")).unwrap();
    std::fs::create_dir_all(models.join("manifests/registry.ollama.ai/library/llama3")).unwrap();
    std::fs::File::create(models.join(format!("blobs/sha256-{HEX}")))
        .unwrap()
        .set_len(4_000)
        .unwrap();
    std::fs::write(
        models.join("manifests/registry.ollama.ai/library/llama3/8b"),
        format!(r#"{{"schemaVersion":2,"layers":[{{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:{HEX}","size":4000}},{{"mediaType":"application/vnd.ollama.image.license","digest":"sha256:00"}}]}}"#),
    )
    .unwrap();
    let m = mock(Box::new(|url, _| {
        match url {
        "/api/ps" => (
            200,
            r#"{"models":[{"name":"llama3:8b","model":"llama3:8b","size":6000,"size_vram":6000,"digest":"abc","details":{"family":"llama","parameter_size":"8.0B","quantization_level":"Q4_0"},"expires_at":"2026-09-30T10:00:00Z","context_length":8192},
                          {"name":"qwen3:4b","size":3000,"size_vram":1000}]}"#
                .into(),
        ),
        "/api/generate" => (200, r#"{"done":true,"done_reason":"unload"}"#.into()),
        _ => (404, "not found".into()),
    }
    }));
    let s = Snapshot {
        processes: vec![proc(20, 1, &["/usr/local/bin/ollama", "serve"])],
        ..Default::default()
    };
    let mut opts = live();
    opts.ports.insert("ollama".into(), m.port);
    let env = HostEnv {
        ollama_models: Some(models.clone()),
        ..Default::default()
    };
    let (ms, st) = run(&s, &opts, &env);
    assert_eq!(ms[0].status, SourceStatus::Available);
    assert_eq!(st["adapter.ollama"], SourceStatus::Available);
    let l3 = &ms[0].models[0];
    assert_eq!(l3.name, "llama3:8b");
    assert_eq!(l3.device, Device::Gpu);
    assert_eq!(
        l3.weights_bytes,
        Measured::exact(4000, "ollama weights blob size")
    );
    assert_eq!(l3.kv_bytes.value, Some(2000));
    assert!(l3.file.as_deref().unwrap().ends_with(HEX));
    let q = &ms[0].models[1];
    assert_eq!(q.device, Device::Mixed);
    assert_eq!(q.weights_bytes.value, Some(3000));
    assert_eq!(q.weights_bytes.quality, oomtop_core::Quality::Estimate);

    let acts = gentle_actions(&ms[0]);
    assert_eq!(acts.len(), 2);
    assert_eq!(acts[0].describe(), "ollama: unload llama3:8b");
    execute_gentle(&acts[0], Duration::from_secs(2)).unwrap();
    let log = m.log.lock().unwrap().clone();
    let (url, body) = log.iter().find(|(u, _)| u == "/api/generate").unwrap();
    assert_eq!(url, "/api/generate");
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body, serde_json::json!({"model": "llama3:8b", "keep_alive": 0}));
    // Pure parsers.
    let tags = parse_ollama_tags(
        &serde_json::json!({"models":[{"name":"llama3:8b","size":4661224676u64,"digest":"365c0bd3c000","modified_at":"2026-09-01T00:00:00Z","details":{"family":"llama","parameter_size":"8.0B","quantization_level":"Q4_0"}}]}),
    );
    assert_eq!(tags[0].size, Some(4661224676));
    assert_eq!(tags[0].quantization_level.as_deref(), Some("Q4_0"));
    assert_eq!(parse_ollama_ps(&serde_json::json!({})).len(), 0);
}

#[test]
fn ollama_manifest_paths() {
    use std::path::PathBuf;
    let d = Path::new("/m");
    let p = |n: &str| ollama::manifest_path(d, n);
    assert_eq!(
        p("llama3"),
        Some(PathBuf::from(
            "/m/manifests/registry.ollama.ai/library/llama3/latest"
        ))
    );
    assert_eq!(
        p("user/model:q4"),
        Some(PathBuf::from("/m/manifests/registry.ollama.ai/user/model/q4"))
    );
    assert_eq!(
        p("hf.co/org/repo:Q4_K_M"),
        Some(PathBuf::from("/m/manifests/hf.co/org/repo/Q4_K_M"))
    );
    assert_eq!(p("../../etc:passwd"), None);
    assert_eq!(ollama::blob_path(d, "sha256:zz"), None);
}

fn tiny_gguf_file(dir: &Path) -> String {
    fn kv_u32(out: &mut Vec<u8>, k: &str, v: u32) {
        out.extend_from_slice(&(k.len() as u64).to_le_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    }
    let mut b = b"GGUF".to_vec();
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes());
    b.extend_from_slice(&6u64.to_le_bytes());
    let arch = "general.architecture";
    b.extend_from_slice(&(arch.len() as u64).to_le_bytes());
    b.extend_from_slice(arch.as_bytes());
    b.extend_from_slice(&8u32.to_le_bytes());
    b.extend_from_slice(&5u64.to_le_bytes());
    b.extend_from_slice(b"llama");
    kv_u32(&mut b, "llama.block_count", 32);
    kv_u32(&mut b, "llama.attention.head_count", 32);
    kv_u32(&mut b, "llama.attention.head_count_kv", 8);
    kv_u32(&mut b, "llama.embedding_length", 4096);
    kv_u32(&mut b, "llama.context_length", 8192);
    b.resize(b.len() + 10_000, 0);
    let p = dir.join("llama-3-8b.Q4_K_M.gguf");
    std::fs::write(&p, &b).unwrap();
    p.display().to_string()
}

#[test]
fn llama_cpp_props_slots_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let model = tiny_gguf_file(dir.path());
    let model2 = model.clone();
    let m = mock(Box::new(move |url, _| {
        match url {
        "/props" => (
            200,
            serde_json::json!({"default_generation_settings": {"n_ctx": 2048, "params": {}}, "total_slots": 2, "model_path": model2, "build_info": "b6000"}).to_string(),
        ),
        "/slots" => (200, r#"[{"id":0,"n_ctx":2048,"is_processing":true},{"id":1,"n_ctx":2048,"is_processing":false}]"#.into()),
        "/metrics" => (
            200,
            "# HELP x\nllamacpp:predicted_tokens_seconds 41.5\nllamacpp:requests_processing 1\nllamacpp:requests_deferred 3\n".into(),
        ),
        _ => (404, "".into()),
    }
    }));
    let port = m.port.to_string();
    let s = Snapshot {
        processes: vec![proc(
            10,
            1,
            &[
                "/opt/homebrew/bin/llama-server",
                "-m",
                &model,
                "--port",
                &port,
                "-ngl",
                "99",
                "--metrics",
            ],
        )],
        ..Default::default()
    };
    let (ms, _) = run(&s, &live(), &HostEnv::default());
    let l = &ms[0];
    assert_eq!(l.status, SourceStatus::Available, "{:?}", l.status);
    assert_eq!(l.busy, Measured::exact(true, "llama.cpp /slots (1/2 processing)"));
    assert_eq!(l.queue.value, Some(3));
    assert_eq!(l.tok_s.value, Some(41.5));
    assert_eq!(l.models.len(), 1, "argv and /props name the same file");
    let lm = &l.models[0];
    assert_eq!(lm.device, Device::Gpu);
    assert!(lm.weights_bytes.value.unwrap() > 0);
    // ctx 2048 × 2 slots = 4096 → 2 × 32 × 8 × 128 × 4096 × 2 B = 512 MiB
    assert_eq!(lm.kv_bytes.value, Some(512 * 1024 * 1024));
    assert_eq!(
        stop_guard(l),
        StopGuard::Busy("a request is running [llama.cpp /slots (1/2 processing)]".into())
    );
}

#[test]
fn llama_cpp_without_metrics_or_with_api_key() {
    let m = mock(Box::new(|url, _| match url {
        "/props" => (
            200,
            r#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1}"#.into(),
        ),
        "/slots" => (
            501,
            r#"{"error":{"code":501,"message":"This server does not support slots endpoint."}}"#.into(),
        ),
        "/metrics" => (501, "This server does not support metrics endpoint.".into()),
        _ => (404, "".into()),
    }));
    let port = m.port.to_string();
    let mut p = proc(10, 1, &["/opt/homebrew/bin/llama-server", "--port", &port]);
    p.cpu_pct = Measured::exact(0.3, "t");
    let s = Snapshot {
        processes: vec![p],
        ..Default::default()
    };
    let (ms, _) = run(&s, &live(), &HostEnv::default());
    assert!(
        matches!(&ms[0].status, SourceStatus::Partial(r) if r.contains("--metrics") && r.contains("/slots")),
        "{:?}",
        ms[0].status
    );
    assert_eq!(ms[0].busy.value, Some(false), "cpu heuristic");
    assert_eq!(ms[0].busy.quality, oomtop_core::Quality::Estimate);

    let k = mock(Box::new(|_, _| (401, r#"{"error":"Invalid API Key"}"#.into())));
    let port = k.port.to_string();
    let s = Snapshot {
        processes: vec![proc(
            11,
            1,
            &[
                "/opt/homebrew/bin/llama-server",
                "--port",
                &port,
                "--api-key",
                "sekrit",
            ],
        )],
        ..Default::default()
    };
    let (ms, _) = run(&s, &live(), &HostEnv::default());
    assert!(matches!(&ms[0].status, SourceStatus::Partial(r) if r.contains("API key")));
    let log = k.log.lock().unwrap();
    assert_eq!(log.len(), 1, "stops after 401");
}

#[test]
fn sd_cpp_jobs_and_stop_guard() {
    let m = mock(Box::new(|url, _| {
        match url {
        "/sdcpp/v1/capabilities" => (200, r#"{"model":"qwen-image","samplers":["euler"]}"#.into()),
        "/sdcpp/v1/jobs" => (
            200,
            r#"{"jobs":[{"id":"a","status":"generating","step":7,"steps":20},{"id":"b","status":"queued","queue_position":1},{"id":"c","status":"completed"}]}"#.into(),
        ),
        _ => (404, "".into()),
    }
    }));
    let port = m.port.to_string();
    let s = Snapshot {
        processes: vec![proc(
            10,
            1,
            &["/x/sd-server", "--listen-ip", "127.0.0.1", "--listen-port", &port],
        )],
        ..Default::default()
    };
    let (ms, _) = run(&s, &live(), &HostEnv::default());
    let sd = &ms[0];
    assert_eq!(sd.status, SourceStatus::Available);
    assert_eq!(sd.busy.value, Some(true));
    assert_eq!(sd.queue.value, Some(1));
    assert_eq!(sd.progress.as_ref().map(|p| (p.done, p.total)), Some((7, 20)));
    match stop_guard(sd) {
        StopGuard::Busy(r) => assert!(r.contains("generation job") && r.contains("7/20"), "{r}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn sd_cpp_without_job_list_uses_cpu() {
    let m = mock(Box::new(|url, _| match url {
        "/sdcpp/v1/capabilities" => (200, "{}".into()),
        _ => (404, "".into()),
    }));
    let port = m.port.to_string();
    let p = proc(10, 1, &["/x/sd-server", "--listen-port", &port]);
    let id = p.id;
    let s = Snapshot {
        processes: vec![p],
        groups: vec![Group {
            id: "model:sd".into(),
            members: vec![Member {
                id,
                ..Default::default()
            }],
            totals: GroupTotals {
                cpu_pct: Measured::exact(180.0, "t"),
                ..Default::default()
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let (ms, _) = run(&s, &live(), &HostEnv::default());
    let sd = &ms[0];
    assert!(matches!(&sd.status, SourceStatus::Partial(r) if r.contains("job list")));
    assert_eq!(sd.busy.value, Some(true));
    assert_eq!(sd.busy.quality, oomtop_core::Quality::Estimate);
    assert_eq!(sd.group_id.as_deref(), Some("model:sd"));
    assert!(
        matches!(stop_guard(sd), StopGuard::Busy(_)),
        "conservative: heuristic busy still refuses"
    );
    // Server down: listed, unavailable, guard unknown.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let s = Snapshot {
        processes: vec![proc(11, 1, &["/x/sd-server", "--listen-port", &port.to_string()])],
        ..Default::default()
    };
    let (ms, st) = run(&s, &live(), &HostEnv::default());
    assert!(matches!(ms[0].status, SourceStatus::Unavailable(_)));
    assert!(matches!(st["adapter.sd_cpp"], SourceStatus::Unavailable(_)));
    assert!(matches!(stop_guard(&ms[0]), StopGuard::Unknown(_)));
}

#[test]
fn vllm_metrics_and_tok_rate() {
    let n = Arc::new(Mutex::new(0u32));
    let n2 = n.clone();
    let m = mock(Box::new(move |url, _| match url {
        "/metrics" => {
            let mut c = n2.lock().unwrap();
            *c += 1;
            let tokens = if *c == 1 { 1000 } else { 1600 };
            (
                200,
                format!(
                    "vllm:num_requests_running{{engine=\"0\",model_name=\"Qwen/Qwen3-8B\"}} 2.0\nvllm:num_requests_waiting{{engine=\"0\",model_name=\"Qwen/Qwen3-8B\"}} 5.0\nvllm:kv_cache_usage_perc{{model_name=\"Qwen/Qwen3-8B\"}} 0.42\nvllm:generation_tokens_total{{model_name=\"Qwen/Qwen3-8B\"}} {tokens}\n"
                ),
            )
        }
        _ => (404, "".into()),
    }));
    let port = m.port.to_string();
    let s = Snapshot {
        processes: vec![proc(
            41,
            1,
            &[
                "/venv/bin/python",
                "-m",
                "vllm.entrypoints.openai.api_server",
                "--model",
                "Qwen/Qwen3-8B",
                "--port",
                &port,
            ],
        )],
        ..Default::default()
    };
    let mut caches = Caches::default();
    let mut st = BTreeMap::new();
    let env = HostEnv::default();
    let ms1 = detect(&s, &live(), &BTreeMap::new(), &mut caches, &env, &mut st);
    assert_eq!(ms1[0].busy.value, Some(true));
    assert_eq!(ms1[0].queue.value, Some(5));
    assert!(ms1[0].tok_s.value.is_none(), "first sample has no rate");
    assert_eq!(ms1[0].models[0].name, "Qwen/Qwen3-8B");
    assert!(ms1[0].models[0]
        .kv_bytes
        .unavailable_reason()
        .unwrap()
        .contains("42%"));
    std::thread::sleep(Duration::from_millis(300));
    let ms2 = detect(&s, &live(), &BTreeMap::new(), &mut caches, &env, &mut st);
    let r = ms2[0].tok_s.value.unwrap();
    assert!(r > 100.0 && r < 2500.0, "{r}");
}

#[test]
fn lm_studio_models_and_unload_action() {
    let m = mock(Box::new(|url, _| {
        match url {
        "/api/v0/models" => (
            200,
            r#"{"object":"list","data":[
              {"id":"qwen2.5-7b-instruct","object":"model","type":"llm","publisher":"lmstudio-community","arch":"qwen2","compatibility_type":"gguf","quantization":"Q4_K_M","state":"loaded","max_context_length":32768,"loaded_context_length":4096},
              {"id":"text-embedding-nomic","type":"embeddings","state":"not-loaded"}]}"#
                .into(),
        ),
        _ => (404, "".into()),
    }
    }));
    let mut opts = live();
    opts.ports.insert("lm_studio".into(), m.port);
    let s = Snapshot {
        processes: vec![proc(
            70,
            1,
            &["/Applications/LM Studio.app/Contents/MacOS/LM Studio"],
        )],
        ..Default::default()
    };
    let (ms, _) = run(&s, &opts, &HostEnv::default());
    assert_eq!(ms[0].kind, ModelServerKind::LmStudio);
    assert_eq!(ms[0].status, SourceStatus::Available);
    assert_eq!(ms[0].models.len(), 1, "only loaded models");
    assert_eq!(ms[0].models[0].name, "qwen2.5-7b-instruct");
    let acts = gentle_actions(&ms[0]);
    assert_eq!(
        acts,
        vec![GentleAction::LmsUnload {
            model: "qwen2.5-7b-instruct".into()
        }]
    );
    assert_eq!(acts[0].describe(), "lms unload qwen2.5-7b-instruct");
    assert!(!lm_studio::valid_model_id("--all"));
    assert!(!lm_studio::valid_model_id("a\nb"));
    assert!(matches!(
        execute_gentle(
            &GentleAction::LmsUnload { model: "-rf".into() },
            Duration::from_millis(10)
        ),
        Err(AdapterError::Decode(_))
    ));
    // App running, local server off → partial, not an error.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut opts = live();
    opts.ports.insert("lm_studio".into(), port);
    let (ms, _) = run(&s, &opts, &HostEnv::default());
    assert!(
        matches!(&ms[0].status, SourceStatus::Partial(r) if r.contains("lms server start")),
        "{:?}",
        ms[0].status
    );
}

#[cfg(unix)]
#[test]
fn child_command_timeout_and_errors() {
    use std::process::Command;
    assert!(lm_studio::run_with_timeout(
        Command::new("/bin/sh").args(["-c", "exit 0"]),
        Duration::from_secs(5)
    )
    .is_ok());
    let e = lm_studio::run_with_timeout(
        Command::new("/bin/sh").args(["-c", "echo nope >&2; exit 3"]),
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(e.to_string().contains("nope"), "{e}");
    let t = std::time::Instant::now();
    assert_eq!(
        lm_studio::run_with_timeout(Command::new("/bin/sleep").arg("5"), Duration::from_millis(100)),
        Err(AdapterError::Timeout)
    );
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn stop_guard_states() {
    let mut ms = ModelServer::default();
    assert!(matches!(stop_guard(&ms), StopGuard::Unknown(_)));
    ms.busy = Measured::exact(false, "t");
    assert_eq!(stop_guard(&ms), StopGuard::Allowed);
    ms.queue = Measured::exact(2, "q");
    assert!(matches!(stop_guard(&ms), StopGuard::Busy(r) if r.contains("2 queued")));
}

#[test]
fn classify_table() {
    let c = |argv: &[&str]| classify(&proc(1, 0, argv));
    assert_eq!(c(&["/x/ollama", "serve"]), Some(ModelServerKind::Ollama));
    assert_eq!(c(&["/x/ollama", "list"]), None);
    assert_eq!(c(&["/x/llamafile", "-m", "x"]), Some(ModelServerKind::LlamaCpp));
    assert_eq!(c(&["/x/mistral-7b.llamafile"]), Some(ModelServerKind::LlamaCpp));
    assert_eq!(
        c(&["python", "-m", "llama_cpp.server"]),
        Some(ModelServerKind::Generic)
    );
    assert_eq!(
        c(&["/x/python", "/src/ComfyUI/main.py"]),
        Some(ModelServerKind::Generic)
    );
    assert_eq!(
        c(&["/x/mlx_lm.server", "--model", "m"]),
        Some(ModelServerKind::Mlx)
    );
    assert_eq!(c(&["/usr/bin/node", "server.js"]), None);
    assert_eq!(
        argv_model_name(&["vllm".into(), "serve".into(), "Qwen/Qwen3-8B".into()]),
        Some("Qwen/Qwen3-8B".into())
    );
}

/// Without an API answer, `models` holds argv guesses (Ollama blob hashes): never offer them for unload.
#[test]
fn gentle_actions_need_an_api_answer() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let blob = format!("/Users/u/.ollama/models/blobs/sha256-{}", "ab".repeat(32));
    let s = Snapshot {
        processes: vec![proc(20, 1, &["/usr/local/bin/ollama", "serve"]), {
            // Live, the blob arrives through the mapped-file scan of the runner.
            let mut r = proc(21, 20, &["/usr/local/bin/ollama", "runner", "--model", &blob]);
            r.model_files = vec![blob.clone()];
            r
        }],
        ..Default::default()
    };
    let mut opts = live();
    opts.ports.insert("ollama".into(), port);
    let (ms, _) = run(&s, &opts, &HostEnv::default());
    assert_eq!(ms.len(), 1, "runner folds into its server");
    assert!(matches!(ms[0].status, SourceStatus::Unavailable(_)));
    assert!(!ms[0].models.is_empty(), "the mapped blob is still listed");
    assert!(gentle_actions(&ms[0]).is_empty(), "{:?}", gentle_actions(&ms[0]));
    // Same for LM Studio with argv/mapped guesses.
    let lms = ModelServer {
        kind: ModelServerKind::LmStudio,
        status: SourceStatus::Partial("local server not running".into()),
        models: vec![LoadedModel {
            name: "qwen3-q4.gguf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(gentle_actions(&lms).is_empty());
}

/// A CPU heuristic saying "idle" does not allow a stop: GPU-bound jobs can idle the CPU.
#[test]
fn heuristic_idle_is_not_confirmation() {
    let mut ms = ModelServer {
        kind: ModelServerKind::SdCpp,
        busy: Measured::estimate(false, "cpu 2% (heuristic ≥ 10%)"),
        ..Default::default()
    };
    match stop_guard(&ms) {
        StopGuard::Unknown(r) => assert!(r.contains("generation job") && r.contains("heuristic"), "{r}"),
        other => panic!("{other:?}"),
    }
    ms.busy = Measured::exact(false, "sd-server /sdcpp/v1/jobs");
    assert_eq!(stop_guard(&ms), StopGuard::Allowed);
    // End to end: sd-server without a job list and a quiet CPU.
    let m = mock(Box::new(|url, _| match url {
        "/sdcpp/v1/capabilities" => (200, "{}".into()),
        _ => (404, "".into()),
    }));
    let p = {
        let mut p = proc(10, 1, &["/x/sd-server", "--listen-port", &m.port.to_string()]);
        p.cpu_pct = Measured::exact(1.0, "t");
        p
    };
    let s = Snapshot {
        processes: vec![p],
        ..Default::default()
    };
    let (ms, _) = run(&s, &live(), &HostEnv::default());
    assert_eq!(ms[0].busy.value, Some(false));
    assert!(matches!(stop_guard(&ms[0]), StopGuard::Unknown(_)));
}

#[test]
fn ipv6_loopback_hosts_are_probed_on_v6() {
    let opts = ProbeOptions::default();
    let p = proc(1, 0, &["/x/llama-server", "--host", "::1", "--port", "9000"]);
    assert_eq!(
        endpoint_of(&p, ModelServerKind::LlamaCpp, &opts).as_deref(),
        Some("http://[::1]:9000")
    );
    let p = proc(1, 0, &["/x/llama-server", "--host", "0.0.0.0"]);
    assert_eq!(
        endpoint_of(&p, ModelServerKind::LlamaCpp, &opts).as_deref(),
        Some("http://127.0.0.1:8080")
    );
    let p = proc(1, 0, &["/x/llama-server", "--host", "192.168.1.5"]);
    assert_eq!(endpoint_of(&p, ModelServerKind::LlamaCpp, &opts), None);
    for ep in ["http://[::1]:9000", "http://127.0.0.1:8080"] {
        assert!(crate::ensure_loopback(ep).is_ok());
    }
}

/// tok/s is the generation speed since the last probe (counter deltas), not the lifetime average.
#[test]
fn llama_cpp_tok_s_from_counter_deltas() {
    let n = Arc::new(Mutex::new(0u32));
    let n2 = n.clone();
    let m = mock(Box::new(move |url, _| match url {
        "/props" => (200, r#"{"total_slots":1}"#.into()),
        "/slots" => (200, "[]".into()),
        "/metrics" => {
            let mut c = n2.lock().unwrap();
            *c += 1;
            // 1st: 1000 tokens / 50 s; 2nd: +300 tokens in +10 s → 30 tok/s; 3rd: idle.
            let (t, secs) = match *c {
                1 => (1000, 50.0),
                _ => (1300, 60.0),
            };
            (
                200,
                format!(
                    "llamacpp:predicted_tokens_seconds 20.0\nllamacpp:tokens_predicted_total {t}\nllamacpp:tokens_predicted_seconds_total {secs}\n"
                ),
            )
        }
        _ => (404, "".into()),
    }));
    let s = Snapshot {
        processes: vec![proc(
            10,
            1,
            &["/x/llama-server", "--port", &m.port.to_string(), "--metrics"],
        )],
        ..Default::default()
    };
    let mut caches = Caches::default();
    let mut st = BTreeMap::new();
    let env = HostEnv::default();
    let first = detect(&s, &live(), &BTreeMap::new(), &mut caches, &env, &mut st);
    assert_eq!(first[0].tok_s.value, Some(20.0), "first sample: lifetime average");
    let second = detect(&s, &live(), &BTreeMap::new(), &mut caches, &env, &mut st);
    assert_eq!(second[0].tok_s.value, Some(30.0));
    assert!(second[0].tok_s.source.starts_with("Δ"));
    let third = detect(&s, &live(), &BTreeMap::new(), &mut caches, &env, &mut st);
    assert_eq!(third[0].tok_s.value, Some(0.0), "idle → 0, not the stale average");
}

/// vLLM's weights: the on-disk size of `--model` in the HF cache (estimate), never a model name mix-up with
/// `--served-model-name`.
#[test]
fn vllm_weights_from_hf_cache() {
    const M: u64 = 1024 * 1024;
    let home = tempfile::tempdir().unwrap();
    let hub = home.path().join("hub");
    let snap = hub.join("models--Qwen--Qwen3-8B/snapshots/r1");
    std::fs::create_dir_all(&snap).unwrap();
    std::fs::File::create(snap.join("model.safetensors"))
        .unwrap()
        .set_len(64 * M)
        .unwrap();
    let m = mock(Box::new(|url, _| match url {
        "/metrics" => (200, "vllm:num_requests_running{model_name=\"qwen\"} 0\n".into()),
        _ => (404, "".into()),
    }));
    let port = m.port.to_string();
    let argv = [
        "/venv/bin/python",
        "/venv/bin/vllm",
        "serve",
        "--served-model-name",
        "qwen",
        "--model",
        "Qwen/Qwen3-8B",
        "--port",
        &port,
    ];
    assert_eq!(
        argv_model_source(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>()).as_deref(),
        Some("Qwen/Qwen3-8B")
    );
    let s = Snapshot {
        processes: vec![proc(41, 1, &argv)],
        ..Default::default()
    };
    let env = HostEnv {
        hf_hub_cache: Some(hub),
        ..Default::default()
    };
    let (ms, _) = run(&s, &live(), &env);
    let v = &ms[0];
    assert_eq!(v.kind, ModelServerKind::Vllm);
    assert_eq!(v.models[0].name, "qwen");
    assert_eq!(v.models[0].weights_bytes.value, Some(64 * M));
    assert_eq!(v.models[0].weights_bytes.quality, oomtop_core::Quality::Estimate);
    assert_eq!(v.busy.value, Some(false));
}

/// Model names taken from argv are redacted (URLs with credentials, tokens).
#[test]
fn argv_model_names_are_redacted() {
    let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let got = argv_model_name(&a(&[
        "mlx_lm.server",
        "--model",
        "https://user:hunter2hunter2@example.com/m",
    ]))
    .unwrap();
    assert!(!got.contains("hunter2"), "{got}");
    assert_eq!(
        argv_model_name(&a(&["mlx_lm.server", "--model", "mlx-community/Qwen3-4B-4bit"])).as_deref(),
        Some("mlx-community/Qwen3-4B-4bit")
    );
    assert_eq!(
        argv_model_name(&a(&["vllm", "serve", "--served-model-name", "x"])).as_deref(),
        Some("x")
    );
}

#[test]
fn ollama_tags_over_http() {
    let m = mock(Box::new(|url, _| {
        match url {
        "/api/tags" => (
            200,
            r#"{"models":[{"name":"llama3:8b","model":"llama3:8b","size":4661224676,"digest":"365c","details":{"family":"llama","parameter_size":"8.0B","quantization_level":"Q4_0"}},{"model":"qwen3:4b","size":2500000000}]}"#.into(),
        ),
        _ => (404, "".into()),
    }
    }));
    let ep = format!("http://127.0.0.1:{}", m.port);
    let tags = ollama::installed(&ep, Duration::from_secs(2)).unwrap();
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].name, "llama3:8b");
    assert_eq!(tags[0].parameter_size.as_deref(), Some("8.0B"));
    assert_eq!(tags[1].name, "qwen3:4b", "`model` is the fallback name");
    assert!(matches!(
        ollama::installed(&format!("{ep}/nope"), Duration::from_secs(2)),
        Err(AdapterError::Status(404))
    ));
    assert!(matches!(
        ollama::installed("http://localhost:11434", Duration::from_secs(2)),
        Err(AdapterError::NotLoopback(_))
    ));
}

#[test]
fn lms_lookup_skips_relative_path_entries() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("lms"), "#!/bin/sh\n").unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".lmstudio/bin")).unwrap();
    std::fs::write(home.join(".lmstudio/bin/lms"), "#!/bin/sh\n").unwrap();
    let os = |s: &std::path::Path| Some(s.as_os_str().to_owned());
    assert_eq!(lm_studio::find_lms_in(os(&bin), None), Some(bin.join("lms")));
    // Relative entries never match, even if `bin/lms` exists relative to the cwd.
    assert_eq!(
        lm_studio::find_lms_in(Some(".:bin:".into()), os(&home)),
        Some(home.join(".lmstudio/bin/lms"))
    );
    assert_eq!(lm_studio::find_lms_in(Some(".".into()), Some("rel".into())), None);
}
