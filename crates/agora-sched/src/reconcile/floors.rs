//! Rule R5 breach detection. A floor counts only when both the floor and the reading are known.

use super::ReconcileInput;
use crate::pyfmt::fmt_fixed;
use crate::textfmt::gib;

/// One breached floor. `resource` names what to free ("disk space", "memory", "CPU"); `value` and `floor` are
/// the reading and the floor as text; `text` is the breach sentence used in the blocker title and the pause.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Breach {
    pub(super) resource: &'static str,
    pub(super) value: String,
    pub(super) floor: String,
    pub(super) text: String,
}

/// Breaches in the order disk, memory, CPU. Empty without a machine snapshot or when no floor is breached.
pub(super) fn breaches(input: &ReconcileInput) -> Vec<Breach> {
    let Some(machine) = input.machine.as_ref() else {
        return Vec::new();
    };
    let settings = &input.settings;
    let signals = machine.signals.as_ref();
    let mut out = Vec::new();

    if let (Some(free), Some(floor)) = (machine.disk_free_kib, settings.disk_floor_kib)
        && free < floor
    {
        let (value, floor) = (gib(free), gib(floor));
        out.push(Breach {
            resource: "disk space",
            text: format!("disk free {value} below the floor of {floor}"),
            value,
            floor,
        });
    }

    let used = signals.and_then(|s| s.memory_used_kib);
    if let (Some(total), Some(used), Some(floor)) =
        (machine.total_memory_kib, used, settings.memory_floor_kib)
    {
        let available = total.saturating_sub(used);
        if available < floor {
            let (value, floor) = (gib(available), gib(floor));
            out.push(Breach {
                resource: "memory",
                text: format!("available memory {value} below the floor of {floor}"),
                value,
                floor,
            });
        }
    }

    let idle = signals.and_then(|s| s.idle_pct);
    if let (Some(idle), Some(floor)) = (idle, settings.idle_floor_pct)
        && idle < floor
    {
        let (value, floor) = (
            format!("{}%", fmt_fixed(idle, 0)),
            format!("{}%", fmt_fixed(floor, 0)),
        );
        out.push(Breach {
            resource: "CPU",
            text: format!("CPU {value} idle below the floor of {floor}"),
            value,
            floor,
        });
    }
    out
}
