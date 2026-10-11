use std::time::Duration;
use quickstream::discovery::discover_devices;

#[test]
fn test_discover_devices_real() {
    let devices = discover_devices(Duration::from_millis(3000));
    println!("Found {} devices:", devices.len());
    for d in &devices {
        println!(" - {}", d.display_label());
    }
}
