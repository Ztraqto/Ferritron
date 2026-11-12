//! Instances that know their track and each other (Ferritron 1.2).
//!
//! Every instance applies its `level`. Its editor shows all instances of the
//! plugin in the process with their tracks, and moves any of them: the edit
//! goes to that instance's host parameter, so the DAW records it as
//! automation on the right track, and wakes the instance if it slept. A note
//! per instance lives in its store and is saved with the project.

use ferritron::prelude::*;
use ferritron::{InstanceHandle, instances};
use serde_json::{Value, json};

#[derive(Default)]
struct Instances;

/// What the editor shows: every instance with its track, level and note.
fn overview(me: &InstanceHandle) -> Value {
    let list: Vec<Value> = instances::<Instances>()
        .iter()
        .map(|i| {
            let track = i.track();
            json!({
                "id": i.id().to_string(),
                "me": i == me,
                "track": track.name,
                "color": track.color,
                "level": i.value("level"),
                "note": i.store().get_str("note"),
            })
        })
        .collect();
    json!({ "kind": "overview", "instances": list })
}

/// Messages from the editor (`ferritron.send`), on the UI thread.
fn on_message(text: &str, me: &InstanceHandle) {
    let Ok(message) = serde_json::from_str::<Value>(text) else { return };
    match message["kind"].as_str() {
        Some("note") => me.store().set("note", message["text"].as_str().unwrap_or_default()),
        Some("level") => {
            let (Some(id), Some(db)) = (message["id"].as_str(), message["db"].as_f64()) else { return };
            if let Some(target) = instances::<Instances>().into_iter().find(|i| i.id().to_string() == id) {
                target.edit("level", db);
            }
        }
        _ => {}
    }
    me.post(overview(me).to_string());
}

#[ferritron::plugin]
impl Process for Instances {
    fn init(&mut self, instance: &InstanceHandle) {
        instance.on_message(on_message);
    }

    fn process(&mut self, ctx: &mut Context) {
        let gain = db_to_gain(ctx.params().level.value());
        ctx.io().for_each_sample(|s| *s *= gain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::TrackInfo;
    use ferritron::testing::Harness;

    #[test]
    fn instances_see_each_other_and_edit_each_other() {
        let mut a = Harness::<Instances>::new(48_000.0, 256);
        let mut b = Harness::<Instances>::new(48_000.0, 256);
        a.set_track(TrackInfo { name: Some("Vocal".into()), color: Some([224, 80, 90]), index: Some(0), ..Default::default() });
        b.set_track(TrackInfo { name: Some("Guitar".into()), ..Default::default() });

        let reply = a.send_message(r#"{"kind":"note","text":"lead take 3"}"#);
        let overview: Value = serde_json::from_str(reply.last().unwrap()).unwrap();
        let list = overview["instances"].as_array().unwrap();
        let mine = list.iter().find(|i| i["me"] == true).unwrap();
        assert_eq!(mine["track"], "Vocal");
        assert_eq!(mine["color"], json!([224, 80, 90]));
        assert_eq!(mine["note"], "lead take 3");
        let guitar = list.iter().find(|i| i["track"] == "Guitar").unwrap();

        // From A's editor, turn B down.
        let id = guitar["id"].as_str().unwrap();
        a.send_message(&format!(r#"{{"kind":"level","id":"{id}","db":-12}}"#));
        assert_eq!(b.params().level.value(), -12.0);
        let mut buf = [1.0f32; 8];
        assert!(b.process(&mut [&mut buf]));
        assert!((buf[0] - db_to_gain(-12.0)).abs() < 1e-4);

        // The note travels with the project; the ID too.
        let saved = a.save_state();
        let id_a = a.instance().id();
        drop(a);
        let mut reopened = Harness::<Instances>::new(48_000.0, 256);
        assert!(reopened.load_state(&saved));
        assert_eq!(reopened.instance().store().get_str("note").as_deref(), Some("lead take 3"));
        assert_eq!(reopened.instance().id(), id_a);
    }

    /// E4 / E5: a watch sees instances come and go, change and move, and editors open; one
    /// editor at a time owns the shared view.
    #[test]
    fn a_watch_follows_instances_and_editors_without_a_thread_of_our_own() {
        use ferritron::{InstanceEvent, SingletonView};
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<(String, ferritron::InstanceId)>>> = Arc::default();
        let log = seen.clone();
        let _watch = ferritron::instances_watch(move |e| {
            let kind = match e {
                InstanceEvent::Added(_) => "added",
                InstanceEvent::Removed { .. } => "removed",
                InstanceEvent::Changed(_) => "changed",
                InstanceEvent::Params(_) => "params",
                InstanceEvent::EditorOpened(_) => "opened",
                InstanceEvent::EditorClosed(_) => "closed",
                InstanceEvent::EditorFocused(_) => "focused",
                _ => "other",
            };
            log.lock().unwrap().push((kind.to_string(), e.id()));
        });
        let wait_for = |kind: &str, id: ferritron::InstanceId| {
            let start = std::time::Instant::now();
            while start.elapsed().as_secs() < 3 {
                if seen.lock().unwrap().iter().any(|(k, i)| k == kind && *i == id) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("no '{kind}' event for {id}: {:?}", seen.lock().unwrap());
        };
        let mut a = Harness::<Instances>::new(48_000.0, 256);
        let mut b = Harness::<Instances>::new(48_000.0, 256);
        let (ia, ib) = (a.instance().id(), b.instance().id());
        wait_for("added", ia);
        wait_for("added", ib);
        a.set_track(TrackInfo { name: Some("Bass".into()), kind: Some(ferritron::TrackKind::Bus), location: Some(ferritron::PluginLocation::PostFader), channels: Some(2), ..Default::default() });
        wait_for("changed", ia);
        let t = a.instance().track();
        assert_eq!((t.kind, t.location, t.channels), (Some(ferritron::TrackKind::Bus), Some(ferritron::PluginLocation::PostFader), Some(2)), "E11: what hosts tell of the mixer");
        b.set_param("level", -3.0);
        wait_for("params", ib);

        // Editors: B opens first, then A. B owns the shared view until A claims it or B closes.
        b.set_editor_open(true);
        a.set_editor_open(true);
        wait_for("opened", ia);
        assert_eq!(ferritron::editors::<Instances>().iter().map(|h| h.id()).filter(|i| *i == ia || *i == ib).collect::<Vec<_>>(), vec![ib, ia]);
        assert!(SingletonView::is_owner(b.instance()) || SingletonView::owner_of(b.instance().plugin_id()).is_some_and(|o| o.id() != ia));
        SingletonView::claim(a.instance());
        assert!(SingletonView::is_owner(a.instance()));
        a.set_editor_open(false);
        wait_for("closed", ia);
        assert!(!SingletonView::is_owner(a.instance()), "a closed editor hands the view on");
        b.set_editor_open(false);

        drop(b);
        wait_for("removed", ib);
    }
}

/// G5: parameter IDs, order and class ID are those of 1.4.0, and a state saved by 1.4.0 loads.
#[cfg(test)]
#[test]
fn loads_state_saved_by_1_4_0() {
    let n = ferritron::testing::check_fixture::<Instances>(include_str!("../../../tests/fixtures/1.4.0/instances.json")).unwrap_or_else(|e| panic!("{e}"));
    assert!(n > 0);
}
