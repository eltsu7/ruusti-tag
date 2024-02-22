// See the "macOS permissions note" in README.md before running this on macOS
// Big Sur or later.

use btleplug::api::{BDAddr, Central, CharPropFlags, Manager as _, Peripheral, ScanFilter};
use btleplug::platform::Manager;
use futures::stream::StreamExt;
use core::fmt;
use std::error::Error;
use std::time::Duration;
use tokio::time;
use uuid::Uuid;
use bitreader::BitReader;

/// Only devices whose name contains this string will be tried.
const PERIPHERAL_NAME_MATCH_FILTER: &str = "Ruuvi";
/// UUID of the characteristic for which we should subscribe to notifications.
const NOTIFY_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x6e400003_b5a3_f393_e0a9_e50e24dcca9e);

#[derive(Debug)]
struct RuuviData {
    mac_address: BDAddr,
    temperature: f32,
    humidity: f32,
    pressure: u32,
    acceleration_x: f32,
    acceleration_y: f32,
    acceleration_z: f32,
    voltage: f32,
    tx_power: u16,
    movement_counter: u8,
    measurement_sequence: u16,
}


impl RuuviData {
    fn new(mac_address: BDAddr, raw_data: Vec<u8>) -> Result<RuuviData, Box<dyn Error>> {
        let mut reader = BitReader::new(&raw_data);
        reader.skip(8).unwrap();


        let temperature = reader.read_u16(16)? as f32 * 0.005;
        let humidity = reader.read_u16(16)? as f32 * 0.0025;
        let pressure = reader.read_u32(16)? + 50_000;
        let acceleration_x = reader.read_i16(16)? as f32 / 1000.0;
        let acceleration_y = reader.read_i16(16)? as f32 / 1000.0;
        let acceleration_z = reader.read_i16(16)? as f32 / 1000.0;
        let voltage = reader.read_u16(11)? as f32 * 0.001 + 1.6;
        let tx_power = reader.read_u16(5)? * 2 - 40;
        let movement_counter = reader.read_u8(8)?;
        let measurement_sequence = reader.read_u16(16)?;

        let rd = RuuviData {mac_address, temperature, humidity, pressure, acceleration_x, acceleration_y, acceleration_z, voltage, tx_power, movement_counter, measurement_sequence};

        Ok(rd)
    }
}

impl fmt::Display for RuuviData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Mac: {}, temp: {}, humidity: {}, measurement sequence: {}", self.mac_address, self.temperature, self.humidity, self.measurement_sequence)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();

    let manager = Manager::new().await?;
    let adapter_list = manager.adapters().await?;
    if adapter_list.is_empty() {
        eprintln!("No Bluetooth adapters found");
        return Ok(());
    }
    let ruuvis: Vec<btleplug::platform::Peripheral> = Vec::new();

    let wanted_macs = [
        BDAddr::from_str_delim("F2:2D:EB:37:8A:D4").unwrap(),
        BDAddr::from_str_delim("D3:1A:DA:17:E5:C6").unwrap(),
    ];
    

    for adapter in adapter_list.iter() {
        println!("Starting scan...");
        adapter
            .start_scan(ScanFilter::default())
            .await
            .expect("Can't scan BLE adapter for connected devices...");

        'outer: loop {
            time::sleep(Duration::from_secs(1)).await;
            let peripherals = adapter_list[0].peripherals().await?;

            for mac in wanted_macs {
                let mut found = false;
                for peripheral in &peripherals {
                    if peripheral.address() == mac {
                        found = true;
                        break;
                    }
                }
                if !found {
                    println!("{} not found yet...", mac);
                    continue 'outer;       
                }
            }
            break;
        }

        let peripherals = adapter.peripherals().await?;
        if peripherals.is_empty() {
            eprintln!("->>> BLE peripheral devices were not found, sorry. Exiting...");
            return Ok(());
        }
        // All peripheral devices in range.
        for peripheral in peripherals.iter() {
            let properties = peripheral.properties().await?;
            let is_connected = peripheral.is_connected().await?;
            let local_name = properties
                .unwrap()
                .local_name
                .unwrap_or(String::from("(peripheral name unknown)"));
            println!(
                "Peripheral {:?} is connected: {:?}",
                &local_name, is_connected
            );
            // Check if it's the peripheral we want.
            if local_name.contains(PERIPHERAL_NAME_MATCH_FILTER) {
                println!("Found matching peripheral {:?}...", &local_name);
                if !is_connected {
                    // Connect if we aren't already connected.
                    if let Err(err) = peripheral.connect().await {
                        eprintln!("Error connecting to peripheral, skipping: {}", err);
                        continue;
                    }
                }
                println!(
                    "Now connected ({:?}) to peripheral {:?}.",
                    is_connected, &local_name
                );
                if is_connected {
                    peripheral.discover_services().await?;
                    for characteristic in peripheral.characteristics() {
                        // Subscribe to notifications from the characteristic with the selected
                        // UUID.
                        if characteristic.uuid == NOTIFY_CHARACTERISTIC_UUID
                        && characteristic.properties.contains(CharPropFlags::NOTIFY)
                        {
                            println!("Subscribing to characteristic {:?}", characteristic.uuid);
                            peripheral.subscribe(&characteristic).await?;
                            // Print the first 4 notifications received.
                            let mut notification_stream = peripheral.notifications().await?.take(4);
                            // Process while the BLE connection is not broken or stopped.
                            while let Some(data) = notification_stream.next().await {
                                let rd = RuuviData::new(peripheral.address(), data.value.clone()).unwrap();
                                println!("{}", rd);
                            }
                        } 
                    }
                    println!("Disconnecting from peripheral {:?}...", local_name);
                    peripheral.disconnect().await?;
                }
            }  
        }
    }
    Ok(())
}
