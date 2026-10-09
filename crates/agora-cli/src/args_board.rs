//! Arguments of the board commands: `card`, `sprint` and `note`.

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum CardCmd {
    Create {
        id: String,
        title: String,
        #[arg(long)]
        lane: Option<String>,
        /// Higher = more urgent.
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        value: Option<f64>,
        #[arg(long)]
        size: Option<f64>,
        #[arg(long)]
        team: Option<String>,
        /// Cards this one waits for.
        #[arg(long, num_args = 1..)]
        after: Vec<String>,
    },
    /// Move to a state: backlog ready claimed doing review done blocked ...
    Move {
        id: String,
        state: String,
    },
    Evidence {
        id: String,
        reference: String,
    },
    Approve {
        id: String,
    },
    Reclaim {
        id: String,
    },
    List {
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        lane: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// Change a card's priority, value, size, team, needs-owner flag or owner action.
    Update {
        id: String,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        value: Option<f64>,
        #[arg(long)]
        size: Option<f64>,
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        needs_owner: Option<bool>,
        #[arg(long)]
        owner_action: Option<String>,
    },
    /// Make a card wait for another (`--remove` drops the dependency).
    Dep {
        id: String,
        after: String,
        #[arg(long)]
        remove: bool,
    },
    /// Link a card to another card, a job, a message or a note.
    Link {
        id: String,
        /// re | blocks | relates | duplicates | caused_by | job | message | note
        rel: String,
        target: String,
        #[arg(long)]
        remove: bool,
    },
    /// Flag a card as needing its owner's attention and notify the escalation chain.
    Escalate {
        id: String,
        /// What the owner has to do.
        #[arg(long)]
        owner_action: String,
        /// Public keys to notify (default: the policy's chain, else the supervisors).
        #[arg(long, num_args = 1..)]
        to: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SprintCmd {
    Create {
        id: String,
        name: String,
        #[arg(long, default_value = "")]
        goal: String,
        /// Start, ms since epoch.
        #[arg(long)]
        starts: i64,
        /// End, ms since epoch (must be after start).
        #[arg(long)]
        ends: i64,
    },
    Add {
        sprint: String,
        card: String,
    },
    Start {
        sprint: String,
    },
    Close {
        sprint: String,
    },
    Summary {
        sprint: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum NoteCmd {
    Publish {
        id: String,
        title: String,
        body: String,
        #[arg(long, default_value = "")]
        tags: String,
        #[arg(long)]
        supersedes: Option<String>,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}
