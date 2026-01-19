//! Performance monitoring and metrics collection
//!
//! Tracks sync performance metrics including:
//! - Transfer speeds and bandwidth utilization
//! - File processing rates
//! - Time breakdown by operation
//! - Resource usage (memory, CPU)

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Performance metrics for a sync operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    /// Total duration of the sync operation
    pub total_duration: Duration,

    /// Time spent scanning source and destination
    pub scan_duration: Duration,

    /// Time spent planning the sync (diffing)
    pub plan_duration: Duration,

    /// Time spent transferring files
    pub transfer_duration: Duration,

    /// Total bytes transferred (written to destination)
    pub bytes_transferred: u64,

    /// Total bytes read from source
    pub bytes_read: u64,

    /// Number of files processed
    pub files_processed: u64,

    /// Number of files created
    pub files_created: u64,

    /// Number of files updated
    pub files_updated: u64,

    /// Number of files deleted
    pub files_deleted: u64,

    /// Number of directories created
    pub directories_created: u64,

    /// Average transfer speed (bytes/sec)
    pub avg_transfer_speed: f64,

    /// Peak transfer speed (bytes/sec)
    pub peak_transfer_speed: f64,

    /// Files per second
    pub files_per_second: f64,

    /// Bandwidth utilization percentage (if rate limit set)
    pub bandwidth_utilization: Option<f64>,
}

impl PerformanceMetrics {
    /// Format bytes/sec as human-readable speed
    pub fn format_speed(bytes_per_sec: f64) -> String {
        if bytes_per_sec >= 1_000_000_000.0 {
            format!("{:.2} GB/s", bytes_per_sec / 1_000_000_000.0)
        } else if bytes_per_sec >= 1_000_000.0 {
            format!("{:.2} MB/s", bytes_per_sec / 1_000_000.0)
        } else if bytes_per_sec >= 1_000.0 {
            format!("{:.2} KB/s", bytes_per_sec / 1_000.0)
        } else {
            format!("{:.0} B/s", bytes_per_sec)
        }
    }

    /// Format duration as human-readable string
    pub fn format_duration(duration: Duration) -> String {
        let total_secs = duration.as_secs();
        if total_secs >= 3600 {
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            format!("{}h {}m {}s", hours, mins, secs)
        } else if total_secs >= 60 {
            let mins = total_secs / 60;
            let secs = total_secs % 60;
            format!("{}m {}s", mins, secs)
        } else {
            format!("{:.2}s", duration.as_secs_f64())
        }
    }

    /// Print performance summary to stdout
    pub fn print_summary(&self) {
        use colored::Colorize;

        println!("\n{}", "Performance Summary:".bold());
        println!(
            "  Total time:      {}",
            Self::format_duration(self.total_duration).cyan()
        );
        println!(
            "    Scanning:      {} ({:.1}%)",
            Self::format_duration(self.scan_duration).cyan(),
            (self.scan_duration.as_secs_f64() / self.total_duration.as_secs_f64()) * 100.0
        );
        println!(
            "    Planning:      {} ({:.1}%)",
            Self::format_duration(self.plan_duration).cyan(),
            (self.plan_duration.as_secs_f64() / self.total_duration.as_secs_f64()) * 100.0
        );
        println!(
            "    Transferring:  {} ({:.1}%)",
            Self::format_duration(self.transfer_duration).cyan(),
            (self.transfer_duration.as_secs_f64() / self.total_duration.as_secs_f64()) * 100.0
        );

        println!(
            "\n  Files:           {} processed",
            self.files_processed.to_string().green()
        );
        if self.files_created > 0 {
            println!(
                "    Created:       {}",
                self.files_created.to_string().green()
            );
        }
        if self.files_updated > 0 {
            println!(
                "    Updated:       {}",
                self.files_updated.to_string().yellow()
            );
        }
        if self.files_deleted > 0 {
            println!(
                "    Deleted:       {}",
                self.files_deleted.to_string().red()
            );
        }
        if self.directories_created > 0 {
            println!(
                "    Dirs created:  {}",
                self.directories_created.to_string().green()
            );
        }

        println!(
            "\n  Data:            {} transferred, {} read",
            Self::format_size(self.bytes_transferred).cyan(),
            Self::format_size(self.bytes_read).cyan()
        );
        println!(
            "  Speed:           {} avg, {} peak",
            Self::format_speed(self.avg_transfer_speed).cyan(),
            Self::format_speed(self.peak_transfer_speed).cyan()
        );
        println!(
            "  Rate:            {:.1} files/sec",
            self.files_per_second.to_string().cyan()
        );

        if let Some(utilization) = self.bandwidth_utilization {
            println!(
                "  Bandwidth:       {:.1}% utilized",
                utilization.to_string().cyan()
            );
        }
    }

    /// Format bytes as human-readable size
    fn format_size(bytes: u64) -> String {
        crate::resource::format_bytes(bytes)
    }
}

/// Performance monitor for tracking sync operations
#[derive(Clone)]
pub struct PerformanceMonitor {
    start_time: Instant,
    scan_start: Option<Instant>,
    scan_duration: Arc<AtomicU64>,
    plan_start: Option<Instant>,
    plan_duration: Arc<AtomicU64>,
    transfer_start: Option<Instant>,
    transfer_duration: Arc<AtomicU64>,
    bytes_transferred: Arc<AtomicU64>,
    bytes_read: Arc<AtomicU64>,
    files_processed: Arc<AtomicU64>,
    files_created: Arc<AtomicU64>,
    files_updated: Arc<AtomicU64>,
    files_deleted: Arc<AtomicU64>,
    directories_created: Arc<AtomicU64>,
    peak_speed: Arc<AtomicU64>,
    rate_limit: Option<u64>,
}

impl PerformanceMonitor {
    /// Create a new performance monitor
    pub fn new(rate_limit: Option<u64>) -> Self {
        Self {
            start_time: Instant::now(),
            scan_start: None,
            scan_duration: Arc::new(AtomicU64::new(0)),
            plan_start: None,
            plan_duration: Arc::new(AtomicU64::new(0)),
            transfer_start: None,
            transfer_duration: Arc::new(AtomicU64::new(0)),
            bytes_transferred: Arc::new(AtomicU64::new(0)),
            bytes_read: Arc::new(AtomicU64::new(0)),
            files_processed: Arc::new(AtomicU64::new(0)),
            files_created: Arc::new(AtomicU64::new(0)),
            files_updated: Arc::new(AtomicU64::new(0)),
            files_deleted: Arc::new(AtomicU64::new(0)),
            directories_created: Arc::new(AtomicU64::new(0)),
            peak_speed: Arc::new(AtomicU64::new(0)),
            rate_limit,
        }
    }

    /// Start timing the scan phase
    pub fn start_scan(&mut self) {
        self.scan_start = Some(Instant::now());
    }

    /// End timing the scan phase
    pub fn end_scan(&mut self) {
        if let Some(start) = self.scan_start.take() {
            let duration = start.elapsed();
            self.scan_duration
                .store(duration.as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Start timing the planning phase
    pub fn start_plan(&mut self) {
        self.plan_start = Some(Instant::now());
    }

    /// End timing the planning phase
    pub fn end_plan(&mut self) {
        if let Some(start) = self.plan_start.take() {
            let duration = start.elapsed();
            self.plan_duration
                .store(duration.as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Start timing the transfer phase
    pub fn start_transfer(&mut self) {
        self.transfer_start = Some(Instant::now());
    }

    /// End timing the transfer phase
    pub fn end_transfer(&mut self) {
        if let Some(start) = self.transfer_start.take() {
            let duration = start.elapsed();
            self.transfer_duration
                .store(duration.as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Record bytes transferred
    pub fn add_bytes_transferred(&self, bytes: u64) {
        self.bytes_transferred.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record bytes read
    pub fn add_bytes_read(&self, bytes: u64) {
        self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record file processed
    pub fn add_file_processed(&self) {
        self.files_processed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record file created
    pub fn add_file_created(&self) {
        self.files_created.fetch_add(1, Ordering::Relaxed);
        self.add_file_processed();
    }

    /// Record file updated
    pub fn add_file_updated(&self) {
        self.files_updated.fetch_add(1, Ordering::Relaxed);
        self.add_file_processed();
    }

    /// Record file deleted
    pub fn add_file_deleted(&self) {
        self.files_deleted.fetch_add(1, Ordering::Relaxed);
    }

    /// Record directory created
    #[allow(dead_code)]
    pub fn add_directory_created(&self) {
        self.directories_created.fetch_add(1, Ordering::Relaxed);
    }

    /// Update peak speed if current speed is higher
    #[allow(dead_code)]
    pub fn update_peak_speed(&self, bytes_per_sec: f64) {
        let speed = bytes_per_sec as u64;
        let mut current = self.peak_speed.load(Ordering::Relaxed);
        while speed > current {
            match self.peak_speed.compare_exchange_weak(
                current,
                speed,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(x) => current = x,
            }
        }
    }

    /// Get final performance metrics
    pub fn get_metrics(&self) -> PerformanceMetrics {
        let total_duration = self.start_time.elapsed();
        let scan_duration = Duration::from_nanos(self.scan_duration.load(Ordering::Relaxed));
        let plan_duration = Duration::from_nanos(self.plan_duration.load(Ordering::Relaxed));
        let transfer_duration =
            Duration::from_nanos(self.transfer_duration.load(Ordering::Relaxed));

        let bytes_transferred = self.bytes_transferred.load(Ordering::Relaxed);
        let bytes_read = self.bytes_read.load(Ordering::Relaxed);
        let files_processed = self.files_processed.load(Ordering::Relaxed);

        let avg_transfer_speed = if transfer_duration.as_secs_f64() > 0.0 {
            bytes_transferred as f64 / transfer_duration.as_secs_f64()
        } else {
            0.0
        };

        let peak_transfer_speed = self.peak_speed.load(Ordering::Relaxed) as f64;

        let files_per_second = if total_duration.as_secs_f64() > 0.0 {
            files_processed as f64 / total_duration.as_secs_f64()
        } else {
            0.0
        };

        let bandwidth_utilization = if let Some(limit) = self.rate_limit {
            if limit > 0 {
                Some((avg_transfer_speed / limit as f64) * 100.0)
            } else {
                None
            }
        } else {
            None
        };

        PerformanceMetrics {
            total_duration,
            scan_duration,
            plan_duration,
            transfer_duration,
            bytes_transferred,
            bytes_read,
            files_processed,
            files_created: self.files_created.load(Ordering::Relaxed),
            files_updated: self.files_updated.load(Ordering::Relaxed),
            files_deleted: self.files_deleted.load(Ordering::Relaxed),
            directories_created: self.directories_created.load(Ordering::Relaxed),
            avg_transfer_speed,
            peak_transfer_speed,
            files_per_second,
            bandwidth_utilization,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_performance_monitor_basic() {
        let mut monitor = PerformanceMonitor::new(None);

        monitor.start_scan();
        thread::sleep(Duration::from_millis(10));
        monitor.end_scan();

        monitor.add_file_created();
        monitor.add_bytes_transferred(1000);

        let metrics = monitor.get_metrics();
        assert!(metrics.scan_duration.as_millis() >= 10);
        assert_eq!(metrics.files_created, 1);
        assert_eq!(metrics.bytes_transferred, 1000);
    }

    #[test]
    fn test_format_speed() {
        assert_eq!(PerformanceMetrics::format_speed(500.0), "500 B/s");
        assert_eq!(PerformanceMetrics::format_speed(1500.0), "1.50 KB/s");
        assert_eq!(PerformanceMetrics::format_speed(1500000.0), "1.50 MB/s");
        assert_eq!(PerformanceMetrics::format_speed(1500000000.0), "1.50 GB/s");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(
            PerformanceMetrics::format_duration(Duration::from_secs(30)),
            "30.00s"
        );
        assert_eq!(
            PerformanceMetrics::format_duration(Duration::from_secs(90)),
            "1m 30s"
        );
        assert_eq!(
            PerformanceMetrics::format_duration(Duration::from_secs(3665)),
            "1h 1m 5s"
        );
    }

    #[test]
    fn test_bandwidth_utilization() {
        let monitor = PerformanceMonitor::new(Some(1_000_000)); // 1 MB/s limit
        monitor.add_bytes_transferred(500_000);

        thread::sleep(Duration::from_secs(1));

        let metrics = monitor.get_metrics();
        assert!(metrics.bandwidth_utilization.is_some());
    }

    #[test]
    fn test_peak_speed_tracking() {
        let monitor = PerformanceMonitor::new(None);

        monitor.update_peak_speed(1_000_000.0);
        monitor.update_peak_speed(500_000.0); // Lower, should not update
        monitor.update_peak_speed(2_000_000.0); // Higher, should update

        let metrics = monitor.get_metrics();
        assert_eq!(metrics.peak_transfer_speed, 2_000_000.0);
    }

    #[test]
    fn test_file_counters() {
        let monitor = PerformanceMonitor::new(None);

        monitor.add_file_created();
        monitor.add_file_created();
        monitor.add_file_updated();
        monitor.add_file_deleted();
        monitor.add_directory_created();

        let metrics = monitor.get_metrics();
        assert_eq!(metrics.files_created, 2);
        assert_eq!(metrics.files_updated, 1);
        assert_eq!(metrics.files_deleted, 1);
        assert_eq!(metrics.directories_created, 1);
        assert_eq!(metrics.files_processed, 3); // created + updated
    }

    #[test]
    #[ignore] // Too timing-sensitive for CI environments
    fn test_phase_duration_accuracy() {
        let mut monitor = PerformanceMonitor::new(None);

        // Scan phase
        monitor.start_scan();
        thread::sleep(Duration::from_millis(50));
        monitor.end_scan();

        // Plan phase
        monitor.start_plan();
        thread::sleep(Duration::from_millis(30));
        monitor.end_plan();

        // Transfer phase
        monitor.start_transfer();
        thread::sleep(Duration::from_millis(20));
        monitor.end_transfer();

        let metrics = monitor.get_metrics();

        // Verify durations are within reasonable bounds (allow 100% variance for CI)
        assert!(
            metrics.scan_duration.as_millis() >= 40 && metrics.scan_duration.as_millis() < 150,
            "scan_duration: {:?}",
            metrics.scan_duration
        );
        assert!(
            metrics.plan_duration.as_millis() >= 20 && metrics.plan_duration.as_millis() < 100,
            "plan_duration: {:?}",
            metrics.plan_duration
        );
        assert!(
            metrics.transfer_duration.as_millis() >= 10
                && metrics.transfer_duration.as_millis() < 100,
            "transfer_duration: {:?}",
            metrics.transfer_duration
        );

        // Total duration should be at least the sum of all phases
        let sum_phases = metrics.scan_duration + metrics.plan_duration + metrics.transfer_duration;
        assert!(
            metrics.total_duration >= sum_phases,
            "total: {:?}, sum: {:?}",
            metrics.total_duration,
            sum_phases
        );
    }

    #[test]
    #[ignore] // Too timing-sensitive for CI environments
    fn test_speed_calculation_accuracy() {
        let mut monitor = PerformanceMonitor::new(None);

        monitor.start_transfer();
        monitor.add_bytes_transferred(1_000_000); // 1 MB
        thread::sleep(Duration::from_secs(1)); // Wait 1 second
        monitor.end_transfer();

        let metrics = monitor.get_metrics();

        // Average speed should be approximately 1 MB/s (allow some variance)
        assert!(
            metrics.avg_transfer_speed >= 900_000.0 && metrics.avg_transfer_speed <= 1_100_000.0,
            "avg_transfer_speed: {:.0}",
            metrics.avg_transfer_speed
        );
    }

    #[test]
    fn test_concurrent_byte_counting() {
        use std::sync::Arc;

        let monitor = Arc::new(PerformanceMonitor::new(None));
        let mut handles = vec![];

        // Spawn 10 threads, each adding 1000 bytes transferred and 2000 bytes read
        for _ in 0..10 {
            let monitor_clone = Arc::clone(&monitor);
            let handle = thread::spawn(move || {
                for _ in 0..100 {
                    monitor_clone.add_bytes_transferred(10);
                    monitor_clone.add_bytes_read(20);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let metrics = monitor.get_metrics();
        // 10 threads * 100 iterations * 10 bytes = 10,000 bytes transferred
        // 10 threads * 100 iterations * 20 bytes = 20,000 bytes read
        assert_eq!(
            metrics.bytes_transferred, 10_000,
            "bytes_transferred: {}",
            metrics.bytes_transferred
        );
        assert_eq!(
            metrics.bytes_read, 20_000,
            "bytes_read: {}",
            metrics.bytes_read
        );
    }

    #[test]
    fn test_concurrent_file_counting() {
        use std::sync::Arc;

        let monitor = Arc::new(PerformanceMonitor::new(None));
        let mut handles = vec![];

        // Spawn multiple threads updating counters concurrently
        for _ in 0..5 {
            let monitor_clone = Arc::clone(&monitor);
            let handle = thread::spawn(move || {
                for _ in 0..10 {
                    monitor_clone.add_file_created();
                    monitor_clone.add_file_updated();
                    monitor_clone.add_file_deleted();
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let metrics = monitor.get_metrics();
        assert_eq!(metrics.files_created, 50); // 5 threads * 10 iterations
        assert_eq!(metrics.files_updated, 50);
        assert_eq!(metrics.files_deleted, 50);
        assert_eq!(metrics.files_processed, 100); // created + updated
    }

    #[test]
    #[ignore] // Too timing-sensitive for CI environments
    fn test_files_per_second_accuracy() {
        let monitor = PerformanceMonitor::new(None);

        // Simulate processing 100 files
        for _ in 0..100 {
            monitor.add_file_created();
        }

        // Wait long enough to get measurable duration
        thread::sleep(Duration::from_millis(100));

        let metrics = monitor.get_metrics();

        // Should process approximately 1000 files/sec (100 files in 0.1 sec)
        // Allow wide range due to thread scheduling variance
        assert!(
            metrics.files_per_second >= 500.0 && metrics.files_per_second <= 2000.0,
            "files_per_second: {:.2}",
            metrics.files_per_second
        );
    }

    #[test]
    #[ignore] // Too timing-sensitive for CI environments
    fn test_bandwidth_utilization_accuracy() {
        let mut monitor = PerformanceMonitor::new(Some(1_000_000)); // 1 MB/s limit

        monitor.start_transfer();
        monitor.add_bytes_transferred(500_000); // 500 KB transferred
        thread::sleep(Duration::from_secs(1)); // Over 1 second
        monitor.end_transfer();

        let metrics = monitor.get_metrics();

        // Utilization should be around 50% (500 KB/s with 1 MB/s limit)
        assert!(metrics.bandwidth_utilization.is_some());
        let utilization = metrics.bandwidth_utilization.unwrap();
        assert!(
            (40.0..=60.0).contains(&utilization),
            "bandwidth_utilization: {:.2}%",
            utilization
        );
    }

    #[test]
    fn test_zero_duration_edge_case() {
        let monitor = PerformanceMonitor::new(None);

        // Get metrics immediately without any transfers
        let metrics = monitor.get_metrics();

        // Should handle zero duration gracefully
        assert_eq!(metrics.avg_transfer_speed, 0.0);
        assert_eq!(metrics.files_per_second, 0.0);
    }

    #[test]
    fn test_peak_speed_concurrent_updates() {
        use std::sync::Arc;

        let monitor = Arc::new(PerformanceMonitor::new(None));
        let mut handles = vec![];

        // Spawn threads updating peak speed concurrently with different values
        for i in 1..=10 {
            let monitor_clone = Arc::clone(&monitor);
            let speed = (i * 100_000) as f64; // 100KB, 200KB, ..., 1MB
            let handle = thread::spawn(move || {
                monitor_clone.update_peak_speed(speed);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let metrics = monitor.get_metrics();
        // Peak should be the maximum: 1,000,000
        assert_eq!(metrics.peak_transfer_speed, 1_000_000.0);
    }

    #[test]
    #[ignore] // Too timing-sensitive for CI environments
    fn test_phase_percentage_accuracy() {
        let mut monitor = PerformanceMonitor::new(None);

        monitor.start_scan();
        thread::sleep(Duration::from_millis(100));
        monitor.end_scan();

        monitor.start_plan();
        thread::sleep(Duration::from_millis(50));
        monitor.end_plan();

        monitor.start_transfer();
        thread::sleep(Duration::from_millis(50));
        monitor.end_transfer();

        let metrics = monitor.get_metrics();

        // Calculate percentages
        let scan_pct =
            (metrics.scan_duration.as_secs_f64() / metrics.total_duration.as_secs_f64()) * 100.0;
        let plan_pct =
            (metrics.plan_duration.as_secs_f64() / metrics.total_duration.as_secs_f64()) * 100.0;
        let transfer_pct = (metrics.transfer_duration.as_secs_f64()
            / metrics.total_duration.as_secs_f64())
            * 100.0;

        // Percentages should sum to approximately 100% (within rounding)
        let total_pct = scan_pct + plan_pct + transfer_pct;
        assert!(
            (95.0..=105.0).contains(&total_pct),
            "total_pct: {:.2}%",
            total_pct
        );

        // Scan should be roughly 50% (100ms out of ~200ms total)
        assert!(
            (40.0..=60.0).contains(&scan_pct),
            "scan_pct: {:.2}%",
            scan_pct
        );
    }
}
