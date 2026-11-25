//! A whole room against scripted guys: what they're sent, what the room posts, when it stops.
use super::*;
use std::sync::Weak;

type Script = Box<dyn Fn(&str) -> Reply + Send + Sync>;

enum Reply {
    Say(&'static str),
    /// Works until told to finish (`Fake::finish`), then says it.
    Later(&'static str),
    Fail,
}

/// Bots whose guys answer from a script. Each `chat.send` is a turn: `status`, then `final`.
struct Fake {
    rooms: OnceLock<RoomsClient>,
    guys: Mutex<HashMap<String, Script>>,
    offline: Mutex<Vec<String>>,
    steers: bool,
    /// Per (gateway, seat): its transcript.
    threads: Mutex<HashMap<(String, String), Vec<Value>>>,
    /// Turns under way: (gateway, seat) → what it will say.
    working: Mutex<HashMap<(String, String), &'static str>>,
    sent: Mutex<Vec<(String, String)>>,
    /// The `cwd` of each `chat.send` that had one.
    folders: Mutex<Vec<String>>,
    aborted: Mutex<Vec<String>>,
    me: OnceLock<Weak<Fake>>,
}

impl Fake {
    fn new(steers: bool) -> Arc<Self> {
        let fake = Arc::new(Self {
            rooms: OnceLock::new(),
            guys: Mutex::default(),
            offline: Mutex::default(),
            steers,
            threads: Mutex::default(),
            working: Mutex::default(),
            sent: Mutex::default(),
            folders: Mutex::default(),
            aborted: Mutex::default(),
            me: OnceLock::new(),
        });
        let _ = fake.me.set(Arc::downgrade(&fake));
        fake
    }

    fn guy(&self, uid: &str, script: impl Fn(&str) -> Reply + Send + Sync + 'static) {
        self.guys.lock().unwrap().insert(uid.to_string(), Box::new(script));
    }

    /// What `uid` was sent, oldest first.
    fn sent_to(&self, uid: &str) -> Vec<String> {
        self.sent.lock().unwrap().iter().filter(|(to, _)| to == uid).map(|(_, text)| text.clone()).collect()
    }

    fn end(&self, gateway: &str, seat: &str, said: &str) {
        let key = (gateway.to_string(), seat.to_string());
        self.threads.lock().unwrap().entry(key).or_default().push(json!({ "role": "assistant", "content": [{ "type": "text", "text": said }] }));
        if let Some(rooms) = self.rooms.get() {
            rooms.on_bot_event(gateway, "chat", &json!({ "sessionKey": seat, "runId": "native-run", "state": "final" }));
        }
    }

    /// Ends the turn `uid` has under way.
    fn finish(&self, uid: &str) {
        let (gateway, agent) = split_uid(uid).unwrap();
        let seat = format!("agent:{agent}:dashboard:seat");
        let said = self.working.lock().unwrap().remove(&(gateway.to_string(), seat.clone())).expect("that guy isn't working");
        self.end(gateway, &seat, said);
    }
}

impl Bots for Fake {
    fn call(&self, gateway: &str, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let me = self.me.get().and_then(Weak::upgrade).unwrap();
        let (gateway, method) = (gateway.to_string(), method.to_string());
        Box::pin(async move {
            let seat = params.get("sessionKey").and_then(Value::as_str).unwrap_or_default().to_string();
            let agent = keys::split(&seat).map(|(a, _)| a.to_string()).unwrap_or_default();
            let uid = format!("{gateway}/{agent}");
            let key = (gateway.clone(), seat.clone());
            match method.as_str() {
                "sessions.create" => Ok(json!({ "ok": true, "key": format!("agent:{}:dashboard:seat", params["agentId"].as_str().unwrap()) })),
                "chat.history" => Ok(json!({ "messages": me.threads.lock().unwrap().get(&key).cloned().unwrap_or_default() })),
                "chat.abort" => {
                    me.aborted.lock().unwrap().push(uid);
                    me.working.lock().unwrap().remove(&key);
                    Ok(json!({ "ok": true }))
                }
                "chat.send" => {
                    let text = params["message"].as_str().unwrap().to_string();
                    me.sent.lock().unwrap().push((uid.clone(), text.clone()));
                    if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                        me.folders.lock().unwrap().push(cwd.to_string());
                    }
                    me.threads.lock().unwrap().entry(key.clone()).or_default().push(json!({ "role": "user", "content": text }));
                    if params.get("delivery").and_then(Value::as_str) == Some("steer") {
                        return Ok(json!({ "runId": "steered", "status": "started" }));
                    }
                    let reply = (me.guys.lock().unwrap().get(&uid).expect("no such guy"))(&text);
                    match reply {
                        Reply::Fail => return Err(RpcError::new("UNAVAILABLE", "no credits left")),
                        Reply::Later(said) => {
                            me.working.lock().unwrap().insert(key, said);
                        }
                        Reply::Say(said) => {
                            let me = me.clone();
                            tauri::async_runtime::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(5)).await;
                                me.end(&gateway, &seat, said);
                            });
                        }
                    }
                    Ok(json!({ "runId": "receipt-run", "status": "started" }))
                }
                other => Err(unsupported(other)),
            }
        })
    }

    fn guy(&self, gateway: &str, agent: &str) -> Option<String> {
        let uid = format!("{gateway}/{agent}");
        self.guys.lock().unwrap().contains_key(&uid).then(|| {
            let mut name = agent.to_string();
            name[..1].make_ascii_uppercase();
            name
        })
    }

    fn online(&self, gateway: &str) -> bool {
        !self.offline.lock().unwrap().iter().any(|g| g == gateway)
    }

    fn steers(&self, _gateway: &str) -> bool {
        self.steers
    }

    fn is_running(&self, gateway: &str, key: &str) -> bool {
        self.working.lock().unwrap().contains_key(&(gateway.to_string(), key.to_string()))
    }
}

type Events = Arc<Mutex<Vec<(String, Value)>>>;

struct World {
    rooms: RoomsClient,
    fake: Arc<Fake>,
    events: Events,
    dir: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn world_with(steers: bool, look_after: Duration) -> World {
    let dir = std::env::temp_dir().join(format!("lg-rooms-{}", uuid::Uuid::new_v4()));
    let events: Events = Arc::default();
    let seen = events.clone();
    let hooks = Hooks {
        status: Box::new(|_| {}),
        event: Box::new(move |name, payload, _| seen.lock().unwrap().push((name.to_string(), payload.clone()))),
        connected: Box::new(|| {}),
    };
    let rooms = RoomsClient::with(dir.clone(), hooks, look_after);
    let fake = Fake::new(steers);
    let _ = fake.rooms.set(rooms.clone());
    rooms.set_bots(fake.clone());
    rooms.start();
    World { rooms, fake, events, dir }
}

fn world() -> World {
    world_with(false, LOOK_AFTER)
}

impl World {
    async fn room(&self, members: &[&str], listening: &[&str]) -> String {
        let made = self.rooms.call("rooms.create", json!({ "name": "Launch", "members": members })).await.unwrap();
        let id = made["id"].as_str().unwrap().to_string();
        if !listening.is_empty() {
            let attention: serde_json::Map<String, Value> = listening.iter().map(|uid| (uid.to_string(), json!("listening"))).collect();
            self.rooms.call("rooms.update", json!({ "id": id, "attention": attention })).await.unwrap();
        }
        id
    }

    async fn say(&self, id: &str, text: &str) {
        self.rooms.call("chat.send", json!({ "sessionKey": room_key(id), "message": text, "idempotencyKey": uuid::Uuid::new_v4().to_string() })).await.unwrap();
    }

    fn count(&self, name: &str, state: &str) -> usize {
        self.events.lock().unwrap().iter().filter(|(n, p)| n == name && p.get("state").and_then(Value::as_str) == Some(state)).count()
    }

    /// Waits until the room has closed `rounds` rounds.
    async fn quiet(&self, rounds: usize) {
        self.until(|| self.count("chat", "final") >= rounds, "the room to go quiet").await;
    }

    async fn until(&self, done: impl Fn() -> bool, what: &str) {
        for _ in 0..600 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("waited too long for {what}; events: {:#?}", self.events.lock().unwrap());
    }

    /// The transcript as "Who: text" (and "→ uid" when a line addresses someone).
    async fn transcript(&self, id: &str) -> Vec<String> {
        let history = self.rooms.call("chat.history", json!({ "sessionKey": room_key(id) })).await.unwrap();
        history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                let text = text_of(m).unwrap_or_default();
                match m["role"].as_str().unwrap() {
                    "system" => format!("* {text}"),
                    "user" => format!("You: {text}"),
                    _ => format!("{}: {text}", m["room"]["from"].as_str().unwrap()),
                }
            })
            .collect()
    }
}

const SCOUT: &str = "attic/scout";
const PATCH: &str = "claude/patch";
const VERA: &str = "garage/vera";

/// The room from docs/GROUP-CHATS.md's walkthrough.
fn launch(w: &World) {
    w.fake.guy(SCOUT, |_| Reply::Say("It's the date parser. @Patch it's in signup/validate.ts, can you fix it?"));
    w.fake.guy(PATCH, |asked| {
        if asked.contains("is it deployed") {
            Reply::Say("Not yet, the deploy needs your approval.")
        } else if asked.contains("invite flow") {
            Reply::Say("Good catch, fixed there too.")
        } else {
            Reply::Say("Fixed and pushed, with a test for Safari's format.")
        }
    });
    w.fake.guy(VERA, |asked| {
        if asked.contains("is it deployed") {
            Reply::Say("NO_REPLY")
        } else {
            Reply::Say("The same parser is used in the invite flow, which will fail the same way. @Patch")
        }
    });
}

#[tokio::test]
async fn the_walkthrough_plays_out_and_stops() {
    let w = world();
    launch(&w);
    let id = w.room(&[SCOUT, PATCH, VERA], &[VERA]).await;

    w.say(&id, "signups are failing on Safari since this morning").await;
    w.quiet(1).await;
    assert_eq!(
        w.transcript(&id).await,
        [
            "You: signups are failing on Safari since this morning",
            "attic/scout: It's the date parser. @Patch it's in signup/validate.ts, can you fix it?",
            "claude/patch: Fixed and pushed, with a test for Safari's format.",
            "garage/vera: The same parser is used in the invite flow, which will fail the same way. @Patch",
            "claude/patch: Good catch, fixed there too.",
        ]
    );
    // Scout led, was never addressed again, and isn't listening: one turn.
    assert_eq!(w.fake.sent_to(SCOUT).len(), 1);

    // With nobody named, it goes to whoever spoke last. Vera looks, and passes.
    w.say(&id, "thanks. is it deployed?").await;
    w.quiet(2).await;
    assert_eq!(w.transcript(&id).await[5..], ["You: thanks. is it deployed?", "claude/patch: Not yet, the deploy needs your approval."]);
    assert_eq!(w.fake.sent_to(SCOUT).len(), 1);
    assert_eq!(w.fake.sent_to(VERA).len(), 2);
    assert_eq!(w.count("chat", "status"), 2);

    // The roster's view of it.
    let rows = w.rooms.call("sessions.list", json!({})).await.unwrap();
    assert_eq!(rows["sessions"][0]["lastMessagePreview"], "Patch: Not yet, the deploy needs your approval.");
    assert_eq!(rows["sessions"][0]["unread"], true);
    assert_eq!(rows["sessions"][0]["hasActiveRun"], false);
    assert_eq!(w.rooms.seat_room("claude", "agent:patch:dashboard:seat"), Some((id.clone(), "Launch".into())));
    assert_eq!(w.rooms.seat_room("attic", "agent:patch:dashboard:seat"), None);
}

#[tokio::test]
async fn a_guy_is_told_the_rules_once_and_only_what_it_missed() {
    let w = world();
    launch(&w);
    let id = w.room(&[SCOUT, PATCH, VERA], &[]).await;
    w.say(&id, "signups are failing on Safari").await;
    w.quiet(1).await;
    w.say(&id, "@Patch is it deployed?").await;
    w.quiet(2).await;

    let patch = w.fake.sent_to(PATCH);
    assert_eq!(patch.len(), 2);
    assert!(patch[0].starts_with("[Room \"Launch\" #"));
    assert!(patch[0].contains("[How this room works]"));
    assert!(patch[0].contains("Scout (@Scout), the lead; Patch (@Patch), which is you; Vera (@Vera)"));
    assert!(patch[0].contains("User: signups are failing on Safari\nScout: It's the date parser."));
    assert!(patch[0].ends_with("[Now] Scout addressed you. Answer in the room. If nothing is needed from you, reply exactly NO_REPLY."));
    // The second time: no rules, not its own reply, not what it was already shown.
    assert!(!patch[1].contains("[How this room works]"));
    assert!(!patch[1].contains("date parser") && !patch[1].contains("Fixed and pushed"));
    assert!(patch[1].contains("[Since your last turn]\nUser: @Patch is it deployed?\n"));
    assert!(patch[1].contains("[Now] User addressed you."));
    // The lead's first turn says why it has it.
    assert!(w.fake.sent_to(SCOUT)[0].contains("you are its lead"));
}

#[tokio::test]
async fn guys_that_keep_addressing_each_other_are_paused_until_you_say() {
    let w = world();
    w.fake.guy("a/ping", |_| Reply::Say("@Pong your turn"));
    w.fake.guy("a/pong", |_| Reply::Say("@Ping your turn"));
    let id = w.room(&["a/ping", "a/pong"], &[]).await;
    w.say(&id, "go").await;
    w.quiet(1).await;
    // You, then the lead's answer, then MAX_HOPS answers to each other; the next one is held.
    assert_eq!(w.transcript(&id).await.len() as u32, 1 + 1 + MAX_HOPS);
    let state = w.rooms.call("rooms.state", json!({ "id": id })).await.unwrap();
    assert_eq!(state["paused"], true);
    assert_eq!(state["working"], json!([]));
    assert_eq!(w.rooms.call("agents.list", json!({})).await.unwrap()["agents"][0]["room"]["paused"], true);

    w.rooms.call("rooms.continue", json!({ "id": id })).await.unwrap();
    w.quiet(2).await;
    assert_eq!(w.transcript(&id).await.len() as u32, 1 + 1 + 2 * MAX_HOPS + 1);
}

#[tokio::test]
async fn stopping_a_room_stops_its_guys_and_drops_what_they_say_after() {
    let w = world();
    w.fake.guy(SCOUT, |_| Reply::Later("@Patch here's a late idea"));
    w.fake.guy(PATCH, |_| Reply::Say("on it"));
    let id = w.room(&[SCOUT, PATCH], &[]).await;
    w.say(&id, "think about it").await;
    w.until(|| w.fake.is_running("attic", "agent:scout:dashboard:seat"), "Scout to start").await;
    let state = w.rooms.call("rooms.state", json!({ "id": id })).await.unwrap();
    assert_eq!(state["working"], json!([{ "uid": SCOUT, "why": "lead" }]));

    w.rooms.call("chat.abort", json!({ "sessionKey": room_key(&id) })).await.unwrap();
    w.until(|| w.fake.aborted.lock().unwrap().contains(&SCOUT.to_string()), "Scout to be stopped").await;
    assert_eq!(w.count("chat", "aborted"), 1);
    assert_eq!(w.transcript(&id).await, ["You: think about it"]);
    assert!(w.fake.sent_to(PATCH).is_empty());
}

#[tokio::test]
async fn offline_and_failing_guys_are_said_so_and_the_room_goes_on() {
    let w = world();
    w.fake.guy(SCOUT, |_| Reply::Say("@Patch and @Vera, one each"));
    w.fake.guy(PATCH, |_| Reply::Fail);
    w.fake.guy(VERA, |_| Reply::Say("done"));
    let id = w.room(&[SCOUT, PATCH, VERA], &[]).await;
    w.fake.offline.lock().unwrap().push("garage".into());
    w.say(&id, "split this").await;
    w.quiet(1).await;
    let said = w.transcript(&id).await;
    assert_eq!(said.len(), 4, "{said:?}");
    assert!(said.contains(&"* Vera is offline, so it missed this.".to_string()), "{said:?}");
    assert!(said.contains(&"* Patch couldn't take its turn: no credits left".to_string()), "{said:?}");
}

#[tokio::test]
async fn writing_to_a_busy_guy_reaches_it_in_its_turn_or_right_after() {
    for steers in [true, false] {
        let w = world_with(steers, LOOK_AFTER);
        w.fake.guy(PATCH, |asked| if asked.contains("[While you were working]") { unreachable!() } else { Reply::Later("done") });
        w.fake.guy(SCOUT, |_| Reply::Say("ok"));
        let id = w.room(&[PATCH, SCOUT], &[]).await;
        w.say(&id, "fix the parser").await;
        w.until(|| w.fake.is_running("claude", "agent:patch:dashboard:seat"), "Patch to start").await;
        w.say(&id, "and the invite flow").await;
        if steers {
            w.until(|| w.fake.sent_to(PATCH).len() == 2, "the nudge").await;
            let nudge = &w.fake.sent_to(PATCH)[1];
            assert!(nudge.contains("[While you were working]\nUser: and the invite flow\n"), "{nudge}");
            // Give the room a moment to note it was taken, then let Patch finish: no second turn.
            tokio::time::sleep(Duration::from_millis(50)).await;
            w.fake.finish(PATCH);
            w.quiet(1).await;
            assert_eq!(w.fake.sent_to(PATCH).len(), 2);
        } else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(w.fake.sent_to(PATCH).len(), 1);
            w.fake.finish(PATCH);
            // Owed a turn: it gets it now, with what you wrote meanwhile.
            w.until(|| w.fake.sent_to(PATCH).len() == 2, "Patch's second turn").await;
            assert!(w.fake.sent_to(PATCH)[1].contains("[Since your last turn]\nUser: and the invite flow\n"));
            w.fake.finish(PATCH);
            w.quiet(1).await;
        }
        assert_eq!(w.count("chat", "status"), 1, "one round, however many messages");
    }
}

#[tokio::test]
async fn a_listener_looks_while_a_long_turn_runs() {
    let w = world_with(false, Duration::from_millis(40));
    w.fake.guy(PATCH, |_| Reply::Later("done"));
    w.fake.guy(VERA, |_| Reply::Say("While Patch works: the invite flow has the same bug."));
    let id = w.room(&[PATCH, VERA], &[VERA]).await;
    w.say(&id, "fix the parser").await;
    w.until(|| w.fake.sent_to(VERA).len() == 1, "Vera's look").await;
    assert!(w.fake.sent_to(VERA)[0].contains("The room has gone quiet"));
    w.until(|| w.events.lock().unwrap().iter().filter(|(n, _)| n == "session.message").count() == 2, "Vera's message").await;
    assert_eq!(w.count("chat", "final"), 0, "Patch is still working");
    w.fake.finish(PATCH);
    w.quiet(1).await;
    assert_eq!(w.fake.sent_to(VERA).len(), 1, "one look per message of yours");
}

#[tokio::test]
async fn rooms_and_their_talk_outlive_a_restart() {
    let w = world();
    launch(&w);
    let id = w.room(&[SCOUT, PATCH, VERA], &[VERA]).await;
    w.say(&id, "signups are failing on Safari").await;
    w.quiet(1).await;
    let before = w.transcript(&id).await;

    let again = RoomsClient::with(w.dir.clone(), Hooks { status: Box::new(|_| {}), event: Box::new(|_, _, _| {}), connected: Box::new(|| {}) }, LOOK_AFTER);
    let agents = again.call("agents.list", json!({})).await.unwrap();
    let room = &agents["agents"][0];
    assert_eq!(room["name"], "Launch");
    assert_eq!(room["room"]["lead"], SCOUT);
    assert_eq!(room["room"]["members"][2], json!({ "uid": VERA, "name": "Vera", "handle": "Vera", "attention": "listening", "seat": "agent:vera:dashboard:seat", "folder": null }));
    let history = again.call("chat.history", json!({ "sessionKey": room_key(&id) })).await.unwrap();
    assert_eq!(history["messages"].as_array().unwrap().len(), before.len());
    assert_eq!(history["messages"][1]["room"], json!({ "from": SCOUT, "to": [PATCH] }));
}

#[tokio::test]
async fn members_come_and_go() {
    let w = world();
    launch(&w);
    let id = w.room(&[SCOUT, PATCH], &[]).await;
    assert_eq!(w.rooms.call("rooms.create", json!({ "members": [SCOUT] })).await.unwrap_err().message, "A room needs at least two guys.");
    assert_eq!(w.rooms.call("rooms.update", json!({ "id": id, "remove": [PATCH] })).await.unwrap_err().message, "A room needs at least two guys.");
    assert!(w.rooms.call("rooms.update", json!({ "id": id, "add": ["attic/nobody"] })).await.is_err());

    w.say(&id, "signups are failing on Safari").await;
    w.quiet(1).await;
    // Vera joins and takes the lead; Scout leaves. Everyone hears who's here now.
    w.rooms.call("rooms.update", json!({ "id": id, "add": [VERA], "remove": [SCOUT], "lead": VERA, "name": "Fixes" })).await.unwrap();
    w.say(&id, "@Patch is it deployed?").await;
    w.quiet(2).await;
    let patch = w.fake.sent_to(PATCH);
    assert!(patch[1].starts_with("[Room \"Fixes\" #"));
    assert!(patch[1].contains("Members: User, who everyone answers to; Patch (@Patch), which is you; Vera (@Vera), the lead."));
    // Someone who left isn't addressed any more, and their name still shows on what they said.
    assert_eq!(w.fake.sent_to(SCOUT).len(), 1);
    assert_eq!(w.transcript(&id).await[1], "attic/scout: It's the date parser. @Patch it's in signup/validate.ts, can you fix it?");

    // A guy told to work in another folder takes a new seat there, and is told where things stand.
    w.rooms.call("rooms.update", json!({ "id": id, "folder": { PATCH: "/tmp/shop" } })).await.unwrap();
    let members = w.rooms.call("agents.list", json!({})).await.unwrap()["agents"][0]["room"]["members"].clone();
    assert_eq!(members[0], json!({ "uid": PATCH, "name": "Patch", "handle": "Patch", "attention": "oncall", "seat": null, "folder": "/tmp/shop" }));
    w.say(&id, "@Patch is it deployed?").await;
    w.quiet(3).await;
    assert!(w.fake.sent_to(PATCH)[2].contains("[How this room works]"));
    assert_eq!(*w.fake.folders.lock().unwrap(), ["/tmp/shop"]);

    w.rooms.call("rooms.delete", json!({ "id": id })).await.unwrap();
    assert_eq!(w.rooms.call("agents.list", json!({})).await.unwrap()["agents"], json!([]));
    assert!(w.rooms.call("chat.history", json!({ "sessionKey": room_key(&id) })).await.is_err());
}
