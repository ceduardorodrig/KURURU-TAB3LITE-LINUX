use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::thread;
use std::time::Duration;

const PSICOPOMPO_MAC: [u8; 6] = [0xd0, 0x94, 0x66, 0xde, 0x8b, 0x58];
const KAVURE_MAC: [u8; 6] = [0xd0, 0x94, 0x66, 0xad, 0xf3, 0xc4];

const BROADCAST_ADDRS: [&str; 2] = [
    "192.168.3.255:9",
    "255.255.255.255:9",
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

    let path = parts[1];

    if path == "/" || path == "/health" {
        let body = r#"{"status":"online","service":"kururu-wol-relay","node":"kururu"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        return;
    }

    if path.starts_with("/wake/") {
        let target = &path[6..].trim_end_matches('/');
        let (mac, name) = match target.to_lowercase().as_str() {
            "psicopompo" => (Some(PSICOPOMPO_MAC), "psicopompo"),
            "kavure" => (Some(KAVURE_MAC), "kavure"),
            other => (parse_mac(other), "custom_target"),
        };

        if let Some(mac_bytes) = mac {
            let res = send_magic_packet(mac_bytes, name);
            let (status_code, body) = match res {
                Ok(_) => (
                    "200 OK",
                    format!(
                        r#"{{"status":"ok","target":"{}","mac":"{}","emitted":true}}"#,
                        name,
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
            let body = r#"{"status":"error","message":"Invalid target or MAC address"}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
        return;
    }

    let body = r#"{"status":"error","message":"Not found"}"#;
    let response = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn run_daemon(port: u16) {
    let bind_addr = format!("0.0.0.0:{}", port);
    log_action(&format!(
        "[Kururu WOL Daemon] Starting HTTP Relay listening on http://{}...",
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
    println!("Kururu Wake-on-LAN (WOL) Tool v1.0");
    println!("Usage:");
    println!("  kururu-wake psicopompo            Send Magic Packet to Psicopompo");
    println!("  kururu-wake kavure                Send Magic Packet to Kavure");
    println!("  kururu-wake <MAC_ADDRESS>         Send Magic Packet to arbitrary MAC");
    println!("  kururu-wake --daemon [PORT]       Run HTTP WOL Relay daemon (default: 9096)");
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
        "psicopompo" => {
            if let Err(e) = send_magic_packet(PSICOPOMPO_MAC, "psicopompo") {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        "kavure" => {
            if let Err(e) = send_magic_packet(KAVURE_MAC, "kavure") {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        custom => {
            if let Some(mac) = parse_mac(custom) {
                if let Err(e) = send_magic_packet(mac, custom) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            } else {
                eprintln!("Error: Unknown target or invalid MAC address '{}'", custom);
                eprintln!("Run 'kururu-wake --help' for usage.");
                std::process::exit(1);
            }
        }
    }
}
