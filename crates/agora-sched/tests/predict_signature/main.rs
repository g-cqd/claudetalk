//! Command signatures, directory placement and history predictions through the public API (ref-admission sections 1
//! and 2; scenario tables F = P1-P14 and I = G1-G9). P8 (status 126/127 excluded) and P13 (a running job's estimate)
//! are the store's history query and daemon's `estimated`, not this crate's API, so they are not covered here.
//!
//! The signature and placement goldens are work.py's own output, pasted as literals. Each signature row was printed by
//!   python3 -I -c 'import sys, json; sys.path.insert(0, "/Users/gc/Developer/skills/skills/planner/scripts");
//!                  import work; work.place = lambda cwd: "app/Packages/Core";
//!                  print(json.dumps(work.signatures(ARGV, None)))'
//! with ARGV the row's argv. The placement goldens run work.place(cwd) with os.path.isdir, os.path.isfile and open
//! replaced by a fake tree, and work.SCRATCH set to the four roots this crate is given. The prediction expectations
//! follow ref-admission section 15 F; the reference derived them by hand, so they were not run through work.py.

mod golden;
mod placement;
mod prediction;

use agora_sched::signature::{PlaceProbe, Signatures, place};

const PLACE: &str = "app/Packages/Core";
const OTHER: &str = "other/Repo";

fn words(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|s| (*s).to_string()).collect()
}

fn sig(full: &str, head: &str, prog: &str, tool: &str) -> Signatures {
    Signatures {
        full: full.to_string(),
        head: head.to_string(),
        prog: prog.to_string(),
        tool: tool.to_string(),
    }
}

fn scratch() -> Vec<String> {
    [
        "/tmp",
        "/private/tmp",
        "/var/folders",
        "/private/var/folders",
    ]
    .iter()
    .map(|root| root.to_string())
    .collect()
}

/// A fake tree for `place`: directories, files with their content, and files that exist but cannot be read.
#[derive(Default)]
struct Tree {
    dirs: Vec<&'static str>,
    files: Vec<(&'static str, &'static str)>,
    unreadable: Vec<&'static str>,
}

impl PlaceProbe for Tree {
    fn is_dir(&self, p: &str) -> bool {
        self.dirs.contains(&p)
    }
    fn is_file(&self, p: &str) -> bool {
        self.files.iter().any(|(f, _)| *f == p) || self.unreadable.contains(&p)
    }
    fn read_head(&self, p: &str, n: usize) -> Option<String> {
        if self.unreadable.contains(&p) {
            return None;
        }
        self.files
            .iter()
            .find(|(f, _)| *f == p)
            .map(|(_, content)| content.chars().take(n).collect())
    }
}

fn place_in(cwd: &str, tree: &Tree) -> String {
    place(Some(cwd), tree, &scratch())
}
