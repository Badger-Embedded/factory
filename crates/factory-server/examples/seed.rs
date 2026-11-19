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

use factory_core::{AgentResult, Event, PlannedPr, Question};
use factory_server::Store;

fn main() -> Result<(), Box<dyn Error>> {
    let db =
        env::var_os("FACTORY_DB").map_or_else(|| PathBuf::from("data/factory.db"), PathBuf::from);
    if let Some(dir) = db.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let store = Store::open(&db)?;

    let spec = "Goal: reboot the device under test from its page.\n\nScope:\n- A Reboot button on the device page.\n- POST /devices/{id}/reboot in maestrod.\n\nOut of scope: scheduled reboots.\n\nAcceptance:\n- The button power-cycles the DUT and the page shows it coming back.";
    let plan = vec![
        PlannedPr {
            title: "maestrod: add a reboot endpoint".into(),
            summary: "POST /devices/{id}/reboot power-cycles the DUT through the relay.".into(),
            number: None,
        },
        PlannedPr {
            title: "web: add a reboot button to the device page".into(),
            summary: "Calls the new endpoint and shows the result.".into(),
            number: None,
        },
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
            Event::Agent(AgentResult::CommitsReady),
            Event::ChecksPassed,
            Event::Published { number: 12 },
        ],
    ]
    .concat();
    let to_merged = [
        to_watching.clone(),
        vec![
            Event::Merged,
            Event::Agent(AgentResult::CommitsReady),
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

    for (request, events) in jobs {
        let id = store.create("maestro", "demo", request)?;
        for event in events {
            store.apply(id, event)?;
        }
    }
    println!("seeded {}", db.display());
    Ok(())
}
