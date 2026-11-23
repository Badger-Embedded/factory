//! Writes the data the browser tests in `e2e/` run against, through the
//! public `Store` API. The tests call it before the server starts and again
//! while it runs, to change a job on the server's behalf.
//!
//! ```sh
//! e2e_fixture seed --db P
//! e2e_fixture project --db P --name N --login L [--login L ...]
//! e2e_fixture job --db P --project N --stage STAGE [--request TEXT]
//! e2e_fixture apply --db P --job ID --event start|approve|cancel|questions
//! e2e_fixture log --db P --job ID --text TEXT [--count N]
//! e2e_fixture tls --db P --dir D
//! ```
//!
//! `job` prints the new job's id. `tls` writes `ca.pem`, `cert.pem` and
//! `key.pem` into `D`: a throwaway CA and a leaf certificate for `127.0.0.1`,
//! so the stub MCP server can serve TLS and the factory can trust it through
//! `FACTORY_MCP_CA_FILE`. It does not open the database.

use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};

use factory_core::{AgentResult, Event, PlannedPr, PrDescription, Question};
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
        notes: None,
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
        "merged" => [to_watching, vec![Event::Merged]].concat(),
        "failed" => vec![
            Event::Start,
            Event::AgentFailed {
                reason: "the worker ran out of memory".into(),
            },
        ],
        "cancelled" => vec![Event::Start, Event::Cancel],
        other => return Err(format!("unknown stage {other}").into()),
    })
}

fn event_named(name: &str) -> Result<Event> {
    Ok(match name {
        "start" => Event::Start,
        "approve" => Event::Approve,
        "cancel" => Event::Cancel,
        "questions" => Event::Agent(AgentResult::Questions(vec![Question {
            text: "Which hub model should the first version support?".into(),
        }])),
        other => return Err(format!("unknown event {other}").into()),
    })
}

fn create_job(store: &Store, project: &str, stage: &str, request: &str) -> Result<i64> {
    let id = store.create(project, ADMIN, request)?;
    for event in events_for(stage)? {
        store.apply(id, event)?;
    }
    Ok(id)
}

fn seed(store: &Store) -> Result<()> {
    store.create_project(
        SHARED_PROJECT,
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
        create_job(store, SHARED_PROJECT, stage, request)?;
    }
    // One more than the Done section lists, so the board has "Show 1 older".
    for number in 1..=5 {
        create_job(
            store,
            SHARED_PROJECT,
            "merged",
            &format!("Merged change {number}"),
        )?;
    }
    create_job(
        store,
        SHARED_PROJECT,
        "cancelled",
        "Rename the hardware crate",
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
        "project" => {
            let name = one("name")?;
            let logins = flags.get("login").ok_or("missing --login")?;
            store.create_project(name, &format!("e2e/{name}"), "main", None, Some(&logins[0]))?;
            for login in &logins[1..] {
                store.add_collaborator(name, login, &logins[0])?;
            }
        }
        "job" => {
            let request = one("request").map_or("A request from the fixture", |text| text.as_str());
            let id = create_job(&store, one("project")?, one("stage")?, request)?;
            println!("{id}");
        }
        "apply" => {
            store.apply(one("job")?.parse()?, event_named(one("event")?)?)?;
        }
        "log" => {
            let job = one("job")?.parse()?;
            let text = one("text")?;
            let count: usize = flags.get("count").map_or(Ok(1), |v| v[0].parse())?;
            for n in 1..=count {
                store.append_factory_log(job, &format!("{text} {n}"))?;
            }
        }
        other => return Err(format!("unknown subcommand {other}").into()),
    }
    Ok(())
}
