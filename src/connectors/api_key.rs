//! A fixed header on every request. There's no login to lose: a rejection is
//! counted, and the next request carries the key again.

use crate::domain::Secret;

pub struct ApiKey {
    header: String,
    value: Secret,
}

impl ApiKey {
    pub fn new(header: String, value: Secret) -> Self {
        ApiKey { header, value }
    }

    pub fn headers(&self) -> Vec<(String, Secret)> {
        vec![(self.header.clone(), self.value.clone())]
    }
}
