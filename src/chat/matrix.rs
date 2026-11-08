//! Matrix over the client-server API, for one unencrypted room and one human.
//!
//! The sync position is saved next to the database, so a restart picks up
//! where the last run stopped. The first run skips the room's history.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::Duration,
};

use reqwest::Url;
use serde::Deserialize;
use serde_json::json;

use crate::domain::Secret;

use super::{Chat, ChatError, Outgoing, Speaker};

/// How long the homeserver may hold a sync open before answering.
const SYNC_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct MatrixConfig {
    pub homeserver: Url,
    /// The agent's own account.
    pub access_token: Secret,
    pub room: String,
    /// The only user whose messages reach the agent.
    pub owner: String,
    /// Where the sync position is kept between runs.
    pub state_path: PathBuf,
}

impl MatrixConfig {
    /// Read from MATRIX_* variables, or `None` when MATRIX_HOMESERVER isn't set.
    pub fn read(env: &HashMap<String, String>, state_dir: &Path) -> Result<Option<Self>, String> {
        let Some(homeserver) = env.get("MATRIX_HOMESERVER") else {
            return Ok(None);
        };
        let homeserver = Url::parse(homeserver).map_err(|e| format!("MATRIX_HOMESERVER: {e}"))?;
        let required = |name: &str| {
            env.get(name)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("Set {name} when MATRIX_HOMESERVER is set"))
        };
        Ok(Some(MatrixConfig {
            homeserver,
            access_token: Secret::new(required("MATRIX_ACCESS_TOKEN")?),
            room: required("MATRIX_ROOM_ID")?,
            owner: required("MATRIX_OWNER")?,
            state_path: state_dir.join("matrix-sync-token"),
        }))
    }
}

pub struct Matrix {
    http: reqwest::Client,
    config: MatrixConfig,
    since: Mutex<String>,
}

impl Matrix {
    /// Join the room and find where to start reading.
    pub async fn connect(config: MatrixConfig) -> Result<Self, ChatError> {
        let http = reqwest::Client::builder()
            .user_agent("evalien")
            .timeout(SYNC_WAIT + Duration::from_secs(30))
            .build()
            .map_err(|e| ChatError(e.to_string()))?;
        let mut matrix = Matrix {
            http,
            config,
            since: Mutex::new(String::new()),
        };

        let join = matrix.url(&["join", &matrix.config.room]);
        let request = matrix.http.post(join).json(&json!({}));
        matrix.call(request).await?;

        let since = match std::fs::read_to_string(&matrix.config.state_path) {
            Ok(saved) if !saved.trim().is_empty() => saved.trim().to_owned(),
            // First run: start from now rather than replaying the room's history.
            _ => matrix.sync(None, Duration::ZERO).await?.next_batch,
        };
        matrix.save(&since)?;
        matrix.since = Mutex::new(since);
        Ok(matrix)
    }

    fn url(&self, path: &[&str]) -> Url {
        let mut url = self.config.homeserver.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty().extend(["_matrix", "client", "v3"]).extend(path);
        }
        url
    }

    async fn sync(&self, since: Option<&str>, wait: Duration) -> Result<SyncResponse, ChatError> {
        let filter = json!({
            "room": {
                "rooms": [self.config.room],
                "timeline": { "types": ["m.room.message"], "limit": 50 },
                "state": { "types": [] },
                "ephemeral": { "types": [] },
                "account_data": { "types": [] },
            },
            "presence": { "types": [] },
            "account_data": { "types": [] },
        });
        let mut url = self.url(&["sync"]);
        url.query_pairs_mut()
            .append_pair("filter", &filter.to_string())
            .append_pair("timeout", &wait.as_millis().to_string());
        if let Some(since) = since {
            url.query_pairs_mut().append_pair("since", since);
        }
        let body = self.call(self.http.get(url)).await?;
        serde_json::from_str(&body).map_err(|e| ChatError(format!("unexpected sync response: {e}")))
    }

    async fn call(&self, request: reqwest::RequestBuilder) -> Result<String, ChatError> {
        let response = request
            .bearer_auth(self.config.access_token.expose())
            .send()
            .await
            .map_err(|e| ChatError(e.to_string()))?;
        let status = response.status();
        let body = response.text().await.map_err(|e| ChatError(e.to_string()))?;
        if !status.is_success() {
            let excerpt: String = body.chars().take(300).collect();
            return Err(ChatError(format!("{status}: {excerpt}")));
        }
        Ok(body)
    }

    /// Write the sync position so a crash never leaves a half-written file.
    fn save(&self, since: &str) -> Result<(), ChatError> {
        let path = &self.config.state_path;
        let partial = path.with_extension("partial");
        std::fs::write(&partial, since)
            .and_then(|()| std::fs::rename(&partial, path))
            .map_err(|e| ChatError(format!("saving {}: {e}", path.display())))
    }
}

impl Chat for Matrix {
    async fn receive(&self) -> Result<Vec<String>, ChatError> {
        let since = self.since.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let response = self.sync(Some(&since), SYNC_WAIT).await?;
        let messages = owner_messages(&response, &self.config.room, &self.config.owner);
        self.save(&response.next_batch)?;
        *self.since.lock().unwrap_or_else(PoisonError::into_inner) = response.next_batch;
        Ok(messages)
    }

    async fn send(&self, message: &Outgoing) -> Result<(), ChatError> {
        let msgtype = match message.speaker {
            Speaker::Agent => "m.text",
            Speaker::Runtime => "m.notice",
        };
        // The transaction ID makes a retried send a no-op on the homeserver.
        let url = self.url(&["rooms", &self.config.room, "send", "m.room.message", &message.key]);
        let request = self
            .http
            .put(url)
            .json(&json!({ "msgtype": msgtype, "body": message.text }));
        self.call(request).await.map(|_| ())
    }
}

#[derive(Deserialize)]
struct SyncResponse {
    next_batch: String,
    #[serde(default)]
    rooms: Rooms,
}

#[derive(Deserialize, Default)]
struct Rooms {
    #[serde(default)]
    join: HashMap<String, JoinedRoom>,
}

#[derive(Deserialize)]
struct JoinedRoom {
    #[serde(default)]
    timeline: Timeline,
}

#[derive(Deserialize, Default)]
struct Timeline {
    #[serde(default)]
    events: Vec<RoomEvent>,
}

#[derive(Deserialize)]
struct RoomEvent {
    #[serde(rename = "type")]
    kind: String,
    sender: String,
    #[serde(default)]
    content: Content,
}

#[derive(Deserialize, Default)]
struct Content {
    msgtype: Option<String>,
    body: Option<String>,
    /// Present on edits, which repeat a message rather than say something new.
    #[serde(rename = "m.new_content")]
    new_content: Option<serde_json::Value>,
}

/// Plain-text messages the owner sent in the room, in order.
fn owner_messages(response: &SyncResponse, room: &str, owner: &str) -> Vec<String> {
    let Some(joined) = response.rooms.join.get(room) else {
        return Vec::new();
    };
    joined
        .timeline
        .events
        .iter()
        .filter(|event| event.kind == "m.room.message" && event.sender == owner)
        .filter(|event| event.content.msgtype.as_deref() == Some("m.text") && event.content.new_content.is_none())
        .filter_map(|event| event.content.body.clone())
        .filter(|body| !body.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(homeserver: &str) -> MatrixConfig {
        MatrixConfig {
            homeserver: Url::parse(homeserver).unwrap(),
            access_token: Secret::new("token"),
            room: "!room:skum.dev".into(),
            owner: "@morgan:skum.dev".into(),
            state_path: PathBuf::from("/tmp/unused"),
        }
    }

    #[test]
    fn urls_encode_room_ids_and_keep_a_base_path() {
        let matrix = Matrix {
            http: reqwest::Client::new(),
            config: config("https://matrix.skum.dev"),
            since: Mutex::default(),
        };
        assert_eq!(
            matrix
                .url(&["rooms", "!room:skum.dev", "send", "m.room.message", "evalien-7-0"])
                .as_str(),
            "https://matrix.skum.dev/_matrix/client/v3/rooms/!room:skum.dev/send/m.room.message/evalien-7-0"
        );
        let proxied = Matrix {
            http: reqwest::Client::new(),
            config: config("http://synapse:8008/base/"),
            since: Mutex::default(),
        };
        assert_eq!(
            proxied.url(&["sync"]).as_str(),
            "http://synapse:8008/base/_matrix/client/v3/sync"
        );
    }

    #[test]
    fn only_the_owners_plain_messages_in_the_room_get_through() {
        let response: SyncResponse = serde_json::from_value(json!({
            "next_batch": "s2",
            "rooms": { "join": {
                "!room:skum.dev": { "timeline": { "events": [
                    { "type": "m.room.message", "sender": "@morgan:skum.dev", "content": { "msgtype": "m.text", "body": "hi" } },
                    { "type": "m.room.message", "sender": "@stranger:evil.org", "content": { "msgtype": "m.text", "body": "rm -rf" } },
                    { "type": "m.room.message", "sender": "@evalien:skum.dev", "content": { "msgtype": "m.text", "body": "my own echo" } },
                    { "type": "m.room.message", "sender": "@morgan:skum.dev", "content": { "msgtype": "m.image", "body": "cat.png" } },
                    { "type": "m.room.message", "sender": "@morgan:skum.dev", "content": { "msgtype": "m.text", "body": "* hi!", "m.new_content": { "body": "hi!" } } },
                    { "type": "m.reaction", "sender": "@morgan:skum.dev", "content": {} },
                    { "type": "m.room.message", "sender": "@morgan:skum.dev", "content": { "msgtype": "m.text", "body": "second" } },
                ] } },
                "!other:skum.dev": { "timeline": { "events": [
                    { "type": "m.room.message", "sender": "@morgan:skum.dev", "content": { "msgtype": "m.text", "body": "elsewhere" } },
                ] } },
            } },
        }))
        .unwrap();
        assert_eq!(
            owner_messages(&response, "!room:skum.dev", "@morgan:skum.dev"),
            ["hi", "second"]
        );
    }

    #[test]
    fn matrix_is_optional_but_complete_when_configured() {
        let env = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let dir = Path::new("/data");
        assert!(MatrixConfig::read(&env(&[]), dir).unwrap().is_none());
        let missing = MatrixConfig::read(&env(&[("MATRIX_HOMESERVER", "https://m.example")]), dir).unwrap_err();
        assert!(missing.contains("MATRIX_ACCESS_TOKEN"), "{missing}");
        let full = MatrixConfig::read(
            &env(&[
                ("MATRIX_HOMESERVER", "https://m.example"),
                ("MATRIX_ACCESS_TOKEN", "t"),
                ("MATRIX_ROOM_ID", "!r:m.example"),
                ("MATRIX_OWNER", "@me:m.example"),
            ]),
            dir,
        )
        .unwrap()
        .unwrap();
        assert_eq!(full.state_path, Path::new("/data/matrix-sync-token"));
        assert_eq!(format!("{:?}", full.access_token), "Secret(…)");
    }
}
