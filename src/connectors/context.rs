//! What a session needs from its surroundings: its connection's name, its
//! items in the vault, and a way to tell the human and the operator.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::domain::{Event, Trajectory};

use super::vault::ConnectionVault;

pub struct Context {
    pub name: String,
    pub vault: ConnectionVault,
    log: Arc<Trajectory>,
    debug: fn(&str),
}

impl Context {
    pub fn new(name: &str, vault: ConnectionVault, log: Arc<Trajectory>, debug: fn(&str)) -> Self {
        Context {
            name: name.to_owned(),
            vault,
            log,
            debug,
        }
    }

    /// Tell the human, through the trajectory.
    pub fn notice(&self, text: String) {
        if let Err(error) = self.log.append(Event::Notice { text }) {
            self.debug(&format!("notice not recorded: {error}"));
        }
    }

    /// Tell the operator.
    pub fn debug(&self, text: &str) {
        (self.debug)(&format!("connectors: {}: {text}", self.name));
    }
}

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each critical section is a few field updates, so a panic elsewhere
    // can't leave the state half-changed.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
