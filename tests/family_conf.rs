//! The `conf` family's ground truth: every setting it reads agrees with what
//! a second, independent path says -- `sysctl -n`, `taskset -p`, `chrt -p`,
//! `prlimit`, a cgroup file read with `cat` -- and `conf.changed` sees a
//! setting the program changed half-way through its run.

use std::collections::BTreeMap;
use std::process::Command;

use serde_json::Value;

const BINARY: &str = env!("CARGO_BIN_EXE_omn");

/// The readings of one `conf.*` run, by setting (and instance, when there is
/// one: `setting[instance]`).
fn readings(args: &[&str]) -> BTreeMap<String, Value> {
    // The options go before the program, which takes everything after `--`.
    let (options, program) = match args.iter().position(|arg| *arg == "--") {
        Some(at) => args.split_at(at),
        None => (args, &[][..]),
    };
    let output = Command::new(BINARY)
        .args(options)
        .args(["-e", "raw", "--format", "json"])
        .args(program)
        .output()
        .expect("omn runs");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_str(&text).expect("json");
    let mut found = BTreeMap::new();
    for result in document["results"].as_array().unwrap() {
        for output in result["outputs"].as_array().unwrap() {
            let Some(events) = output["events"].as_array() else {
                continue;
            };
            for event in events {
                // Attributes are a list of {name, value}: made a map here.
                let attrs: serde_json::Map<String, Value> = event["attrs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|attr| {
                        (
                            attr["name"].as_str().unwrap().to_owned(),
                            attr["value"].clone(),
                        )
                    })
                    .collect();
                let attrs = Value::Object(attrs);
                let attrs = &attrs;
                let setting = attrs["setting"].as_str().unwrap_or_default().to_owned();
                let key = match attrs["instance"].as_str() {
                    Some(instance) => format!("{setting}[{instance}]"),
                    None => setting,
                };
                found.insert(key, attrs.clone());
            }
        }
    }
    found
}

fn value(readings: &BTreeMap<String, Value>, key: &str) -> String {
    let attrs = readings
        .get(key)
        .unwrap_or_else(|| panic!("no reading of {key}: {:?}", readings.keys()));
    assert_eq!(attrs["state"], "set", "{key}: {attrs}");
    attrs["value"].as_str().unwrap().to_owned()
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[test]
fn the_machine_settings_are_what_sysctl_and_sysfs_say() {
    let machine = readings(&["conf.machine"]);
    for (setting, key) in [
        ("vm.swappiness", "vm.swappiness"),
        ("vm.overcommit_memory", "vm.overcommit_memory"),
        ("vm.dirty_ratio", "vm.dirty_ratio"),
        ("vm.min_free_kbytes", "vm.min_free_kbytes"),
        ("net.core.somaxconn", "net.core.somaxconn"),
        (
            "net.ipv4.tcp_congestion_control",
            "net.ipv4.tcp_congestion_control",
        ),
        (
            "kernel.sched_autogroup_enabled",
            "kernel.sched_autogroup_enabled",
        ),
    ] {
        let Some(expected) = run("sysctl", &["-n", key]) else {
            eprintln!("skipped {setting}: sysctl cannot read {key}");
            continue;
        };
        // sysctl separates a vector with tabs; the file, with tabs too.
        assert_eq!(value(&machine, setting), expected, "{setting}");
    }
    if let Ok(text) = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled") {
        let chosen = text
            .split('[')
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .unwrap();
        assert_eq!(value(&machine, "mm.thp.enabled"), chosen);
    }
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online").unwrap();
    let count: usize = online
        .trim()
        .split(',')
        .map(|part| match part.split_once('-') {
            Some((low, high)) => high.parse::<usize>().unwrap() - low.parse::<usize>().unwrap() + 1,
            None => 1,
        })
        .sum();
    assert_eq!(value(&machine, "cpu.online_count"), count.to_string());
    // Every instance the kernel has, and none it does not.
    let policies = std::fs::read_dir("/sys/devices/system/cpu/cpufreq")
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().join("scaling_governor").exists())
                .count()
        })
        .unwrap_or(0);
    let read = machine
        .keys()
        .filter(|key| key.starts_with("cpufreq.governor["))
        .count();
    assert_eq!(read, policies);
    // Nothing is answered with a guess.
    for (key, attrs) in &machine {
        match attrs["state"].as_str().unwrap() {
            "set" => assert!(attrs["value"].is_string(), "{key}: {attrs}"),
            _ => assert!(
                attrs["reason"].is_string() && attrs.get("value").is_none(),
                "{key}: {attrs}"
            ),
        }
    }
}

/// A program launched with its settings already chosen by the caller is
/// read with them: affinity, nice, policy, OOM adjustment, a limit.
#[test]
fn the_process_settings_are_what_the_process_was_given() {
    let program = [
        "taskset",
        "-c",
        "0",
        "nice",
        "-n",
        "7",
        "chrt",
        "-b",
        "0",
        "prlimit",
        "--nofile=1000:2000",
        // The adjustment set by the caller before the program, as every other
        // setting here is: a program that sets it itself (`echo 345 >
        // /proc/self/oom_score_adj`) can be read before it has, under load.
        "choom",
        "-n",
        "345",
        "--",
        "sleep",
        "0.5",
    ];
    let mut args = vec!["conf.process", "--"];
    args.extend(program);
    let process = readings(&args);
    assert_eq!(value(&process, "process.cpus_allowed"), "0");
    assert_eq!(value(&process, "process.cpus_allowed_count"), "1");
    assert_eq!(value(&process, "process.nice"), "7");
    assert_eq!(value(&process, "process.oom_score_adj"), "345");
    assert_eq!(value(&process, "process.limit.nofile"), "1000");
    assert_eq!(process["process.limit.nofile"]["hard"], "2000");
    let policy = process
        .iter()
        .find(|(key, _)| key.starts_with("thread.sched_policy["))
        .map(|(_, attrs)| attrs["value"].as_str().unwrap().to_owned());
    assert_eq!(policy.as_deref(), Some("SCHED_BATCH"));
}

/// A program in a user scope with memory limits reads those limits, from its
/// own cgroup.
#[test]
fn the_cgroup_limits_are_the_scope_s() {
    let made = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "-q",
            "-p",
            "MemoryHigh=200M",
            "-p",
            "MemoryMax=256M",
            "true",
        ])
        .output();
    if !made.is_ok_and(|output| output.status.success()) {
        return eprintln!("skipped: no user scope with memory limits here");
    }
    let output = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "-q",
            "-p",
            "MemoryHigh=200M",
            "-p",
            "MemoryMax=256M",
        ])
        .arg(BINARY)
        .args([
            "conf.process",
            "-e",
            "raw",
            "--format",
            "json",
            "--",
            "sleep",
            "0.3",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("\"268435456\""), "{text}");
    assert!(text.contains("\"209715200\""), "{text}");
}

/// The program changes its own OOM adjustment and affinity half-way through;
/// `conf.changed` names both, with the values on either side.
#[test]
fn a_setting_changed_during_the_run_is_said() {
    let changed = readings(&[
        "conf.changed",
        "-m",
        "poll@50ms",
        "--",
        "sh",
        "-c",
        "sleep 0.3; echo 222 > /proc/$$/oom_score_adj; taskset -pc 0 $$ >/dev/null; sleep 0.4",
    ]);
    let adj = &changed["process.oom_score_adj"];
    assert_eq!(
        (adj["before"].as_str(), adj["after"].as_str()),
        (Some("0"), Some("222")),
        "{adj}"
    );
    let mask = &changed["process.cpus_allowed"];
    assert_eq!(mask["after"], "0", "{mask}");
    // The score the kernel derives moves with them, and is not a setting.
    assert!(!changed.contains_key("process.oom_score"), "{changed:?}");
    assert!(changed["watched"]["changed"].as_u64().unwrap() >= 2);
}
