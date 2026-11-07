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
        let gain = db_to_gain(ctx.params.level.value());
        ctx.io.for_each_sample(|s| *s *= gain);
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
        a.set_track(TrackInfo { name: Some("Vocal".into()), color: Some([224, 80, 90]), index: Some(0) });
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
}
