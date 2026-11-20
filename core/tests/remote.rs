//! A remote control (phone, tablet) dials a computer that accepts it next to its paired
//! peer. Needs local discovery (mDNS) or the internet, so it is ignored by default:
//! `cargo test -p kvm-core --test remote -- --ignored`

use std::collections::HashSet;
use std::time::Duration;

use kvm_core::{Event, Node};
use tokio::sync::mpsc::UnboundedReceiver;

async fn node(name: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("kvm-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Node::bind(&dir, name).await.unwrap()
}

async fn next(events: &mut UnboundedReceiver<Event>, want: impl Fn(&Event) -> bool) -> Event {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("link ended");
            if want(&event) {
                return event;
            }
        }
    })
    .await
    .expect("timed out")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn remote_control_connects_and_types() {
    let computer = node("computer").await;
    let phone = node("phone").await;
    let other = node("other").await;

    // The computer keeps its paired peer and accepts the phone on the side.
    let (_peer_link, _) = computer.link(other.id());
    let (_remotes, mut computer_events) = computer.accept_remotes(HashSet::from([phone.id()]));
    let (phone_link, mut phone_events) = phone.dial(computer.id());

    let Event::Connected { name, peer } = next(&mut phone_events, |e| matches!(e, Event::Connected { .. })).await else {
        unreachable!()
    };
    assert_eq!((name.as_str(), peer), ("computer", computer.id()));
    let Event::Connected { peer, .. } = next(&mut computer_events, |e| matches!(e, Event::Connected { .. })).await else {
        unreachable!()
    };
    assert_eq!(peer, phone.id());

    phone_link.enter();
    next(&mut computer_events, |e| *e == Event::Entered).await;
    phone_link.send_text("ciao è".into());
    assert_eq!(next(&mut computer_events, |e| matches!(e, Event::Text(_))).await, Event::Text("ciao è".into()));

    // It shares its clipboard, but doesn't lock the computer or send it files.
    let downloads = std::env::temp_dir().join(format!("kvm-test-downloads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&downloads);
    kvm_core::set_download_dir(downloads.clone());
    let file = std::env::temp_dir().join(format!("kvm-test-sent-{}.txt", std::process::id()));
    std::fs::write(&file, b"hello").unwrap();
    phone_link.send_lock();
    phone_link.send_files(vec![file.clone()], false);
    phone_link.send_clipboard(b"clip".to_vec());
    assert_eq!(next(&mut computer_events, |e| matches!(e, Event::Clipboard(_))).await, Event::Clipboard(b"clip".to_vec()));
    let done = next(&mut phone_events, |e| matches!(e, Event::FileSendDone { .. })).await;
    assert!(matches!(done, Event::FileSendDone { error: Some(_), .. }), "{done:?}");
    let mut seen = Vec::new();
    while let Ok(event) = computer_events.try_recv() {
        seen.push(event);
    }
    assert!(!seen.iter().any(|e| matches!(e, Event::LockRequested | Event::FileReceived { .. })), "{seen:?}");
    assert!(!downloads.join(file.file_name().unwrap()).exists());

    // Someone the computer doesn't trust is refused.
    let stranger = node("stranger").await;
    let (_stranger_link, mut stranger_events) = stranger.dial(computer.id());
    let refused = tokio::time::timeout(Duration::from_secs(5), next(&mut stranger_events, |e| matches!(e, Event::Connected { .. })));
    assert!(refused.await.is_err(), "an untrusted device connected");
}
