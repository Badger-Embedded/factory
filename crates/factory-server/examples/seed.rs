//! Fills a database with jobs in every stage, to look at the dashboard
//! without workers:
//!
//! ```sh
//! cargo run -p factory-server --example seed
//! cargo run -p factory-server
//! ```

use std::env;
use std::error::Error;
use std::path::PathBuf;

use factory_core::{AgentResult, Event, PlannedPr, PrDescription, Question};
use factory_server::Store;

fn ready() -> PrDescription {
    PrDescription {
        title: "maestrod: add a reboot endpoint".into(),
        body: "## Why\n\nThe device page needs it.".into(),
    }
}

fn planned(title: &str, summary: &str) -> PlannedPr {
    PlannedPr {
        title: title.into(),
        summary: summary.into(),
        number: None,
        description: None,
        notes: None,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let db =
        env::var_os("FACTORY_DB").map_or_else(|| PathBuf::from("data/factory.db"), PathBuf::from);
    if let Some(dir) = db.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let store = Store::open(&db)?;

    let spec = "Goal: reboot the device under test from its page.\n\nScope:\n- A Reboot button on the device page.\n- POST /devices/{id}/reboot in maestrod.\n\nOut of scope: scheduled reboots.\n\nAcceptance:\n- The button power-cycles the DUT and the page shows it coming back.";
    let plan = vec![
        planned(
            "maestrod: add a reboot endpoint",
            "POST /devices/{id}/reboot power-cycles the DUT through the relay.",
        ),
        planned(
            "web: add a reboot button to the device page",
            "Calls the new endpoint and shows the result.",
        ),
    ];
    let to_spec = vec![
        Event::Start,
        Event::Agent(AgentResult::Clarified),
        Event::Agent(AgentResult::Spec(spec.into())),
    ];
    let to_plan = [
        to_spec.clone(),
        vec![Event::Approve, Event::Agent(AgentResult::Plan(plan))],
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
    let to_merged = [
        to_watching.clone(),
        vec![
            Event::Merged,
            Event::Agent(AgentResult::CommitsReady(ready())),
            Event::ChecksPassed,
            Event::Published { number: 13 },
            Event::Merged,
        ],
    ]
    .concat();

    let jobs: Vec<(&str, Vec<Event>)> = vec![
        ("Show the SD card's write speed in the device list", vec![]),
        ("Log relay switches to the journal", vec![Event::Start]),
        (
            "Support a second USB hub on the controller",
            vec![
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
        ),
        ("Add a reboot button to the device page", to_spec),
        ("Add a reboot button to the device page", to_plan),
        ("Add a reboot button to the device page", to_implementing),
        ("Add a reboot button to the device page", to_watching),
        ("Add a reboot button to the device page", to_merged),
        (
            "Retry flashing when the SD mux reports EBUSY",
            vec![
                Event::Start,
                Event::AgentFailed {
                    reason: "the worker ran out of memory".into(),
                },
            ],
        ),
        (
            "Rename the hardware crate",
            vec![Event::Start, Event::Cancel],
        ),
    ];

    // API cost in USD of each job above, in order. The queued job has run no agent.
    let costs = [0.0, 0.18, 0.41, 0.77, 1.35, 2.62, 4.08, 6.93, 0.52, 0.09];

    for ((request, events), usd) in jobs.into_iter().zip(costs) {
        let id = store.create("maestro", "demo", request)?;
        for event in events {
            store.apply(id, event)?;
        }
        if usd > 0.0 {
            store.add_cost(id, usd, false)?;
        }
    }
    println!("seeded {}", db.display());
    Ok(())
}
