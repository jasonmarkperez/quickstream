//! SSDP UPnP discovery and XML device description parser for DLNA MediaRenderers.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DLNADevice {
    pub friendly_name: String,
    pub location: String,
    pub host: String,
    pub port: u16,
    pub manufacturer: String,
    pub model_name: String,
    pub av_transport_control_url: String,
    pub rendering_control_url: String,
    pub udn: String,
}

impl DLNADevice {
    pub fn display_label(&self) -> String {
        let mut details = Vec::new();
        if !self.manufacturer.is_empty() {
            details.push(self.manufacturer.as_str());
        }
        if !self.model_name.is_empty() && self.model_name != self.friendly_name {
            details.push(self.model_name.as_str());
        }
        let detail_str = if !details.is_empty() {
            format!(" [{}]", details.join(", "))
        } else {
            String::new()
        };
        format!("{}{}", self.friendly_name, detail_str)
    }
}

/// Extract content between XML tag delimiters `<tag>...</tag>`, ignoring namespaces.
fn extract_xml_tag(xml: &str, tag_name: &str) -> Option<String> {
    let open_pat_simple = format!("<{}>", tag_name);
    let close_pat_simple = format!("</{}>", tag_name);

    // Try simple tag match
    if let Some(start) = xml.find(&open_pat_simple) {
        let content_start = start + open_pat_simple.len();
        if let Some(end) = xml[content_start..].find(&close_pat_simple) {
            return Some(xml[content_start..content_start + end].trim().to_string());
        }
    }

    // Try tag with namespace (e.g. <u:tag> or <ns:tag>) or attributes (<tag attr="x">)
    let search_open = "<";
    let mut cursor = 0;
    while let Some(open_idx) = xml[cursor..].find(search_open) {
        let abs_open = cursor + open_idx;
        let tag_close_idx = match xml[abs_open..].find('>') {
            Some(idx) => abs_open + idx,
            None => break,
        };
        let tag_header = &xml[abs_open + 1..tag_close_idx];
        let tag_name_part = tag_header.split_whitespace().next().unwrap_or("");
        let base_name = if let Some(colon) = tag_name_part.find(':') {
            &tag_name_part[colon + 1..]
        } else {
            tag_name_part
        };

        if base_name == tag_name {
            // Found opening tag, find corresponding closing tag
            let content_start = tag_close_idx + 1;
            let close_search = "</";
            let mut search_cursor = content_start;
            while let Some(close_idx) = xml[search_cursor..].find(close_search) {
                let abs_close = search_cursor + close_idx;
                if let Some(end_tag_bracket) = xml[abs_close..].find('>') {
                    let end_header = &xml[abs_close + 2..abs_close + end_tag_bracket];
                    let end_base = if let Some(colon) = end_header.find(':') {
                        &end_header[colon + 1..]
                    } else {
                        end_header
                    };
                    if end_base.trim() == tag_name {
                        return Some(xml[content_start..abs_close].trim().to_string());
                    }
                }
                search_cursor = abs_close + 2;
            }
        }
        cursor = tag_close_idx + 1;
    }

    None
}

/// Robust HTTP GET to download device XML over TcpStream with Content-Length support.
pub fn fetch_xml(url: &str) -> Option<String> {
    let url_trimmed = url.trim();
    let without_proto = url_trimmed.strip_prefix("http://")?;
    let (host_port, path) = if let Some(slash) = without_proto.find('/') {
        (&without_proto[..slash], &without_proto[slash..])
    } else {
        (without_proto, "/")
    };

    let (host, port) = if let Some(colon) = host_port.find(':') {
        let h = &host_port[..colon];
        let p = host_port[colon + 1..].parse::<u16>().ok()?;
        (h, p)
    } else {
        (host_port, 80)
    };

    let addrs = (host, port).to_socket_addrs().ok()?;
    let mut stream = None;
    for addr in addrs {
        if let Ok(s) = TcpStream::connect_timeout(&addr, Duration::from_millis(1500)) {
            let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
            let _ = s.set_write_timeout(Some(Duration::from_millis(1500)));
            stream = Some(s);
            break;
        }
    }
    let mut stream = stream?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Quickstream/0.2.0 UPnP/1.1\r\nConnection: close\r\n\r\n",
        path, host_port
    );

    stream.write_all(request.as_bytes()).ok()?;

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let mut header_end = None;

    while let Ok(n) = stream.read(&mut chunk) {
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = Some(pos);
            break;
        }
    }

    let header_pos = match header_end {
        Some(p) => p,
        None => {
            if let Some(p) = buffer.windows(2).position(|w| w == b"\n\n") {
                p
            } else {
                return if !buffer.is_empty() {
                    Some(String::from_utf8_lossy(&buffer).to_string())
                } else {
                    None
                };
            }
        }
    };

    let header_str = String::from_utf8_lossy(&buffer[..header_pos]);
    let delimiter_len = if buffer[header_pos..].starts_with(b"\r\n\r\n") { 4 } else { 2 };
    let body_start = header_pos + delimiter_len;

    // Check Content-Length header
    let content_length: Option<usize> = header_str.lines().find_map(|line| {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v.trim().parse::<usize>().ok();
            }
        }
        None
    });

    if let Some(cl) = content_length {
        let needed = cl.saturating_sub(buffer.len() - body_start);
        let mut read_remaining = 0;
        while read_remaining < needed {
            let to_read = (needed - read_remaining).min(chunk.len());
            match stream.read(&mut chunk[..to_read]) {
                Ok(0) => break,
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    read_remaining += n;
                }
                Err(_) => break,
            }
        }
    } else {
        while let Ok(n) = stream.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..n]);
        }
    }

    if buffer.len() > body_start {
        Some(String::from_utf8_lossy(&buffer[body_start..]).to_string())
    } else {
        None
    }
}

/// Parse UPnP Device Description XML and extract device information and service URLs.
pub fn parse_device_description(xml_data: &str, location_url: &str) -> Option<DLNADevice> {
    // Parse host and port from location URL
    let url_trimmed = location_url.trim();
    let without_proto = url_trimmed.strip_prefix("http://")?;
    let host_port = if let Some(slash) = without_proto.find('/') {
        &without_proto[..slash]
    } else {
        without_proto
    };

    let (host, port) = if let Some(colon) = host_port.find(':') {
        let h = colon;
        (&host_port[..h], host_port[h + 1..].parse::<u16>().unwrap_or(80))
    } else {
        (host_port, 80)
    };

    let friendly_name = extract_xml_tag(xml_data, "friendlyName")
        .unwrap_or_else(|| format!("MediaRenderer ({})", host));
    let manufacturer = extract_xml_tag(xml_data, "manufacturer").unwrap_or_default();
    let model_name = extract_xml_tag(xml_data, "modelName").unwrap_or_default();
    let udn = extract_xml_tag(xml_data, "UDN").unwrap_or_default();

    // Look for service blocks: AVTransport & RenderingControl
    let mut av_transport_control = String::new();
    let mut rendering_control = String::new();

    let mut cursor = 0;
    while let Some(service_start) = xml_data[cursor..].find("<service>") {
        let abs_start = cursor + service_start;
        let abs_end = match xml_data[abs_start..].find("</service>") {
            Some(idx) => abs_start + idx + 10,
            None => break,
        };
        let service_block = &xml_data[abs_start..abs_end];

        let service_type = extract_xml_tag(service_block, "serviceType").unwrap_or_default();
        let control_url_rel = extract_xml_tag(service_block, "controlURL").unwrap_or_default();

        let full_control_url = if control_url_rel.starts_with("http://") {
            control_url_rel
        } else if control_url_rel.starts_with('/') {
            format!("http://{}{}", host_port, control_url_rel)
        } else {
            format!("http://{}/{}", host_port, control_url_rel)
        };

        if service_type.contains(":AVTransport:") {
            av_transport_control = full_control_url;
        } else if service_type.contains(":RenderingControl:") {
            rendering_control = full_control_url;
        }

        cursor = abs_end;
    }

    // A valid DLNA MediaRenderer must provide an AVTransport service
    if av_transport_control.is_empty() {
        return None;
    }

    Some(DLNADevice {
        friendly_name,
        location: location_url.to_string(),
        host: host.to_string(),
        port,
        manufacturer,
        model_name,
        av_transport_control_url: av_transport_control,
        rendering_control_url: rendering_control,
        udn,
    })
}

/// Discover DLNA MediaRenderers on the local network via SSDP UDP multicast (239.255.255.250:1900).
pub fn discover_devices(timeout: Duration) -> Vec<DLNADevice> {
    let multicast_addr = "239.255.255.250:1900";
    let search_targets = [
        "urn:schemas-upnp-org:device:MediaRenderer:1",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "ssdp:all",
    ];

    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let _ = socket.set_read_timeout(Some(Duration::from_millis(400)));
    let _ = socket.set_multicast_ttl_v4(2);

    for target in &search_targets {
        let msg = format!(
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: {}\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 2\r\n\
             ST: {}\r\n\
             \r\n",
            multicast_addr, target
        );
        let _ = socket.send_to(msg.as_bytes(), multicast_addr);
    }

    let start = Instant::now();
    let mut discovered_locations = HashSet::new();
    let mut devices = Vec::new();
    let mut buf = [0u8; 4096];

    while start.elapsed() < timeout {
        if let Ok((len, _)) = socket.recv_from(&mut buf) {
            let resp = String::from_utf8_lossy(&buf[..len]);
            for line in resp.lines() {
                if let Some((header_name, header_val)) = line.split_once(':') {
                    if header_name.trim().eq_ignore_ascii_case("location") {
                        let url = header_val.trim().to_string();
                        if !discovered_locations.contains(&url) {
                            discovered_locations.insert(url.clone());
                            if let Some(xml) = fetch_xml(&url) {
                                if let Some(dev) = parse_device_description(&xml, &url) {
                                    devices.push(dev);
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    devices
}

/// Asynchronous background device discovery manager.
pub struct DeviceManager {
    pub devices: Arc<Mutex<Vec<DLNADevice>>>,
    pub is_scanning: Arc<Mutex<bool>>,
    pub active_device: Arc<Mutex<Option<DLNADevice>>>,
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceManager {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(Mutex::new(Vec::new())),
            is_scanning: Arc::new(Mutex::new(false)),
            active_device: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start_background_scan(&self) {
        let devices_clone = Arc::clone(&self.devices);
        let scanning_clone = Arc::clone(&self.is_scanning);

        thread::spawn(move || {
            {
                let mut scanning = scanning_clone.lock().unwrap();
                if *scanning {
                    return;
                }
                *scanning = true;
            }

            let found = discover_devices(Duration::from_millis(2500));

            {
                let mut dev_list = devices_clone.lock().unwrap();
                for dev in found {
                    if !dev_list.iter().any(|d| d.location == dev.location) {
                        dev_list.push(dev);
                    }
                }
            }

            {
                let mut scanning = scanning_clone.lock().unwrap();
                *scanning = false;
            }
        });
    }

    pub fn get_devices(&self) -> Vec<DLNADevice> {
        self.devices.lock().unwrap().clone()
    }

    pub fn is_scanning(&self) -> bool {
        *self.is_scanning.lock().unwrap()
    }

    pub fn set_active_device(&self, dev: Option<DLNADevice>) {
        *self.active_device.lock().unwrap() = dev;
    }

    pub fn get_active_device(&self) -> Option<DLNADevice> {
        self.active_device.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_device_description() {
        let sample_xml = r#"<?xml version="1.0"?>
        <root xmlns="urn:schemas-upnp-org:device-1-0">
          <device>
            <friendlyName>Living Room Stereo</friendlyName>
            <manufacturer>Primare</manufacturer>
            <modelName>I15 Prisma</modelName>
            <UDN>uuid:12345-6789</UDN>
            <serviceList>
              <service>
                <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
                <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
                <controlURL>/AVTransport/control</controlURL>
              </service>
              <service>
                <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
                <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
                <controlURL>/RenderingControl/control</controlURL>
              </service>
            </serviceList>
          </device>
        </root>"#;

        let dev = parse_device_description(sample_xml, "http://192.168.1.50:8000/desc.xml").unwrap();
        assert_eq!(dev.friendly_name, "Living Room Stereo");
        assert_eq!(dev.manufacturer, "Primare");
        assert_eq!(dev.model_name, "I15 Prisma");
        assert_eq!(dev.av_transport_control_url, "http://192.168.1.50:8000/AVTransport/control");
        assert_eq!(dev.rendering_control_url, "http://192.168.1.50:8000/RenderingControl/control");
        assert_eq!(dev.display_label(), "Living Room Stereo [Primare, I15 Prisma]");
    }
}
