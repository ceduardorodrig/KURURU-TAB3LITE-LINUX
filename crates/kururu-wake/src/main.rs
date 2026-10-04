use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BROADCAST_ADDRS: [&str; 2] = [
    "192.168.3.255:9",
    "255.255.255.255:9",
];

const CONFIG_PATHS: [&str; 2] = [
    "/etc/kururu-wake.conf",
    "/etc/wol-relay.env",
];

const LOG_FILE: &str = "/var/log/kururu-wol.log";

/// Returns "[HH:MM:SS]" in Brasília time (UTC-3, fixed — Brazil abolished DST in 2019).
/// Pure Rust, no libc / TZ database lookup required.
fn timestamp_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // UTC-3 = subtract 3 hours (10800s)
    let local = secs.saturating_sub(3 * 3600);
    let h = (local / 3600) % 24;
    let m = (local / 60) % 60;
    let s = local % 60;
    format!("[{:02}:{:02}:{:02}]", h, m, s)
}

fn log_action(msg: &str) {
    let ts = timestamp_str();
    let line = format!("{} {}", ts, msg);
    println!("{}", line);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_FILE) {
        let _ = writeln!(f, "{}", line);
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

fn forward_get(addr_str: &str, path: &str, timeout_ms: u64) -> Result<String, String> {
    let socket_addrs: Vec<std::net::SocketAddr> = addr_str
        .to_socket_addrs()
        .map_err(|e| format!("Failed to resolve {}: {}", addr_str, e))?
        .collect();
    if socket_addrs.is_empty() {
        return Err(format!("No address resolved for {}", addr_str));
    }
    let mut stream = TcpStream::connect_timeout(&socket_addrs[0], Duration::from_millis(timeout_ms))
        .map_err(|e| format!("Connect to {} failed: {}", addr_str, e))?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(timeout_ms)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(timeout_ms)));

    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, addr_str
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Write failed: {}", e))?;

    let mut resp = Vec::new();
    stream
        .read_to_end(&mut resp)
        .map_err(|e| format!("Read failed: {}", e))?;
    let resp_str = String::from_utf8_lossy(&resp).to_string();
    if resp_str.starts_with("HTTP/1.1 200") || resp_str.starts_with("HTTP/1.0 200") {
        Ok(resp_str)
    } else {
        Err(format!(
            "Non-200 response from {}: {}",
            addr_str,
            resp_str.lines().next().unwrap_or("empty")
        ))
    }
}

fn handle_dispatcher_client(mut stream: TcpStream) {
    let mut buf = [0u8; 1024];
    let bytes_read = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let req = String::from_utf8_lossy(&buf[..bytes_read]);
    let first_line = req.lines().next().unwrap_or_default();
    let parts: Vec<&str> = first_line.split_whitespace().collect();

    if parts.len() < 2 {
        return;
    }

    let is_head = parts[0] == "HEAD";
    let path = parts[1];

    let kururu_addr = env::var("KURURU_ADDR").unwrap_or_else(|_| "100.127.188.45:9096".to_string());
    let psicopompo_addr = env::var("PSICOPOMPO_ADDR").unwrap_or_else(|_| "100.82.51.112:9096".to_string());
    let kavure_addr = env::var("KAVURE_ADDR").unwrap_or_else(|_| "100.124.146.77:9096".to_string());

    if path == "/" || path == "/health" || path == "/health/" {
        let k_ok = forward_get(&kururu_addr, "/health", 800).is_ok();
        let p_ok = if !k_ok {
            forward_get(&psicopompo_addr, "/health", 800).is_ok()
        } else {
            true
        };
        let v_ok = if !k_ok && !p_ok {
            forward_get(&kavure_addr, "/health", 800).is_ok()
        } else {
            true
        };

        let any_online = k_ok || p_ok || v_ok;
        let (status_code, body) = if any_online {
            (
                "200 OK",
                format!(
                    r#"{{"status":"online","service":"wol-dispatcher","kururu":{},"psicopompo":{},"kavure":{}}}"#,
                    k_ok, p_ok, v_ok
                ),
            )
        } else {
            (
                "503 Service Unavailable",
                r#"{"status":"error","service":"wol-dispatcher","message":"All LAN wake nodes unreachable"}"#.to_string(),
            )
        };

        let response = if is_head {
            format!(
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status_code,
                body.len()
            )
        } else {
            format!(
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status_code,
                body.len(),
                body
            )
        };
        let _ = stream.write_all(response.as_bytes());
        return;
    }

    let target = if path.starts_with("/wake/") {
        path[6..].trim_end_matches('/').to_lowercase()
    } else {
        String::new()
    };

    if target == "kavure" {
        log_action("[Dispatcher] Wake request for kavure -> trying kururu primary...");
        match forward_get(&kururu_addr, "/wake/kavure", 1500) {
            Ok(_) => {
                log_action("[Dispatcher] Successfully woke kavure via kururu");
                let body = r#"{"status":"ok","target":"kavure","via":"kururu","emitted":true}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
            Err(e1) => {
                log_action(&format!(
                    "[Dispatcher] Kururu failed ({}). Falling back to psicopompo...",
                    e1
                ));
                match forward_get(&psicopompo_addr, "/wake", 1500) {
                    Ok(_) => {
                        log_action("[Dispatcher] Successfully woke kavure via psicopompo fallback");
                        let body = r#"{"status":"ok","target":"kavure","via":"psicopompo","emitted":true}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(e2) => {
                        log_action(&format!(
                            "[Dispatcher] Psicopompo fallback also failed ({})",
                            e2
                        ));
                        let body = format!(
                            r#"{{"status":"error","target":"kavure","message":"All wake nodes offline (kururu: {}, psicopompo: {})"}}"#,
                            e1, e2
                        );
                        let response = format!(
                            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                }
            }
        }
    } else if target == "psicopompo" {
        log_action("[Dispatcher] Wake request for psicopompo -> trying kururu primary...");
        match forward_get(&kururu_addr, "/wake/psicopompo", 1500) {
            Ok(_) => {
                log_action("[Dispatcher] Successfully woke psicopompo via kururu");
                let body = r#"{"status":"ok","target":"psicopompo","via":"kururu","emitted":true}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
            Err(e1) => {
                log_action(&format!(
                    "[Dispatcher] Kururu failed ({}). Falling back to kavure...",
                    e1
                ));
                match forward_get(&kavure_addr, "/wake", 1500) {
                    Ok(_) => {
                        log_action("[Dispatcher] Successfully woke psicopompo via kavure fallback");
                        let body = r#"{"status":"ok","target":"psicopompo","via":"kavure","emitted":true}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(e2) => {
                        log_action(&format!("[Dispatcher] Kavure fallback also failed ({})", e2));
                        let body = format!(
                            r#"{{"status":"error","target":"psicopompo","message":"All wake nodes offline (kururu: {}, kavure: {})"}}"#,
                            e1, e2
                        );
                        let response = format!(
                            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                }
            }
        }
    } else {
        let body = format!(r#"{{"status":"error","message":"Unknown target '{}'"}}"#, target);
        let response = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
    }
}

fn run_dispatcher(port: u16) {
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
        "[WOL Dispatcher] Starting Smart Dispatcher HTTP listener on {}...",
        bind_addr
    ));

    let listener = match TcpListener::bind(&bind_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind Dispatcher listener on {}: {}", bind_addr, e);
            std::process::exit(1);
        }
    };

    for stream in listener.incoming() {
        if let Ok(s) = stream {
            thread::spawn(move || {
                handle_dispatcher_client(s);
            });
        }
    }
}

fn print_usage() {
    println!("Kururu Wake-on-LAN (WOL) Tool v1.2");
    println!("Usage:");
    println!("  kururu-wake <HOSTNAME|MAC>         Send Magic Packet to configured target or MAC");
    println!("  kururu-wake --daemon [PORT]        Run HTTP WOL Relay daemon (default: 9096)");
    println!("  kururu-wake --dispatcher [PORT]    Run Smart WoL Dispatcher with auto-failover (default: 9096)");
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
        "--dispatcher" | "--dispatch" => {
            let port = args
                .get(1)
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(9096);
            run_dispatcher(port);
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
