use std::{io, time::Duration};

use thiserror::Error as ThisError;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("bind TCP listener: {0}")]
    BindTCPListener(io::Error),
    #[error("get TCP listener socket address: {0}")]
    GetTCPListenerAddress(io::Error),
    #[error("req_count did not reach {target_count} within {timeout:?} (current count: {current_count})")]
    AwaitReqCountTimeout {
        current_count: u64,
        target_count: u64,
        timeout: Duration,
    },
    #[error("concurrent_req_count did not reach {target_count} within {timeout:?} (current count: {current_count})")]
    AwaitConcurrentReqCountTimeout {
        current_count: u64,
        target_count: u64,
        timeout: Duration,
    },
}
