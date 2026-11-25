use super::*;
use anyhow::bail;
use std::sync::mpsc;

#[derive(Clone)]
struct FakeDevice {
    log: Arc<Mutex<Vec<String>>>,
    actual_timeout: i32,
    fail_feed: Option<usize>,
    fail_stop: bool,
    feeds: usize,
}

impl Default for FakeDevice {
    fn default() -> Self {
        Self {
            log: Arc::default(),
            actual_timeout: 40,
            fail_feed: None,
            fail_stop: false,
            feeds: 0,
        }
    }
}

impl FakeDevice {
    fn record(&self, item: impl Into<String>) {
        self.log.lock().unwrap().push(item.into());
    }

    fn events(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl ManagedDevice for FakeDevice {
    fn set_timeout(&mut self, seconds: i32) -> Result<i32> {
        self.record(format!("set {seconds}"));
        Ok(self.actual_timeout)
    }

    fn keep_alive(&mut self) -> Result<()> {
        self.feeds += 1;
        self.record("feed");
        ensure!(self.fail_feed != Some(self.feeds), "test keepalive failure");
        Ok(())
    }

    fn disable_and_close(&mut self) -> Result<()> {
        self.record("disable");
        ensure!(!self.fail_stop, "test disable failure");
        self.record("close");
        Ok(())
    }

    fn close_without_stopping(&mut self) -> Result<()> {
        self.record("abandon");
        Ok(())
    }
}

fn config() -> WatchdogConfig {
    WatchdogConfig {
        feed_interval: Duration::from_secs(1),
        ..Default::default()
    }
}

fn wait_for(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "watchdog worker did not advance");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn disabled_and_invalid_configurations_never_open_device() {
    let disabled = WatchdogConfig {
        enabled: false,
        ..config()
    };
    let manager = Manager::start_with(
        &disabled,
        || panic!("disabled watchdog checked health"),
        |_| -> Result<FakeDevice> { panic!("disabled watchdog opened device") },
    )
    .unwrap();
    assert_eq!(manager.effective_timeout(), Duration::ZERO);
    assert!(manager.stop().is_ok());
    for (timeout, interval) in [(30, 0), (2, 2), (301, 10)] {
        let configuration = WatchdogConfig {
            timeout: Duration::from_secs(timeout),
            feed_interval: Duration::from_secs(interval),
            ..config()
        };
        let result = Manager::start_with(
            &configuration,
            || Ok(()),
            |_| -> Result<FakeDevice> { panic!("invalid config opened device") },
        );
        assert!(result.is_err());
    }
}

#[test]
fn feeds_only_after_health_check_and_stops_once_concurrently() {
    let device = FakeDevice::default();
    let health_device = device.clone();
    let manager = Manager::start_with(
        &config(),
        move || {
            health_device.record("health");
            Ok(())
        },
        |_| Ok(device.clone()),
    )
    .unwrap();
    assert_eq!(manager.effective_timeout(), Duration::from_secs(40));
    assert_eq!(device.events(), ["set 30", "feed"]);
    wait_for(|| device.events().len() >= 4);
    thread::scope(|scope| {
        scope.spawn(|| manager.stop().unwrap());
        scope.spawn(|| manager.stop().unwrap());
    });
    assert!(manager.failure().is_none());
    assert_eq!(
        device.events(),
        ["set 30", "feed", "health", "feed", "disable", "close"]
    );
}

#[test]
fn health_failure_abandons_without_feeding_or_disabling() {
    let device = FakeDevice::default();
    let manager = Manager::start_with(
        &config(),
        || bail!("test health endpoint down"),
        |_| Ok(device.clone()),
    )
    .unwrap();
    wait_for(|| manager.failure().is_some());
    let error = manager.stop().unwrap_err();
    assert!(format!("{error:#}").contains("test health endpoint down"));
    assert_eq!(device.events(), ["set 30", "feed", "abandon"]);
}

#[test]
fn feed_failure_abandons_without_retry() {
    let device = FakeDevice {
        fail_feed: Some(2),
        ..FakeDevice::default()
    };
    let manager = Manager::start_with(&config(), || Ok(()), |_| Ok(device.clone())).unwrap();
    wait_for(|| manager.failure().is_some());
    assert!(manager.stop().is_err());
    assert_eq!(device.events(), ["set 30", "feed", "feed", "abandon"]);
}

#[test]
fn initialization_failures_never_disable() {
    let setups = [
        (
            FakeDevice {
                actual_timeout: 1,
                ..FakeDevice::default()
            },
            &["set 30", "abandon"][..],
        ),
        (
            FakeDevice {
                fail_feed: Some(1),
                ..FakeDevice::default()
            },
            &["set 30", "feed", "abandon"][..],
        ),
    ];
    for (device, events) in setups {
        let result = Manager::start_with(&config(), || Ok(()), |_| Ok(device.clone()));
        assert!(result.is_err());
        assert_eq!(device.events(), events);
    }
}

#[test]
fn normal_shutdown_during_inflight_health_does_not_feed_again() {
    let device = FakeDevice::default();
    let (entered, start) = mpsc::channel();
    let (unblock, wait) = mpsc::channel();
    let manager = Manager::start_with(
        &config(),
        move || {
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(())
        },
        |_| Ok(device.clone()),
    )
    .unwrap();
    start.recv_timeout(Duration::from_secs(5)).unwrap();
    thread::scope(|scope| {
        let stopper = scope.spawn(|| manager.stop());
        wait_for(|| manager.shared.state.lock().unwrap().shutdown == Some(Shutdown::Normal));
        unblock.send(()).unwrap();
        stopper.join().unwrap().unwrap();
    });
    assert_eq!(device.events(), ["set 30", "feed", "disable", "close"]);
}

#[test]
fn late_successful_health_checks_are_not_allowed_to_feed() {
    let shared = Shared::default();
    let mut device = FakeDevice::default();
    run(
        &shared,
        &mut device,
        || {
            thread::sleep(HEALTH_TIMEOUT + Duration::from_millis(10));
            Ok(())
        },
        Duration::from_millis(1),
    );
    assert_eq!(device.events(), ["abandon"]);
    assert!(
        shared
            .state
            .lock()
            .unwrap()
            .failure
            .as_deref()
            .unwrap()
            .contains("deadline exceeded")
    );
}

#[test]
fn failed_stop_returns_error_and_drops_descriptor_without_retry() {
    let device = FakeDevice {
        fail_stop: true,
        ..FakeDevice::default()
    };
    let manager = Manager::start_with(&config(), || Ok(()), |_| Ok(device.clone())).unwrap();
    assert!(manager.stop().is_err());
    assert!(manager.stop().is_err());
    assert_eq!(device.events(), ["set 30", "feed", "disable", "abandon"]);
}

#[test]
fn abnormal_service_drop_leaves_watchdog_armed() {
    let device = FakeDevice::default();
    let manager = Manager::start_with(&config(), || Ok(()), |_| Ok(device.clone())).unwrap();
    drop(manager);
    assert_eq!(device.events(), ["set 30", "feed", "abandon"]);
}
