mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quickstream::discovery::{discover_devices, fetch_device_descriptions, fetch_xml, ssdp_location_for_sender};

#[test]
#[ignore = "real SSDP scan on the local network (macOS firewall prompt); run with --include-ignored"]
fn test_discover_devices_real() {
    common::isolate();
    let devices = discover_devices(Duration::from_millis(3000));
    println!("Found {} devices:", devices.len());
    for d in &devices {
        println!(" - {}", d.display_label());
    }
}

type Responder = Arc<dyn Fn(&mut TcpStream) + Send + Sync>;

/// Minimal HTTP server on 127.0.0.1 that answers every GET with `respond`,
/// one thread per connection. Stops when dropped.
struct DescServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DescServer {
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/desc.xml", self.port)
    }
}

impl Drop for DescServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn spawn_desc_server(respond: Responder) -> DescServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let handle = std::thread::spawn(move || {
        while !stop_t.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut s, _)) => {
                    let respond = respond.clone();
                    std::thread::spawn(move || {
                        let _ = s.set_nonblocking(false);
                        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut reader = BufReader::new(s.try_clone().unwrap());
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                                break;
                            }
                        }
                        respond(&mut s);
                        let _ = s.flush();
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    DescServer {
        port,
        stop,
        handle: Some(handle),
    }
}

fn renderer_xml(name: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><device><friendlyName>{}</friendlyName><UDN>uuid:{}</UDN><serviceList><service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>/AVTransport/control</controlURL></service></serviceList></device></root>"#,
        name, name
    )
}

fn write_xml(s: &mut TcpStream, xml: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        xml.len(),
        xml
    );
    let _ = s.write_all(resp.as_bytes());
}

/// A device that trickles its description must not hold discovery past a fixed deadline.
#[test]
fn test_fetch_xml_has_overall_deadline() {
    common::isolate();
    let server = spawn_desc_server(Arc::new(|s: &mut TcpStream| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\n\r\n<root>");
        for _ in 0..60 {
            if s.write_all(b" ").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }));
    let start = Instant::now();
    let res = fetch_xml(&server.url());
    let elapsed = start.elapsed();
    assert!(res.is_none(), "an incomplete description must not be returned");
    assert!(elapsed < Duration::from_secs(5), "fetch_xml took {:?}", elapsed);
}

/// Description bodies are capped (~256 KB) so a responder cannot exhaust memory.
#[test]
fn test_fetch_xml_rejects_oversized_body() {
    common::isolate();
    let server = spawn_desc_server(Arc::new(|s: &mut TcpStream| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nConnection: close\r\n\r\n<root>");
        let filler = vec![b' '; 64 * 1024];
        for _ in 0..32 {
            if s.write_all(&filler).is_err() {
                return;
            }
        }
        let _ = s.write_all(b"</root>");
    }));
    assert!(
        fetch_xml(&server.url()).is_none(),
        "a 2 MB description must be rejected"
    );
}

/// Chunked descriptions are decoded.
#[test]
fn test_fetch_xml_decodes_chunked_body() {
    common::isolate();
    let xml = renderer_xml("Chunky");
    let xml_t = xml.clone();
    let server = spawn_desc_server(Arc::new(move |s: &mut TcpStream| {
        let (a, b) = xml_t.split_at(xml_t.find("Chunky").unwrap() + 2);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            a.len(),
            a,
            b.len(),
            b
        );
        let _ = s.write_all(resp.as_bytes());
    }));
    assert_eq!(fetch_xml(&server.url()).as_deref(), Some(xml.as_str()));
}

#[test]
fn test_ssdp_location_must_match_sender_ip() {
    common::isolate();
    let resp = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: http://192.168.1.50:49152/desc.xml\r\nST: upnp:rootdevice\r\n\r\n";
    let sender: IpAddr = "192.168.1.50".parse().unwrap();
    assert_eq!(
        ssdp_location_for_sender(resp, sender).as_deref(),
        Some("http://192.168.1.50:49152/desc.xml")
    );
    // IPv4-mapped IPv6 sender address is the same host.
    let mapped: IpAddr = "::ffff:192.168.1.50".parse().unwrap();
    assert!(ssdp_location_for_sender(resp, mapped).is_some());

    // Another host's address, a hostname, a non-HTTP scheme, or no LOCATION at all.
    let other: IpAddr = "192.168.1.51".parse().unwrap();
    assert!(ssdp_location_for_sender(resp, other).is_none());
    let named = "HTTP/1.1 200 OK\r\nlocation: http://renderer.local:49152/desc.xml\r\n\r\n";
    assert!(ssdp_location_for_sender(named, sender).is_none());
    let https = "HTTP/1.1 200 OK\r\nLocation: https://192.168.1.50/desc.xml\r\n\r\n";
    assert!(ssdp_location_for_sender(https, sender).is_none());
    assert!(ssdp_location_for_sender("HTTP/1.1 200 OK\r\nST: x\r\n\r\n", sender).is_none());
    // A responder pointing at loopback / some other internal service is rejected.
    let pivot = "HTTP/1.1 200 OK\r\nLOCATION: http://127.0.0.1:8080/admin\r\n\r\n";
    assert!(ssdp_location_for_sender(pivot, sender).is_none());
}

fn delayed_renderer(name: &'static str, delay: Duration) -> DescServer {
    spawn_desc_server(Arc::new(move |s: &mut TcpStream| {
        std::thread::sleep(delay);
        write_xml(s, &renderer_xml(name));
    }))
}

/// Descriptions are fetched concurrently, and results keep submission order.
#[test]
fn test_fetch_device_descriptions_runs_in_parallel() {
    common::isolate();
    let names = ["Alpha", "Bravo", "Charlie", "Delta"];
    let servers: Vec<DescServer> = names
        .iter()
        .map(|n| delayed_renderer(n, Duration::from_millis(700)))
        .collect();
    let urls: Vec<String> = servers.iter().map(|s| s.url()).collect();

    let start = Instant::now();
    let devices = fetch_device_descriptions(&urls, 8, Duration::from_secs(3));
    let elapsed = start.elapsed();

    let found: Vec<&str> = devices.iter().map(|d| d.friendly_name.as_str()).collect();
    assert_eq!(found, names);
    // Sequential fetching would take 4 x 700ms = 2.8s.
    assert!(elapsed < Duration::from_millis(1800), "took {:?}", elapsed);
}

/// Concurrency is bounded.
#[test]
fn test_fetch_device_descriptions_bounds_concurrency() {
    common::isolate();
    let servers: Vec<DescServer> = ["A", "B", "C", "D"]
        .iter()
        .map(|n| delayed_renderer(n, Duration::from_millis(500)))
        .collect();
    let urls: Vec<String> = servers.iter().map(|s| s.url()).collect();
    let start = Instant::now();
    let devices = fetch_device_descriptions(&urls, 2, Duration::from_secs(3));
    let elapsed = start.elapsed();
    assert_eq!(devices.len(), 4);
    assert!(
        elapsed >= Duration::from_millis(1000),
        "2 at a time needs two rounds, took {:?}",
        elapsed
    );
    assert!(elapsed < Duration::from_millis(2500), "took {:?}", elapsed);
}

/// One stalled device does not delay the others beyond its own fetch timeout.
#[test]
fn test_fetch_device_descriptions_slow_device_does_not_block_others() {
    common::isolate();
    let stalled = spawn_desc_server(Arc::new(|s: &mut TcpStream| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\n");
        std::thread::sleep(Duration::from_secs(6));
    }));
    let fast = delayed_renderer("Fast", Duration::from_millis(0));
    let urls = vec![stalled.url(), fast.url()];
    let start = Instant::now();
    let devices = fetch_device_descriptions(&urls, 8, Duration::from_millis(800));
    let elapsed = start.elapsed();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].friendly_name, "Fast");
    assert!(elapsed < Duration::from_millis(1800), "took {:?}", elapsed);
}

#[test]
fn test_fetch_xml_plain_body() {
    common::isolate();
    let xml = renderer_xml("Plain");
    let xml_t = xml.clone();
    let server = spawn_desc_server(Arc::new(move |s: &mut TcpStream| write_xml(s, &xml_t)));
    assert_eq!(fetch_xml(&server.url()).as_deref(), Some(xml.as_str()));
}
