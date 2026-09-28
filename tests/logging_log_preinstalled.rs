//! A host that installed its own `log` logger first (Android's
//! `android_logger::init_once`) must still get the composed tracing
//! subscriber — and `freedom_mobile_init_logging` must say so. With
//! `try_init` the subscriber went in but the `LogTracer` step then failed,
//! so the call returned false although the subscriber was installed.
//!
//! Its own test binary: both the `log` logger and the tracing subscriber
//! are process-global and can only be installed once.

use freedom_mobile_ffi::freedom_mobile_init_logging;

struct HostLogger;

impl log::Log for HostLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, _: &log::Record) {}
    fn flush(&self) {}
}

static HOST_LOGGER: HostLogger = HostLogger;

#[test]
fn host_log_logger_does_not_turn_install_into_false() {
    log::set_logger(&HOST_LOGGER).expect("log slot already taken");
    assert!(!tracing::dispatcher::has_been_set());

    assert!(
        freedom_mobile_init_logging(),
        "subscriber installed but reported as failure"
    );
    assert!(tracing::dispatcher::has_been_set());
    // Idempotent: still ours.
    assert!(freedom_mobile_init_logging());
}
