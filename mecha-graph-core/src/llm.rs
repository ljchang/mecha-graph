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
        if health_ok(base_url) {
            return Ok(Backend::Shared {
                base_url: base_url.to_string(),
            });
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
        let binary = cfg.server_bin.as_deref().unwrap_or("llama-server").to_string();

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

        let child = cmd.spawn().map_err(|e| {
            Error::Other(format!("could not start '{binary}': {e}"))
        })?;
        let mut backend = Backend::Managed {
            base_url: base_url.to_string(),
            child,
        };

        let deadline = Instant::now() + SPAWN_HEALTH_TIMEOUT;
        while Instant::now() < deadline {
            if health_ok(base_url) {
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

/// What the server actually has loaded, from its own `/props`.
///
/// Asked rather than asserted, because the shared server belongs to mecha and
/// mecha's model is the user's to change. llama-server serves whatever is
/// loaded and ignores the `model` field of a request, so a client that names a
/// model is not selecting one — it is only deciding what to write down. Naming
/// `qwen3.6-35b-a3b` while the box actually serves gemma4 would put a false
/// value in `extract_state.model`, which is the one column that answers "what
/// produced this fact" and the one PROMPT_VERSION re-extraction keys off.
fn served_model(base_url: &str) -> Option<String> {
    let resp = ureq::get(&format!("{base_url}/props"))
        .timeout(Duration::from_millis(1500))
        .call()
        .ok()?;
    let body: serde_json::Value = resp.into_json().ok()?;
    body.get("model_alias")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

fn health_ok(base_url: &str) -> bool {
    // /health rather than / on purpose: it is llama-server's, so an ollama
    // listening on the same port answers 404 and is correctly not adopted.
    ureq::get(&format!("{base_url}/health"))
        .timeout(Duration::from_millis(1500))
        .call()
        .is_ok()
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
    backend: Backend,
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
        let model = match served_model(backend.base_url()) {
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
            None => wanted,
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
        self.post(system, user, serde_json::json!({ "type": "json_object" }))
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
        self.post(
            system,
            user,
            serde_json::json!({
                "type": "json_schema",
                "json_schema": { "name": name, "strict": true, "schema": schema },
            }),
        )
    }

    fn post(
        &self,
        system: &str,
        user: &str,
        response_format: serde_json::Value,
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

        let resp = ureq::post(&format!("{}/v1/chat/completions", self.base_url()))
            .timeout(self.timeout)
            .send_json(body)
            .map_err(|e| match e {
                // A refusal here arrives as a real status with a JSON body
                // naming the bad field. Swallowing it into "request failed"
                // is how a one-line flag mistake costs an evening.
                ureq::Error::Status(code, r) => {
                    let detail = r.into_string().unwrap_or_default();
                    Error::Other(format!(
                        "llama-server {code}: {}",
                        detail.chars().take(400).collect::<String>()
                    ))
                }
                other => Error::Other(format!(
                    "llama-server at {} unreachable: {other}",
                    self.base_url()
                )),
            })?;

        let payload: serde_json::Value = resp
            .into_json()
            .map_err(|e| Error::Other(format!("bad llama-server response: {e}")))?;

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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(DEFAULT_MAX_TOKENS > 4096);
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
