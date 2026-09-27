//! Chat client for a local llama-server (OpenAI-compatible surface).
//!
//! Replaced the ollama client on 2026-08-20. The reason was not tidiness: the
//! box was holding **two copies of the same 35B model** — mecha's llama-server
//! at :8080 (unsloth UD-Q4_K_M, `--jinja`, `--spec-type draft-mtp`) and
//! ollama's own llama-server holding a second, stock-quant copy under a
//! *chatml* template with `--context-shift` on. 57.5 GB of a 121 GB unified
//! pool for one model, and the two fought over the same GB10 every night —
//! measurably: mecha's interactive generation sat at 28 tok/s against a
//! recorded 79.8 baseline while the nightly ran, and nothing reported it.
//!
//! ## Sharing, and the one thing that may start a server
//!
//! Installed beside mecha, this must use the model mecha already has loaded.
//! Standalone, it needs a server of its own. Both are the same question asked
//! once: **is something already answering at the endpoint?** — resolved by
//! [`Backend::resolve`], never by looking for mecha. `mecha-graph-core` knows
//! nothing about any agent (lib.rs rule 1), so there is deliberately no code
//! here that reads `~/.mecha/config.toml` or checks whether mecha is
//! installed. A user who starts their own llama-server gets the shared path
//! for free, which is the same answer for the right reason.
//!
//! Spawning is gated on `[llm] model_path` being **explicitly configured**,
//! and that gate is the whole safety argument. Probe-and-spawn on its own
//! would re-create the bug this module deletes: mecha's server is restartable
//! and has been absent before (2026-08-19, when a reboot restored every
//! consumer and not the server), so an automatic fallback would answer a
//! transient outage by loading a second 20 GB copy — silently, at 03:30, and
//! for the rest of the night. With the gate, a machine that has not named a
//! GGUF cannot start a second copy at all. Nothing ever spawns at a URL that
//! already answers.
//!
//! ## Why thinking stays on
//!
//! qwen3.6 reasons before answering and the first instinct was to switch that
//! off for a temperature-0.1 JSON extraction. Measured, that was wrong twice
//! over. Thinking off returned `Luke works_with Friday` — the durability and
//! subject rules the prompt spends most of its length on are exactly what
//! deliberation buys. Thinking on returned the one durable fact and skipped
//! the moment-anchored ones.
//!
//! What killed the 2026-08-20 nightly with 300 s timeouts is **not
//! established**, and an earlier draft of this note asserted it confidently.
//! The measured contributor is contention: two copies of the model on one GPU
//! put interactive generation at 28 tok/s against a 79.8 baseline, which turns
//! a documented 45 s/episode into minutes. `--reasoning-budget` is a
//! llama-server flag ollama's runner never passes, so reasoning did run
//! unbounded there — a plausible additional factor, never isolated. (Note that
//! mecha's own "non-terminating reasoning" diagnosis was *retired* on
//! 2026-08-10; the empty turns were unparsed tool calls emitted before
//! `</think>` closed. CHANGELOG 0.1.2.)
//!
//! Grammar-constrained output composes with thinking: llama.cpp applies the
//! schema lazily, *after* the thinking block closes, so `json_schema` and
//! reasoning coexist (measured — 3,240 chars of reasoning, then valid JSON).
//!
//! ## What the schema buys
//!
//! `response_format: json_schema` compiles to a GBNF the sampler must
//! satisfy, so the closed predicate vocabulary stops being an instruction the
//! model may ignore. Nothing downstream ever validated it — `propose_fact`
//! stages whatever string arrives — so an out-of-vocab predicate used to
//! become a candidate no consumer could interpret.
//!
//! The failure this module guards hardest is the *silent* one: a reply that
//! is HTTP 200 with an empty `content` because reasoning consumed the whole
//! budget. That reads as "the model had nothing to say", and the old code
//! then marked the episode processed. [`ChatClient::post`] refuses it by name.

use crate::error::{Error, Result};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// mecha's local provider. Deliberately the same endpoint
/// `~/.mecha/config.toml` points at — one model, one copy of the weights, one
/// set of measured flags — but reached by probing, not by knowing.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";

/// The `--alias` llama-server is started with, not an ollama tag.
pub const DEFAULT_MODEL: &str = "qwen3.6-35b-a3b";

/// Generous because the server may be shared. A queued request inherits every
/// other tenant's latency, and the old 300 s ceiling is what turned a slow
/// episode into a dropped one. mecha's own provider allows 900 s.
const DEFAULT_TIMEOUT_SECS: u64 = 900;

/// The canary's own ceiling (`ChatClient::canary`). Room for a router's cold
/// load (33–39 s measured) and a queue behind other tenants, far short of a
/// real request's 900 s. Erring short costs a stopped batch — every episode
/// still pending — never a lost one.
const CANARY_TIMEOUT_SECS: u64 = 180;

/// What the canary asks about: nothing, so that whatever fails on it is the
/// request's and not an input's.
const CANARY_INPUT: &str = "(This input is empty. There is nothing in it.)";

/// Must sit **comfortably above** the server's `--reasoning-budget` (4096),
/// or the thinking block consumes the whole allowance and the turn comes back
/// with an empty `content`. That is not a hypothetical: at `max_tokens` 1024
/// this model returned 1024 tokens of reasoning and no answer. mecha's
/// `[agent] max_tokens` carries the same number for the same reason — move
/// the two together.
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// How long to wait for a server we started to answer `/health`. Loading ~20 GB
/// of weights took 14 s warm on this box; cold off a slow disk is minutes.
const SPAWN_HEALTH_TIMEOUT: Duration = Duration::from_secs(600);

/// How long to wait for a server that is already there, and loading, to
/// finish — the same allowance as one we started, because it is the same
/// load. Waited out rather than spawned over: the port is taken.
const LOAD_WAIT: Duration = SPAWN_HEALTH_TIMEOUT;

/// Where the completions go, and who owns the process behind them.
pub enum Backend {
    /// Someone else's server — mecha's, or one the user started. We never
    /// stop it, and we never start one at a URL that answers.
    Shared { base_url: String },
    /// Ours. Killed when this value drops, so a CLI run leaves nothing behind.
    Managed { base_url: String, child: Child },
}

impl Backend {
    pub fn base_url(&self) -> &str {
        match self {
            Backend::Shared { base_url } | Backend::Managed { base_url, .. } => base_url,
        }
    }

    pub fn is_managed(&self) -> bool {
        matches!(self, Backend::Managed { .. })
    }

    /// Probe first, spawn only if told how. See the module note for why the
    /// order and the gate are both load-bearing.
    pub fn resolve(base_url: &str, model: &str) -> Result<Self> {
        Self::resolve_within(base_url, model, LOAD_WAIT)
    }

    fn resolve_within(base_url: &str, model: &str, load_wait: Duration) -> Result<Self> {
        let shared = || {
            Ok(Backend::Shared {
                base_url: base_url.to_string(),
            })
        };
        match health(base_url) {
            Health::Ready => return shared(),
            // Someone is there, mid-load: a server starting, or one swapping
            // its model (503 "Loading model"). Neither is "nothing is
            // answering" — reading it that way refused the night with a false
            // cause, or spawned a second server onto a taken port (found on
            // review of #22). Wait for the load; never spawn over it.
            Health::Loading => {
                let deadline = Instant::now() + load_wait;
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    match health(base_url) {
                        Health::Ready => return shared(),
                        // A stall mid-load is still a load in progress.
                        Health::Loading | Health::Unknown(_) if Instant::now() < deadline => {}
                        Health::Loading | Health::Unknown(_) => {
                            return Err(Error::Other(format!(
                                "llama-server at {base_url} is still loading after {}s",
                                load_wait.as_secs()
                            )))
                        }
                        Health::Absent => {
                            return Err(Error::Other(format!(
                                "llama-server at {base_url} was loading, then stopped answering"
                            )))
                        }
                    }
                }
            }
            Health::Unknown(why) => {
                return Err(Error::Other(format!(
                    "{base_url}/health gave no clean answer ({why}). Something may be \
                     listening there, busy, so mecha-graph neither uses it nor starts a \
                     second server over it; nothing was marked attempted."
                )))
            }
            Health::Absent => {}
        }

        let cfg = crate::integrations::load_config()?.llm;
        let Some(model_path) = cfg.model_path.clone() else {
            return Err(Error::Other(format!(
                "no llama-server answering at {base_url}, and no [llm] model_path \
                 configured in {}.\n\
                 Either start a server (mecha's own unit is `systemctl --user start \
                 llama-local`), or set `[llm] model_path = \"/path/to/model.gguf\"` \
                 to let mecha-graph run one of its own.",
                crate::integrations::config_path().display()
            )));
        };
        if !model_path.exists() {
            return Err(Error::Other(format!(
                "[llm] model_path does not exist: {}",
                model_path.display()
            )));
        }

        let port = port_of(base_url).ok_or_else(|| {
            Error::Other(format!("cannot parse a port out of base_url '{base_url}'"))
        })?;
        let binary = cfg
            .server_bin
            .as_deref()
            .unwrap_or("llama-server")
            .to_string();

        let mut cmd = Command::new(&binary);
        cmd.arg("-m")
            .arg(&model_path)
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .args(["--alias", model])
            // `--jinja` uses the model's OWN chat template. ollama's
            // `--no-jinja --chat-template chatml` override is what made
            // per-request template controls silently inert there.
            .arg("--jinja")
            // The bound that stops qwen3.6 reasoning forever. Omitting this is
            // precisely how the ollama path produced 300 s timeouts.
            .args(["--reasoning-budget", "4096"])
            .args(["-ngl", "999"])
            // Episodes are clipped to 6000 chars, so a large window here buys
            // nothing and reserves real memory. Raise it via server_args if a
            // caller needs more.
            .args(["-c", "32768"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(extra) = &cfg.server_args {
            cmd.args(extra);
        }

        let child = cmd
            .spawn()
            .map_err(|e| Error::Other(format!("could not start '{binary}': {e}")))?;
        let mut backend = Backend::Managed {
            base_url: base_url.to_string(),
            child,
        };

        let deadline = Instant::now() + SPAWN_HEALTH_TIMEOUT;
        while Instant::now() < deadline {
            if health(base_url) == Health::Ready {
                return Ok(backend);
            }
            if let Backend::Managed { child, .. } = &mut backend {
                // Died on the way up — a bad flag, a corrupt GGUF. Say so now
                // rather than after ten more minutes of polling a corpse.
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(Error::Other(format!(
                        "llama-server exited before answering /health ({status}). \
                         Run it by hand to see why; stdout/stderr are suppressed here."
                    )));
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(Error::Other(format!(
            "llama-server did not answer {base_url}/health within {}s",
            SPAWN_HEALTH_TIMEOUT.as_secs()
        )))
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // Only ever our own child. A shared server outlives us by definition.
        if let Backend::Managed { child, .. } = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// What a server says about itself: the model it is serving, if that can be
/// told, and — on a router, whose `model` field *selects* — every id it will
/// accept, so a configured fallback can be checked before any request uses it.
#[derive(Debug, Default)]
struct Served {
    /// Whether `/props` answered at all. A server `Backend::resolve` already
    /// found answering whose `/props` does not is unknown, and unknown is not
    /// "not a router": `connect` refuses rather than send an unchecked name.
    props_read: bool,
    /// Why `/props` gave nothing, when it did not: the refusal names it,
    /// because a stall on a busy server and a server with no `/props` call
    /// for different fixes (found on review of #22).
    props_error: Option<String>,
    resident: Option<String>,
    /// `Some` on a router: `Some(ids)` from a `/models` it read, `Some(None)`
    /// when that list could not be read.
    router_ids: Option<Option<Vec<String>>>,
    /// Why a router's `/models` gave nothing, for the same reason as
    /// `props_error`: busy and absent call for different fixes.
    models_error: Option<String>,
}

/// What the server actually has loaded, from its own `/props`.
///
/// Asked rather than asserted, because the shared server belongs to mecha and
/// mecha's model is the user's to change. llama-server serves whatever is
/// loaded and ignores the `model` field of a request, so a client that names a
/// model is not selecting one — it is only deciding what to write down. Naming
/// `qwen3.6-35b-a3b` while the box actually serves gemma4 would put a false
/// value in `extract_state.model`, which is the one column that answers "what
/// produced this fact" and the one PROMPT_VERSION re-extraction keys off.
///
/// **Behind a llama-server router the bare `/props` is a placeholder**
/// (`role: "router"`, `model_alias: "llama-server"`), and the request's
/// `model` field *selects* rather than being ignored. Reading that placeholder
/// as the served model sent `"model": "llama-server"` on every request, which
/// the router refuses — 2026-09-27, the night mecha's :8080 became a router:
/// 100 extractions and 30 summaries failed, and the extractions were marked
/// attempted. So on a router the answer is the one model resident there, read
/// from `/models`; `None` — use the configured model, **on a router only if it
/// lists that name** (`connect` refuses otherwise) — when that cannot be told
/// (nothing loaded, two loaded, or a list this does not fully read).
fn probe(base_url: &str) -> Served {
    // Ten seconds, not 1.5: a shared server's queue can hold a probe as it
    // holds a request, and a probe that times out is now a refusal.
    let fetch = |path: &str| -> std::result::Result<serde_json::Value, String> {
        ureq::get(&format!("{base_url}{path}"))
            .timeout(Duration::from_secs(10))
            .call()
            .map_err(|e| e.to_string())?
            .into_json()
            .map_err(|e| format!("unreadable body: {e}"))
    };
    let props = match fetch("/props") {
        Ok(p) => p,
        Err(why) => {
            return Served {
                props_error: Some(why),
                ..Default::default()
            }
        }
    };
    let router = props.get("role").and_then(|r| r.as_str()) == Some("router");
    let (models, models_error) = match router.then(|| fetch("/models")) {
        Some(Ok(m)) => (Some(m), None),
        Some(Err(why)) => (None, Some(why)),
        None => (None, None),
    };
    Served {
        props_read: true,
        props_error: None,
        resident: served_from(&props, models.as_ref()),
        router_ids: router.then(|| router_ids(models.as_ref())),
        models_error,
    }
}

/// Every model id a router lists — the names a request may carry — or `None`
/// when there is no list to read (not the same as an empty one).
fn router_ids(models: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let data = models?.get("data")?.as_array()?;
    Some(
        data.iter()
            .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .collect(),
    )
}

/// The pure half of [`probe`]: a single-model server's `model_alias`,
/// or a router's one resident model.
fn served_from(props: &serde_json::Value, models: Option<&serde_json::Value>) -> Option<String> {
    if props.get("role").and_then(|r| r.as_str()) == Some("router") {
        // Only from a list whose every status is known: "nothing resident" or
        // "this one" is a claim a list this does not understand cannot back.
        // Same rule as mecha's provider::router::readable.
        const KNOWN: [&str; 5] = ["unloaded", "loading", "loaded", "sleeping", "downloading"];
        // `sleeping` counts as resident, as in mecha's `RouterModel::is_resident`:
        // a sleeping model is still selectable by name, so beside a loaded one
        // "which" is ambiguous. Under `--models-max 1` (the router this was
        // written for) two can never be resident at once, so this is the edge
        // case it looks like, not the steady state.
        let data = models?.get("data")?.as_array()?;
        let status = |m: &serde_json::Value| {
            m.get("status")
                .and_then(|s| s.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        if data.is_empty() || data.iter().any(|m| !KNOWN.contains(&status(m).as_str())) {
            return None;
        }
        let mut resident = data
            .iter()
            .filter(|m| matches!(status(m).as_str(), "loaded" | "loading" | "sleeping"));
        return match (resident.next(), resident.next()) {
            (Some(one), None) => one.get("id").and_then(|i| i.as_str()).map(str::to_string),
            _ => None,
        };
    }
    props
        .get("model_alias")
        .and_then(|m| m.as_str())
        // The router's placeholder, refused by name as well as by `role`:
        // should a router ever answer without the role, the worst case is "use
        // the configured model", never "name the placeholder on every request".
        .filter(|m| !m.is_empty() && *m != "llama-server")
        .map(str::to_string)
}

/// What `/health` says about the server behind a URL.
#[derive(Debug, PartialEq, Eq)]
enum Health {
    /// 2xx: up, with its model loaded.
    Ready,
    /// 503: llama-server is there and loading a model.
    Loading,
    /// Nothing listening (connection refused), or something that is not
    /// llama-server answering (a 404 from ollama): no server here to share.
    Absent,
    /// No clean answer — a timeout, a reset, a read that failed. Something
    /// may be listening, busy, so this is neither "share it" nor "start one":
    /// starting one is a second copy of the model at a URL that answers
    /// (found on review of #22).
    Unknown(String),
}

fn health(base_url: &str) -> Health {
    // /health rather than / on purpose: it is llama-server's, so an ollama
    // listening on the same port answers 404 and is correctly not adopted.
    // The probe's 10 s, for the probe's reason: a shared server's queue can
    // hold /health too, and a stall here does not refuse — it falls through
    // to spawning a second copy of the model (found on review).
    match ureq::get(&format!("{base_url}/health"))
        .timeout(Duration::from_secs(10))
        .call()
    {
        Ok(_) => Health::Ready,
        Err(ureq::Error::Status(503, _)) => Health::Loading,
        Err(ureq::Error::Status(..)) => Health::Absent,
        Err(ureq::Error::Transport(t)) if t.kind() == ureq::ErrorKind::ConnectionFailed => {
            Health::Absent
        }
        Err(e) => Health::Unknown(e.to_string()),
    }
}

fn port_of(base_url: &str) -> Option<u16> {
    base_url
        .rsplit(':')
        .next()
        .and_then(|s| s.trim_end_matches('/').parse().ok())
}

pub struct ChatClient {
    pub model: String,
    pub timeout: Duration,
    pub max_tokens: u32,
    /// On, and measured to matter: the prompt's durability and subject rules
    /// are what deliberation buys, and the runaway that motivated turning it
    /// off was an unbounded-reasoning bug in ollama, not a cost of thinking.
    /// Exposed rather than hardcoded so the A/B can be re-run.
    pub think: bool,
    /// Waits between retries of a request the server did not answer (a 5xx,
    /// a refused or reset connection). A router answers 503 while it loads a
    /// model, and a load takes 30–40 s from disk, so the default spans ~50 s.
    retry_delays: Vec<Duration>,
    /// How long [`ChatClient::canary`] waits. Short, because the canary is
    /// trivial: a server that cannot answer it in this long is not answering.
    canary_timeout: Duration,
    backend: Backend,
}

#[cfg(test)]
impl ChatClient {
    /// A client aimed at `base_url` with no probing — for tests that need a
    /// server's answer (or its absence) to reach `post`.
    pub(crate) fn at(base_url: &str) -> ChatClient {
        ChatClient {
            model: "test-model".into(),
            timeout: Duration::from_secs(5),
            max_tokens: 64,
            think: false,
            retry_delays: Vec::new(),
            canary_timeout: Duration::from_secs(1),
            backend: Backend::Shared {
                base_url: base_url.to_string(),
            },
        }
    }
}

impl ChatClient {
    /// Resolve a backend and connect. Fails loudly when there is no server and
    /// no configured way to start one — never silently degrades to a second
    /// copy of the model.
    pub fn connect(model: &str) -> Result<Self> {
        let cfg = crate::integrations::load_config()?.llm;
        let base_url = std::env::var("MECHA_GRAPH_CHAT_URL")
            .ok()
            .or(cfg.base_url)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let wanted = cfg.model.as_deref().unwrap_or(model).to_string();
        let backend = Backend::resolve(&base_url, &wanted)?;

        // Adopt the served model's name. On a Shared backend that is mecha's
        // choice, not ours; on a Managed one it is the --alias we just passed,
        // so this is a no-op there.
        //
        // Deliberately NOT an error when it differs. Refusing would mean the
        // nightly dies the first time someone tries a different model in the
        // TUI — making the graph's health depend on remembering to edit a
        // second config, which is the failure this whole consolidation was
        // about. And spawning our own server to get the "right" model would be
        // the duplicate-model bug returning wearing a better excuse.
        //
        // A warning is the honest middle: the swap is visible, the provenance
        // is truthful, and `extract_state.model` + PROMPT_VERSION already give
        // you the tools to find and re-extract whatever a given model produced.
        let served = probe(backend.base_url());
        if !served.props_read {
            return Err(Error::Other(format!(
                "mecha-graph: {} answered its health check but not /props ({}), so what \
                 it serves — and, on a router, what it will accept — is unknown. Refusing \
                 before any request, so nothing is marked attempted. A timeout is a busy \
                 server, and tomorrow's run retries; a 404 is a server without \
                 llama-server's /props (vLLM, a proxy), which is not supported as the \
                 chat endpoint.",
                backend.base_url(),
                served
                    .props_error
                    .as_deref()
                    .unwrap_or("no reason recorded")
            )));
        }
        let model = match served.resident {
            Some(served) => {
                if cfg.model.is_some() && served != wanted {
                    eprintln!(
                        "mecha-graph: [llm] model is '{wanted}' but {} serves '{served}' — \
                         using '{served}' and recording it as the extractor.",
                        backend.base_url()
                    );
                }
                served
            }
            // On a router the name *selects*, so falling back is safe only to
            // a name it lists. Anything else would be refused on every request
            // — and extract marks each refused episode attempted, so a batch
            // would be burned, as on 2026-09-27. Refused here, before the
            // first request, every episode stays retryable (found on review).
            None => {
                if let Some(ids) = &served.router_ids {
                    let listed = ids.as_ref().is_some_and(|l| l.iter().any(|i| i == &wanted));
                    if !listed {
                        let what = match ids {
                            None => format!(
                                "its /models could not be read ({})",
                                served
                                    .models_error
                                    .as_deref()
                                    .unwrap_or("a list this client does not fully read")
                            ),
                            Some(l) if l.is_empty() => "it lists no models".to_string(),
                            Some(l) => format!("it lists: {}", l.join(", ")),
                        };
                        return Err(Error::Other(format!(
                            "mecha-graph: the llama-server router at {} has no single resident \
                             model to use and does not list '{wanted}' — {what}. Set [llm] model \
                             (or EXTRACT_MODEL) to a listed name, or load one (`mecha model use \
                             …`). Refusing before any request, so nothing is marked attempted.",
                            backend.base_url()
                        )));
                    }
                }
                wanted
            }
        };

        Ok(ChatClient {
            model,
            timeout: Duration::from_secs(
                std::env::var("MECHA_GRAPH_CHAT_TIMEOUT_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_TIMEOUT_SECS),
            ),
            max_tokens: DEFAULT_MAX_TOKENS,
            think: true,
            retry_delays: [5, 15, 30].map(Duration::from_secs).to_vec(),
            canary_timeout: Duration::from_secs(CANARY_TIMEOUT_SECS),
            backend,
        })
    }

    pub fn base_url(&self) -> &str {
        self.backend.base_url()
    }

    pub fn is_managed(&self) -> bool {
        self.backend.is_managed()
    }

    /// One JSON-mode completion, shape unconstrained beyond "is an object".
    /// For callers whose output shape is a single obvious field.
    pub fn complete_json(&self, system: &str, user: &str) -> Result<serde_json::Value> {
        self.post(system, user, Self::json_object_format())
    }

    /// One completion whose output is constrained by `schema` at the sampler.
    /// Prefer this wherever the shape is known: it removes a whole error class
    /// rather than reporting it.
    pub fn complete_schema(
        &self,
        system: &str,
        user: &str,
        name: &str,
        schema: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.post(system, user, Self::schema_format(name, schema))
    }

    /// [`ChatClient::complete_schema`] without the backoff between retries:
    /// for a request made *because* the canary just answered, where waiting
    /// out a server recovery pays for a condition already ruled out (found
    /// on review of #22: ~100 s of sleep per episode on a server that 5xx's
    /// one input).
    pub fn complete_schema_once(
        &self,
        system: &str,
        user: &str,
        name: &str,
        schema: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.post_within(
            system,
            user,
            Self::schema_format(name, schema),
            self.timeout,
            &[],
        )
    }

    /// The `response_format` [`ChatClient::complete_json`] sends.
    pub fn json_object_format() -> serde_json::Value {
        serde_json::json!({ "type": "json_object" })
    }

    /// The `response_format` [`ChatClient::complete_schema`] sends.
    pub fn schema_format(name: &str, schema: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "json_schema",
            "json_schema": { "name": name, "strict": true, "schema": schema },
        })
    }

    /// Does the server answer *this* request — same model, same system
    /// prompt, same `response_format` — with nothing in it? One completion of
    /// an empty input under a short timeout.
    ///
    /// This is how a failed input is charged: an input that fails while the
    /// canary answers is the input's; one that fails with the canary is the
    /// server's or the request's. Asked, never read off the error — comparing
    /// error strings cannot tell a slow input from a stalled link, or three
    /// episodes that each time out from a server refusing them all, because
    /// the messages are built from values constant across a batch (found on
    /// review of #22).
    ///
    /// The caller's system prompt and format, not a stand-in, because both
    /// are built from graph data: an extraction schema whose predicate enum
    /// the server refuses would 400 every episode while a two-field literal
    /// answered, and the canary would sign off a whole batch as poison (found
    /// on review of #22). Any JSON answer passes; the content is not graded.
    ///
    /// Sent once, with no retries: it is a liveness check, so its bound is
    /// `canary_timeout` and nothing more, and failing it stops the run with
    /// nothing marked — the side to err on.
    pub fn canary(&self, system: &str, response_format: serde_json::Value) -> Result<()> {
        self.post_within(
            system,
            CANARY_INPUT,
            response_format,
            self.canary_timeout,
            &[],
        )
        .map(|_| ())
    }

    fn post(
        &self,
        system: &str,
        user: &str,
        response_format: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.post_within(
            system,
            user,
            response_format,
            self.timeout,
            &self.retry_delays,
        )
    }

    fn post_within(
        &self,
        system: &str,
        user: &str,
        response_format: serde_json::Value,
        timeout: Duration,
        retry_delays: &[Duration],
    ) -> Result<serde_json::Value> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user",   "content": user }
            ],
            "temperature": 0.1,
            "max_tokens": self.max_tokens,
            "stream": false,
            "response_format": response_format,
        });
        if !self.think {
            // Only honoured when llama-server runs with `--jinja`; the guard
            // below is what makes a server that isn't say so.
            body["chat_template_kwargs"] = serde_json::json!({ "enable_thinking": false });
        }

        // Three outcomes, and who they belong to decides what callers do:
        // - an answer that refuses (4xx) is `Other` — the input's or the
        //   request's, which only `canary` can tell apart;
        // - no answer within `timeout` is `Timeout`: a slow input or a
        //   stalled link, which the error alone cannot tell apart either;
        // - no answer at all (a 5xx, a refused or reset connection) is the
        //   server's. It is retried over `retry_delays` — a router answers 503
        //   while it loads a model — and only a sustained one is `Transport`.
        let mut attempt = 0;
        let resp = loop {
            let sent = ureq::post(&format!("{}/v1/chat/completions", self.base_url()))
                .timeout(timeout)
                .send_json(body.clone());
            let err = match sent {
                Ok(r) => break r,
                // A refusal here arrives as a real status with a JSON body
                // naming the bad field. Swallowing it into "request failed"
                // is how a one-line flag mistake costs an evening.
                Err(ureq::Error::Status(code, r)) => {
                    let detail = r.into_string().unwrap_or_default();
                    let msg = format!(
                        "llama-server {code}: {}",
                        detail.chars().take(400).collect::<String>()
                    );
                    if code < 500 {
                        return Err(Error::Other(msg));
                    }
                    Error::Transport(msg)
                }
                Err(other) => {
                    let text = other.to_string();
                    if text.contains("timed out") {
                        return Err(Error::Timeout(format!(
                            "no answer from llama-server within {}s (timed out)",
                            timeout.as_secs()
                        )));
                    }
                    Error::Transport(format!(
                        "llama-server at {} unreachable: {text}",
                        self.base_url()
                    ))
                }
            };
            match retry_delays.get(attempt) {
                Some(wait) => {
                    attempt += 1;
                    eprintln!("mecha-graph: {err} — retrying in {}s", wait.as_secs());
                    std::thread::sleep(*wait);
                }
                None => return Err(err),
            }
        };

        let payload: serde_json::Value = resp
            .into_json()
            .map_err(|e| Error::Transport(format!("bad llama-server response: {e}")))?;

        let choice = payload
            .pointer("/choices/0")
            .ok_or_else(|| Error::Other("no choices in llama-server response".into()))?;
        let content = choice
            .pointer("/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or_default();
        let finish = choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .unwrap_or("");
        let reasoning_len = choice
            .pointer("/message/reasoning_content")
            .and_then(|c| c.as_str())
            .map(str::len)
            .unwrap_or(0);

        if content.trim().is_empty() {
            // The named failure. HTTP 200, no content, and — before this
            // guard — an episode marked attempted as though the model had
            // simply found nothing.
            if reasoning_len > 0 || finish == "length" {
                return Err(Error::Other(format!(
                    "empty completion after {reasoning_len} chars of reasoning \
                     (finish_reason={finish}): the server did not honour \
                     chat_template_kwargs.enable_thinking=false. Check that \
                     llama-server runs with --jinja (a chatml override silently \
                     ignores it)."
                )));
            }
            return Err(Error::Other(format!(
                "empty completion from {} (finish_reason={finish})",
                self.model
            )));
        }

        serde_json::from_str(strip_code_fence(content))
            .map_err(|e| Error::Parse(format!("model returned invalid JSON: {e}")))
    }
}

/// Both response formats compile to a grammar, so a fence should be
/// impossible — but an unconstrained answer wraps JSON in ```json by habit,
/// and the cost of being wrong here is the whole episode. Cheap insurance,
/// not a silent repair: anything that is not exactly a fenced block is
/// returned untouched and fails parsing loudly.
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    match rest.trim_start().strip_suffix("```") {
        Some(inner) => inner.trim(),
        None => t,
    }
}

/// Stub HTTP servers for tests that need a model server's answer — or its
/// absence — to reach `post`. One copy, used by `llm` and `extract` tests.
#[cfg(test)]
pub(crate) mod test_http {
    use std::io::{Read, Write};

    /// Read one whole HTTP request — headers, then `Content-Length` bytes —
    /// so a stub never answers (and closes) mid-upload, which a client sees
    /// as a reset: a no-answer error the test did not mean to produce.
    fn read_request(s: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            match s.read(&mut chunk) {
                Ok(0) | Err(_) => return String::new(),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        while buf.len() < head_end + len {
            match s.read(&mut chunk) {
                Ok(0) | Err(_) => return String::new(),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        String::from_utf8_lossy(&buf[head_end..]).into_owned()
    }

    /// A reply `stub` never sends: it reads the request and holds the
    /// connection open, so the client times out.
    pub(crate) const HANG: u16 = 0;

    /// A 200 carrying `content` as the model's answer.
    pub(crate) fn answer(content: &str) -> (u16, String) {
        (
            200,
            serde_json::json!({"choices": [{"message": {"content": content}, "finish_reason": "stop"}]})
                .to_string(),
        )
    }

    /// Answers each request with the next canned status and body, in order
    /// ([`HANG`] holds that one unanswered).
    pub(crate) fn stub(replies: Vec<(u16, String)>) -> String {
        stub_recording(replies).0
    }

    /// [`stub`], also keeping each request's body, in order.
    pub(crate) fn stub_recording(
        replies: Vec<(u16, String)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            for (code, body) in replies {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut s);
                log.lock().unwrap().push(request);
                if code == HANG {
                    // Leaked, not dropped: a closed socket is a reset, the
                    // no-answer case, where this reply means "no answer yet".
                    std::mem::forget(s);
                    continue;
                }
                let _ = write!(
                    s,
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, seen)
    }

    /// Accepts and drops every connection without answering — the no-answer
    /// case, on a port it keeps (a freed port can be taken by a parallel
    /// test's stub and answer, which made this flaky).
    pub(crate) fn dead_server() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for s in listener.incoming() {
                drop(s);
            }
        });
        url
    }

    /// Accepts, reads, and never answers — the timeout case.
    pub(crate) fn hung_server() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for s in listener.incoming().flatten() {
                let mut s = s;
                read_request(&mut s);
                held.push(s);
            }
        });
        url
    }
}

#[cfg(test)]
mod tests {
    use super::test_http::{answer, dead_server, hung_server, stub};
    use super::*;

    /// A router's bare `/props` is never the answer — the night it was, every
    /// request named "llama-server" and the router refused them all.
    #[test]
    fn a_router_is_asked_for_its_resident_model_not_its_placeholder() {
        let placeholder = serde_json::json!({"role": "router", "model_alias": "llama-server"});
        let listing = |pairs: &[(&str, &str)]| serde_json::json!({"data": pairs.iter().map(|(id, v)| serde_json::json!({"id": id, "status": {"value": v}})).collect::<Vec<_>>()});
        let one = listing(&[
            ("qwen3.6-35b-a3b", "unloaded"),
            ("qwen3.6-35b-a3b-uncensored", "loaded"),
        ]);
        assert_eq!(
            served_from(&placeholder, Some(&one)).as_deref(),
            Some("qwen3.6-35b-a3b-uncensored")
        );
        // No list, nothing resident, two resident, an unknown status: no
        // answer, so the configured model is used — never "llama-server".
        assert_eq!(served_from(&placeholder, None), None);
        assert_eq!(
            served_from(&placeholder, Some(&listing(&[("a", "unloaded")]))),
            None
        );
        assert_eq!(
            served_from(
                &placeholder,
                Some(&listing(&[("a", "loaded"), ("b", "loading")]))
            ),
            None
        );
        assert_eq!(
            served_from(&placeholder, Some(&listing(&[("a", "resident")]))),
            None
        );
        assert_eq!(
            served_from(&placeholder, Some(&serde_json::json!({"data": []}))),
            None
        );
    }

    #[test]
    fn a_loaded_and_a_sleeping_model_are_two_resident_and_no_answer() {
        let placeholder = serde_json::json!({"role": "router"});
        let list = serde_json::json!({"data": [
            {"id": "a", "status": {"value": "loaded"}},
            {"id": "b", "status": {"value": "sleeping"}}]});
        assert_eq!(served_from(&placeholder, Some(&list)), None);
    }

    #[test]
    fn a_routers_ids_are_what_a_fallback_is_checked_against() {
        let list = serde_json::json!({"data": [
            {"id": "qwen3.6-35b-a3b", "status": {"value": "unloaded"}},
            {"id": "gemma-4-26b-a4b", "status": {"value": "unloaded"}}]});
        assert_eq!(
            router_ids(Some(&list)),
            Some(vec![
                "qwen3.6-35b-a3b".to_string(),
                "gemma-4-26b-a4b".to_string()
            ])
        );
        assert_eq!(router_ids(None), None, "no list is not an empty list");
        assert_eq!(
            router_ids(Some(&serde_json::json!({"data": []}))),
            Some(vec![])
        );
    }

    /// A 5xx and no answer at all are the server's; a 4xx is an answer; a
    /// timeout is its own class, because the error cannot say whose it is.
    #[test]
    fn who_a_failure_belongs_to_decides_its_error() {
        let schema = serde_json::json!({"type": "object"});
        let url = stub(vec![
            (503, r#"{"error":{"message":"Loading model"}}"#.into()),
            (400, r#"{"error":{"message":"bad request"}}"#.into()),
        ]);
        let c = ChatClient::at(&url);
        assert!(matches!(
            c.complete_schema("s", "u", "x", schema.clone()),
            Err(Error::Transport(_))
        ));
        assert!(matches!(
            c.complete_schema("s", "u", "x", schema.clone()),
            Err(Error::Other(_))
        ));
        let c = ChatClient::at(&dead_server());
        assert!(matches!(
            c.complete_schema("s", "u", "x", schema.clone()),
            Err(Error::Transport(_))
        ));
        let mut c = ChatClient::at(&hung_server());
        c.timeout = Duration::from_secs(1);
        match c.complete_schema("s", "u", "x", schema) {
            Err(Error::Timeout(m)) => assert!(m.contains("timed out"), "{m}"),
            other => panic!("a timeout is neither Other nor Transport: {other:?}"),
        }
    }

    /// The canary and the request made after it answered are sent once: the
    /// backoff is for a server recovering, which the canary rules out (found
    /// on review of #22: ~100 s of sleep per episode otherwise). The 200
    /// queued behind each 503 is never reached.
    #[test]
    fn the_canary_and_the_request_after_it_do_not_back_off() {
        let busy = (503, r#"{"error":{"message":"Loading model"}}"#.to_string());
        let ok = answer(r#"{"a":1}"#);
        let mut c = ChatClient::at(&stub(vec![busy.clone(), ok.clone()]));
        c.retry_delays = vec![Duration::from_millis(50)];
        assert!(matches!(
            c.canary("s", ChatClient::json_object_format()),
            Err(Error::Transport(_))
        ));
        let mut c = ChatClient::at(&stub(vec![busy.clone(), ok.clone()]));
        c.retry_delays = vec![Duration::from_millis(50)];
        let once = c.complete_schema_once("s", "u", "x", serde_json::json!({"type": "object"}));
        assert!(matches!(once, Err(Error::Transport(_))), "{once:?}");
        // The ordinary request still waits the load out.
        let mut c = ChatClient::at(&stub(vec![busy, ok]));
        c.retry_delays = vec![Duration::from_millis(50)];
        assert!(c
            .complete_schema("s", "u", "x", serde_json::json!({"type": "object"}))
            .is_ok());
    }

    /// A router loading a model answers 503 first; the retry is the warm-up.
    #[test]
    fn a_503_while_a_model_loads_is_retried_into_an_answer() {
        let ok = r#"{"choices":[{"message":{"content":"{\"a\":1}"},"finish_reason":"stop"}]}"#;
        let url = stub(vec![
            (503, r#"{"error":{"message":"Loading model"}}"#.into()),
            (200, ok.into()),
        ]);
        let mut c = ChatClient::at(&url);
        c.retry_delays = vec![Duration::from_millis(50)];
        let got = c.complete_schema("s", "u", "x", serde_json::json!({"type": "object"}));
        assert!(got.is_ok(), "{got:?}");
    }

    /// A 503 from `/health` is a server loading, not an absent one: waited
    /// for and then shared, never refused as "nothing is answering" or
    /// spawned over (found on review of #22). A 404 is still not adopted.
    #[test]
    fn a_loading_server_is_waited_for_not_spawned_over() {
        let loading = (503, r#"{"error":{"message":"Loading model"}}"#.to_string());
        let url = stub(vec![loading.clone(), loading.clone(), (200, "{}".into())]);
        assert_eq!(health(&url), Health::Loading);
        let got = Backend::resolve_within(&url, "m", Duration::from_secs(30));
        assert!(matches!(got, Ok(Backend::Shared { .. })), "{:?}", got.err());

        let url = stub(vec![loading.clone(), loading.clone(), loading]);
        let got = Backend::resolve_within(&url, "m", Duration::from_millis(300));
        let Err(Error::Other(m)) = got else {
            panic!("a load that does not finish is an error, not a spawn")
        };
        assert!(m.contains("still loading"), "{m}");

        let url = stub(vec![(404, "{}".into())]);
        assert_eq!(health(&url), Health::Absent);

        // A reset is no clean answer: refused, never spawned over.
        let url = dead_server();
        assert!(
            matches!(health(&url), Health::Unknown(_)),
            "{:?}",
            health(&url)
        );
        let Err(Error::Other(m)) = Backend::resolve_within(&url, "m", Duration::from_secs(1))
        else {
            panic!("an unknown health is a refusal")
        };
        assert!(m.contains("neither uses it nor starts"), "{m}");
    }

    /// The canary passes on any JSON answer, and fails on a refusal and on
    /// silence — each the way the request it stands for would have.
    #[test]
    fn the_canary_fails_where_the_request_would() {
        let fmt = || ChatClient::json_object_format();
        let c = ChatClient::at(&stub(vec![answer(r#"{"anything":[]}"#)]));
        assert!(c.canary("s", fmt()).is_ok());
        let refused = r#"{"error":{"message":"model 'llama-server' not found"}}"#;
        let c = ChatClient::at(&stub(vec![(400, refused.into())]));
        assert!(c.canary("s", fmt()).is_err());
        let c = ChatClient::at(&hung_server());
        assert!(matches!(c.canary("s", fmt()), Err(Error::Timeout(_))));
        let c = ChatClient::at(&stub(vec![answer("not json")]));
        assert!(c.canary("s", fmt()).is_err(), "an answer that is not JSON");
    }

    #[test]
    fn a_single_model_server_answers_with_its_alias() {
        let props = serde_json::json!({"model_alias": "qwen3.6-35b-a3b"});
        assert_eq!(
            served_from(&props, None).as_deref(),
            Some("qwen3.6-35b-a3b")
        );
        assert_eq!(
            served_from(&serde_json::json!({"model_alias": ""}), None),
            None
        );
        // The router's placeholder is refused even without its role.
        assert_eq!(
            served_from(&serde_json::json!({"model_alias": "llama-server"}), None),
            None
        );
    }

    #[test]
    fn plain_json_is_untouched() {
        assert_eq!(strip_code_fence(r#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(strip_code_fence("  {\"a\":1}\n"), r#"{"a":1}"#);
    }

    #[test]
    fn fenced_json_is_unwrapped() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), r#"{"a":1}"#);
        assert_eq!(strip_code_fence("```\n{\"a\":1}\n```"), r#"{"a":1}"#);
    }

    #[test]
    fn an_unclosed_fence_is_left_to_fail_loudly() {
        // Half a fence means the answer was truncated. Guessing at the rest
        // would turn a visible failure into a silently partial extraction.
        let s = "```json\n{\"a\":1}";
        assert_eq!(strip_code_fence(s), s);
    }

    #[test]
    fn the_default_endpoint_is_mechas_server_not_ollamas() {
        // Not 11434. If this regresses, a shared install stops finding the
        // model mecha already has loaded and starts looking for its own.
        assert_eq!(DEFAULT_BASE_URL, "http://127.0.0.1:8080");
    }

    #[test]
    fn max_tokens_leaves_room_after_the_reasoning_budget() {
        // The server bounds thinking at 4096. A max_tokens at or below that
        // is how a turn comes back with reasoning and an empty answer — the
        // exact shape of the bug this module exists to stop returning.
        // Checked at compile time: the bound is a constant.
        const { assert!(DEFAULT_MAX_TOKENS > 4096) };
    }

    #[test]
    fn a_port_is_parsed_out_of_the_base_url() {
        assert_eq!(port_of("http://127.0.0.1:8080"), Some(8080));
        assert_eq!(port_of("http://127.0.0.1:8080/"), Some(8080));
        assert_eq!(port_of("http://localhost:11434"), Some(11434));
    }

    #[test]
    fn a_url_with_no_port_refuses_rather_than_guessing() {
        // Spawning against a guessed port would start a server nothing talks
        // to, and leave the caller waiting out the full health timeout.
        assert_eq!(port_of("http://localhost"), None);
    }
}
