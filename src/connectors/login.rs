//! A login in progress, shared: the first `connect` starts it, later ones
//! join it, and everyone waiting learns how it ended.

use std::time::Duration;

use tokio::sync::watch;

use crate::domain::ConnectionStatus;

type Outcome = Option<Result<(), String>>;

/// The login's side: finish it once, and everyone waiting hears.
pub struct Login {
    done: watch::Sender<Outcome>,
}

/// A caller's side: wait for the login to finish, up to a timeout.
pub struct Waiting(watch::Receiver<Outcome>);

impl Login {
    pub fn new() -> (Login, Waiting) {
        let (done, waiting) = watch::channel(None);
        (Login { done }, Waiting(waiting))
    }

    pub fn join(&self) -> Waiting {
        Waiting(self.done.subscribe())
    }

    /// Whether anyone is still waiting for it.
    pub fn is_awaited(&self) -> bool {
        self.done.receiver_count() > 0
    }

    pub fn finish(self, result: Result<(), String>) {
        self.done.send_replace(Some(result));
    }
}

impl Waiting {
    /// How the login ended, or `None` if `timeout` passed first. Stops
    /// waiting either way, so [`Login::is_awaited`] counts only the others.
    pub async fn wait(mut self, timeout: Duration) -> Option<Result<(), String>> {
        match tokio::time::timeout(timeout, self.0.wait_for(Option::is_some)).await {
            Ok(Ok(outcome)) => outcome.clone(),
            Ok(Err(_)) => Some(Err("the login was dropped".into())),
            Err(_) => None,
        }
    }
}

/// What `connect` answers once the wait is over.
pub fn answer(name: &str, waited: Option<Result<(), String>>, timeout: Duration) -> Result<ConnectionStatus, String> {
    match waited {
        Some(Ok(())) => Ok(ConnectionStatus::Connected),
        Some(Err(message)) => Err(message),
        None => Err(format!("{name}: no sign-in within {}s", timeout.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn everyone_waiting_hears_how_it_ended() {
        let (login, first) = Login::new();
        let second = login.join();
        login.finish(Ok(()));
        assert_eq!(first.wait(Duration::from_secs(1)).await, Some(Ok(())));
        assert_eq!(second.wait(Duration::from_secs(1)).await, Some(Ok(())));
    }

    #[tokio::test]
    async fn one_caller_giving_up_leaves_the_others_waiting() {
        let (login, impatient) = Login::new();
        let patient = login.join();
        assert_eq!(impatient.wait(Duration::from_millis(1)).await, None);
        assert!(login.is_awaited());
        drop(patient);
        assert!(!login.is_awaited());
    }
}
