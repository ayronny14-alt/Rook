// how a scheduled-task result gets delivered. currently: desktop notification
// or silent (just logged). telegram/email are stubs wired for later.

use tracing::warn;

pub fn notify(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.chars().take(300).collect::<String>();
    // run in a blocking thread so we don't touch the async runtime
    std::thread::spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.summary(&format!("Rook: {}", title)).body(&body);
        #[cfg(target_os = "windows")]
        n.app_id("com.svrn.rook");
        if let Err(e) = n.show() {
            warn!("notification failed: {}", e);
        }
    });
}

pub fn dispatch(channel: &str, title: &str, body: &str) {
    match channel {
        "notification" => notify(title, body),
        "silent" => { /* log-only path, nothing to do */ }
        // tg/email hooks go here later
        other => {
            warn!(
                "unknown output channel {:?} - falling back to notification",
                other
            );
            notify(title, body);
        }
    }
}
