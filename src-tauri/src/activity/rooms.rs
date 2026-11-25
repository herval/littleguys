//! What rooms (runtime/rooms) need from the roster, and what the roster needs from them.
use super::*;
use crate::runtime::rooms::{Bots, RoomsClient, ROOMS_GATEWAY_ID};
use crate::runtime::RuntimeKind;
use futures_util::future::BoxFuture;
use std::sync::Weak;

pub(super) fn is_rooms(g: &GatewayRoster) -> bool {
    g.status.kind == RuntimeKind::Rooms
}

struct RosterBots(Weak<Activity>);

impl Bots for RosterBots {
    fn call(&self, gateway: &str, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let (activity, gateway, method) = (self.0.upgrade(), gateway.to_string(), method.to_string());
        Box::pin(async move {
            let gone = || RpcError::new("UNAVAILABLE", "That bot isn't connected.");
            let activity = activity.ok_or_else(gone)?;
            let client = activity.client(&gateway).ok_or_else(gone)?;
            // Through Activity, like a window's call: it knows how each bot takes `delivery`.
            activity.call(&client, &method, params, None).await
        })
    }

    fn guy(&self, gateway: &str, agent: &str) -> Option<String> {
        let activity = self.0.upgrade()?;
        let mut state = activity.state.lock().unwrap();
        let g = state.gateway(gateway).filter(|g| g.roster.enabled && g.roster.status.phase == Phase::Connected)?;
        let row = g.roster.agents.iter().find(|a| a.get("id").and_then(Value::as_str) == Some(agent))?;
        let name = row.pointer("/identity/name").or_else(|| row.get("name")).and_then(Value::as_str).filter(|n| !n.trim().is_empty());
        Some(name.unwrap_or(agent).trim().to_string())
    }

    fn online(&self, gateway: &str) -> bool {
        self.0.upgrade().and_then(|a| a.client(gateway)).is_some_and(|c| c.status().phase == Phase::Connected)
    }

    fn steers(&self, gateway: &str) -> bool {
        self.0.upgrade().and_then(|a| a.client(gateway)).is_some_and(|c| c.status().capabilities.steer)
    }

    fn is_running(&self, gateway: &str, key: &str) -> bool {
        self.0.upgrade().is_some_and(|a| a.is_running(gateway, key))
    }
}

impl Activity {
    /// The bots, as a room reaches its members.
    pub fn bots(self: &Arc<Self>) -> Arc<dyn Bots> {
        Arc::new(RosterBots(Arc::downgrade(self)))
    }

    pub fn set_rooms(&self, rooms: RoomsClient) {
        let _ = self.rooms.set(rooms);
    }

    /// A bot's event, for the room waiting on one of its guys.
    pub(super) fn tell_rooms(&self, gateway: &str, name: &str, payload: &Value) {
        if gateway != ROOMS_GATEWAY_ID {
            if let Some(rooms) = self.rooms.get() {
                rooms.on_bot_event(gateway, name, payload);
            }
        }
    }

    /// The room a guy's thread is its seat in, if it is one: `(id, name)`.
    pub(super) fn seat_room(&self, gateway: &str, key: &str) -> Option<(String, String)> {
        self.rooms.get()?.seat_room(gateway, key)
    }

    /// Seats say which room they're for (`room: {id, name}`), so lists can leave them out:
    /// what happens there shows in the room. They're never unread by themselves.
    pub(super) fn mark_seats(&self, roster: &mut GatewayRoster) {
        if is_rooms(roster) || self.rooms.get().is_none() {
            return;
        }
        for row in roster.sessions.iter_mut() {
            let seat = row.get("key").and_then(Value::as_str).and_then(|key| self.seat_room(&roster.id, key));
            if let Some((id, name)) = seat {
                row["room"] = json!({ "id": id, "name": name });
                row["unread"] = json!(false);
            }
        }
    }
}
