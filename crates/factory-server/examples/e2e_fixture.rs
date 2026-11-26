//! Writes the data the browser tests in `e2e/` run against, through the
//! public `Store` API. The tests call it before the server starts and again
//! while it runs, to change a job on the server's behalf.
//!
//! ```sh
//! e2e_fixture seed --db P
//! e2e_fixture project --db P --name N [--repo OWNER/REPO] [--forge NAME] --login L [--login L ...]
//! e2e_fixture job --db P --project N --stage STAGE [--request TEXT]
//! e2e_fixture renovate --db P --project N --number NUMBER --url URL
//! e2e_fixture picked --db P --project N --number NUMBER --url URL
//! e2e_fixture reviewed --db P --project N --number NUMBER --url URL [--fork true] [--comment off|posted|failed]
//! e2e_fixture apply --db P --job ID --event start|approve|cancel|questions
//! e2e_fixture cost --db P --job ID --usd AMOUNT [--local-cli claude-code|codex|copilot]
//! e2e_fixture log --db P --job ID --text TEXT [--count N]
//! e2e_fixture mcp --db P --job ID --server S --tool T [--blocked true]
//! e2e_fixture tls --db P --dir D
//! ```
//!
//! `job`, `renovate`, `picked` and `reviewed` print the new job's id. `tls` writes `ca.pem`, `cert.pem` and
//! `key.pem` into `D`: a throwaway CA and a leaf certificate for `127.0.0.1`,
//! so the stub MCP and Forgejo servers can serve TLS and the factory can trust
//! them through `FACTORY_MCP_CA_FILE` and `FACTORY_FORGEJO_CA_FILE`. It does not open the database.

use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};

use factory_core::{
    Agent, AgentResult, Cli, DiffVerdict, Event, FixOutcome, PlannedPr, PrDescription, Question,
    RenovateAccess, RenovateCi, RetryLimits, Session,
};
use factory_runner::ForgeKind;
use factory_server::Store;

const ADMIN: &str = "admin@e2e.test";
const COLLABORATOR: &str = "alice@e2e.test";
const SHARED_PROJECT: &str = "maestro";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn ready() -> PrDescription {
    PrDescription {
        title: "maestrod: add a reboot endpoint".into(),
        body: "## Why\n\nThe device page needs it.".into(),
    }
}

fn planned(title: &str) -> PlannedPr {
    PlannedPr {
        title: title.into(),
        summary: "Power-cycles the device under test.".into(),
        number: None,
        description: None,
        notes: Some("Keep the endpoint behind the device lock.".into()),
    }
}

/// The events that take a new job to `stage`.
fn events_for(stage: &str) -> Result<Vec<Event>> {
    let to_spec = vec![
        Event::Start,
        Event::Agent(AgentResult::Clarified),
        Event::Agent(AgentResult::Spec("Goal: reboot the device.".into())),
    ];
    let to_plan = [
        to_spec.clone(),
        vec![
            Event::Approve,
            Event::Agent(AgentResult::Plan(vec![planned(
                "maestrod: add a reboot endpoint",
            )])),
        ],
    ]
    .concat();
    let to_implementing = [to_plan.clone(), vec![Event::Approve]].concat();
    let to_watching = [
        to_implementing.clone(),
        vec![
            Event::Agent(AgentResult::CommitsReady(ready())),
            Event::ChecksPassed,
            Event::Published { number: 12 },
        ],
    ]
    .concat();
    Ok(match stage {
        "requested" => vec![],
        "started" => vec![Event::Start],
        "questions" => vec![
            Event::Start,
            Event::Agent(AgentResult::Questions(vec![
                Question {
                    text: "Which hub model should the first version support?".into(),
                },
                Question {
                    text: "Should both hubs share one power budget?".into(),
                },
            ])),
        ],
        "spec" => to_spec,
        "plan" => to_plan,
        "implementing" => to_implementing,
        "watching" => to_watching,
        "reviewed" => [
            to_watching,
            vec![
                Event::StartReview {
                    head: "0123456789abcdef".into(),
                },
                Event::ReviewApproved,
            ],
        ]
        .concat(),
        "merged" => [to_watching, vec![Event::Merged]].concat(),
        "failed" => vec![
            Event::Start,
            Event::AgentFailed {
                reason: "the worker ran out of memory".into(),
            },
        ],
        "checks-error" => [
            to_implementing,
            vec![Event::Agent(AgentResult::CommitsReady(ready()))],
            (0..5)
                .map(|_| Event::ChecksError {
                    error: "bash: just: command not found".into(),
                })
                .collect(),
        ]
        .concat(),
        "cancelled" => vec![Event::Start, Event::Cancel],
        other => return Err(format!("unknown stage {other}").into()),
    })
}

fn events_named(name: &str) -> Result<Vec<Event>> {
    Ok(match name {
        "start" => vec![Event::Start],
        "approve" => vec![Event::Approve],
        "cancel" => vec![Event::Cancel],
        "questions" => vec![Event::Agent(AgentResult::Questions(vec![Question {
            text: "Which hub model should the first version support?".into(),
        }]))],
        "renovate-stopped" => vec![
            Event::RenovateObserved {
                head: "abc1234".into(),
                ci: RenovateCi::Failed {
                    log: "smoke test failed".into(),
                },
                limits: RetryLimits { auto: 0, total: 0 },
                auto_merge: false,
                access: Box::default(),
            },
            Event::Commented,
        ],
        "renovate-read-transient" => vec![
            read_only_auto_merge_observation(RenovateCi::Failed {
                log: "registry timeout".into(),
            }),
            Event::Agent(AgentResult::FailureVerdict {
                transient: true,
                reason: "registry timeout".into(),
            }),
            Event::Commented,
        ],
        "renovate-read-ready" => vec![
            read_only_auto_merge_observation(RenovateCi::Passed {
                details: Some(Box::new(factory_core::PrDiff {
                    diff: "+serde = 1.0.2".into(),
                    body: "Bumps serde".into(),
                })),
            }),
            Event::Agent(AgentResult::DiffVerdict(Box::new(DiffVerdict {
                safe: true,
                reason: "only a version changes".into(),
                summary: "Bumps serde".into(),
                from: None,
                to: None,
            }))),
            read_only_auto_merge_observation(RenovateCi::Passed { details: None }),
        ],
        other => return Err(format!("unknown event {other}").into()),
    })
}

fn read_only_auto_merge_observation(ci: RenovateCi) -> Event {
    Event::RenovateObserved {
        head: "abc1234".into(),
        ci,
        limits: RetryLimits { auto: 3, total: 7 },
        auto_merge: true,
        access: Box::new(RenovateAccess {
            can_rerun: false,
            can_merge: false,
            account: "project-bot".into(),
        }),
    }
}

fn create_job(
    store: &Store,
    project: &str,
    stage: &str,
    request: &str,
    after: &[i64],
) -> Result<i64> {
    let id = store.create_after(project, ADMIN, request, after)?;
    for event in events_for(stage)? {
        store.apply(id, event)?;
    }
    Ok(id)
}

/// A request job on pull request `number` that waits for the user to merge.
fn picked(store: &Store, project: &str, number: u64, url: &str) -> Result<i64> {
    let picked = factory_core::PickedPr {
        number,
        url: url.to_owned(),
        title: "Handle the reboot error".into(),
        body: String::new(),
        head_branch: "carol/reboot".into(),
        base_branch: "main".into(),
        fork: false,
        mode: factory_core::PrMode::Request,
    };
    let id = store.create_pull_request_job(project, ADMIN, "Handle the error", &[], picked)?;
    let plan = vec![planned("Handle the error")];
    let events = events_for("implementing")?
        .into_iter()
        .map(|event| match event {
            Event::Agent(AgentResult::Plan(_)) => Event::Agent(AgentResult::Plan(plan.clone())),
            other => other,
        })
        .chain([
            Event::Agent(AgentResult::CommitsReady(ready())),
            Event::ChecksPassed,
            Event::Published { number },
            Event::MergeLeftToUser,
        ]);
    for event in events {
        store.apply(id, event)?;
    }
    Ok(id)
}

/// A quick review of pull request `number` that waits in Needs you with the
/// score 4. `comment` is `off`, `posted` or `failed`.
fn reviewed(
    store: &Store,
    project: &str,
    number: u64,
    url: &str,
    fork: bool,
    comment: &str,
) -> Result<i64> {
    let picked = factory_core::PickedPr {
        number,
        url: url.to_owned(),
        title: "Handle the reboot error".into(),
        body: String::new(),
        head_branch: "carol/reboot".into(),
        base_branch: "main".into(),
        fork,
        mode: factory_core::PrMode::QuickReview {
            comment: comment != "off",
        },
    };
    let id = store.create_pull_request_job(project, ADMIN, "", &[], picked)?;
    store.apply(id, Event::Start)?;
    store.apply(
        id,
        Event::QuickReviewed {
            score: 4,
            review: "The retry loop never gives up.\nAdd a cap.".into(),
            head: "0123456789abcdef0123".into(),
        },
    )?;
    match comment {
        "off" => {}
        "posted" => {
            store.apply(id, Event::Commented)?;
        }
        "failed" => {
            store.apply(
                id,
                Event::CommentFailed {
                    error: "the forge refused the comment".into(),
                },
            )?;
        }
        other => return Err(format!("unknown --comment {other}").into()),
    }
    Ok(id)
}

fn seed_local(store: &Store, url: &str, models: Option<&Vec<String>>) -> Result<()> {
    store.save_local_settings(Some(url), Cli::Codex, 10)?;
    store.replace_local_models(models.map_or(&[][..], Vec::as_slice), 0)?;
    Ok(())
}

fn seed(store: &Store) -> Result<()> {
    store.create_project(
        SHARED_PROJECT,
        &ForgeKind::GitHub,
        "e2e/maestro",
        "main",
        Some("just test"),
        Some(ADMIN),
    )?;
    store.add_collaborator(SHARED_PROJECT, COLLABORATOR, ADMIN)?;
    let live = [
        ("requested", "Show the SD card write speed"),
        ("started", "Log relay switches to the journal"),
        ("questions", "Support a second USB hub"),
        ("spec", "Add a reboot button, spec ready"),
        ("plan", "Add a reboot button, plan ready"),
        ("implementing", "Add a reboot button, implementing"),
        ("watching", "Add a reboot button, watching"),
        ("failed", "Retry flashing when the SD mux is busy"),
    ];
    for (stage, request) in live {
        create_job(store, SHARED_PROJECT, stage, request, &[])?;
    }
    // One more than the Done section lists, so the board has "Show 1 older".
    for number in 1..=5 {
        create_job(
            store,
            SHARED_PROJECT,
            "merged",
            &format!("Merged change {number}"),
            &[],
        )?;
    }
    create_job(
        store,
        SHARED_PROJECT,
        "cancelled",
        "Rename the hardware crate",
        &[],
    )?;
    Ok(())
}

fn write_tls(dir: &Path) -> Result<()> {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};

    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(Vec::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = ca_params.self_signed(&ca_key)?;
    let leaf_key = KeyPair::generate()?;
    let leaf =
        CertificateParams::new(vec!["127.0.0.1".to_owned()])?.signed_by(&leaf_key, &ca, &ca_key)?;
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("ca.pem"), ca.pem())?;
    std::fs::write(dir.join("cert.pem"), leaf.pem())?;
    std::fs::write(dir.join("key.pem"), leaf_key.serialize_pem())?;
    Ok(())
}

fn end_fix(store: &Store, job: i64, fix: i64) -> Result<()> {
    store.apply(fix, Event::Cancel)?;
    store.apply(
        job,
        Event::FixEnded {
            job: fix,
            outcome: FixOutcome::Cancelled,
        },
    )?;
    Ok(())
}

fn apply(store: &Store, flags: &HashMap<String, Vec<String>>) -> Result<()> {
    let one = |name: &str| -> Result<&str> {
        Ok(flags.get(name).ok_or(format!("--{name} is required"))?[0].as_str())
    };
    let job: i64 = one("job")?.parse()?;
    if flags.contains_key("fix") {
        return end_fix(store, job, one("fix")?.parse()?);
    }
    for event in events_named(one("event")?)? {
        store.apply(job, event)?;
    }
    Ok(())
}

fn cost(store: &Store, flags: &HashMap<String, Vec<String>>) -> Result<()> {
    let one = |name: &str| -> Result<&str> {
        Ok(flags.get(name).ok_or(format!("--{name} is required"))?[0].as_str())
    };
    let job = one("job")?.parse()?;
    store.add_cost(job, one("usd")?.parse()?, false)?;
    if let Ok(cli) = one("local-cli") {
        let cli = Cli::from_id(cli).ok_or("unknown --local-cli")?;
        store.session_uuid(job, Session::Planning, Agent::Local)?;
        store.record_session_local_cli(job, Session::Planning, cli)?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one match arm per fixture subcommand"
)]
fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or("missing subcommand")?;
    let mut flags: HashMap<String, Vec<String>> = HashMap::new();
    while let Some(flag) = args.next() {
        let name = flag
            .strip_prefix("--")
            .ok_or("expected a --flag")?
            .to_owned();
        let value = args.next().ok_or("a flag needs a value")?;
        flags.entry(name).or_default().push(value);
    }
    let one = |name: &str| -> Result<&String> {
        flags
            .get(name)
            .and_then(|values| values.first())
            .ok_or_else(|| format!("missing --{name}").into())
    };
    if command == "tls" {
        return write_tls(Path::new(one("dir")?));
    }
    let db = PathBuf::from(one("db")?);
    if let Some(dir) = db.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let store = Store::open(&db)?;
    match command.as_str() {
        "seed" => seed(&store)?,
        "local-models" => seed_local(&store, one("url")?, flags.get("model"))?,
        "project" => {
            let name = one("name")?;
            let logins = flags.get("login").ok_or("missing --login")?;
            let repo = one("repo").map_or_else(|_| format!("e2e/{name}"), Clone::clone);
            store.create_project(
                name,
                &ForgeKind::GitHub,
                &repo,
                "main",
                None,
                Some(&logins[0]),
            )?;
            for login in &logins[1..] {
                store.add_collaborator(name, login, &logins[0])?;
            }
            if let Ok(forge) = one("forge") {
                rusqlite::Connection::open(&db)?.execute(
                    "UPDATE projects SET forge = ?1 WHERE name = ?2",
                    [forge, name],
                )?;
            }
        }
        "job" => {
            let request = one("request").map_or("A request from the fixture", |text| text.as_str());
            let after = flags.get("after").map_or(Ok(Vec::new()), |ids| {
                ids.iter().map(|id| id.parse()).collect()
            })?;
            let id = create_job(&store, one("project")?, one("stage")?, request, &after)?;
            println!("{id}");
        }
        "renovate" => {
            let id = store
                .create_renovate_job(
                    one("project")?,
                    one("number")?.parse()?,
                    "Update dependency foo to v2",
                    one("url")?,
                )?
                .ok_or("the pull request already has a job")?;
            println!("{id}");
        }
        "picked" => println!(
            "{}",
            picked(
                &store,
                one("project")?,
                one("number")?.parse()?,
                one("url")?
            )?
        ),
        "reviewed" => println!(
            "{}",
            reviewed(
                &store,
                one("project")?,
                one("number")?.parse()?,
                one("url")?,
                flags.get("fork").is_some_and(|v| v[0] == "true"),
                flags.get("comment").map_or("off", |v| v[0].as_str()),
            )?
        ),
        "apply" => apply(&store, &flags)?,
        "cost" => cost(&store, &flags)?,
        "log" => {
            let job = one("job")?.parse()?;
            let text = one("text")?;
            let count: usize = flags.get("count").map_or(Ok(1), |v| v[0].parse())?;
            for n in 1..=count {
                store.append_factory_log(job, &format!("{text} {n}"))?;
            }
        }
        "mcp" => {
            let job = one("job")?.parse()?;
            let (server, tool) = (one("server")?, one("tool")?);
            let blocked = match flags.get("blocked").map(|v| v[0].as_str()) {
                None | Some("false") => false,
                Some("true") => true,
                Some(other) => {
                    return Err(format!("--blocked takes true or false, not {other}").into());
                }
            };
            if blocked {
                store.record_mcp_tool_blocked(job, server, tool)?;
            } else {
                store.record_mcp_tool_call(job, server, tool)?;
            }
        }
        other => return Err(format!("unknown subcommand {other}").into()),
    }
    Ok(())
}
