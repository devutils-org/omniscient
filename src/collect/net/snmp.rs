//! `net.snmp` over `/proc/net/snmp`, `/proc/net/netstat` and `/proc/net/snmp6`.
//!
//! Every protocol counter the kernel keeps, by the name `nstat` gives it:
//! the section and the field run together (`TcpRetransSegs`,
//! `TcpExtListenOverflows`, `Ip6InReceives`). That is what `nstat` and
//! `netstat -s` are mostly used for, and the sources beside this one
//! (`net.backlog`, `net.tcp.retransmit`) each answer one of these questions
//! with more context; this one answers all of them with none.
//!
//! The counters only grow, so a poll or a count reports increments, and only
//! those that moved, as `nstat` does: three hundred counters a tick of which
//! a handful moved would bury the handful. A counter that is absent from an
//! interval moved by zero in it. A snapshot reports every counter as it
//! stands, under a name that says so.
//!
//! A few fields are not counters but settings or gauges (`TcpCurrEstab`,
//! `IpForwarding`, `TcpRtoMin`, `TcpMaxConn`, which is -1): an increment of
//! one is meaningless, so they are reported as they stand, every interval,
//! and marked `gauge`.

use std::collections::BTreeMap;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

const SNMP: &str = "/proc/net/snmp";
const NETSTAT: &str = "/proc/net/netstat";
const SNMP6: &str = "/proc/net/snmp6";

/// The fields of `/proc/net/snmp` that are settings or gauges rather than
/// counters, by `nstat`'s name.
const GAUGES: [&str; 7] = [
    "IpForwarding",
    "IpDefaultTTL",
    "TcpRtoAlgorithm",
    "TcpRtoMin",
    "TcpRtoMax",
    "TcpMaxConn",
    "TcpCurrEstab",
];

fn is_gauge(name: &str) -> bool {
    GAUGES.contains(&name)
}

/// One counter as read: its `nstat` name, the file's section, and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Counter {
    section: String,
    value: i64,
}

/// Reads the paired-line files (`snmp`, `netstat`): a line of names under a
/// section prefix, then a line of values under the same prefix.
fn read_paired(text: &str, counters: &mut BTreeMap<String, Counter>) {
    let mut lines = text.lines();
    while let (Some(names), Some(values)) = (lines.next(), lines.next()) {
        let (Some((section, names)), Some((value_section, values))) =
            (names.split_once(':'), values.split_once(':'))
        else {
            continue;
        };
        if section != value_section {
            continue;
        }
        for (name, value) in names.split_whitespace().zip(values.split_whitespace()) {
            if let Ok(value) = value.parse::<i64>() {
                counters.insert(
                    format!("{section}{name}"),
                    Counter {
                        section: section.to_owned(),
                        value,
                    },
                );
            }
        }
    }
}

/// Reads `snmp6`, a name and a value per line, the name already `nstat`'s.
fn read_flat(text: &str, counters: &mut BTreeMap<String, Counter>) {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<i64>() else {
            continue;
        };
        // `Ip6InReceives` is section `Ip6`, `Icmp6InMsgs` is `Icmp6`,
        // `Udp6InDatagrams` is `Udp6`, `UdpLite6...` is `UdpLite6`.
        let section = name
            .find('6')
            .map(|at| &name[..=at])
            .unwrap_or(name)
            .to_owned();
        counters.insert(name.to_owned(), Counter { section, value });
    }
}

fn read_all() -> Result<BTreeMap<String, Counter>, SampleError> {
    let mut counters = BTreeMap::new();
    for path in [SNMP, NETSTAT] {
        let text = std::fs::read_to_string(path)
            .map_err(|error| SampleError::Transient(format!("{path}: {error}")))?;
        read_paired(&text, &mut counters);
    }
    // A kernel without IPv6 has no `snmp6`, and no IPv6 counters to report.
    if let Ok(text) = std::fs::read_to_string(SNMP6) {
        read_flat(&text, &mut counters);
    }
    Ok(counters)
}

pub struct Prepare(pub Mode);

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        match target {
            Target::System => Ok(Box::new(SnmpCollector {
                id: CollectorId::new("net.snmp/fs:proc"),
                mode: self.0,
                previous: BTreeMap::new(),
                pass_first_tick: false,
                losses: Losses::default(),
            })),
            other => Err(format!(
                "the protocol counters are the machine's (its network namespace's), and a {} \
                 target asks for a share of them the kernel does not keep; use --system",
                other.kind()
            )),
        }
    }
}

pub struct SnmpCollector {
    id: CollectorId,
    mode: Mode,
    /// Each counter's reading the next increment is measured against.
    previous: BTreeMap<String, i64>,
    /// The first tick after the baseline taken at `start`: its interval is
    /// the microseconds since the program's release, so it is passed over
    /// and the next tick measures from the baseline.
    pass_first_tick: bool,
    losses: Losses,
}

impl SnmpCollector {
    fn event(&self, name: &str, counter: &Counter, value: Attr, ts_ns: u64, seq: u64) -> Event {
        let mut attrs = vec![
            value,
            Attr::text("name", name),
            // The `counter` key reads this: one series per counter, so a run
            // keyed on it is `nstat`'s table.
            Attr::text("counter", name),
            Attr::text("section", counter.section.clone()),
        ];
        if is_gauge(name) {
            attrs.push(Attr::text("gauge", "a setting or a gauge, as it stands"));
        }
        Event {
            ts_ns,
            ts_err_ns: 0,
            kind: "net.snmp".to_owned(),
            keys: Vec::new(),
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }

    /// The increments since the previous reading, of the counters that
    /// moved, and every gauge as it stands.
    fn increments(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let counters = read_all()?;
        let ts_ns = monotonic_ns();
        let mut events = Vec::new();
        for (name, counter) in &counters {
            let previous = self.previous.insert(name.clone(), counter.value);
            if is_gauge(name) {
                let value = Attr::int("value", counter.value);
                events.push(self.event(name, counter, value, ts_ns, seq + events.len() as u64));
                continue;
            }
            let Some(previous) = previous else {
                // A counter that appeared since the baseline: its own baseline.
                continue;
            };
            if counter.value < previous {
                self.losses.failed_reads += 1;
                self.losses.note_times(
                    format!(
                        "`{name}` went backwards, so the namespace's counters were replaced; that \
                     interval is not measured"
                    ),
                    1,
                );
                continue;
            }
            let moved = (counter.value - previous) as u64;
            if moved == 0 {
                continue;
            }
            let value = Attr::uint("value", moved);
            events.push(self.event(name, counter, value, ts_ns, seq + events.len() as u64));
        }
        Ok(events)
    }
}

impl Collector for SnmpCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        match self.mode {
            Mode::Snapshot => "value_since_boot",
            _ => "value",
        }
    }

    /// The baseline, before the observed program is released.
    fn start(&mut self) -> Result<(), String> {
        if self.mode == Mode::Snapshot {
            return Ok(());
        }
        let counters = read_all().map_err(|error| format!("{error:?}"))?;
        self.previous = counters
            .into_iter()
            .map(|(name, counter)| (name, counter.value))
            .collect();
        self.pass_first_tick = matches!(self.mode, Mode::Poll { .. });
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        match self.mode {
            Mode::Snapshot => {
                let counters = read_all()?;
                let ts_ns = monotonic_ns();
                Ok(counters
                    .iter()
                    .enumerate()
                    .map(|(offset, (name, counter))| {
                        let value = Attr::int("value_since_boot", counter.value);
                        self.event(name, counter, value, ts_ns, seq + offset as u64)
                    })
                    .collect())
            }
            // One total, at the close, measured from the baseline.
            Mode::Count => Ok(Vec::new()),
            _ => {
                if std::mem::take(&mut self.pass_first_tick) {
                    return Ok(Vec::new());
                }
                self.increments(seq)
            }
        }
    }

    /// A count's one reading; a poll's last interval is the runner's last
    /// tick, and a snapshot has no window to close.
    fn stop(&mut self) -> Vec<Event> {
        if self.mode != Mode::Count {
            return Vec::new();
        }
        match self.increments(0) {
            Ok(events) => events,
            Err(error) => {
                self.losses.failed_reads += 1;
                self.losses.note_times(
                    format!("the closing read failed, so the run measured nothing: {error:?}"),
                    1,
                );
                Vec::new()
            }
        }
    }

    fn notes(&self) -> Vec<String> {
        match self.mode {
            Mode::Snapshot => Vec::new(),
            _ => vec![
                "only the counters that moved are reported, as `nstat` does: a counter absent \
                 from an interval moved by zero in it"
                    .to_owned(),
            ],
        }
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paired_files_name_a_counter_as_nstat_does() {
        let mut counters = BTreeMap::new();
        read_paired(
            "Tcp: RtoAlgorithm MaxConn RetransSegs\nTcp: 1 -1 124\n\
             TcpExt: ListenOverflows\nTcpExt: 62\n",
            &mut counters,
        );
        assert_eq!(counters["TcpRetransSegs"].value, 124);
        assert_eq!(counters["TcpMaxConn"].value, -1, "a setting, not a counter");
        assert_eq!(counters["TcpExtListenOverflows"].section, "TcpExt");
    }

    #[test]
    fn snmp6_names_are_already_nstat_s() {
        let mut counters = BTreeMap::new();
        read_flat(
            "Ip6InReceives                   \t48837\nIcmp6InMsgs 3\nUdpLite6InErrors 0\n",
            &mut counters,
        );
        assert_eq!(counters["Ip6InReceives"].value, 48837);
        assert_eq!(counters["Ip6InReceives"].section, "Ip6");
        assert_eq!(counters["Icmp6InMsgs"].section, "Icmp6");
        assert_eq!(counters["UdpLite6InErrors"].section, "UdpLite6");
    }

    /// A known number of connections refused by a port nobody listens on: a
    /// refused connect is a RST received in SYN-SENT, `TcpAttemptFails`, and
    /// the RST sent for it is `TcpOutRsts`; both move by at least the number
    /// made, other traffic on the machine adding to them.
    #[test]
    fn a_counted_run_reports_the_refusals_it_caused() {
        let mut collector = Box::new(Prepare(Mode::Count))
            .attach(&Target::System)
            .expect("system");
        collector.start().expect("the baseline");
        // A port bound and closed is a port nobody listens on, here.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        const REFUSED: u64 = 25;
        for _ in 0..REFUSED {
            assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
        }
        assert!(
            collector.sample(0).unwrap().is_empty(),
            "a count reads at the close"
        );
        let events = collector.stop();
        let moved = |name: &str| {
            events
                .iter()
                .find(|event| event.attr("name").unwrap().to_string() == name)
                .and_then(|event| event.attr("value").unwrap().as_f64())
                .unwrap_or(0.0)
        };
        assert!(moved("TcpAttemptFails") >= REFUSED as f64, "{events:?}");
        assert!(moved("TcpOutRsts") >= REFUSED as f64, "{events:?}");
        assert!(
            events.iter().all(|event| event.attr("gauge").is_some()
                || event.attr("value").unwrap().as_f64() > Some(0.0)),
            "only what moved"
        );
    }

    #[test]
    fn a_snapshot_reports_every_counter_as_it_stands() {
        let mut collector = Box::new(Prepare(Mode::Snapshot))
            .attach(&Target::System)
            .expect("system");
        let events = collector.sample(0).expect("one read");
        let truth = read_all().unwrap();
        assert_eq!(events.len(), truth.len(), "every counter");
        let established = events
            .iter()
            .find(|event| event.attr("name").unwrap().to_string() == "TcpCurrEstab")
            .expect("a gauge");
        assert!(established.attr("gauge").is_some());
        assert!(collector.stop().is_empty());
    }

    #[test]
    fn a_process_target_is_refused() {
        let Err(error) = Box::new(Prepare(Mode::Snapshot)).attach(&Target::Pid(vec![1])) else {
            panic!("the counters are the machine's");
        };
        assert!(error.contains("--system"), "{error}");
    }
}
