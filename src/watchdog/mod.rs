mod device;
pub use device::{
    CloseResult, Device, Inspection, Support, inspect, open_for_exercise, validate_exercise,
};

use crate::config::WatchdogConfig;
use anyhow::{Context, Result, anyhow, ensure};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

trait ManagedDevice: Send {
    fn set_timeout(&mut self, seconds: i32) -> Result<i32>;
    fn keep_alive(&mut self) -> Result<()>;
    fn disable_and_close(&mut self) -> Result<()>;
    fn close_without_stopping(&mut self) -> Result<()>;
}

impl ManagedDevice for Device {
    fn set_timeout(&mut self, seconds: i32) -> Result<i32> {
        Device::set_timeout(self, seconds)
    }

    fn keep_alive(&mut self) -> Result<()> {
        Device::keep_alive(self)
    }

    fn disable_and_close(&mut self) -> Result<()> {
        self.stop_user_and_close().map(|_| ())
    }

    fn close_without_stopping(&mut self) -> Result<()> {
        if self.is_open() {
            Device::close_without_stopping(self)?;
        }
        Ok(())
    }
}

fn open(path: &str) -> Result<Device> {
    let inspection = inspect(path)?;
    let mut device = Device::open(path, inspection.device_number)?;
    let compare_support = device.support().and_then(|support| {
        ensure!(
            support.identity == inspection.identity
                && support.can_set_timeout
                && support.can_keepalive,
            "watchdog identity or required ioctl support differs"
        );
        Ok(())
    });
    if let Err(error) = compare_support {
        return Err(with_cleanup(
            error,
            Device::close_without_stopping(&mut device).map(|_| ()),
        )
        .context("opened watchdog was released without disabling; hardware may remain armed"));
    }
    Ok(device)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shutdown {
    Normal,
    Abandon,
}

#[derive(Default)]
struct State {
    shutdown: Option<Shutdown>,
    failure: Option<String>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// Owns the userspace watchdog lifecycle. The health callback must be bounded
/// and check Core's local service liveness, independently of external services.
pub struct Manager {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    effective_timeout: Duration,
}

impl Manager {
    pub fn start(
        config: &WatchdogConfig,
        health: impl Fn() -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        Self::start_with(config, health, open)
    }

    fn start_with<D: ManagedDevice + 'static>(
        config: &WatchdogConfig,
        health: impl Fn() -> Result<()> + Send + 'static,
        open: impl FnOnce(&str) -> Result<D>,
    ) -> Result<Self> {
        let mut manager = Self {
            shared: Arc::new(Shared::default()),
            worker: Mutex::default(),
            effective_timeout: Duration::ZERO,
        };
        if !config.enabled {
            return Ok(manager);
        }
        let period = config.feed_interval;
        ensure!(
            config.timeout >= Duration::from_secs(1)
                && config.timeout <= Duration::from_secs(300)
                && config.timeout.subsec_nanos() == 0
                && period >= Duration::from_secs(1)
                && period < config.timeout,
            "invalid watchdog timeout or feed interval"
        );
        let mut device = open(&config.device).context("open watchdog")?;
        let mut initialize = || -> Result<Duration> {
            let actual = device
                .set_timeout(config.timeout.as_secs() as i32)
                .context("set/read watchdog timeout")?;
            ensure!(
                actual > 0 && Duration::from_secs(actual as u64) > period,
                "driver watchdog timeout is shorter than the feed interval"
            );
            device.keep_alive().context("initial watchdog feed")?;
            Ok(Duration::from_secs(actual as u64))
        };
        manager.effective_timeout = match initialize() {
            Ok(timeout) => timeout,
            Err(error) => {
                return Err(with_cleanup(error, device.close_without_stopping())
                    .context("watchdog startup failed; released without disabling"));
            }
        };
        let shared = Arc::clone(&manager.shared);
        *manager.worker.get_mut().unwrap() = Some(
            thread::Builder::new()
                .name("naos-watchdog".into())
                .spawn(move || run(&shared, &mut device, health, period))
                .context("start watchdog worker; descriptor released without disabling")?,
        );
        Ok(manager)
    }

    pub fn effective_timeout(&self) -> Duration {
        self.effective_timeout
    }

    pub fn failure(&self) -> Option<String> {
        self.shared.state.lock().unwrap().failure.clone()
    }

    pub fn stop(&self) -> Result<()> {
        self.finish(Shutdown::Normal)
    }

    fn finish(&self, shutdown: Shutdown) -> Result<()> {
        let mut worker = self.worker.lock().unwrap();
        if let Some(worker) = worker.take() {
            self.shared.state.lock().unwrap().shutdown = Some(shutdown);
            self.shared.wake.notify_all();
            if worker.join().is_err() {
                self.shared.state.lock().unwrap().failure = Some("watchdog worker panicked".into());
            }
        }
        if let Some(failure) = self.failure() {
            return Err(anyhow!(failure));
        }
        Ok(())
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        let _ = self.finish(Shutdown::Abandon);
    }
}

fn run(
    shared: &Shared,
    device: &mut impl ManagedDevice,
    health: impl Fn() -> Result<()>,
    period: Duration,
) {
    let mut next_tick = Instant::now() + period;
    let shutdown = loop {
        let state = shared.state.lock().unwrap();
        let remaining = next_tick.saturating_duration_since(Instant::now());
        let (state, _) = shared
            .wake
            .wait_timeout_while(state, remaining, |state| state.shutdown.is_none())
            .unwrap();
        if let Some(shutdown) = state.shutdown {
            break shutdown;
        }
        drop(state);

        let started = Instant::now();
        let health_result = health().and_then(|()| {
            ensure!(
                started.elapsed() < HEALTH_TIMEOUT,
                "watchdog health check deadline exceeded"
            );
            Ok(())
        });

        let state = shared.state.lock().unwrap();
        if let Some(shutdown) = state.shutdown {
            break shutdown;
        }
        // The worker alone owns the device. Holding the stop-state lock makes
        // the decision to feed atomic with respect to a normal stop request.
        let result = health_result.and_then(|()| device.keep_alive());
        drop(state);
        if let Err(error) = result {
            record_failure(
                shared,
                with_cleanup(error, device.close_without_stopping())
                    .context("watchdog health/feed failed"),
            );
            return;
        }
        let now = Instant::now();
        while next_tick <= now {
            next_tick += period;
        }
    };

    let result = match shutdown {
        Shutdown::Normal => device.disable_and_close().map_err(|error| {
            with_cleanup(error, device.close_without_stopping()).context("stop watchdog")
        }),
        Shutdown::Abandon => device.close_without_stopping(),
    };
    if let Err(error) = result {
        record_failure(shared, error);
    }
}

fn with_cleanup(error: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup) => error.context(format!("release watchdog descriptor: {cleanup:#}")),
    }
}

fn record_failure(shared: &Shared, error: anyhow::Error) {
    shared.state.lock().unwrap().failure = Some(format!("{error:#}"));
}

#[cfg(test)]
mod tests;
