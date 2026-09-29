//! Standalone: clear a Cloudflare challenge and print the session JSON.
//!
//!   picaro-cf-solve https://flacmania.biz/ [--save config/cf-cookies.json]

use picaro_webview::solve_cf;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut save: Option<String> = None;
    if let Some(i) = args.iter().position(|a| a == "--save") {
        save = args.get(i + 1).cloned();
        args.drain(i..=i + 1);
    }
    let Some(url) = args.first().cloned() else {
        eprintln!("usage: picaro-cf-solve <url> [--save <cf-cookies.json>]");
        std::process::exit(2);
    };
    let timeout = std::env::var("PICARO_CF_SOLVE_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60u64);
    match solve_cf(&url, timeout).await {
        Ok(session) => {
            let json = serde_json::to_string_pretty(&session).unwrap();
            if let Some(path) = save {
                // Merge into the cookie store keyed by host.
                let mut store: serde_json::Map<String, serde_json::Value> =
                    std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default();
                store.insert(
                    session.host.clone(),
                    serde_json::to_value(&session).unwrap(),
                );
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&path, serde_json::to_string_pretty(&store).unwrap()).unwrap();
                eprintln!("saved session for {} to {path}", session.host);
            }
            println!("{json}");
        }
        Err(e) => {
            eprintln!("solve failed: {e}");
            std::process::exit(1);
        }
    }
}
