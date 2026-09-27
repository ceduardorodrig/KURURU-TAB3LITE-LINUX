use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::thread;
use std::time::Duration;

const BROADCAST_ADDRS: [&str; 2] = [
    "192.168.3.255:9",
    "255.255.255.255:9",
];

const CONFIG_PATHS: [&str; 2] = [
    "/etc/kururu-wake.conf",
    "/etc/wol-relay.env",
];

const LOG_FILE: &str = "/var/log/kururu-wol.log";

fn log_action(msg: &str) {
    println!("{}", msg);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_FILE) {
        let _ = writeln!(f, "{}", msg);
    }
}

fn parse_mac(input: &str) -> Option<[u8; 6]> {
    let cleaned: String = input.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() != 12 {
        return None;
    }

    let mut mac = [0u8; 6];
    for i in 0..6 {
        mac[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(mac)
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

fn resolve_target_mac(target: &str) -> Option<[u8; 6]> {
    let lower = target.to_lowercase();

    // 1. Check environment variable (e.g. WOL_PSICOPOMPO_MAC)
    let env_var = format!("WOL_{}_MAC", lower.to_uppercase());
    if let Ok(val) = env::var(&env_var) {
        if let Some(mac) = parse_mac(&val) {
            return Some(mac);
        }
    }

    // 2. Check configuration files (/etc/kururu-wake.conf or /etc/wol-relay.env)
    for path in &CONFIG_PATHS {
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('#') || trimmed.is_empty() {
                    continue;
                }
                if let Some((k, v)) = trimmed.split_once('=') {
                    let k_clean = k.trim().to_lowercase();
                    let v_clean = v.trim().trim_matches('"').trim_matches('\'');
                    if k_clean == lower
                        || k_clean == format!("wol_{}_mac", lower)
                        || k_clean == format!("{}_mac", lower)
                    {
                        if let Some(mac) = parse_mac(v_clean) {
                            return Some(mac);
                        }
                    }
                }
            }
        }
    }

    // 3. Fallback: Parse target directly if it is a raw MAC address
    if let Some(mac) = parse_mac(target) {
        return Some(mac);
    }

    None
}

fn send_magic_packet(mac: [u8; 6], target_name: &str) -> Result<(), String> {
    let mut packet = [0u8; 102];
    packet[..6].fill(0xFF);
    for i in 0..16 {
        packet[6 + i * 6..6 + (i + 1) * 6].copy_from_slice(&mac);
    }

    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| format!("Failed to bind UDP socket: {}", e))?;
    socket
        .set_broadcast(true)
        .map_err(|e| format!("Failed to enable broadcast: {}", e))?;

    let mac_str = format_mac(mac);
    log_action(&format!(
        "[Kururu WOL] Sending Magic Packet burst to {} ({})",
        target_name, mac_str
    ));

    // Send burst of 5 packets to multiple broadcast targets for maximum reliability
    for _ in 0..5 {
        for &addr in &BROADCAST_ADDRS {
            let _ = socket.send_to(&packet, addr);
        }
        thread::sleep(Duration::from_millis(25));
    }

    log_action(&format!(
        "[Kururu WOL] Successfully emitted Magic Packet to {} ({})",
        target_name, mac_str
    ));

    Ok(())
}

fn handle_http_client(mut stream: TcpStream) {
    let mut buf = [0u8; 1024];
    let bytes_read = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };

    let req = String::from_utf8_lossy(&buf[..bytes_read]);
    let first_line = req.lines().next().unwrap_or_default();
    let parts: Vec<&str> = first_line.split_whitespace().collect();

    if parts.len() < 2 {
        return;
    }

    let is_head = parts[0] == "HEAD";
    let path = parts[1];

    if path == "/" || path == "/health" || path == "/health/" {
        let body = r#"{"status":"online","service":"kururu-wol-relay","node":"kururu"}"#;
        let response = if is_head {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
        } else {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        };
        let _ = stream.write_all(response.as_bytes());
        return;
    }

    let target_opt = if path.starts_with("/wake/") {
        Some(path[6..].trim_end_matches('/'))
    } else if path == "/wake" || path == "/wake/" {
        Some(
            env::var("WOL_TARGET_HOST")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "kavure".to_string()),
        )
        .map(|s| Box::leak(s.into_boxed_str()) as &str)
    } else {
        None
    };

    if let Some(target) = target_opt {
        let mac = resolve_target_mac(target);

        if let Some(mac_bytes) = mac {
            let res = send_magic_packet(mac_bytes, target);
            let (status_code, body) = match res {
                Ok(_) => (
                    "200 OK",
                    format!(
                        r#"{{"status":"ok","target":"{}","mac":"{}","emitted":true}}"#,
                        target,
                        format_mac(mac_bytes)
                    ),
                ),
                Err(e) => (
                    "500 Internal Server Error",
                    format!(r#"{{"status":"error","message":"{}"}}"#, e),
                ),
            };

            let response = format!(
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status_code,
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        } else {
            let body = format!(
                r#"{{"status":"error","message":"Target '{}' not found in /etc/kururu-wake.conf and is not a valid MAC"}}"#,
                target
            );
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    }
}

fn run_daemon(port: u16) {
    let host = env::var("WOL_LISTEN_ADDR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let effective_port = env::var("WOL_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(port);
    let bind_addr = format!("{}:{}", host, effective_port);
    log_action(&format!(
        "[Kururu WOL Daemon] Starting HTTP listener on {}...",
        bind_addr
    ));

    let listener = match TcpListener::bind(&bind_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind HTTP listener on {}: {}", bind_addr, e);
            std::process::exit(1);
        }
    };

    for stream in listener.incoming() {
        if let Ok(s) = stream {
            thread::spawn(move || {
                handle_http_client(s);
            });
        }
    }
}

fn print_usage() {
    println!("Kururu Wake-on-LAN (WOL) Tool v1.1");
    println!("Usage:");
    println!("  kururu-wake <HOSTNAME|MAC>         Send Magic Packet to configured target or MAC");
    println!("  kururu-wake --daemon [PORT]        Run HTTP WOL Relay daemon (default: 9096)");
    println!("\nConfiguration:");
    println!("  Targets are mapped in /etc/kururu-wake.conf (e.g. host=aa:bb:cc:dd:ee:ff)");
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        print_usage();
        return;
    }

    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            print_usage();
        }
        "-d" | "--daemon" | "--serve" => {
            let port = args
                .get(1)
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(9096);
            run_daemon(port);
        }
        target => {
            if let Some(mac) = resolve_target_mac(target) {
                if let Err(e) = send_magic_packet(mac, target) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            } else {
                eprintln!(
                    "Error: Target '{}' not resolved in /etc/kururu-wake.conf and not a valid MAC",
                    target
                );
                eprintln!("Run 'kururu-wake --help' for usage.");
                std::process::exit(1);
            }
        }
    }
}
