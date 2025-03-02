#[doc(hidden)]
pub mod deps {
    pub use linkme;
    pub use serde_json;
}

pub mod catalog;

#[doc(hidden)]
pub mod function_name;

pub mod dispatch;
use std::sync::atomic::{AtomicBool, Ordering};

use dispatch::{Dispatch, SetDispatchError};

pub mod random;

#[cfg(not(feature = "disabled"))]
pub mod macros;

#[cfg(feature = "disabled")]
pub mod macros_stubs;

pub fn init(dispatcher: &'static dyn Dispatch) -> Result<(), SetDispatchError> {
    if cfg!(not(feature = "disabled")) {
        dispatch::set_dispatcher(dispatcher)?;
        catalog::init_catalog();
    }
    Ok(())
}

pub fn init_boxed(dispatcher: Box<dyn Dispatch>) -> Result<(), SetDispatchError> {
    if cfg!(not(feature = "disabled")) {
        init(Box::leak(dispatcher))
    } else {
        Ok(())
    }
}

// Precept Faults are enabled by default
static FAULTS_ENABLED: AtomicBool = AtomicBool::new(true);

#[inline]
pub fn faults_enabled() -> bool {
    FAULTS_ENABLED.load(Ordering::Acquire)
}

#[inline]
pub fn disable_faults() {
    tracing::warn!("Precept Faults disabled");
    FAULTS_ENABLED.store(false, Ordering::Release);
}

#[inline]
pub fn enable_faults() {
    FAULTS_ENABLED.store(true, Ordering::Release);
}
