use bitreader::BitReader;
use btleplug::api::{BDAddr, Central, CharPropFlags, Manager as _, Peripheral, ScanFilter};
use btleplug::platform::Manager;
use core::{f64, fmt};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use dotenv::dotenv;
use futures::stream::{self, StreamExt};
use influxdb2::Client;
use std::error::Error;
use uuid::Uuid;
use serde::Deserialize;
use serde_json;
use std::fs;
use tokio::time::{sleep, sleep_until};

/// UUID of the characteristic for which we should subscribe to notifications.
const NOTIFY_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x6e400003_b5a3_f393_e0a9_e50e24dcca9e);

#[derive(Debug)]
struct RuuviData {
    name: String,
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
    fn new(name: String, mac_address: BDAddr, raw_data: Vec<u8>) -> Result<RuuviData, Box<dyn Error>> {
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

        Ok(RuuviData {
            name,
            mac_address,
            temperature,
            humidity,
            pressure,
            acceleration_x,
            acceleration_y,
            acceleration_z,
            voltage,
            tx_power,
            movement_counter,
            measurement_sequence,
        })
    }
}
impl fmt::Display for RuuviData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Mac: {}, temp: {}, humidity: {}, measurement sequence: {}",
            self.mac_address, self.temperature, self.humidity, self.measurement_sequence
        )
    }
}

#[derive(Deserialize)]
struct Config {
    influx_bucket: String,
    influx_measurement: String,
    influx_host: String,
    influx_org: String,
    influx_token: String,
    tags: HashMap<String, String>,
    delay_in_secs: u32,
}

struct RustSniffer {
    influx_client: Client,
    influx_bucket: String,
    influx_measurement: String,
    tag_names: HashMap<String, BDAddr>,
    ruuvis: HashMap<String, btleplug::platform::Peripheral>,
    delay: u32,
}

impl RustSniffer {
    async fn new(config: Config) -> RustSniffer {
        let mut tag_names: HashMap<String, BDAddr> = HashMap::new();

        for (name, mac) in config.tags {
            tag_names.insert(name, BDAddr::from_str_delim(&mac).unwrap());
        };


        RustSniffer {
            influx_client: Client::new(config.influx_host, config.influx_org, config.influx_token),
            influx_bucket: config.influx_bucket,
            influx_measurement: config.influx_measurement,
            ruuvis: HashMap::new(),
            tag_names,
            delay: config.delay_in_secs,
        }
    }

    async fn discover(&mut self) -> Result<(), Box<dyn Error>>{
        let manager = Manager::new().await?;
        let adapter_list = manager.adapters().await?;
        if adapter_list.is_empty() {
            eprintln!("No Bluetooth adapters found");
            return Ok(());
        }

        for adapter in adapter_list.iter() {
            println!("Starting scan...");
            adapter
                .start_scan(ScanFilter::default())
                .await
                .expect("Can't scan BLE adapter for connected devices...");

            loop {
                if self.tag_names.len() == self.ruuvis.len() {
                    break;
                }
                sleep(Duration::from_secs(1)).await;
                let peripherals = adapter_list[0].peripherals().await?;

                for (name, mac) in &self.tag_names {
                    for peripheral in &peripherals {
                        if peripheral.address() == *mac {
                            if !self.ruuvis
                                .iter()
                                .any(|ruuvi| ruuvi.1.address() == peripheral.address())
                            {
                                println!("found {}", mac);
                                self.ruuvis.insert(name.to_string(), peripheral.clone());
                            }
                        }
                    }
                }
            }
        }
        println!("Connecting and subscribing..");
        for (_name, ruuvi) in self.ruuvis.iter() {
            ruuvi.connect().await?;
            ruuvi.discover_services().await?;
            for characteristic in ruuvi.characteristics() {
                if characteristic.uuid == NOTIFY_CHARACTERISTIC_UUID
                    && characteristic.properties.contains(CharPropFlags::NOTIFY)
                {
                    ruuvi.subscribe(&characteristic).await?;
                    break;
                }
            }
            println!(
                "Ruuvi {} connected: {}",
                ruuvi.address(),
                ruuvi.is_connected().await?
            );
        }
        Ok(())
    }

    async fn start(&mut self) -> Result<(), Box<dyn Error>> {
        println!("Starting, trying to connect to all tags...");
        loop {
            match self.discover().await {
                Ok(_) => break,
                Err(_) => {
                    println!("Discovery failed, retrying...");
                    sleep(Duration::from_secs(1)).await;
                },
            }
        }
        println!("\nStarting polling notifications..");
        loop {
            let continue_time = Instant::now() + Duration::from_secs(self.delay.into());
            self.update_data().await?;
            sleep_until(continue_time.into()).await;
        }
    }

    async fn update_data(
        &self
    ) -> Result<(), Box<dyn Error>> {
        println!("Reading data...");
        let mut ruuvi_datas: Vec<RuuviData> = Vec::new();
        for (name, ruuvi) in self.ruuvis.iter() {
            if let Ok(mut notification) = ruuvi.notifications().await {
                if let Some(data) = notification.next().await {
                    ruuvi_datas.push(RuuviData::new(name.to_string(), ruuvi.address(), data.value.clone()).unwrap());
                }
            }
        }
        self.send_data(ruuvi_datas).await?;
        Ok(())
    }

    async fn send_data(&self, ruuvi_datas: Vec<RuuviData>) -> Result<(), Box<dyn Error>> {
        use influxdb2::models::DataPoint;

        let mut points: Vec<DataPoint> = Vec::new();
        for data in ruuvi_datas {
            points.push(
                DataPoint::builder(self.influx_measurement.clone())
                    .tag("name", data.name)
                    .tag("mac", data.mac_address.to_string())
                    .field("temperature", data.temperature as f64)
                    .field("humidity", data.humidity as f64)
                    .field("pressure", data.pressure as i64)
                    .field("acceleration_x", data.acceleration_x as f64)
                    .field("acceleration_y", data.acceleration_y as f64)
                    .field("acceleration_z", data.acceleration_z as f64)
                    .field("voltage", data.voltage as f64)
                    .field("tx_power", data.tx_power as i64)
                    .field("movement_counter", data.movement_counter as i64)
                    .field("measurement_sequence", data.measurement_sequence as i64)
                    .build()?,
            );
        }
        println!("Sending data ({} points)...", points.len());
        self.influx_client
            .write(&self.influx_bucket, stream::iter(points))
            .await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();
    dotenv().ok();
    
    let config: Config = serde_json::from_str(&fs::read_to_string("config.json").unwrap()).unwrap(); 

    let mut rs = RustSniffer::new(config).await;
    rs.start().await
}
