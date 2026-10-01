//! Tool definitions (SPEC §12.3): names, descriptions, input schemas, output schemas and annotations.
//!
//! Every tool returns `structuredContent` that conforms to its `outputSchema` (checked by the tests with a
//! small JSON-Schema subset validator), plus the same JSON serialized in a text block for older clients.

use serde_json::{json, Map, Value};

pub const GET_HEADROOM: &str = "get_headroom";
pub const CAN_FIT: &str = "can_fit";
pub const TOP_CONSUMERS: &str = "top_consumers";
pub const LIST_GROUPS: &str = "list_groups";
pub const LIST_MODEL_SERVERS: &str = "list_model_servers";
pub const LIST_SANDBOXES: &str = "list_sandboxes";
pub const EXPLAIN_SLOWDOWN: &str = "explain_slowdown";
pub const SUGGEST_RECLAIM: &str = "suggest_reclaim";
pub const RECLAIM: &str = "reclaim";

/// Read-only tools, always registered.
pub const READ_ONLY_TOOLS: [&str; 8] = [
    GET_HEADROOM,
    CAN_FIT,
    TOP_CONSUMERS,
    LIST_GROUPS,
    LIST_MODEL_SERVERS,
    LIST_SANDBOXES,
    EXPLAIN_SLOWDOWN,
    SUGGEST_RECLAIM,
];

/// Accepted values for `list_groups.kind` (canonical names and short aliases).
pub const KIND_VALUES: [&str; 10] = [
    "agent_session",
    "agent",
    "app",
    "model_server",
    "model",
    "sandbox",
    "build_daemon",
    "daemon",
    "system",
    "other",
];

/// Values of `reclaim.status`.
pub const RECLAIM_STATUSES: [&str; 9] = [
    "done",
    "partial",
    "declined",
    "cancelled",
    "not_confirmed",
    "needs_user",
    "unavailable",
    "nothing_to_do",
    "failed",
];

/// Whether `name` is a registered tool for this configuration.
pub fn is_tool(name: &str, allow_actions: bool) -> bool {
    READ_ONLY_TOOLS.contains(&name) || (allow_actions && name == RECLAIM)
}

// ---------------------------------------------------------------------------------------------------------
// schema helpers
// ---------------------------------------------------------------------------------------------------------

fn t(ty: &str) -> Value {
    json!({ "type": ty })
}

fn uint() -> Value {
    json!({ "type": "integer", "minimum": 0 })
}

fn nullable(ty: &str) -> Value {
    json!({ "type": [ty, "null"] })
}

fn arr(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

fn obj(props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut p = Map::new();
    for (k, v) in props {
        p.insert((*k).to_string(), v.clone());
    }
    json!({ "type": "object", "properties": p, "required": required })
}

fn nullable_obj(props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut v = obj(props, required);
    v["type"] = json!(["object", "null"]);
    v
}

/// `Measured<T>`: `{ value, source, quality }`; quality is `"exact"`, `"estimate"` or `{"unavailable": ".."}`.
fn measured(ty: &str) -> Value {
    obj(
        &[
            ("value", nullable(ty)),
            ("source", t("string")),
            ("quality", json!({ "type": ["string", "object"] })),
        ],
        &["value", "source", "quality"],
    )
}

fn common() -> Vec<(&'static str, Value)> {
    vec![
        ("schema_version", uint()),
        ("as_of_ms", uint()),
        ("summary", t("string")),
    ]
}

fn with_common(props: Vec<(&'static str, Value)>, required: &[&'static str]) -> Value {
    let mut all = common();
    all.extend(props);
    let mut req = vec!["schema_version", "as_of_ms", "summary"];
    req.extend_from_slice(required);
    obj(&all, &req)
}

/// Compact group summary returned by `top_consumers`, `list_groups` and nested in model servers/sandboxes.
pub fn group_summary_schema() -> Value {
    obj(
        &[
            ("id", t("string")),
            ("kind", t("string")),
            ("label", t("string")),
            ("root_pid", nullable("integer")),
            ("process_count", uint()),
            ("footprint", measured("integer")),
            ("resident", measured("integer")),
            ("gpu", measured("integer")),
            ("swapped", measured("integer")),
            ("cpu_pct", measured("number")),
            ("reclaim_gain", measured("integer")),
            ("swap_gain", measured("integer")),
            ("idle", t("boolean")),
            ("idle_for_s", nullable("integer")),
            ("orphan", t("boolean")),
            ("protected", t("boolean")),
            ("is_self", t("boolean")),
            ("is_caller", t("boolean")),
            ("lower_bound", t("boolean")),
            ("configured_mem", nullable("integer")),
            ("owner_group", nullable("string")),
            ("confidence", t("string")),
            ("matched_by", nullable("string")),
            ("reclaim_candidate", t("boolean")),
        ],
        &[
            "id",
            "kind",
            "label",
            "root_pid",
            "process_count",
            "footprint",
            "cpu_pct",
            "reclaim_gain",
            "idle",
            "orphan",
            "protected",
            "is_caller",
            "lower_bound",
            "reclaim_candidate",
        ],
    )
}

fn candidate_schema() -> Value {
    obj(
        &[
            ("group_id", t("string")),
            ("label", t("string")),
            ("kind", t("string")),
            ("gain", uint()),
            ("idle_for_s", nullable("integer")),
            ("orphan", t("boolean")),
            ("swap_gain", nullable("integer")),
            ("gpu_gain", nullable("integer")),
        ],
        &["group_id", "label", "kind", "gain"],
    )
}

fn headroom_schema() -> Value {
    let gpu = obj(
        &[
            ("accelerator_id", t("string")),
            ("budget", measured("integer")),
            ("in_use", measured("integer")),
            ("free", measured("integer")),
            ("unified", t("boolean")),
            ("name", t("string")),
            ("margin", uint()),
            ("headroom", nullable("integer")),
        ],
        &["accelerator_id", "budget", "in_use", "free", "unified"],
    );
    obj(
        &[
            ("available_now", measured("integer")),
            ("safety_margin", uint()),
            ("headroom", nullable("integer")),
            ("pressure", nullable("string")),
            ("swap_growing", t("boolean")),
            ("reclaimable", measured("integer")),
            ("reclaimable_swap", measured("integer")),
            ("gpu", arr(gpu)),
            ("as_of_ms", uint()),
            ("total", nullable("integer")),
            ("margin_boosted", t("boolean")),
            ("notes", arr(t("string"))),
        ],
        &[
            "available_now",
            "safety_margin",
            "headroom",
            "swap_growing",
            "reclaimable",
            "gpu",
        ],
    )
}

fn forecast_schema() -> Value {
    nullable_obj(
        &[
            ("target", t("string")),
            ("killer", nullable("string")),
            ("eta_s", uint()),
            ("confidence", t("number")),
            ("rate_per_min", t("number")),
            ("window_s", uint()),
        ],
        &["target", "eta_s", "confidence"],
    )
}

fn output_schema(name: &str) -> Value {
    match name {
        GET_HEADROOM => with_common(
            vec![
                ("mode", t("string")),
                ("headroom", headroom_schema()),
                ("pressure", measured("string")),
                (
                    "swap",
                    obj(
                        &[
                            ("used", measured("integer")),
                            ("total", measured("integer")),
                            ("in_per_min", measured("integer")),
                            ("out_per_min", measured("integer")),
                            ("growing", t("boolean")),
                        ],
                        &["used", "total", "growing"],
                    ),
                ),
                ("forecast", forecast_schema()),
                ("oom_killer", t("string")),
                ("likely_victim", json!({ "type": ["object", "null"] })),
            ],
            &["mode", "headroom", "pressure", "swap", "forecast"],
        ),
        CAN_FIT => with_common(
            vec![
                (
                    "fit",
                    obj(
                        &[
                            (
                                "answer",
                                json!({ "type": "string", "enum": ["yes", "yes_after_reclaim", "no"] }),
                            ),
                            ("reclaim", arr(candidate_schema())),
                            ("gain", uint()),
                            ("shortfall", uint()),
                        ],
                        &["answer"],
                    ),
                ),
                (
                    "need",
                    obj(
                        &[
                            ("bytes", uint()),
                            ("gpu_bytes", nullable("integer")),
                            ("label", nullable("string")),
                        ],
                        &["bytes"],
                    ),
                ),
                ("need_source", t("string")),
                ("headroom", nullable("integer")),
                ("gpu_free", nullable("integer")),
                ("valid_for_s", uint()),
                ("expires_at_ms", uint()),
                ("reason", t("string")),
                ("host_need", uint()),
                ("gpu_checked", t("boolean")),
                ("error", nullable("string")),
                ("notes", arr(t("string"))),
                ("exit_code", t("integer")),
                ("advisory", t("boolean")),
            ],
            &[
                "fit",
                "need",
                "need_source",
                "headroom",
                "valid_for_s",
                "reason",
                "exit_code",
                "advisory",
            ],
        ),
        TOP_CONSUMERS => with_common(
            vec![
                (
                    "by",
                    json!({ "type": "string", "enum": ["footprint", "gpu", "cpu"] }),
                ),
                ("n", uint()),
                ("memory_total", nullable("integer")),
                ("groups", arr(group_summary_schema())),
            ],
            &["by", "n", "groups"],
        ),
        LIST_GROUPS => with_common(
            vec![
                ("kind", nullable("string")),
                ("total", uint()),
                ("returned", uint()),
                ("truncated", t("boolean")),
                ("groups", arr(group_summary_schema())),
            ],
            &["kind", "total", "returned", "truncated", "groups"],
        ),
        LIST_MODEL_SERVERS => with_common(
            vec![
                ("count", uint()),
                (
                    "model_servers",
                    arr(obj(
                        &[
                            ("id", t("string")),
                            ("kind", t("string")),
                            ("endpoint", nullable("string")),
                            ("pids", arr(uint())),
                            ("group_id", nullable("string")),
                            ("models", arr(t("object"))),
                            ("busy", measured("boolean")),
                            ("status", json!({ "type": ["string", "object"] })),
                            ("group", json!({ "type": ["object", "null"] })),
                        ],
                        &["id", "kind", "pids", "models", "group"],
                    )),
                ),
            ],
            &["count", "model_servers"],
        ),
        LIST_SANDBOXES => with_common(
            vec![
                ("count", uint()),
                (
                    "sandboxes",
                    arr(obj(
                        &[
                            ("id", t("string")),
                            ("kind", t("string")),
                            ("runtime", t("string")),
                            ("label", t("string")),
                            ("host_pids", arr(uint())),
                            ("configured_mem", measured("integer")),
                            ("guest_mem", measured("integer")),
                            ("started_by_group", nullable("string")),
                            ("footprint_lower_bound", t("boolean")),
                            ("host_cost", measured("integer")),
                            ("group", json!({ "type": ["object", "null"] })),
                        ],
                        &["id", "kind", "runtime", "label", "host_pids", "host_cost"],
                    )),
                ),
            ],
            &["count", "sandboxes"],
        ),
        EXPLAIN_SLOWDOWN => with_common(
            vec![
                (
                    "causes",
                    arr(obj(
                        &[
                            ("kind", t("string")),
                            ("score", t("number")),
                            ("title", t("string")),
                            ("evidence", arr(t("string"))),
                            ("fix", nullable("string")),
                        ],
                        &["kind", "score", "title", "evidence", "fix"],
                    )),
                ),
                (
                    "thermal",
                    obj(
                        &[
                            ("pressure", measured("string")),
                            ("throttle_factor", measured("number")),
                            ("low_power_mode", measured("boolean")),
                            ("on_battery", measured("boolean")),
                            ("battery_pct", measured("number")),
                        ],
                        &["pressure", "throttle_factor"],
                    ),
                ),
                ("cpu_total_pct", measured("number")),
            ],
            &["causes", "thermal"],
        ),
        SUGGEST_RECLAIM => with_common(
            vec![
                ("candidates", arr(candidate_schema())),
                ("total_gain", uint()),
                ("total_swap_gain", uint()),
                ("headroom", nullable("integer")),
                ("headroom_after", nullable("integer")),
                ("excluded", arr(t("string"))),
                ("command", t("string")),
                ("side_effects", t("boolean")),
            ],
            &[
                "candidates",
                "total_gain",
                "headroom",
                "excluded",
                "command",
                "side_effects",
            ],
        ),
        RECLAIM => with_common(
            vec![
                ("status", json!({ "type": "string", "enum": RECLAIM_STATUSES })),
                ("executed", t("boolean")),
                (
                    "targets",
                    arr(obj(
                        &[
                            ("group_id", t("string")),
                            ("label", t("string")),
                            ("kind", t("string")),
                            ("pid", uint()),
                            ("signal", t("string")),
                            ("expected_gain", nullable("integer")),
                            ("idle", t("boolean")),
                            ("idle_for_s", nullable("integer")),
                            ("orphan", t("boolean")),
                            ("process_count", uint()),
                        ],
                        &["group_id", "label", "pid", "signal", "expected_gain"],
                    )),
                ),
                (
                    "refused",
                    arr(obj(
                        &[("group_id", t("string")), ("reason", t("string"))],
                        &["group_id", "reason"],
                    )),
                ),
                (
                    "outcomes",
                    arr(obj(
                        &[
                            ("group_id", t("string")),
                            ("pid", uint()),
                            ("ok", t("boolean")),
                            ("message", t("string")),
                            ("exited", t("boolean")),
                        ],
                        &["group_id", "pid", "ok", "message"],
                    )),
                ),
                ("expected_gain", uint()),
                ("measured_gain", nullable("integer")),
                ("available_before", nullable("integer")),
                ("available_after", nullable("integer")),
                ("user_action", nullable("string")),
                ("command", nullable("string")),
            ],
            &["status", "executed", "targets", "refused", "outcomes", "command"],
        ),
        _ => obj(&[], &[]),
    }
}

fn tool(name: &str, title: &str, description: &str, input: Value, annotations: Value) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input,
        "outputSchema": output_schema(name),
        "annotations": annotations,
    })
}

fn read_only(title: &str) -> Value {
    json!({
        "title": title,
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": false,
    })
}

fn no_args() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

/// Tool definitions for `tools/list`. `reclaim` is present only with `allow_actions`.
pub fn tool_definitions(allow_actions: bool) -> Vec<Value> {
    let mut tools = vec![
        tool(
            GET_HEADROOM,
            "Memory headroom",
            "How much memory can be used right now without swapping: available memory, safety margin, \
             headroom (available − margin, may be negative), GPU/Metal budget, memory pressure, swap trend and \
             OOM forecast. Samples the machine on each call.",
            no_args(),
            read_only("Memory headroom"),
        ),
        tool(
            CAN_FIT,
            "Can it fit?",
            "Call before loading a large model or starting heavy work. Give exactly one of `bytes`, `size` \
             (e.g. \"13G\") or `model_path` (GGUF/safetensors; only headers are read). Answers `yes`, \
             `yes_after_reclaim` (with the idle groups to stop and their estimated gain) or `no` (with the \
             shortfall). Advisory only and valid for 10 s; never includes the calling agent's own session.",
            json!({
                "type": "object",
                "properties": {
                    "bytes": { "type": "integer", "minimum": 0, "description": "Memory needed, in bytes." },
                    "size": { "type": "string", "description": "Memory needed as a size, e.g. \"13G\", \"800MiB\"." },
                    "model_path": { "type": "string", "description": "Local model file to estimate (headers only)." },
                    "gpu_resident": { "type": "boolean", "description": "The need lives in GPU memory (checked against the GPU/Metal budget too). Default false." },
                    "label": { "type": "string", "description": "What is being loaded, for the answer text." }
                },
                "additionalProperties": false
            }),
            read_only("Can it fit?"),
        ),
        tool(
            TOP_CONSUMERS,
            "Top consumers",
            "Top groups (agent sessions, apps, model servers, sandboxes, build daemons) by memory footprint, \
             GPU memory or CPU. Numbers are true memory (footprint / PSS, not RSS) with source and quality.",
            json!({
                "type": "object",
                "properties": {
                    "by": { "type": "string", "enum": ["footprint", "gpu", "cpu"], "description": "Sort key (default footprint)." },
                    "n": { "type": "integer", "minimum": 1, "maximum": 100, "description": "How many groups (default 10)." }
                },
                "additionalProperties": false
            }),
            read_only("Top consumers"),
        ),
        tool(
            LIST_GROUPS,
            "List groups",
            "Every attributed group with its memory, CPU, idle/orphan state and estimated reclaim gain, largest \
             first. Filter by `kind` (agent, app, model, sandbox, daemon, system, other).",
            json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": KIND_VALUES, "description": "Only groups of this kind." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Maximum groups returned (default 50)." }
                },
                "additionalProperties": false
            }),
            read_only("List groups"),
        ),
        tool(
            LIST_MODEL_SERVERS,
            "List model servers",
            "Local model servers (Ollama, llama.cpp, sd.cpp, vLLM, LM Studio, MLX…) with loaded models, \
             device, throughput/progress and the memory of their group.",
            no_args(),
            read_only("List model servers"),
        ),
        tool(
            LIST_SANDBOXES,
            "List sandboxes",
            "Containers, VMs and process sandboxes with their host memory cost (a lower bound for \
             Virtualization.framework VMs) and configured memory.",
            no_args(),
            read_only("List sandboxes"),
        ),
        tool(
            EXPLAIN_SLOWDOWN,
            "Explain slowdown",
            "Ranked likely causes of slowness or memory pressure (swap storm, memory pressure, thermal \
             throttling, Low Power Mode, low battery, CPU saturation, imminent OOM) with evidence and a fix. \
             Reports nothing throttle-related while the machine is idle.",
            no_args(),
            read_only("Explain slowdown"),
        ),
        tool(
            SUGGEST_RECLAIM,
            "Suggest reclaim",
            "Idle build daemons, orphans and idle model servers that could be stopped, with estimated RAM \
             gain (and swap freed separately). No side effects: it only lists; the user runs \
             `oomtop reclaim` to act. Never includes the calling agent's own session.",
            no_args(),
            read_only("Suggest reclaim"),
        ),
    ];
    if allow_actions {
        tools.push(tool(
            RECLAIM,
            "Reclaim memory",
            "Stop the given reclaim candidates (idle build daemons, orphans, idle model servers — the ids \
             suggest_reclaim returns; anything else is refused). Model servers with an unload API are unloaded \
             without a signal; other group roots get SIGTERM, never SIGKILL; a model server running a job is \
             refused. oomtop itself asks the user to confirm the exact list and gains through MCP elicitation \
             and acts only on an explicit accept. Without client elicitation support nothing is stopped and the \
             `oomtop reclaim` command is returned for the user. Protected groups and the calling agent's own \
             session are always refused.",
            json!({
                "type": "object",
                "required": ["group_ids"],
                "properties": {
                    "group_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "maxItems": 64,
                        "description": "Group ids to stop (from suggest_reclaim / list_groups)."
                    }
                },
                "additionalProperties": false
            }),
            json!({
                "title": "Reclaim memory",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false,
            }),
        ));
    }
    tools
}
