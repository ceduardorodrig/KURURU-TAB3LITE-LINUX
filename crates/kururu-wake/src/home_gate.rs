//! Home-LAN presence gate for `POST /power/shutdown`.
//!
//! The endpoint is reachable over Tailscale, so without this gate a host that left the house is
//! still powered off when the house loses mains. The host must prove it is on the home LAN: the
//! home gateway IP is on-link and answers with the expected hardware address.
//!
//! During a real outage the router may stop answering before the request arrives, so the last live
//! confirmation stays valid for a bounded time. Only silence may use that memory: a different
//! hardware address on the gateway IP erases it, and a host with no on-link route to the gateway
//! is refused outright.

use std::net::Ipv4Addr;
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use crate::{format_mac, parse_mac};

pub const ENV_SWITCH: &str = "WOL_HOME_GATE";
pub const ENV_GATEWAY_IP: &str = "WOL_HOME_GATEWAY_IP";
pub const ENV_GATEWAY_MAC: &str = "WOL_HOME_GATEWAY_MAC";
pub const ENV_CACHE_MAX_AGE: &str = "WOL_HOME_CACHE_MAX_AGE_SECS";

const DEFAULT_GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 3, 1);
/// The sentinel needs 180 s of accumulated blackout before it asks, and flapping mains stretches
/// that. 30 minutes covers it with a wide margin.
const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 60);
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    pub gateway_ip: Ipv4Addr,
    pub gateway_mac: [u8; 6],
    pub cache_max_age: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Config {
    /// Explicitly disabled by the operator: every shutdown request is accepted.
    Off,
    Armed(Home),
    /// The gate cannot verify anything, so every shutdown request is refused.
    Invalid(String),
}

impl Config {
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Config {
        let get = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        match get(ENV_SWITCH).map(|v| v.to_ascii_lowercase()).as_deref() {
            None | Some("on") => {}
            Some("off") => return Config::Off,
            Some(_) => return Config::Invalid(format!("{} must be 'on' or 'off'", ENV_SWITCH)),
        }

        let gateway_ip = match get(ENV_GATEWAY_IP) {
            None => DEFAULT_GATEWAY_IP,
            Some(v) => match v.parse() {
                Ok(ip) => ip,
                Err(_) => {
                    return Config::Invalid(format!("{} is not an IPv4 address", ENV_GATEWAY_IP))
                }
            },
        };
        let gateway_mac = match get(ENV_GATEWAY_MAC).and_then(|v| parse_mac(&v)) {
            Some(mac) => mac,
            None => return Config::Invalid(format!("{} is missing or not a MAC", ENV_GATEWAY_MAC)),
        };
        let cache_max_age = match get(ENV_CACHE_MAX_AGE) {
            None => DEFAULT_CACHE_MAX_AGE,
            Some(v) => match v.parse() {
                Ok(secs) => Duration::from_secs(secs),
                Err(_) => {
                    return Config::Invalid(format!(
                        "{} is not a number of seconds",
                        ENV_CACHE_MAX_AGE
                    ))
                }
            },
        };

        Config::Armed(Home {
            gateway_ip,
            gateway_mac,
            cache_max_age,
        })
    }

    pub fn describe(&self) -> String {
        match self {
            Config::Off => format!("DISABLED by {}=off", ENV_SWITCH),
            Config::Armed(home) => format!(
                "armed: gateway {} at {}, cached confirmation valid for {}s",
                home.gateway_ip,
                format_mac(home.gateway_mac),
                home.cache_max_age.as_secs()
            ),
            Config::Invalid(reason) => format!(
                "MISCONFIGURED ({}): every shutdown request will be refused",
                reason
            ),
        }
    }
}

/// Runs a network tool. `None` means it could not be executed at all.
pub trait Runner {
    fn run(&self, tool: &str, args: &[&str]) -> Option<(bool, String)>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, tool: &str, args: &[&str]) -> Option<(bool, String)> {
        // Absolute paths first: the Android boot hook's PATH has neither /bin nor /usr/bin.
        for dir in ["/usr/bin/", "/bin/", "/usr/sbin/", "/sbin/", ""] {
            if let Ok(out) = Command::new(format!("{}{}", dir, tool)).args(args).output() {
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                return Some((out.status.success(), stdout));
            }
        }
        None
    }
}

/// What the network says right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// The expected gateway answered just now.
    Home,
    /// Another device holds the gateway IP.
    Foreign([u8; 6]),
    /// No interface has an on-link route to the gateway.
    OffSubnet,
    /// Nothing proves or disproves it, typically a silent router.
    Silent,
}

fn route_covers(destination: &str, ip: Ipv4Addr) -> bool {
    let (network, prefix) = destination.split_once('/').unwrap_or((destination, "32"));
    match (network.parse::<Ipv4Addr>(), prefix.parse::<u32>()) {
        (Ok(network), Ok(prefix @ 1..=32)) => {
            let mask = u32::MAX << (32 - prefix);
            u32::from(network) & mask == u32::from(ip) & mask
        }
        _ => false,
    }
}

/// Interfaces that reach `ip` directly. Routed paths (`via`, the VPN) never count.
fn on_link_devices(routes: &str, ip: Ipv4Addr) -> Vec<String> {
    let mut devices = Vec::new();
    for line in routes.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.contains(&"via") || tokens.contains(&"linkdown") {
            continue;
        }
        if !tokens.first().is_some_and(|dst| route_covers(dst, ip)) {
            continue;
        }
        if let Some(pair) = tokens.windows(2).find(|pair| pair[0] == "dev") {
            if !pair[1].starts_with("tailscale") {
                devices.push(pair[1].to_string());
            }
        }
    }
    devices
}

/// Parses `ip -4 neigh show to <ip> dev <dev>` into (hardware address, is `REACHABLE`).
fn parse_neighbour(output: &str, ip: Ipv4Addr) -> Option<([u8; 6], bool)> {
    let ip = ip.to_string();
    output.lines().find_map(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.first() != Some(&ip.as_str()) {
            return None;
        }
        let pair = tokens.windows(2).find(|pair| pair[0] == "lladdr")?;
        let mac = parse_mac(pair[1])?;
        Some((mac, tokens.last() == Some(&"REACHABLE")))
    })
}

/// Actively probes the gateway, then reads the neighbour table.
///
/// An entry alone proves nothing because it may be stale. It counts only if the gateway answered a
/// ping sent through that same interface, or the kernel itself marks the entry `REACHABLE`.
pub fn observe(home: &Home, runner: &impl Runner) -> Presence {
    let ip = home.gateway_ip.to_string();
    let routes = match runner.run("ip", &["-4", "route", "show", "table", "main"]) {
        Some((true, out)) => out,
        _ => return Presence::Silent,
    };
    let devices = on_link_devices(&routes, home.gateway_ip);
    if devices.is_empty() {
        return Presence::OffSubnet;
    }

    let mut foreign = None;
    for dev in &devices {
        // One second per interface keeps the answer inside the sentinel's 3 s HTTP timeout.
        let answered = matches!(
            runner.run("ping", &["-c", "1", "-W", "1", "-I", dev, &ip]),
            Some((true, _))
        );
        let entry = match runner.run("ip", &["-4", "neigh", "show", "to", &ip, "dev", dev]) {
            Some((true, out)) => parse_neighbour(&out, home.gateway_ip),
            _ => None,
        };
        match entry {
            Some((mac, reachable)) if mac == home.gateway_mac => {
                if answered || reachable {
                    return Presence::Home;
                }
            }
            Some((mac, _)) => foreign = Some(mac),
            None => {}
        }
    }
    foreign.map_or(Presence::Silent, Presence::Foreign)
}

/// A point in time on both clocks. The monotonic one stops during suspend, the wall one does not.
#[derive(Debug, Clone, Copy)]
pub struct Stamp {
    pub mono: Instant,
    pub wall: SystemTime,
}

impl Stamp {
    pub fn now() -> Stamp {
        Stamp {
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    /// The larger of both ages, so neither a suspend nor a clock step backwards rejuvenates it.
    fn age_at(self, now: Stamp) -> Duration {
        let mono = now.mono.saturating_duration_since(self.mono);
        let wall = now.wall.duration_since(self.wall).unwrap_or_default();
        mono.max(wall)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow(String),
    Refuse(String),
}

pub struct HomeGate {
    pub config: Config,
    last_home: Mutex<Option<Stamp>>,
}

impl HomeGate {
    pub fn new(config: Config) -> HomeGate {
        HomeGate {
            config,
            last_home: Mutex::new(None),
        }
    }

    /// Probes the network and updates the memory of the last confirmed presence.
    ///
    /// Returns what was seen and the age of the remembered confirmation, if any.
    pub fn refresh(
        &self,
        home: &Home,
        runner: &impl Runner,
        now: Stamp,
    ) -> (Presence, Option<Duration>) {
        let presence = observe(home, runner);
        let mut last = self
            .last_home
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match presence {
            Presence::Home => *last = Some(now),
            Presence::Foreign(_) => *last = None,
            Presence::OffSubnet | Presence::Silent => {}
        }
        let age = last.map(|stamp| stamp.age_at(now));
        (presence, age)
    }

    /// Decides one shutdown request.
    pub fn check(&self, runner: &impl Runner, now: Stamp) -> Verdict {
        let home = match &self.config {
            Config::Off => return Verdict::Allow("home gate disabled".to_string()),
            Config::Invalid(reason) => {
                return Verdict::Refuse(format!("home gate misconfigured: {}", reason))
            }
            Config::Armed(home) => home,
        };

        match self.refresh(home, runner, now) {
            (Presence::Home, _) => Verdict::Allow("home gateway confirmed".to_string()),
            (Presence::Foreign(mac), _) => Verdict::Refuse(format!(
                "foreign network: gateway {} is {}",
                home.gateway_ip,
                format_mac(mac)
            )),
            (Presence::OffSubnet, _) => Verdict::Refuse(format!(
                "away: no on-link route to home gateway {}",
                home.gateway_ip
            )),
            (Presence::Silent, Some(age)) if age <= home.cache_max_age => Verdict::Allow(format!(
                "home gateway silent, last confirmed {}s ago",
                age.as_secs()
            )),
            (Presence::Silent, Some(age)) => Verdict::Refuse(format!(
                "home gateway silent and last confirmation is {}s old",
                age.as_secs()
            )),
            (Presence::Silent, None) => {
                Verdict::Refuse("home gateway silent and presence was never confirmed".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME_MAC: &str = "02:00:00:00:00:01";
    const OTHER_MAC: &str = "02:00:00:00:00:99";
    const HOME_ROUTES: &str =
        "default via 192.168.3.1 dev eno1 proto dhcp src 192.168.3.100 metric 100 \n\
        172.17.0.0/16 dev docker0 proto kernel scope link src 172.17.0.1 \n\
        192.168.3.0/24 dev eno1 proto kernel scope link src 192.168.3.100 metric 100 \n";
    const AWAY_ROUTES: &str = "default via 10.0.0.1 dev wlan0 proto dhcp \n\
        10.0.0.0/24 dev wlan0 proto kernel scope link src 10.0.0.23 \n";

    /// Injected route table, ping result and neighbour table.
    struct Net {
        routes: &'static str,
        ping: bool,
        neigh: String,
    }

    impl Runner for Net {
        fn run(&self, tool: &str, args: &[&str]) -> Option<(bool, String)> {
            match (tool, args.get(1)) {
                ("ip", Some(&"route")) => Some((true, self.routes.to_string())),
                ("ip", Some(&"neigh")) => Some((true, self.neigh.clone())),
                ("ping", _) => Some((self.ping, String::new())),
                _ => None,
            }
        }
    }

    fn net(routes: &'static str, ping: bool, neigh: &str) -> Net {
        Net {
            routes,
            ping,
            neigh: neigh.to_string(),
        }
    }

    fn entry(mac: &str, state: &str) -> String {
        format!("192.168.3.1 lladdr {} {} \n", mac, state)
    }

    fn at_home() -> Net {
        net(HOME_ROUTES, true, &entry(HOME_MAC, "REACHABLE"))
    }

    fn router_dead() -> Net {
        net(HOME_ROUTES, false, "192.168.3.1  FAILED \n")
    }

    fn config(pairs: &[(&str, &str)]) -> Config {
        Config::from_lookup(|key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        })
    }

    fn gate() -> HomeGate {
        HomeGate::new(config(&[(ENV_GATEWAY_MAC, HOME_MAC)]))
    }

    fn later(start: Stamp, secs: u64) -> Stamp {
        let d = Duration::from_secs(secs);
        Stamp {
            mono: start.mono + d,
            wall: start.wall + d,
        }
    }

    fn allowed(verdict: Verdict) -> bool {
        matches!(verdict, Verdict::Allow(_))
    }

    #[test]
    fn at_home_with_the_expected_gateway_is_allowed() {
        assert!(allowed(gate().check(&at_home(), Stamp::now())));
    }

    #[test]
    fn away_on_another_subnet_is_refused_even_with_a_fresh_cache() {
        let (gate, start) = (gate(), Stamp::now());
        assert!(allowed(gate.check(&at_home(), start)));
        let away = net(AWAY_ROUTES, true, "");
        assert!(!allowed(gate.check(&away, later(start, 5))));
    }

    #[test]
    fn reaching_the_home_subnet_through_a_route_is_not_presence() {
        let routed = "default via 10.0.0.1 dev wlan0 \n\
            192.168.3.0/24 via 100.64.0.1 dev wlan0 \n\
            192.168.3.0/24 dev tailscale0 scope link \n";
        let away = net(routed, true, &entry(HOME_MAC, "REACHABLE"));
        assert!(!allowed(gate().check(&away, Stamp::now())));
    }

    #[test]
    fn same_subnet_with_a_different_gateway_mac_is_refused() {
        for state in ["REACHABLE", "STALE"] {
            let foreign = net(HOME_ROUTES, true, &entry(OTHER_MAC, state));
            assert!(!allowed(gate().check(&foreign, Stamp::now())), "{}", state);
        }
    }

    #[test]
    fn a_foreign_gateway_erases_the_cached_presence() {
        let (gate, start) = (gate(), Stamp::now());
        assert!(allowed(gate.check(&at_home(), start)));
        let foreign = net(HOME_ROUTES, true, &entry(OTHER_MAC, "REACHABLE"));
        assert!(!allowed(gate.check(&foreign, later(start, 5))));
        assert!(!allowed(gate.check(&router_dead(), later(start, 10))));
    }

    #[test]
    fn home_with_the_router_down_is_allowed_from_the_cache() {
        let (gate, start) = (gate(), Stamp::now());
        assert!(allowed(gate.check(&at_home(), start)));
        for secs in [180, 600, 1800] {
            assert!(allowed(gate.check(&router_dead(), later(start, secs))));
        }
    }

    #[test]
    fn the_cache_expires() {
        let (gate, start) = (gate(), Stamp::now());
        assert!(allowed(gate.check(&at_home(), start)));
        assert!(!allowed(gate.check(&router_dead(), later(start, 1801))));
    }

    #[test]
    fn a_silent_router_does_not_renew_the_cache() {
        let (gate, start) = (gate(), Stamp::now());
        assert!(allowed(gate.check(&at_home(), start)));
        assert!(allowed(gate.check(&router_dead(), later(start, 1700))));
        assert!(!allowed(gate.check(&router_dead(), later(start, 1900))));
    }

    #[test]
    fn time_spent_suspended_ages_the_cache() {
        let (gate, start) = (gate(), Stamp::now());
        assert!(allowed(gate.check(&at_home(), start)));
        let resumed = Stamp {
            mono: start.mono + Duration::from_secs(10),
            wall: start.wall + Duration::from_secs(7200),
        };
        assert!(!allowed(gate.check(&router_dead(), resumed)));
    }

    #[test]
    fn a_stale_entry_without_an_answer_is_not_a_confirmation() {
        let stale = net(HOME_ROUTES, false, &entry(HOME_MAC, "STALE"));
        let (gate, start) = (gate(), Stamp::now());
        // Never confirmed: the stale entry alone must not open the gate.
        assert!(!allowed(gate.check(&stale, start)));
        // Nor may it renew an older confirmation.
        assert!(allowed(gate.check(&at_home(), start)));
        assert!(allowed(gate.check(&stale, later(start, 1700))));
        assert!(!allowed(gate.check(&stale, later(start, 1900))));
    }

    #[test]
    fn an_answered_ping_or_a_reachable_entry_confirms_the_expected_mac() {
        let answered = net(HOME_ROUTES, true, &entry(HOME_MAC, "DELAY"));
        assert!(allowed(gate().check(&answered, Stamp::now())));
        let icmp_filtered = net(HOME_ROUTES, false, &entry(HOME_MAC, "REACHABLE"));
        assert!(allowed(gate().check(&icmp_filtered, Stamp::now())));
    }

    #[test]
    fn a_ping_answer_without_a_neighbour_entry_is_not_presence() {
        let unknown = net(HOME_ROUTES, true, "");
        assert!(!allowed(gate().check(&unknown, Stamp::now())));
    }

    #[test]
    fn missing_tools_are_refused_without_a_cache() {
        struct Nothing;
        impl Runner for Nothing {
            fn run(&self, _: &str, _: &[&str]) -> Option<(bool, String)> {
                None
            }
        }
        assert!(!allowed(gate().check(&Nothing, Stamp::now())));
    }

    #[test]
    fn missing_or_bad_configuration_refuses_even_at_home() {
        let bad: [&[(&str, &str)]; 5] = [
            &[],
            &[(ENV_GATEWAY_MAC, "nonsense")],
            &[(ENV_GATEWAY_MAC, HOME_MAC), (ENV_GATEWAY_IP, "router")],
            &[(ENV_GATEWAY_MAC, HOME_MAC), (ENV_CACHE_MAX_AGE, "30m")],
            &[(ENV_GATEWAY_MAC, HOME_MAC), (ENV_SWITCH, "of")],
        ];
        for pairs in bad {
            let gate = HomeGate::new(config(pairs));
            assert!(
                !allowed(gate.check(&at_home(), Stamp::now())),
                "{:?}",
                pairs
            );
        }
    }

    #[test]
    fn only_an_explicit_off_disables_the_gate() {
        let gate = HomeGate::new(config(&[(ENV_SWITCH, "off")]));
        assert!(allowed(
            gate.check(&net(AWAY_ROUTES, false, ""), Stamp::now())
        ));
    }

    #[test]
    fn gateway_and_cache_age_are_overridable() {
        let parsed = config(&[
            (ENV_GATEWAY_IP, "10.0.0.1"),
            (ENV_GATEWAY_MAC, HOME_MAC),
            (ENV_CACHE_MAX_AGE, "60"),
        ]);
        assert_eq!(
            parsed,
            Config::Armed(Home {
                gateway_ip: Ipv4Addr::new(10, 0, 0, 1),
                gateway_mac: [2, 0, 0, 0, 0, 1],
                cache_max_age: Duration::from_secs(60),
            })
        );
    }
}
