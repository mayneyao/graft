use crate::page::{Page, PAGESIZE};

impl Page {
    pub fn test_filled(value: u8) -> Self {
        Page::from(&[value; PAGESIZE.as_usize()])
    }
}

use std::{future::Future, time::Duration};

pub async fn assert_would_timeout<F, O>(f: F)
where
    F: Future<Output = O>,
{
    // pause time, causing Tokio to trigger the timeout once it can make no additional progress on the future.
    tokio::time::pause();
    tokio::select! {
        _ = tokio::time::sleep(Duration::MAX) => {}
        _ = f => panic!("expected timeout"),
    }
}
