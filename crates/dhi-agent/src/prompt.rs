//! Prompt loading and rendering.
//!
//! copy ARTEX: one static template per role (`ops/prompts/planner.md`,
//! `ops/prompts/worker.md`), variables are `{{name}}` from a closed set, an
//! unknown `{{…}}` in a template is a startup error, and a declared variable
//! missing from the render is a startup error rather than a runtime surprise.
//! The planner system prompt is static and the graph snapshot goes in the user
//! turn; the worker intent goes in the system prompt.
//!
//! DHI adaptation, reason: a code-owned invariant tail is appended to every
//! rendered system prompt, so a template edit cannot delete the invariants.
//!
//! Template layout: everything before the first `{{variable}}` is the static
//! system head. The first variable belongs to the system turn; every
//! following variable, and the text between them, belongs to the user turn.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::profile::{Role, find_prompts_dir};

/// Appended to every rendered system prompt, after the template.
pub const INVARIANT_TAIL: &str = "<!-- INVARIANTS — DO NOT REMOVE -->\n\
You never decide domain truth. A goal is met only when a confirmed fact proves it.\n\
Planner: you never run tools. You add one intent or prove one goal from a confirmed fact.\n\
Worker: you never create intents. Write one fact draft through `record_fact`; domain tool calls are decided by the guard.\n\
Use only the output mechanism this role is given. Never answer in JSON text.\n";

/// The only variables this crate defines: `goal`, `goals`, `facts`,
/// `open_intents`, `failures`, `required_topology`, `swarm_workers`,
/// `instructions`, `intent`, `context`. Nothing else is renderable.
pub const DECLARED_VARIABLES: &[&str] = &[
    "goal",
    "goals",
    "facts",
    "open_intents",
    "failures",
    "required_topology",
    "swarm_workers",
    "instructions",
    "intent",
    "context",
];

/// Variables of one planner render. The graph snapshot is `facts` and goes in
/// the user turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerVars {
    pub goal: String,
    pub goals: String,
    pub facts: String,
    pub open_intents: usize,
    pub failures: String,
    /// The selector-required topology as a display value, `none` when the
    /// plan stays optional.
    pub required_topology: String,
    /// The selector-required swarm worker count as a display value, `none`
    /// when the count stays with the planner.
    pub swarm_workers: String,
    /// Pending human instructions, one bullet each, or `none`.
    pub instructions: String,
}

/// Variables of one worker render. The intent text goes in the system prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerVars {
    pub intent: String,
    pub context: String,
}

impl PlannerVars {
    /// The record form written to `agent_runs.variables`.
    pub fn to_variables(&self) -> serde_json::Value {
        serde_json::json!({
            "goal": self.goal,
            "goals": self.goals,
            "facts": self.facts,
            "open_intents": self.open_intents,
            "failures": self.failures,
            "required_topology": self.required_topology,
            "swarm_workers": self.swarm_workers,
            "instructions": self.instructions,
        })
    }

    fn pairs(&self) -> [(&'static str, String); 8] {
        [
            ("goal", self.goal.clone()),
            ("goals", self.goals.clone()),
            ("facts", self.facts.clone()),
            ("open_intents", self.open_intents.to_string()),
            ("failures", self.failures.clone()),
            ("required_topology", self.required_topology.clone()),
            ("swarm_workers", self.swarm_workers.clone()),
            ("instructions", self.instructions.clone()),
        ]
    }
}

impl WorkerVars {
    /// The record form written to `agent_runs.variables`.
    pub fn to_variables(&self) -> serde_json::Value {
        serde_json::json!({
            "intent": self.intent,
            "context": self.context,
        })
    }

    fn pairs(&self) -> [(&'static str, String); 2] {
        [
            ("intent", self.intent.clone()),
            ("context", self.context.clone()),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    /// Raw file, for `agent_runs.prompt` reconstruction.
    pub source: String,
    /// Hash of `source` + the invariant tail, recorded as `prompt_hash`.
    pub hash: String,
}

impl PromptTemplate {
    /// Stable version label, e.g. `planner@5b2e0d1c7a91`.
    pub fn version(&self, role: Role) -> String {
        format!("{}@{}", role.as_str(), self.hash)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSet {
    pub planner: PromptTemplate,
    pub worker: PromptTemplate,
}

impl PromptSet {
    /// Reads both role templates from `dir` and validates them. Every
    /// `{{…}}` must be one of the declared variables.
    pub fn load(dir: &Path) -> Result<Self, PromptError> {
        let planner =
            PromptTemplate::load_file(dir.join(Role::Planner.prompt_file()), Role::Planner)?;
        let worker = PromptTemplate::load_file(dir.join(Role::Worker.prompt_file()), Role::Worker)?;
        Ok(Self { planner, worker })
    }

    /// Loads from `DHI_PROMPTS_DIR`, else the nearest `ops/prompts` above
    /// `start`. A startup error when neither exists.
    pub fn discover(start: &Path) -> Result<Self, PromptError> {
        let dir = find_prompts_dir(start).ok_or_else(|| PromptError::DirMissing {
            searched: start.display().to_string(),
        })?;
        Self::load(&dir)
    }

    /// Planner: `(system, user)`. The system prompt is the template head plus
    /// the invariant tail; the goal and graph snapshot go in the user turn.
    pub fn render_planner(&self, vars: &PlannerVars) -> Result<(String, String), PromptError> {
        self.planner.render(Role::Planner, &vars.pairs())
    }

    /// Worker: `(system, user)`. The intent text is in the system prompt; the
    /// user turn carries the context.
    pub fn render_worker(&self, vars: &WorkerVars) -> Result<(String, String), PromptError> {
        self.worker.render(Role::Worker, &vars.pairs())
    }

    pub fn get(&self, role: Role) -> &PromptTemplate {
        match role {
            Role::Planner => &self.planner,
            Role::Worker => &self.worker,
        }
    }
}

impl PromptTemplate {
    pub fn load_file(path: std::path::PathBuf, role: Role) -> Result<Self, PromptError> {
        let source = std::fs::read_to_string(&path).map_err(|err| PromptError::Io {
            path: path.display().to_string(),
            message: err.to_string(),
        })?;
        Self::parse(source, role)
    }

    /// Validates the template and hashes it together with the invariant tail.
    pub fn parse(source: String, role: Role) -> Result<Self, PromptError> {
        // Validation pass: every declared variable is accepted, so this only
        // trips on an unknown or unterminated `{{…}}`.
        validate(&source, role)?;

        let mut hasher = Sha256::new();
        hasher.update(source.as_bytes());
        hasher.update(b"\n--\n");
        hasher.update(INVARIANT_TAIL.as_bytes());
        let hash = hex(hasher.finalize())[..12].to_string();

        Ok(Self { source, hash })
    }

    fn render(
        &self,
        role: Role,
        given: &[(&'static str, String)],
    ) -> Result<(String, String), PromptError> {
        // The first variable stays in the system prompt; everything after it
        // is the user turn. Text and values stream into one scratch buffer so
        // no two closures ever own a piece of it.
        let sink = Sink::default();
        split(
            &self.source,
            role,
            &mut |name| {
                let value = given
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
                    .ok_or_else(|| PromptError::MissingVariable {
                        role: role.to_string(),
                        name: name.to_string(),
                    })?;
                sink.push_var(&value);
                Ok(())
            },
            &mut |text| sink.push(text),
        )?;

        let (head, rest) = sink.finish();
        let system = format!("{}\n\n{INVARIANT_TAIL}", head.trim_end());
        let user = rest.trim().to_string();
        Ok((system, user))
    }
}

/// Accumulates a template render: text before the first variable goes to the
/// system head (with that variable's value), everything after to the user
/// turn. Interior mutability lets the text and variable callbacks share one
/// sink.
#[derive(Default)]
struct Sink {
    inner: std::cell::RefCell<SinkInner>,
}

#[derive(Default)]
struct SinkInner {
    head: String,
    rest: String,
    seen_variable: bool,
}

impl Sink {
    fn push(&self, text: &str) {
        let mut inner = self.inner.borrow_mut();
        if inner.seen_variable {
            inner.rest.push_str(text);
        } else {
            inner.head.push_str(text);
        }
    }

    fn push_var(&self, value: &str) {
        let mut inner = self.inner.borrow_mut();
        if inner.seen_variable {
            inner.rest.push_str(value);
        } else {
            inner.head.push_str(value);
        }
        // Everything after the first variable, even a literal run, is user
        // turn. Set the flag after copying the value so an empty first value
        // still moves the boundary.
        inner.seen_variable = true;
    }

    fn finish(self) -> (String, String) {
        let inner = self.inner.into_inner();
        (inner.head, inner.rest)
    }
}

/// Startup validation: every `{{…}}` must be a declared variable and every
/// `{{` must be closed.
fn validate(source: &str, role: Role) -> Result<(), PromptError> {
    split(source, role, &mut |_name| Ok(()), &mut |_text: &str| {})
}

/// Walks `source` split on `{{name}}`. `on_text` receives each literal run in
/// order, `on_var` receives each variable name (already validated as
/// declared). Unknown and unterminated variables are errors here, so every
/// caller gets the startup checks.
fn split(
    source: &str,
    role: Role,
    on_var: &mut dyn FnMut(&str) -> Result<(), PromptError>,
    on_text: &mut dyn FnMut(&str),
) -> Result<(), PromptError> {
    let mut cursor = source;
    while let Some((before, after_open)) = cursor.split_once("{{") {
        let Some((name, after)) = after_open.split_once("}}") else {
            return Err(PromptError::UnterminatedVar {
                role: role.to_string(),
            });
        };
        let name = name.trim();
        if !DECLARED_VARIABLES.contains(&name) {
            return Err(PromptError::UnknownVariable {
                role: role.to_string(),
                name: name.to_string(),
            });
        }
        on_text(before);
        on_var(name)?;
        cursor = after;
    }
    on_text(cursor);
    Ok(())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("prompt file {path}: {message}")]
    Io { path: String, message: String },
    #[error("no ops/prompts directory found from {searched}")]
    DirMissing { searched: String },
    #[error("{role} prompt uses unknown variable: {{{{{name}}}}}")]
    UnknownVariable { role: String, name: String },
    #[error("{role} prompt has an unterminated variable: missing `}}}}`")]
    UnterminatedVar { role: String },
    #[error("{role} prompt declares variable {{{{{name}}}}} but the render did not supply it")]
    MissingVariable { role: String, name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops_prompts() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ops/prompts")
    }

    fn loaded() -> PromptSet {
        PromptSet::load(&ops_prompts()).expect("ops/prompts loads")
    }

    fn planner_vars() -> PlannerVars {
        PlannerVars {
            goal: "prove a forced mate".to_string(),
            goals: "- [open] 0123abcd-0000-0000-0000-000000000001 prove a forced mate".to_string(),
            facts: "- [origin] 0123abcd-0000-0000-0000-000000000002 Origin: prove a forced mate\n\
                    - [confirmed] 89abcd00-0000-0000-0000-000000000003 found a rook lift"
                .to_string(),
            open_intents: 2,
            failures: "- DHI+ swarm team produced no successful worker result".to_string(),
            required_topology: "none".to_string(),
            swarm_workers: "none".to_string(),
            instructions: "- focus on the endgame first".to_string(),
        }
    }

    fn worker_vars() -> WorkerVars {
        WorkerVars {
            intent: "Evaluate the rook lift".to_string(),
            context: "- fact 89abcdef: the rook can reach h5\n- guard: read-only".to_string(),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dhi-prompts-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = ops_prompts();
        for file in ["planner.md", "worker.md"] {
            std::fs::copy(source.join(file), dir.join(file)).unwrap();
        }
        dir
    }

    #[test]
    fn ops_prompts_load_and_declare_only_good_variables() {
        let set = loaded();
        assert!(set.planner.hash.len() >= 8);
        assert_ne!(set.planner.hash, set.worker.hash);
        assert_eq!(
            set.planner.version(Role::Planner),
            format!("planner@{}", set.planner.hash)
        );
        assert_eq!(
            set.worker.version(Role::Worker),
            format!("worker@{}", set.worker.hash)
        );
    }

    /// Tools the worker does not have in this slice.
    const WORKER_RETIRED_TOOLS: &[&str] = &["add_intent", "prove_goal", "report_finding"];

    #[test]
    fn ops_prompts_are_free_of_retired_protocol_wording() {
        for file in ["planner.md", "worker.md"] {
            let text = std::fs::read_to_string(ops_prompts().join(file)).unwrap();
            assert!(
                !text.contains("report_result"),
                "{file} still names report_result"
            );
            assert!(!text.contains("```json"), "{file} still has a JSON block");
            assert!(!text.contains("\"done\""), "{file} still has a done field");
            assert!(
                !text.contains("\"accepted\""),
                "{file} still has an accepted field"
            );
        }
        let planner = std::fs::read_to_string(ops_prompts().join("planner.md")).unwrap();
        assert!(planner.contains("add_intent"));
        assert!(planner.contains("prove_goal"));
        assert!(planner.contains("NO PLAN:"));
        let worker = std::fs::read_to_string(ops_prompts().join("worker.md")).unwrap();
        assert!(worker.contains("record_fact"));
        for tool in WORKER_RETIRED_TOOLS {
            assert!(!worker.contains(tool), "worker still names `{tool}`");
        }
    }

    #[test]
    fn planner_render_puts_graph_in_the_user_turn() {
        let set = loaded();
        let vars = planner_vars();
        let (system, user) = set.render_planner(&vars).unwrap();

        assert!(
            user.contains("prove a forced mate") && user.contains("89abcd00"),
            "user turn carries goal and facts: {user}"
        );
        assert!(
            user.contains("0123abcd-0000-0000-0000-000000000001"),
            "goals carry full ids: {user}"
        );
        assert!(
            user.contains("0123abcd-0000-0000-0000-000000000002"),
            "facts carry full ids: {user}"
        );
        assert!(user.contains("Open intents: 2"));
        assert!(
            user.contains("no successful worker result"),
            "recent failures reach the planner: {user}"
        );
        assert!(
            !system.contains("89abcd00") && !system.contains("found a rook lift"),
            "system prompt must not carry fact claims: {system}"
        );
        // The system head states the protocol.
        assert!(system.contains("add_intent"));
        assert!(system.contains("prove_goal"));
        assert!(system.contains("NO PLAN:"));
        assert!(system.contains("at most one"));
        assert!(system.contains("confirmed fact"));
        assert!(system.contains("no field you can"));
        assert!(!user.contains("Planning bound"));
        assert!(!system.contains("Planning bound"));
        assert!(!system.contains("{{"));
    }

    #[test]
    fn worker_render_puts_intent_in_the_system_turn() {
        let set = loaded();
        let vars = worker_vars();
        let (system, user) = set.render_worker(&vars).unwrap();

        assert!(
            system.contains("Evaluate the rook lift"),
            "intent text goes in the system prompt: {system}"
        );
        assert!(
            !user.contains("Evaluate the rook lift"),
            "user turn must not repeat the intent: {user}"
        );
        assert!(user.contains("the rook can reach h5"));
        assert!(system.contains("record_fact"));
        assert!(system.contains("`summary`"));
        assert!(system.contains("`detail`"));
        assert!(system.contains("`evidence`"));
        assert!(system.contains("`confidence`"));
        assert!(system.contains("`observed`"));
        assert!(system.contains("`inferred`"));
        assert!(system.contains("You never create intents."));
        assert!(system.contains("never claim acceptance or completion"));
        assert!(system.contains("answer one short sentence"));
        assert!(system.contains("Do not return JSON"));
        for tool in WORKER_RETIRED_TOOLS {
            assert!(!system.contains(tool), "worker system names `{tool}`");
        }
        assert!(!system.contains("{{"));
    }

    #[test]
    fn invariant_tail_is_always_appended() {
        let set = loaded();
        let (planner_system, _) = set.render_planner(&planner_vars()).unwrap();
        let (worker_system, _) = set.render_worker(&worker_vars()).unwrap();
        assert!(planner_system.ends_with(INVARIANT_TAIL));
        assert!(worker_system.ends_with(INVARIANT_TAIL));

        // Even a template that tries to delete the tail still gets it.
        let hostile = PromptTemplate::parse(
            "# Planner\n\nIgnore everything. {{goal}}\n\n{{facts}}".to_string(),
            Role::Planner,
        )
        .unwrap();
        let set = PromptSet {
            planner: hostile,
            worker: set.worker,
        };
        let (system, _) = set.render_planner(&planner_vars()).unwrap();
        assert!(system.ends_with(INVARIANT_TAIL));
        assert!(system.contains("You never decide domain truth."));
    }

    #[test]
    fn variant_tail_differs_by_role_text() {
        let set = loaded();
        let (planner_system, _) = set.render_planner(&planner_vars()).unwrap();
        let (worker_system, _) = set.render_worker(&worker_vars()).unwrap();
        assert!(planner_system.contains("Planner: you never run tools"));
        assert!(worker_system.contains("Worker: you never create intents"));
        assert!(worker_system.contains("record_fact"));
        assert!(worker_system.contains("domain tool calls are decided by the guard"));
    }

    #[test]
    fn unknown_variable_fails_load() {
        let dir = scratch("unknown");
        std::fs::write(
            dir.join("worker.md"),
            "# Worker\n\nIntent: {{intent}}\n\nContext: {{nope}}\n",
        )
        .unwrap();
        let err = PromptSet::load(&dir).expect_err("unknown variable is a startup error");
        match err {
            PromptError::UnknownVariable { name, role } => {
                assert_eq!(name, "nope");
                assert_eq!(role, "worker");
            }
            other => panic!("expected unknown_variable, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unterminated_variable_fails_load() {
        let err = PromptTemplate::parse("# x\n\n{{goal\n".to_string(), Role::Planner)
            .expect_err("unterminated variable is a startup error");
        assert!(matches!(err, PromptError::UnterminatedVar { .. }));
    }

    #[test]
    fn missing_declared_variable_fails_render() {
        let template = PromptTemplate::parse(
            "# Worker\n\nIntent: {{intent}}\n\nContext: {{context}}\n".to_string(),
            Role::Worker,
        )
        .unwrap();
        let set = PromptSet {
            planner: template.clone(),
            worker: template,
        };
        // `context` is missing from the data, not from the template.
        let err = set
            .render_worker(&WorkerVars {
                intent: "do it".to_string(),
                context: String::new(),
            })
            .err();
        assert!(err.is_none(), "an empty value is present, not missing");

        let template = PromptTemplate::parse(
            "# Worker\n\n{{intent}}\n\n{{context}}\n".to_string(),
            Role::Worker,
        )
        .unwrap();
        let err = template
            .render(Role::Worker, &[("intent", "do it".to_string())])
            .expect_err("a declared variable with no value is a startup error");
        assert!(matches!(
            err,
            PromptError::MissingVariable { ref name, .. } if name == "context"
        ));
    }

    #[test]
    fn editing_a_prompt_changes_the_system_turn_and_the_hash() {
        let original = loaded();
        let dir = scratch("edit");
        let worker = std::fs::read_to_string(dir.join("worker.md")).unwrap();
        std::fs::write(
            dir.join("worker.md"),
            worker.replace("# Worker prompt", "# Worker prompt (v2)"),
        )
        .unwrap();

        let edited = PromptSet::load(&dir).unwrap();
        let (before, _) = original.render_worker(&worker_vars()).unwrap();
        let (after, _) = edited.render_worker(&worker_vars()).unwrap();
        assert_ne!(before, after);
        assert!(after.contains("(v2)"));
        assert_ne!(original.worker.hash, edited.worker.hash);
        assert_eq!(
            original.planner.hash, edited.planner.hash,
            "the planner template did not change"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_finds_ops_prompts_from_the_manifest_dir() {
        let set = PromptSet::discover(&ops_prompts()).unwrap();
        assert!(set.render_planner(&planner_vars()).is_ok());
    }

    #[test]
    fn variables_record_form_is_typed() {
        let planner = planner_vars().to_variables();
        assert_eq!(planner["goal"], "prove a forced mate");
        assert_eq!(planner["open_intents"], 2);
        let worker = worker_vars().to_variables();
        assert_eq!(worker["intent"], "Evaluate the rook lift");
    }
}
