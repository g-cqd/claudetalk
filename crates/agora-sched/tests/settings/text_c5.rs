use super::*;

// settings_text.

const WORK_PY_PREFIXES: [&str; 15] = [
    "cpu budget: ",
    "capacity: ",
    "memory budget: ",
    "idle min: ",
    "memory limit: ",
    "load factor: ",
    "load wait: ",
    "aging: ",
    "backfill wait: ",
    "quiet drain: ",
    "quiet max: ",
    "job qos: ",
    "small weight: ",
    "log limit: ",
    "keep: ",
];

#[test]
fn settings_text_starts_with_the_fifteen_work_py_lines_in_work_py_order() {
    let text = settings_text(&WorkSettings::default(), &machine_16_gib(), true);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 25, "15 work.py lines and 10 agora lines");
    for (line, prefix) in lines.iter().zip(WORK_PY_PREFIXES) {
        assert!(
            line.starts_with(prefix),
            "{line:?} should start with {prefix:?}"
        );
    }
}

#[test]
fn settings_text_renders_the_default_values_in_work_py_units() {
    let text = settings_text(&WorkSettings::default(), &machine_16_gib(), true);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[..15],
        [
            "cpu budget: 8 cores (the default: the effective CPU capacity); the running jobs' predicted cores fit it",
            "capacity: off (the default): no cap on declared weights",
            "memory budget: 14.0 GiB (the default: the memory minus 2 GiB); jobs start while used memory (active, wired and compressed) is at or below it",
            "idle min: 10%; jobs start while the CPU is at least this idle",
            "memory limit: warning; jobs start below warning memory pressure",
            "load factor: 1.5; on Linux only, jobs start below load 12.00 on 8 effective CPUs",
            "load wait: 30 min 0 s; a job that the machine's pressure alone holds back longer fails",
            "aging: 1; waiting jobs go shortest predicted run time first, less this many seconds for each second waited",
            "backfill wait: 5 min 0 s; a job that waited longer keeps every job behind it",
            "quiet drain: 1 min 0 s; the longest an exclusive job holds new jobs predicted above the small weight before it starts",
            "quiet max: 3 min 0 s; the longest an exclusive job keeps them out once it runs",
            "job qos: utility; every job but the exclusive ones runs at this QoS, and its children with it, so the user's own work comes first",
            "small weight: 1; a job predicted to use this many cores or fewer may start past the head for a while",
            "log limit: 268435456 bytes; a job whose log grows past it is stopped",
            "keep: 14 days of events and of the logs of ended tasks",
        ]
    );
}

#[test]
fn settings_text_appends_one_line_per_agora_key_after_the_work_py_lines() {
    let text = settings_text(&WorkSettings::default(), &machine_16_gib(), true);
    let lines: Vec<&str> = text.lines().collect();
    let agora = &lines[15..];
    let prefixes = [
        "self accept: ",
        "timer quota: ",
        "run role: ",
        "max waiting: ",
        "disk floor: ",
        "memory floor: ",
        "idle floor: ",
        "reconcile interval: ",
        "lease ttl: ",
        "claim ttl: ",
    ];
    assert_eq!(agora.len(), prefixes.len());
    for (line, prefix) in agora.iter().zip(prefixes) {
        assert!(
            line.starts_with(prefix),
            "{line:?} should start with {prefix:?}"
        );
    }
    assert!(
        agora[1].contains("32"),
        "timer quota line shows the quota: {}",
        agora[1]
    );
    assert_eq!(
        agora[2],
        "run role: worker; identities below it cannot run or accept commands"
    );
    assert_eq!(agora[3], "max waiting: 64 jobs per identity");
    assert!(
        agora[4].contains("5.0 GiB"),
        "disk floor line shows 5 GiB: {}",
        agora[4]
    );
    assert!(
        agora[7].contains("30.0 s"),
        "reconcile line shows 30 s: {}",
        agora[7]
    );
    assert!(
        agora[8].contains("15 min 0 s"),
        "lease line shows 900 s: {}",
        agora[8]
    );
    assert!(
        agora[9].contains("30 min 0 s"),
        "claim line shows 1800 s: {}",
        agora[9]
    );
}

#[test]
fn settings_text_says_job_qos_is_not_applied_off_macos_only() {
    let settings = WorkSettings::default();
    let on_mac = settings_text(&settings, &machine_16_gib(), true);
    let elsewhere = settings_text(&settings, &machine_16_gib(), false);
    let qos_line = |text: &str| {
        text.lines()
            .find(|line| line.starts_with("job qos: "))
            .expect("the job qos line is present")
            .to_owned()
    };
    assert!(!qos_line(&on_mac).contains("not applied here"));
    assert!(qos_line(&elsewhere).ends_with("(not applied here: macOS only)"));
}

// C5: exclusive_at.

#[test]
fn c5_exclusive_needs_two_cpus_and_a_weight_covering_every_cpu() {
    let cases = [
        (1.0, 1.0, false),
        (1.0, 0.5, false),
        (2.0, 2.0, true),
        (3.5, 4.0, false),
        (8.0, 8.0, true),
        (1.5, 1.5, false),
        (1.9999, 2.0, false),
    ];
    for (weight, cpus, expected) in cases {
        assert_eq!(
            exclusive_at(weight, cpus),
            expected,
            "weight {weight} on {cpus} CPUs"
        );
    }
}
