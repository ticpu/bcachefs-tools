use anyhow::{Context, Result};
use clap::{Arg, Command};
use prometheus::{GaugeVec, Registry};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

struct BcacheFSCollector {
    base_path: String,
    metrics: HashMap<String, GaugeVec>,
    btree_metrics: HashMap<String, GaugeVec>,
}

struct DiskStats {
    read_ios: u64,
    read_sectors: u64,
    write_ios: u64,
    write_sectors: u64,
}

impl BcacheFSCollector {
    fn new() -> Self {
        BcacheFSCollector {
            base_path: "/sys/fs/bcachefs".to_string(),
            metrics: HashMap::new(),
            btree_metrics: HashMap::new(),
        }
    }

    fn initialize_metrics(&mut self, registry: &Registry) -> Result<()> {
        let bucket_types = vec![
            "free",
            "sb",
            "journal",
            "btree",
            "user",
            "cached",
            "parity",
            "stripe",
            "need_gc_gens",
            "need_discard",
            "unstriped",
            "capacity",
        ];

        for bucket_type in bucket_types {
            let metric = GaugeVec::new(
                prometheus::opts!(
                    format!("{}_buckets", bucket_type),
                    format!("Number of {} buckets", bucket_type)
                ),
                &["uuid", "device"],
            )?;
            registry.register(Box::new(metric.clone()))?;
            self.metrics.insert(bucket_type.to_string(), metric);
        }

        let io_metrics = vec![
            ("io_read_bytes", "Bytes read from device"),
            ("io_write_bytes", "Bytes written to device"),
            ("io_read_iops", "Read IOPS on device"),
            ("io_write_iops", "Write IOPS on device"),
        ];

        for (name, description) in io_metrics {
            let opts = prometheus::opts!(name, description);
            let metric = GaugeVec::new(opts, &["uuid", "device"])?;
            registry.register(Box::new(metric.clone()))?;
            self.metrics.insert(name.to_string(), metric);
        }

        Ok(())
    }

    fn get_filesystems(&self) -> Result<Vec<String>> {
        fs::read_dir(&self.base_path)?
            .filter_map(|entry| {
                entry.ok().and_then(|e| {
                    if e.path().is_dir() {
                        e.file_name().into_string().ok()
                    } else {
                        None
                    }
                })
            })
            .collect::<Vec<String>>()
            .into_iter()
            .map(Ok)
            .collect()
    }

    fn get_device_labels(&self, uuid: &str) -> Result<HashMap<String, String>> {
        let fs_path = Path::new(&self.base_path).join(uuid);
        let mut labels = HashMap::new();

        for entry in fs::read_dir(fs_path)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let dev_dir = file_name
                .to_str()
                .context("Invalid device directory name")?;
            if dev_dir.starts_with("dev-") {
                let label_path = entry.path().join("label");
                let label = fs::read_to_string(label_path)?.trim().to_string();
                labels.insert(dev_dir.to_string(), label);
            }
        }

        Ok(labels)
    }

    fn collect_metrics(&mut self, registry: &Registry) -> Result<()> {
        let filesystems = self.get_filesystems()?;

        for uuid in filesystems {
            let labels = self.get_device_labels(&uuid)?;

            for (dev_dir, label) in &labels {
                self.collect_alloc_debug_metrics(&uuid, dev_dir, label)?;
                self.collect_disk_stats(&uuid, dev_dir, label)?;
            }

            self.collect_btree_metrics(&uuid, registry)?;
        }

        Ok(())
    }

    fn collect_alloc_debug_metrics(&self, uuid: &str, dev_dir: &str, label: &str) -> Result<()> {
        let alloc_debug_path: PathBuf = [&self.base_path, uuid, dev_dir, "alloc_debug"]
            .iter()
            .collect();
        let content = fs::read_to_string(alloc_debug_path)?;

        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Some(metric) = self.metrics.get(parts[0]) {
                    if let Ok(value) = parts[1].parse::<i64>() {
                        metric
                            .get_metric_with_label_values(&[uuid, label])?
                            .set(value as f64);
                    }
                }
            }
        }

        Ok(())
    }

    fn collect_disk_stats(&self, uuid: &str, dev_dir: &str, label: &str) -> Result<()> {
        let stat_path: PathBuf = [&self.base_path, uuid, dev_dir, "block", "stat"]
            .iter()
            .collect();

        if let Ok(disk_stats) = read_disk_stats(&stat_path) {
            let read_bytes = disk_stats.read_sectors * 512;
            let write_bytes = disk_stats.write_sectors * 512;
            self.metrics["io_read_bytes"]
                .get_metric_with_label_values(&[uuid, label])?
                .set(read_bytes as f64);
            self.metrics["io_write_bytes"]
                .get_metric_with_label_values(&[uuid, label])?
                .set(write_bytes as f64);
            self.metrics["io_read_iops"]
                .get_metric_with_label_values(&[uuid, label])?
                .set(disk_stats.read_ios as f64);
            self.metrics["io_write_iops"]
                .get_metric_with_label_values(&[uuid, label])?
                .set(disk_stats.write_ios as f64);
        }
        Ok(())
    }

    fn collect_btree_metrics(&mut self, uuid: &str, registry: &Registry) -> Result<()> {
        let accounting_file: PathBuf = [&self.base_path, uuid, "internal", "accounting"]
            .iter()
            .collect();

        if accounting_file.exists() {
            let content = fs::read_to_string(accounting_file)?;
            let re = Regex::new(r"^btree btree=(\w+): (\d+)$")?;

            for line in content.lines() {
                if let Some(captures) = re.captures(line) {
                    let btree_type = captures.get(1).context("Missing btree type")?.as_str();
                    if btree_type != "(unknown)" {
                        let metric_name = format!("accounting_btree_{}", btree_type);
                        let metric_entry = self.btree_metrics.entry(metric_name.clone());
                        let metric = metric_entry.or_insert_with(|| {
                            let gauge_vec = GaugeVec::new(
                                prometheus::opts!(
                                    metric_name.clone(),
                                    format!("Btree size for {}", btree_type)
                                ),
                                &["uuid"],
                            )
                            .unwrap();
                            registry.register(Box::new(gauge_vec.clone())).unwrap();
                            gauge_vec
                        });
                        let value: f64 = captures
                            .get(2)
                            .context("Missing btree value")?
                            .as_str()
                            .parse()?;
                        metric.get_metric_with_label_values(&[uuid])?.set(value);
                    }
                }
            }
        }
        Ok(())
    }
}

fn read_disk_stats(stat_path: &Path) -> Result<DiskStats> {
    let content = fs::read_to_string(stat_path)
        .with_context(|| format!("Failed to read_disk_stats from {:?}", stat_path))?;
    let parts: Vec<u64> = content
        .split_whitespace()
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();

    if parts.len() < 11 {
        anyhow::bail!("Unexpected format in sysfs stat file");
    }

    Ok(DiskStats {
        read_ios: parts[0],
        read_sectors: parts[2],
        write_ios: parts[4],
        write_sectors: parts[6],
    })
}

fn start_exporter(address: SocketAddr) -> Result<()> {
    let mut collector = BcacheFSCollector::new();
    let registry = Registry::new_custom(Some("bcachefs".to_string()), None)
        .expect("failed to create registry instance");
    collector.initialize_metrics(&registry)?;
    let mut builder = prometheus_exporter::Builder::new(address);
    builder.with_registry(registry.clone());
    let exporter = builder.start().expect("failed to start exporter");
    loop {
        let guard = exporter.wait_request();
        collector.collect_metrics(&registry)?;
        drop(guard);
    }
}

fn main() -> Result<()> {
    let matches = Command::new("BcacheFS Prometheus Exporter")
        .version("1.0")
        .about("Exports BcacheFS metrics to Prometheus")
        .arg(
            Arg::new("address")
                .short('a')
                .long("address")
                .value_name("ADDRESS")
                .default_value("127.0.0.1:8001")
                .help("Address to bind to, e.g., [::1]:8001"),
        )
        .get_matches();

    let binding: SocketAddr = matches.get_one::<String>("address").unwrap().parse()?;

    if let Err(e) = start_exporter(binding) {
        eprintln!("Error starting exporter: {}.", e);
    }

    Ok(())
}
