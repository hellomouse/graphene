//! Utility functions for testing

/// Vec-like that can be initialized with zeroes
pub trait Zeroed {
    /// create and initialize array of given length with zeroes
    fn zeroed(length: usize) -> Self;
}

impl Zeroed for Vec<u8> {
    fn zeroed(length: usize) -> Self {
        let mut vec = Vec::with_capacity(length);
        vec.resize(length, 0);
        vec
    }
}

pub fn setup_log_handlers() {
    use tracing_error::ErrorLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    color_eyre::install().unwrap();

    let fmt_layer = fmt::layer();
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(ErrorLayer::default())
        .init();
}

pub fn initialize_logging() {
    use parking_lot::Once;

    static INITIALIZE: Once = Once::new();
    INITIALIZE.call_once(setup_log_handlers);
}
