//! A two-agent demo built on `condesate`: **mnemosyne**, an Opus-driven main
//! agent whose own conversation transcript is periodically wiped, backed by
//! **morpheus**, a Sonnet-driven background curator that turns the raw
//! conversation into `condesate::faraday`'s permanent memory structures
//! (Diary, IdeaBook, SlipIndex) before each wipe — so mnemosyne keeps working
//! from a bounded context while still being able to recall anything that
//! happened arbitrarily long ago, via `recall_memory`.
//!
//! Like `leader-search`, morpheus is a genuinely attenuated child of
//! mnemosyne (`condesate::GuardedServices::spawn_child`) — spawned once at
//! startup and reused for the life of the process, not per-message. stdout is
//! pure chat by default; set `VERBOSE=1` to see every check either agent
//! makes live on stderr, and/or `AUDIT_LOG_PATH` to append them to a file.
//! See `memory.rs` for how `faraday` gets wired into real tools. Curation
//! runs after every turn by default so memory (and the dashboard reading it)
//! keeps up with the conversation; `--curate-every N` / `--reset-every N`
//! change that cadence — see `parse_args` and the README.
//!
//! A read-only diagnostic HTTP API also starts alongside the chat loop (see
//! `api.rs`) — what `dashboard/`, a small React app, polls to show every
//! agent, everything in memory, and the audit trail live while you chat. Pass
//! `--debug` to have mnemosyne start the dashboard's own dev server and open
//! it in your browser automatically (see `dashboard.rs`) — independent of
//! `VERBOSE`, which controls stderr trace detail instead.
//!
//! Run: `cargo run -p mnemosyne [-- --debug] [-- --curate-every N]` (needs
//! PROVIDER + an API key, e.g. in `.env`).

mod api;
mod dashboard;
mod memory;
mod model;

use anyhow::Result;
use condesate::{
    Action, Agent, AgentContext, AgentLoop, AgentManifest, AuditSink, BasicAgent, Bus, Condition,
    FileAudit, GrantSet, GuardedServices, HarnessId, Kernel, MemoryAudit, Message, ModelClass,
    Pattern, ReActLoop, ResourcePattern, Rule, RuleSetPolicy, ServiceHandle, Storage, SubjectMatch,
    SystemClock, TenantId, Tool, ToolSpec, TracingAudit, TrustTier,
};
use memory::{
    JotIdea, ListLiveIdeas, ListMemoryTopics, MemoryBank, RecallMemory, RecordDiaryEntry,
    ReviseIdea, StrikeIdea, TagSlip,
};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

const MNEMOSYNE_ID: &str = "mnemosyne";
const MORPHEUS_ID: &str = "morpheus";
/// How many think-steps morpheus gets per curation pass — several tool calls
/// (record, tag, maybe jot/revise a few ideas) are expected per batch of
/// messages, unlike mnemosyne's ordinary 8-step conversational turns.
const MORPHEUS_MAX_STEPS: usize = 16;
/// Runaway-loop backstop: the most tool calls morpheus's grants permit in
/// one curation pass, via `Condition::StepsUnder` — the same primitive
/// `condesate::security::policy` documents as existing exactly for this. Set
/// below `MORPHEUS_MAX_STEPS` so a stuck model (e.g. re-recording the same
/// fact) gets a visible "denied" observation to react to, rather than
/// silently running out of think-steps with no explanation in the
/// transcript. Curation resets this every pass — see `curate`'s
/// `begin_activation` call — so it never accumulates across passes.
///
/// Live-observed and worth being honest about: on small, low-information
/// batches (e.g. one fact restated across two short turns), morpheus
/// sometimes re-issues `record_diary_entry` for the same fact several times
/// before stopping, even with an explicit instruction not to — a real
/// prompted-tool-calling quirk on repetitive input, not a bug this backstop
/// is meant to fully prevent (see `README.md`'s "What isn't real" section).
/// This cap bounds the wasted calls when it happens; it does not claim to
/// eliminate the behavior itself. 8 comfortably covers legitimate work
/// (record + tag for 3-4 distinct facts) while keeping the worst case cheap.
const MORPHEUS_STEPS_PER_PASS_CAP: u32 = 5;
/// The same backstop, staggered by curation stage — and the reason it's three
/// numbers rather than one.
///
/// `Condition::StepsUnder` is evaluated against the activation's *shared*
/// step counter, so a single cap across all six tools is a budget the first
/// stage can spend entirely. Live-observed, and exactly what happened: on a
/// one-turn batch morpheus re-recorded the same fact seven times, hit the cap,
/// and was then denied `tag_slip` — the pass produced no slips and never
/// reached `jot_idea` at all, so the IdeaBook stayed permanently empty. The
/// backstop meant to bound a runaway had starved the rest of the job.
///
/// Giving the later stages higher ceilings reserves budget for them: once the
/// counter passes 5 the recorder stops being permitted while tagging and
/// idea-jotting still are. A runaway recorder now costs its own stage, not
/// the whole pass.
const MORPHEUS_TAG_STEPS_CAP: u32 = 9;
const MORPHEUS_IDEA_STEPS_CAP: u32 = 13;

/// The root authority: broad tool-invoke (covers both agents' tool names —
/// each still only *requests* the narrow slice it actually needs, see
/// `morpheus_manifest`) and the one right mnemosyne needs to bring morpheus
/// into existence at all.
fn root_grants() -> Vec<Rule> {
    vec![
        Rule::allow("tools", SubjectMatch::default(), &[Action::Invoke], ResourcePattern::Tool(Pattern::Any)),
        Rule::allow(
            "spawn-morpheus",
            SubjectMatch::agent(MNEMOSYNE_ID),
            &[Action::Spawn],
            ResourcePattern::Spawn(Pattern::parse(MORPHEUS_ID)),
        ),
    ]
}

/// No shared storage is needed here — memory lives in `faraday`'s own
/// structures (see `memory::MemoryBank`), reached directly by the tools
/// below rather than through `GuardedServices::storage_get/set`. Both
/// principals still need a `ServiceHandle` to exist at all.
struct NullStorage;

#[async_trait::async_trait]
impl Storage for NullStorage {
    async fn get(&self, _k: &str) -> Result<Option<String>> {
        Ok(None)
    }
    async fn set(&self, _k: &str, _v: &str) -> Result<()> {
        Ok(())
    }
    async fn keys(&self, _p: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
}

fn model_class(role: &str) -> ModelClass {
    ModelClass { provider: "mnemosyne".into(), family: role.into(), revision: role.into(), embedding_space: None, quantization: None }
}

/// Builds mnemosyne's `GuardedServices` and returns the concrete
/// `MemoryAudit` sink alongside it — same shape as `leader-search`'s
/// `build_leader_services`, including the `VERBOSE`/`AUDIT_LOG_PATH` sink
/// chain (see that crate's README for the reasoning).
async fn build_mnemosyne_services(verbose: bool) -> Result<(Arc<GuardedServices>, Arc<MemoryAudit>)> {
    let grants = root_grants();
    let audit = MemoryAudit::new();
    let mut live_audit: Arc<dyn AuditSink> = audit.clone();
    if verbose {
        live_audit = Arc::new(TracingAudit { inner: live_audit });
    }
    if let Ok(path) = std::env::var("AUDIT_LOG_PATH") {
        eprintln!("[mnemosyne] audit trail: also appending to {path}");
        live_audit = Arc::new(FileAudit::open(&path, live_audit).await?);
    }
    let kernel = Kernel::new(GrantSet::new(grants.clone()), Arc::new(RuleSetPolicy::new()), live_audit, Arc::new(SystemClock));
    let raw = ServiceHandle {
        me: HarnessId::new(MNEMOSYNE_ID),
        roster: Arc::new(vec![HarnessId::new(MNEMOSYNE_ID), HarnessId::new(MORPHEUS_ID)]),
        storage: Arc::new(NullStorage),
        bus: Bus::new(HashMap::new()),
    };
    let manifest = AgentManifest {
        harness: HarnessId::new(MNEMOSYNE_ID),
        agent: MNEMOSYNE_ID.to_string(),
        model_class: model_class("main"),
        tenant: TenantId::new("local"),
        requested_trust: TrustTier::Privileged,
        requested: grants,
        cache_classes: vec![],
    };
    let admission = kernel.admit(&manifest, None);
    let services = kernel.attach(&admission, raw);
    services.begin_activation(MNEMOSYNE_ID, u64::MAX);
    Ok((Arc::new(services), audit))
}

/// Spawns morpheus as a real attenuated child of mnemosyne — once, at
/// startup, reused for the whole run (curating is a recurring background
/// job, not an on-demand tool the way `leader-search`'s search agent is).
/// Requests exactly the six tools it needs, each narrowly named rather than
/// reusing mnemosyne's own broad `Tool::Any` grant — the same
/// least-privilege shape `search_agent_manifest` uses in `leader-search`.
async fn spawn_morpheus(
    mnemosyne: &GuardedServices,
    bank: MemoryBank,
    model_name: &str,
) -> Result<(Arc<GuardedServices>, BasicAgent, String)> {
    let tool_names =
        ["record_diary_entry", "tag_slip", "list_live_ideas", "jot_idea", "revise_idea", "strike_idea"];
    let requested: Vec<Rule> = tool_names
        .iter()
        .map(|name| {
            // Per-stage ceilings against one shared counter — see
            // `MORPHEUS_TAG_STEPS_CAP` for why this isn't a single number.
            let max_steps = match *name {
                "record_diary_entry" => MORPHEUS_STEPS_PER_PASS_CAP,
                "tag_slip" => MORPHEUS_TAG_STEPS_CAP,
                _ => MORPHEUS_IDEA_STEPS_CAP,
            };
            Rule::allow(
                &format!("curate-{name}"),
                SubjectMatch::agent(MORPHEUS_ID),
                &[Action::Invoke],
                ResourcePattern::Tool(Pattern::Exact(name.to_string())),
            )
            .with_conditions(vec![Condition::StepsUnder { max_steps }])
        })
        .collect();
    let manifest = AgentManifest {
        harness: HarnessId::new(MORPHEUS_ID),
        agent: MORPHEUS_ID.to_string(),
        model_class: model_class("curator"),
        tenant: TenantId::new("local"),
        requested_trust: TrustTier::Standard,
        requested,
        cache_classes: vec![],
    };
    let raw = ServiceHandle {
        me: HarnessId::new(MORPHEUS_ID),
        roster: Arc::new(vec![HarnessId::new(MNEMOSYNE_ID), HarnessId::new(MORPHEUS_ID)]),
        storage: Arc::new(NullStorage),
        bus: Bus::new(HashMap::new()),
    };
    let services = Arc::new(
        mnemosyne
            .spawn_child(&manifest, raw)
            .await
            .map_err(|refusal| anyhow::anyhow!("spawning morpheus was denied: {refusal}"))?,
    );

    let (model, label) = model::choose_model(model_name)?;
    let harness = HarnessId::new(MORPHEUS_ID);
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(RecordDiaryEntry { bank: bank.clone(), harness }),
        Arc::new(TagSlip { bank: bank.clone() }),
        Arc::new(ListLiveIdeas { bank: bank.clone() }),
        Arc::new(JotIdea { bank: bank.clone() }),
        Arc::new(ReviseIdea { bank: bank.clone() }),
        Arc::new(StrikeIdea { bank }),
    ];
    let specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec()).collect();
    let agent = BasicAgent::new(MORPHEUS_ID, morpheus_system_prompt(&specs), model, tools);
    Ok((services, agent, label))
}

fn mnemosyne_system_prompt(tool_specs: &[ToolSpec]) -> String {
    format!(
        "You are mnemosyne. Your own memory of this conversation is bounded — it gets wiped \
         periodically to keep your context small — but nothing is actually lost: a background \
         curator (morpheus) continuously reads what you say and builds it into permanent, \
         topic-organized long-term memory behind the scenes. When you need something that isn't \
         in your visible recent conversation (the user references something earlier, or asks \
         what you know about a topic), call list_memory_topics to see what's tracked, then \
         recall_memory with a specific topic to pull it back — with its surrounding context, not \
         just a bare fact. Don't assume something is forgotten just because you can't see it \
         above — check memory first. Keep answers concise.\n\n{}",
        condesate::tool_instructions(tool_specs)
    )
}

fn morpheus_system_prompt(tool_specs: &[ToolSpec]) -> String {
    format!(
        "You are morpheus, a background memory curator. You are handed a batch of new \
         conversation turns mnemosyne (the main agent) hasn't curated yet. Your job is to build \
         them into condesate's long-term memory structures — not transcribe them verbatim, but \
         extract what's actually worth keeping:\n\
         1. For each *distinct* fact, decision, or notable tool result worth remembering \
            permanently, call record_diary_entry exactly once, with the right kind (observation, \
            decision, tool_result, retrieved, or reflection). If the same fact is restated or \
            confirmed more than once in this batch (e.g. the user says it, then later a reply \
            recaps it back), that is still only one fact — record it once, not once per mention.\n\
         2. Call tag_slip to file each recorded entry (by its returned address) under a short, \
            reusable topic name — this is what makes it findable later via recall_memory. Reuse \
            existing topic names when the new material is about the same thing as before, rather \
            than inventing near-duplicate topics.\n\
         3. Then, before you stop, look back over the same batch for anything *unsettled* and \
            call jot_idea for it. This is not an optional afterthought — it is half the job, and \
            most batches contain something: an open question, a decision not yet made, a guess \
            or inference you drew that the conversation hasn't confirmed, a stated intention, \
            something the user said they'd revisit. A fact you recorded in step 1 can also have \
            an open thread hanging off it — record the settled part, jot the unsettled part. If \
            it relates to something you or an earlier curation pass already jotted, call \
            list_live_ideas on that topic first — then revise_idea to update it in place, or \
            strike_idea if it's now settled or turned out wrong, rather than jotting a \
            disconnected duplicate. Only skip this step if the batch genuinely contains nothing \
            open at all.\n\
         Once you've processed the whole batch, stop calling tools and reply with a one-line \
         summary of what you recorded. Be selective — not every line of small talk needs a diary \
         entry.\n\n\
         Note on the tool-calling rules below: \"never call the same tool twice for the same \
         request\" means never *retry* a call that already succeeded for the *same* fact — it \
         does not mean you get only one tool call total. Calling record_diary_entry and tag_slip \
         several times in this one batch — once per distinct fact — is normal and expected. Once \
         every distinct fact has exactly one recorded entry, stop.\n\n{}",
        condesate::tool_instructions(tool_specs)
    )
}

/// One curation pass: hands morpheus everything in `slice` as a single batch
/// and lets its own `ReActLoop` do the extracting. A no-op on an empty slice
/// (nothing new since the last pass — can happen when `RESET_EVERY` fires
/// right after `CURATE_EVERY` already cleared the backlog).
///
/// `services` is reused across every pass (morpheus is spawned once, not
/// per-pass), so `begin_activation` here is load-bearing, not cosmetic: it's
/// what resets the tool-call step counter each `Condition::StepsUnder` rule
/// checks against back to zero. Skipping it would let that counter climb
/// forever across passes until every one of morpheus's grants permanently
/// denies — the runaway-loop backstop would itself become the runaway bug.
async fn curate(
    agent: &BasicAgent,
    services: Arc<GuardedServices>,
    trace: bool,
    pass: usize,
    slice: &[Message],
) -> Result<()> {
    if slice.is_empty() {
        return Ok(());
    }
    services.begin_activation(format!("curate-{pass}"), u64::MAX);
    let transcript_text =
        slice.iter().map(|m| format!("{:?}: {}", m.role, m.content)).collect::<Vec<_>>().join("\n\n");
    let mut ctx = AgentContext::new(services);
    ctx.trace = trace;
    ctx.transcript.push(Message::user(format!(
        "New conversation since the last curation pass:\n\n{transcript_text}\n\nCurate it now."
    )));
    ReActLoop { max_steps: MORPHEUS_MAX_STEPS }.run(agent as &dyn Agent, &mut ctx).await?;
    Ok(())
}

const USAGE: &str = "usage: mnemosyne [--debug] [--curate-every N] [--reset-every N]\n\n  \
     --debug            also start the dashboard's dev server and open it in your browser\n  \
     --curate-every N   turns between morpheus's curation passes (default 1: every turn)\n  \
     --reset-every N    turns between wipes of mnemosyne's own transcript (default 20)\n  \
     -h, --help         print this message";

/// Everything you'd want to vary between two runs of the same demo, on the
/// command line where you can see it.
///
/// These are flags rather than env vars on purpose. `--debug` has a visible
/// side effect on the machine (it spawns `npm` and opens a browser tab) and
/// the cadence knobs are the two numbers you actually tune while showing
/// this thing to someone — neither should be something a stale line in a
/// `.env` file decides for you. Parsed by hand: three options don't justify
/// a CLI framework, the same reasoning `api.rs` uses for its HTTP routes.
struct Args {
    debug: bool,
    /// Turns between curation passes. Defaults to 1 — morpheus curates after
    /// every exchange, so Faraday (and the dashboard reading it) reflects the
    /// conversation as it happens rather than going bare until a pass fires.
    curate_every: usize,
    /// Turns between wipes of mnemosyne's own transcript. A curation pass
    /// always runs immediately before a reset regardless of `curate_every`,
    /// so nothing raw is dropped unremembered.
    reset_every: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self { debug: false, curate_every: 1, reset_every: 20 }
    }
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        // `--flag N` and `--flag=N` both work; a count with no value is a
        // typo worth failing on, not a silent fallback to the default.
        let mut value = |flag: &str| -> Result<usize> {
            let raw = match arg.split_once('=') {
                Some((_, v)) => v.to_string(),
                None => argv.next().ok_or_else(|| {
                    anyhow::anyhow!("{flag} needs a number, e.g. `{flag} 5`\n\n{USAGE}")
                })?,
            };
            let n: usize = raw
                .parse()
                .map_err(|_| anyhow::anyhow!("{flag} needs a number, got '{raw}'\n\n{USAGE}"))?;
            if n == 0 {
                anyhow::bail!("{flag} must be at least 1");
            }
            Ok(n)
        };

        match arg.split('=').next().unwrap_or(&arg) {
            "--debug" => args.debug = true,
            "--curate-every" => args.curate_every = value("--curate-every")?,
            "--reset-every" => args.reset_every = value("--reset-every")?,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument '{other}'\n\n{USAGE}"),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Before anything else, so a typo'd flag fails immediately rather than
    // after loading `.env` and reaching out for a model.
    let args = parse_args()?;

    dotenvy::dotenv().ok();
    let verbose = std::env::var("VERBOSE").is_ok();

    // "Turns" throughout — one exchange (your input plus mnemosyne's reply) —
    // not raw `Message` struct count, which would be roughly double this and
    // harder to reason about from the chat.
    let (curate_every, reset_every) = (args.curate_every, args.reset_every);
    if curate_every >= reset_every {
        eprintln!(
            "[mnemosyne] note: --curate-every ({curate_every}) >= --reset-every ({reset_every}) — \
             curation will only ever run right before a reset forces it, its own cadence never \
             fires on its own"
        );
    }

    let main_model_name = std::env::var("MAIN_MODEL").unwrap_or_else(|_| "claude-opus-4-6".into());
    let morpheus_model_name =
        std::env::var("MORPHEUS_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());

    let (main_model, main_label) = model::choose_model(&main_model_name)?;
    eprintln!("[mnemosyne] model: {main_label}");
    if !verbose {
        eprintln!("[mnemosyne] quiet mode — set VERBOSE=1 to see reasoning + live audit on stderr");
    }

    let (mnemosyne_services, audit) = build_mnemosyne_services(verbose).await?;

    let bank = MemoryBank::new(Arc::new(SystemClock));
    let (morpheus_services, morpheus_agent, morpheus_label) =
        spawn_morpheus(&mnemosyne_services, bank.clone(), &morpheus_model_name).await?;
    let cadence = if curate_every == 1 {
        "curates every turn".to_string()
    } else {
        format!("curates every {curate_every} turns")
    };
    eprintln!("[morpheus] model: {morpheus_label} ({cadence}, and before every {reset_every}-turn reset)");

    let api_port: u16 = std::env::var("API_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(4477);
    let api_addr = format!("127.0.0.1:{api_port}");
    {
        let services = mnemosyne_services.clone();
        let bank = bank.clone();
        let audit = audit.clone();
        let addr = api_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = api::serve(&addr, services, bank, audit).await {
                eprintln!("[mnemosyne] diagnostic API failed to start on {addr}: {e}");
            }
        });
    }
    eprintln!("[mnemosyne] diagnostic API starting on http://{api_addr} (see dashboard/ for a UI)");

    let dashboard_port: u16 =
        std::env::var("DASHBOARD_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(5183);
    let dashboard_child = if args.debug {
        match dashboard::launch(dashboard_port, &format!("http://{api_addr}")).await {
            Ok(child) => Some(child),
            Err(e) => {
                eprintln!("[mnemosyne] dashboard could not be started ({e}) — continuing without it");
                None
            }
        }
    } else {
        None
    };

    let mnemosyne_tools: Vec<Arc<dyn Tool>> =
        vec![Arc::new(ListMemoryTopics { bank: bank.clone() }), Arc::new(RecallMemory { bank })];
    let specs: Vec<ToolSpec> = mnemosyne_tools.iter().map(|t| t.spec()).collect();
    let mnemosyne_agent =
        BasicAgent::new(MNEMOSYNE_ID, mnemosyne_system_prompt(&specs), main_model, mnemosyne_tools);

    println!("mnemosyne ready — type a message, or 'exit' to quit.\n");
    let stdin = std::io::stdin();
    let mut transcript: Vec<Message> = Vec::new();
    // Index into `transcript`: everything before this has already been
    // handed to morpheus.
    let mut curated_through = 0usize;
    let mut messages_since_curation = 0usize;
    let mut messages_since_reset = 0usize;
    let mut curation_pass = 0usize;

    loop {
        print!("you> ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "exit" || line == "quit" {
            break;
        }

        let mut ctx = AgentContext::new(mnemosyne_services.clone());
        ctx.trace = verbose;
        ctx.transcript = transcript.clone();
        ctx.transcript.push(Message::user(line.to_string()));

        let outcome = match (ReActLoop { max_steps: 8 }).run(&mnemosyne_agent as &dyn Agent, &mut ctx).await {
            Ok(o) => o,
            Err(e) => {
                eprintln!("mnemosyne> error: {e}\n");
                continue; // not a real exchange — the cadence counters don't advance
            }
        };
        println!("mnemosyne> {}\n", outcome.final_text);
        transcript.push(Message::user(line.to_string()));
        transcript.push(Message::assistant(outcome.final_text));

        messages_since_curation += 1;
        messages_since_reset += 1;

        if messages_since_reset >= reset_every {
            // Curate first, unconditionally, regardless of CURATE_EVERY's own
            // cadence — nothing raw is ever dropped without a chance to be
            // remembered first.
            eprintln!(
                "[morpheus] curating {} turn(s) before context reset...",
                (transcript.len() - curated_through) / 2
            );
            curation_pass += 1;
            if let Err(e) = curate(&morpheus_agent, morpheus_services.clone(), verbose, curation_pass, &transcript[curated_through..]).await {
                eprintln!("[morpheus] curation error: {e}");
            }
            eprintln!("[mnemosyne] context reset after {reset_every} turns — long-term memory kept in Faraday, recall it with recall_memory");
            transcript.clear();
            curated_through = 0;
            messages_since_curation = 0;
            messages_since_reset = 0;
        } else if messages_since_curation >= curate_every {
            eprintln!("[morpheus] curating {} turn(s)...", (transcript.len() - curated_through) / 2);
            curation_pass += 1;
            if let Err(e) = curate(&morpheus_agent, morpheus_services.clone(), verbose, curation_pass, &transcript[curated_through..]).await {
                eprintln!("[morpheus] curation error: {e}");
            }
            curated_through = transcript.len();
            messages_since_curation = 0;
        }
    }

    // A final pass so nothing said right before `exit` is lost unrecorded.
    if curated_through < transcript.len() {
        eprintln!("[morpheus] final curation pass before exit...");
        curation_pass += 1;
        let _ = curate(&morpheus_agent, morpheus_services.clone(), verbose, curation_pass, &transcript[curated_through..]).await;
    }

    if let Some(child) = dashboard_child {
        dashboard::shutdown(child, dashboard_port).await;
    }

    let events = audit.events().await;
    let denials = audit.refusals().await;
    eprintln!("[audit] {} boundary crossing(s) checked this run, {} denied", events.len(), denials.len());
    println!("bye.");
    Ok(())
}
