//! Sign in to a consumer drive and print the refresh token, for the live tests
//! (`tests/live.rs`) and for trying a provider by hand:
//!
//! ```sh
//! cargo run -p lightcraft-cloud --example cloud-signin -- gdrive <client id> [<client secret>]
//! cargo run -p lightcraft-cloud --example cloud-signin -- onedrive <client id>
//! cargo run -p lightcraft-cloud --example cloud-signin -- dropbox <app key>
//! ```
//!
//! - Google: a Google Cloud project with the Drive API enabled and an OAuth client of type
//!   "Desktop app"; access is limited to the files LightCraft creates (`drive.file`).
//! - Microsoft: an app registration with a "Mobile and desktop applications" platform and the
//!   loopback redirect URI `http://127.0.0.1` (any port); access is limited to the app's folder.
//! - Dropbox: an app with "App folder" access, PKCE allowed, and the redirect URI
//!   `http://127.0.0.1:53682` registered.
//!
//! Open the printed URL, allow access, and the browser comes back to this program.

use std::time::Duration;

use lightcraft_cloud::Net;
use lightcraft_cloud::crypto::OsRandom;
use lightcraft_cloud::dropbox::DEFAULT_REDIRECT_PORT;
use lightcraft_cloud::oauth::{LoopbackFlow, OAuthClient};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (provider, client_id) = match (args.first(), args.get(1)) {
        (Some(p), Some(id)) => (p.as_str(), id.clone()),
        _ => {
            eprintln!("usage: cloud-signin gdrive|onedrive|dropbox <client id> [<client secret>]");
            std::process::exit(2);
        }
    };
    let (client, var) = match provider {
        "gdrive" => (OAuthClient::google_drive(client_id, args.get(2).cloned()), "LIGHTCRAFT_TEST_GDRIVE_REFRESH_TOKEN"),
        "onedrive" => (OAuthClient::onedrive(client_id), "LIGHTCRAFT_TEST_ONEDRIVE_REFRESH_TOKEN"),
        "dropbox" => (OAuthClient::dropbox(client_id, DEFAULT_REDIRECT_PORT), "LIGHTCRAFT_TEST_DROPBOX_REFRESH_TOKEN"),
        other => {
            eprintln!("unknown provider {other:?}: use gdrive, onedrive or dropbox");
            std::process::exit(2);
        }
    };
    let flow = LoopbackFlow::start(client, &mut OsRandom)?;
    println!("Open this URL in your browser and allow access:\n\n{}\n", flow.authorize_url());
    let tokens = flow.wait(&Net::default(), Duration::from_secs(300))?;
    println!("{var}={}", tokens.refresh_token.unwrap_or_default());
    Ok(())
}
